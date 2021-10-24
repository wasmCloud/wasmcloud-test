#![cfg(not(target_arch = "wasm32"))]
//!
//! simple test harness to load a capability provider and test it
//!
use crate::testing::TestResult;
use anyhow::anyhow;
use async_trait::async_trait;
use futures::{future::BoxFuture, Stream};
use serde::Serialize;
use std::{fs, io::Write, ops::Deref, path::PathBuf, sync::Arc};
use tokio::sync::OnceCell;
use toml::value::Value as TomlValue;
use wasmbus_rpc::{
    core::{HealthCheckRequest, HealthCheckResponse, HostData, LinkDefinition, WasmCloudEntity},
    provider::ratsio::{ops::Message as NatsMessage, NatsClient, NatsSid},
    Context, Message, RpcError, RpcResult, SendOpts, Transport,
};

pub type SimpleValueMap = std::collections::HashMap<String, String>;
pub type TomlMap = std::collections::BTreeMap<String, toml::Value>;
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

const DEFAULT_START_DELAY_SEC: u64 = 1;
const DEFAULT_RPC_TIMEOUT_MILLIS: u64 = 2000;
const DEFAULT_NATS_URL: &str = "127.0.0.1:4222";
// use a unique lattice prefix to separate test traffic
const TEST_LATTICE_PREFIX: &str = "TEST";
const TEST_HOST_ID: &str = "NwasmCloudTestProvider0000000000000000000000000000000000";

static ONCE: OnceCell<Provider> = OnceCell::const_new();
pub type TestFunc = fn() -> BoxFuture<'static, RpcResult<()>>;

fn to_value_map<T: Serialize>(data: &T) -> RpcResult<SimpleValueMap> {
    let mut map = SimpleValueMap::default();
    let json = serde_json::to_string(data)
        .map_err(|e| RpcError::Ser(format!("invalid 'values' map: {}", e)))?;
    let b64 = base64::encode_config(&json, base64::STANDARD_NO_PAD);
    map.insert("config_b64".to_string(), b64);
    Ok(map)
}

#[derive(Clone)]
pub struct Provider {
    inner: Arc<ProviderProcess>,
}

/// info needed to test a capability provider process. If this structure goes out of scope,
/// the provider will exit
pub struct ProviderProcess {
    pub file: std::fs::File,
    pub host_data: HostData,
    pub path: PathBuf,
    pub proc: std::process::Child,
    pub config: TomlMap,
    pub nats_client: Arc<NatsClient>,
    pub rpc_client: wasmbus_rpc::RpcClient,
    pub timeout_ms: std::sync::Mutex<u64>,
}

impl ProviderProcess {
    /// generate the `origin` field for an Invocation. To the receiving provider,
    /// the origin field looks like an actor
    pub fn origin(&self) -> WasmCloudEntity {
        WasmCloudEntity {
            contract_id: "".to_string(),
            link_name: "".to_string(),
            public_key: self.host_data.host_id.to_string(),
        }
    }

    /// generate the `target` field for sending an Invocation to the provider
    pub fn target(&self) -> WasmCloudEntity {
        WasmCloudEntity {
            contract_id: "".to_string(),
            link_name: self.host_data.link_name.clone(),
            public_key: self.host_data.provider_key.clone(),
        }
    }

    /// link the test to the provider
    pub async fn link_to_test(&self, values: SimpleValueMap) -> Result<(), anyhow::Error> {
        let topic = format!(
            "wasmbus.rpc.{}.{}.{}.linkdefs.put",
            &self.host_data.lattice_rpc_prefix,
            &self.host_data.provider_key,
            &self.host_data.link_name,
        );
        let origin = self.origin();
        let ld = LinkDefinition {
            actor_id: origin.public_key.clone(),
            contract_id: self
                .config
                .get("contract_id")
                .and_then(|v| v.as_str())
                .unwrap_or("wasmcloud:example")
                .to_string(),
            link_name: self.host_data.link_name.clone(),
            provider_id: self.host_data.provider_key.clone(),
            values,
        };
        let bytes = serde_json::to_vec(&ld)?;
        self.rpc_client.publish(&topic, &bytes).await?;

        Ok(())
    }

    /// send a health check
    pub async fn health_check(&self) -> Result<(), anyhow::Error> {
        let topic = format!(
            "wasmbus.rpc.{}.{}.{}.health",
            &self.host_data.lattice_rpc_prefix,
            &self.host_data.provider_key,
            &self.host_data.link_name,
        );
        let resp: HealthCheckResponse = self
            .send_ctl_json(&topic, HealthCheckRequest::default())
            .await?;
        if !resp.healthy {
            return Err(anyhow!("provider returned unhealthy"));
        }
        Ok(())
    }

    /// send an rpc message to the provider
    pub async fn send_rpc(&self, message: Message<'_>) -> RpcResult<Vec<u8>> {
        match self
            .rpc_client
            //.send_timeout(self.origin(), self.target(), message, std::time::Duration::from_millis(self.get_timeout_ms()))
            .send(self.origin(), self.target(), message)
            .await
        {
            Err(e) => {
                eprintln!("ProviderTest: rpc returned error: {}", e);
                Err(e)
            }
            Ok(v) => Ok(v),
        }
    }

    /// subscribe to rpc messages from the provider using the established link
    pub async fn subscribe_rpc(
        &self,
    ) -> RpcResult<(NatsSid, impl Stream<Item = NatsMessage> + Send + Sync)> {
        let subject = wasmbus_rpc::rpc_topic(&self.origin(), &self.host_data.lattice_rpc_prefix);
        self.nats_client
            .subscribe(subject)
            .await
            .map_err(|e| wasmbus_rpc::RpcError::Nats(e.to_string()))
    }

    /// send a control message to the provider:put link, get link, or shutdown
    pub async fn send_ctl_json<Arg, Resp>(
        &self,
        topic: &str,
        data: Arg,
    ) -> Result<Resp, anyhow::Error>
    where
        Arg: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let bytes = serde_json::to_vec(&data)?;
        let resp_bytes = self.rpc_client.request(topic, &bytes).await?;
        let resp = serde_json::from_slice::<Resp>(&resp_bytes)?;
        Ok(resp)
    }

    /// Send shutdown signal to provider process
    pub async fn shutdown(&self) -> Result<(), anyhow::Error> {
        let shutdown_topic = format!(
            "wasmbus.rpc.{}.{}.{}.shutdown",
            &self.host_data.lattice_rpc_prefix,
            &self.host_data.provider_key,
            self.host_data.link_name
        );
        eprintln!(
            "Sending shutdown to provider {}",
            &self.host_data.provider_key
        );
        self.rpc_client.publish(&shutdown_topic, b"").await?;
        Ok(())
    }

    /*
    fn get_timeout_ms(&self) -> u64 {
        let lock = self.timeout_ms.try_lock();
        if let Ok(rg) = lock {
            *rg
        } else {
            // only if mutex was poisioned, which would only happen
            // if somebody paniced while holding this mutex.
            // but test threads stop after a panic, so this should never happen
            DEFAULT_RPC_TIMEOUT_MILLIS
        }
    }
     */
}

#[async_trait]
impl Transport for Provider {
    async fn send(
        &self,
        _ctx: &Context,
        message: Message<'_>,
        _opts: Option<SendOpts>,
    ) -> std::result::Result<Vec<u8>, RpcError> {
        self.inner.send_rpc(message).await
    }

    /// sets the time period for an expected response to rpc messages,
    /// after which an RpcError::Timeout will occur.
    fn set_timeout(&self, interval: std::time::Duration) {
        let lock = self.timeout_ms.try_lock();
        if let Ok(mut rg) = lock {
            *rg = interval.as_millis() as u64
        } else {
            // only if mutex was poisioned, which would only happen
            // if somebody paniced while holding this mutex.
            // but test threads stop after a panic, so this should never happen
        }
    }
}

impl Deref for Provider {
    type Target = ProviderProcess;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub(crate) fn nats_url(config: &TomlMap) -> String {
    config
        .get("nats_url")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_NATS_URL)
        .to_string()
}

/// load toml configuration. looks for environment variable "PROVIDER_TEST_CONFIG",
/// otherwise loads defaults.
pub fn load_config() -> Result<TomlMap, anyhow::Error> {
    let path = if let Ok(path) = std::env::var("PROVIDER_TEST_CONFIG") {
        PathBuf::from(path)
    } else {
        PathBuf::from("./provider_test_config.toml")
    };
    let data = if !path.is_file() {
        Err(anyhow!(
                "Missing configuration file '{}'. Config file should be 'provider_test_config.toml' in the current directory, or a .toml file whose path is in the environment variable 'PROVIDER_TEST_CONFIG'", &path.display())
            )
    } else {
        fs::read_to_string(&path)
            .map_err(|e| anyhow!("failed reading config from {}: {}", &path.display(), e))
    }?;
    let map = toml::from_str(&data).map_err(|e| {
        anyhow!(
            "parse error in configuration file loaded from {}: {}",
            &path.display(),
            e
        )
    })?;
    Ok(map)
}

/// Starts a capability provider from its par file for testing.
/// Configuration file path should be in the environment variable PROVIDER_TEST_CONFIG
/// Par file path should be either in the environment variable PROVIDER_TEST_PAR
/// or in the config file as "par_file"
pub async fn start_provider_test(config: TomlMap) -> Result<Provider, anyhow::Error> {
    let exe_path = match config.get("bin_path") {
        Some(TomlValue::String(name)) => PathBuf::from(name),
        _ => {
            return Err(anyhow!(
                "Must specifiy binary path in 'bin_path' in config file"
            ))
        }
    };
    let pubkey = format!("_PubKey_{}", &exe_path.display());
    let exe_file = fs::File::open(&exe_path)?;

    // set logging level for capability provider with the "RUST_LOG" environment variable,
    // default level is "info"
    let log_level = match config.get("rust_log") {
        Some(TomlValue::String(level)) if level.parse::<log::Level>().is_ok() => level.to_string(),
        None => "info".to_string(),
        Some(x) => {
            eprintln!("invalid 'rust_log' setting '{}', using 'info'", x);
            "info".to_string()
        }
    };
    // set RUST_BACKTRACE, if requested
    // default is disabled
    let enable_backtrace = match config.get("rust_backtrace") {
        Some(TomlValue::String(sval)) if sval.as_str() == "1" => "1",
        Some(TomlValue::Integer(num)) if *num > 0 => "1",
        Some(TomlValue::Boolean(bval)) if *bval => "1",
        _ => "0",
    };

    let keys = wascap::prelude::KeyPair::new_user();
    let host_data = HostData {
        host_id: "_TEST_".to_string(),
        lattice_rpc_prefix: config
            .get("lattice_rpc_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or(TEST_LATTICE_PREFIX)
            .to_string(),
        lattice_rpc_url: nats_url(&config),
        link_name: config
            .get("link_name")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        provider_key: pubkey,
        cluster_issuers: vec![keys.public_key()],
        ..Default::default()
    };
    let buf = serde_json::to_vec(&host_data)?;
    let mut encoded = base64::encode_config(&buf, base64::STANDARD_NO_PAD);
    encoded.push_str("\r\n");

    // provider's stdout is piped through our stdout
    let mut child_proc = std::process::Command::new(&exe_path)
        .stdin(std::process::Stdio::piped())
        .env("RUST_LOG", &log_level)
        .env("RUST_BACKTRACE", enable_backtrace)
        .spawn()
        .map_err(|e| anyhow!("launching provider bin at {}: {}", &exe_path.display(), e))?;

    let mut stdin = child_proc
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open child stdin"))?;

    stdin.write_all(encoded.as_bytes())?;

    // Connect to nats
    let nats_client = NatsClient::new(host_data.nats_options())
        .await
        .map_err(|e| {
            anyhow!(
                "nats connection to {} failed: {}",
                &host_data.lattice_rpc_url,
                e.to_string()
            )
        })?;
    let client = wasmbus_rpc::RpcClient::new(
        nats_client.clone(),
        &host_data.lattice_rpc_prefix,
        keys,
        TEST_HOST_ID.to_string(),
        None,
    );

    Ok(Provider {
        inner: Arc::new(ProviderProcess {
            file: exe_file,
            path: exe_path,
            proc: child_proc,
            host_data,
            config,
            nats_client,
            rpc_client: client,
            timeout_ms: std::sync::Mutex::new(DEFAULT_RPC_TIMEOUT_MILLIS),
        }),
    })
}

/// given a list of regex patterns for test cases, run all tests
/// that match any of the patterns.
/// The order of the test runs is based on the order of patterns.
/// Tests are run at most once, even if they match more than one pattern.
/// This is like the `run_selected!` macro, except that it spawns
/// a thread for running the test case, so it can handle panics
/// (and failed assertions, which panic).
#[macro_export]
macro_rules! run_selected_spawn {
    ( $opt:expr, $($tname:ident),* $(,)? ) => {{
        let mut unique = std::collections::BTreeSet::new();
        let handle = tokio::runtime::Handle::current();
        let all_tests = vec![".*".to_string()];
        let pats : &Vec<String> = $opt.patterns.as_ref();
        let mut results: Vec<TestResult> = Vec::new();

        // Each test case regex (pats) is checked against all test names (tname).
        // This would be simpler to use a RegexSet, but then the tests would
        // always be run in the same order - these run in the order of the patterns.
        for tc_exp in pats.iter() {
            let pattern = tc_exp.as_str();
            let re = match $crate::regex::Regex::new(pattern) {
                Ok(re) => re,
                Err(e) => {
                    let error = wasmbus_rpc::RpcError::Other(format!(
                        "invalid regex spec '{}' for test case: {}",
                        pattern, e
                    ));
                    results.push(("TestCase", Err::<(),wasmbus_rpc::RpcError>(error)).into());
                    break;
                }
            };
            $(
            let name = stringify!($tname);
            if re.is_match(name) {
                // run it if it hasn't been run before
                if unique.insert(name) {
                    let opts = $opt.clone();
                    let join = handle.spawn( async move {
                        $tname(&opts).await
                        }
                    ).await;
                    let tr:TestResult = match join {
                        Ok(res) => (name, res).into(),
                        Err(e) => (name, Err::<(),wasmbus_rpc::RpcError>(
                            wasmbus_rpc::RpcError::Other(format!("join error: {}", e.to_string()))
                        )).into()
                    };
                    results.push(tr);
                }
            }
            )*
        }
        results
    }};
}

/// Execute all tests. In the current implementation,
/// all tests are run sequentially, and always in the same order.
// A future version of this should take a parameter for a scheduling strategy,
// which could permit options such as
// enum RunStrategy{
//   Deterministic,
//   Random,
//   Parallel(u16),  // num_threads
// }
// this is not in use now, since run_selected! seems to work fine running
// all test cases in the current thread (async executor).
// The reason I had put the spawn in was to catch panics from assert
// calls that fail.
pub async fn run_tests(
    tests: Vec<(&'static str, TestFunc)>,
) -> std::result::Result<Vec<TestResult>, Box<dyn std::error::Error>> {
    let mut results: Vec<TestResult> = Vec::new();
    let handle = tokio::runtime::Handle::current();
    for t in tests.into_iter() {
        let rc: RpcResult<()> = handle.spawn((&t.1)()).await?;
        results.push((t.0, rc).into());
    }

    let provider = test_provider().await;
    let _ = provider.shutdown().await; // send shutdown message

    Ok(results)
}

pub async fn test_provider() -> Provider {
    ONCE.get_or_init(|| async {
        match load_provider().await {
            Ok(p) => p,
            Err(e) => {
                panic!("failed to load provider: {}", e);
            }
        }
    })
    .await
    .clone()
}

pub async fn load_provider() -> Result<Provider, Box<dyn std::error::Error>> {
    let mut conf = load_config()?;
    let values = conf.remove("values");

    let prov = start_provider_test(conf).await?;

    // give it time to startup
    let delay_time_sec = match prov.config.get("start_delay_sec") {
        Some(n) => {
            let n = n.as_integer().unwrap_or(0);
            if !(0..=60).contains(&n) {
                return Err(RpcError::InvalidParameter(format!(
                    "configuration value 'start_delay_sec' is too large: {}",
                    n
                ))
                .into());
            }
            std::cmp::max(n as u64, DEFAULT_START_DELAY_SEC)
        }
        None => DEFAULT_START_DELAY_SEC,
    };
    tokio::time::sleep(std::time::Duration::from_secs(delay_time_sec)).await;

    let link_values = if let Some(toml::Value::Table(map)) = values {
        to_value_map(&map)?
    } else {
        SimpleValueMap::default()
    };
    prov.link_to_test(link_values).await?;

    Ok(prov)
}
