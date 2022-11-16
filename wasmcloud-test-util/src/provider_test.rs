#![cfg(not(target_arch = "wasm32"))]
//!
//! simple test harness to load a capability provider and test it
//!
use crate::testing::TestResult;
use anyhow::anyhow;
use async_trait::async_trait;
use futures::future::BoxFuture;
use nkeys::{KeyPair, KeyPairType};
use std::{fs, io::Write, ops::Deref, path::PathBuf, sync::Arc};
use tokio::sync::OnceCell;
use toml::value::Value as TomlValue;
use wasmbus_rpc::{
    common::{Context, Message, SendOpts, Transport},
    core::{HealthCheckRequest, HealthCheckResponse, HostData, LinkDefinition, WasmCloudEntity},
    error::{RpcError, RpcResult},
    rpc_client::RpcClient,
};

pub type SimpleValueMap = std::collections::HashMap<String, String>;
pub type TomlMap = std::collections::BTreeMap<String, toml::Value>;
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

const DEFAULT_START_DELAY_SEC: u64 = 1;
const DEFAULT_RPC_TIMEOUT_MILLIS: u64 = 2000;
const DEFAULT_NATS_URL: &str = "127.0.0.1:4222";
// use a unique lattice prefix to separate test traffic
const TEST_LATTICE_PREFIX: &str = "TEST";
const TEST_HOST_ID: &str = "_TEST_";

static ONCE: OnceCell<Provider> = OnceCell::const_new();
pub type TestFunc = fn() -> BoxFuture<'static, RpcResult<()>>;

fn to_value_map(data: &toml::map::Map<String, TomlValue>) -> RpcResult<SimpleValueMap> {
    let mut map = SimpleValueMap::default();
    // copy simple values into the map
    for (k, v) in data.iter() {
        match v {
            TomlValue::Integer(_) | TomlValue::Float(_) | TomlValue::Boolean(_) => {
                map.insert(k.clone(), v.to_string());
            }
            // string is handled separately because 'to_string()' adds quotes
            TomlValue::String(s) => {
                map.insert(k.clone(), s.clone());
            }
            // intentionally omitted
            TomlValue::Array(_) | TomlValue::Table(_) | TomlValue::Datetime(_) => {}
        };
    }
    // copy the entire map as base64-encoded json with value "config_b64"
    let json = serde_json::to_string(data)
        .map_err(|e| RpcError::Ser(format!("invalid 'values' map: {}", e)))?;
    let b64 = base64::encode_config(&json, base64::STANDARD_NO_PAD);
    map.insert("config_b64".to_string(), b64);
    Ok(map)
}

#[derive(Clone, Debug)]
pub struct Provider {
    inner: Arc<ProviderProcess>,
}

/// info needed to test a capability provider process. If this structure goes out of scope,
/// the provider will exit
#[derive(Debug)]
pub struct ProviderProcess {
    pub file: std::fs::File,
    pub host_data: HostData,
    pub actor_id: String,
    pub path: PathBuf,
    pub proc: std::process::Child,
    pub config: TomlMap,
    pub nats_client: async_nats::Client,
    pub rpc_client: RpcClient,
    pub timeout_ms: std::sync::Mutex<u64>,
}

impl ProviderProcess {
    /// generate the `origin` field for an Invocation. To the receiving provider,
    /// the origin field looks like an actor
    pub fn origin(&self) -> WasmCloudEntity {
        WasmCloudEntity {
            contract_id: "".to_string(),
            link_name: "".to_string(),
            public_key: self.actor_id.clone(),
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
        let mut ld = LinkDefinition::default();
        ld.actor_id = self.actor_id.clone();
        ld.contract_id = self
            .config
            .get("contract_id")
            .and_then(|v| v.as_str())
            .unwrap_or("wasmcloud:example")
            .to_string();
        ld.link_name = self.host_data.link_name.clone();
        ld.provider_id = self.host_data.provider_key.clone();
        ld.values = values;
        let bytes = serde_json::to_vec(&ld)?;
        self.rpc_client.publish(topic, bytes).await?;

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
            .send(
                self.origin(),
                self.target(),
                &self.host_data.lattice_rpc_prefix,
                message,
            )
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
    pub async fn subscribe_rpc(&self) -> RpcResult<async_nats::Subscriber> {
        let subject =
            wasmbus_rpc::rpc_client::rpc_topic(&self.origin(), &self.host_data.lattice_rpc_prefix);
        self.nats_client
            .subscribe(subject)
            .await
            .map_err(|e| RpcError::Nats(e.to_string()))
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
        let resp_bytes = self.rpc_client.request(topic.to_string(), bytes).await?;
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
        self.rpc_client.publish(shutdown_topic, Vec::new()).await?;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        Ok(())
    }
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
pub fn load_config() -> Result<TomlMap, RpcError> {
    let path = if let Ok(path) = std::env::var("PROVIDER_TEST_CONFIG") {
        PathBuf::from(path)
    } else {
        PathBuf::from("./provider_test_config.toml")
    };
    let data = if !path.is_file() {
        Err(RpcError::ProviderInit(format!(
            "Missing configuration file '{}'. Config file should be 'provider_test_config.toml' \
             in the current directory, or a .toml file whose path is in the environment variable \
             'PROVIDER_TEST_CONFIG'",
            &path.display()
        )))
    } else {
        fs::read_to_string(&path).map_err(|e| {
            RpcError::ProviderInit(format!(
                "failed reading config from {}: {}",
                &path.display(),
                e
            ))
        })
    }?;
    let map = toml::from_str(&data).map_err(|e| {
        RpcError::ProviderInit(format!(
            "parse error in configuration file loaded from {}: {}",
            &path.display(),
            e
        ))
    })?;
    Ok(map)
}

/// Starts a capability provider from its par file for testing.
/// Configuration file path should be in the environment variable PROVIDER_TEST_CONFIG
/// Par file path should be either in the environment variable PROVIDER_TEST_PAR
/// or in the config file as "par_file"
pub async fn start_provider_test(
    config: TomlMap,
    exe_path: &std::path::Path,
    ld: LinkDefinition,
) -> Result<Provider, RpcError> {
    let exe_file = fs::File::open(exe_path)?;
    // generate a fake host key, which we will use for cluster issuer and host id
    let host_key = KeyPair::new(KeyPairType::Server);
    eprintln!("host_id:     {}", host_key.public_key());
    eprintln!("contract_id: {}", &ld.contract_id);
    let actor_id = ld.actor_id.clone();

    // set logging level for capability provider with the "RUST_LOG" environment variable,
    // default level is "info"
    let log_level = match config.get("rust_log") {
        Some(TomlValue::String(level)) => level.to_string(),
        _ => "info".to_string(),
    };
    eprintln!("log_level:   {}", &log_level);

    // set RUST_BACKTRACE, if requested
    // default is disabled
    let enable_backtrace = match config.get("rust_backtrace") {
        Some(TomlValue::String(sval)) if sval.as_str() == "1" => "1",
        Some(TomlValue::Integer(num)) if *num > 0 => "1",
        Some(TomlValue::Boolean(bval)) if *bval => "1",
        _ => "0",
    };

    let mut host_data = HostData::default();
    host_data.host_id = TEST_HOST_ID.to_string(); // host_key.public_key();
    host_data.lattice_rpc_prefix = config
        .get("lattice_rpc_prefix")
        .and_then(|v| v.as_str())
        .unwrap_or(TEST_LATTICE_PREFIX)
        .to_string();
    host_data.link_name = ld.link_name.clone();
    host_data.lattice_rpc_url = nats_url(&config);
    host_data.provider_key = ld.provider_id.clone();
    host_data.invocation_seed = host_key.seed().unwrap();
    host_data.cluster_issuers = vec![host_key.public_key()];
    host_data.link_definitions = vec![ld];
    host_data.structured_logging = false;

    let buf = serde_json::to_vec(&host_data).map_err(|e| RpcError::Ser(e.to_string()))?;
    let mut encoded = base64::encode_config(&buf, base64::STANDARD_NO_PAD);
    encoded.push_str("\r\n");

    // provider's stdout is piped through our stdout
    let mut child_proc = std::process::Command::new(exe_path)
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .env("RUST_LOG", &log_level)
        .env("RUST_BACKTRACE", enable_backtrace)
        .spawn()
        .map_err(|e| {
            RpcError::Other(format!(
                "launching provider bin at {}: {}",
                &exe_path.display(),
                e
            ))
        })?;

    let mut stdin = child_proc
        .stdin
        .take()
        .ok_or_else(|| RpcError::ProviderInit("failed to open child stdin".into()))?;

    stdin.write_all(encoded.as_bytes())?;

    // Connect to nats
    let nats_client = host_data.nats_connect().await?;
    wasmbus_rpc::provider::init_host_bridge_for_test(nats_client.clone(), &host_data)?;

    Ok(Provider {
        inner: Arc::new(ProviderProcess {
            file: exe_file,
            path: exe_path.to_owned(),
            proc: child_proc,
            actor_id,
            config,
            nats_client: nats_client.clone(),
            rpc_client: RpcClient::new(
                nats_client,
                host_key.public_key(),
                Some(std::time::Duration::from_millis(
                    host_data.default_rpc_timeout_ms.unwrap_or(2000),
                )),
                Arc::new(host_key),
            ),
            host_data,
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
                    let error = RpcError::Other(format!(
                        "invalid regex spec '{}' for test case: {}",
                        pattern, e
                    ));
                    results.push(("TestCase", Err::<(),RpcError>(error)).into());
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
                        Err(e) => (name, Err::<(),RpcError>(
                            RpcError::Other(format!("join error: {}", e.to_string()))
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
    for (name, tfunc) in tests.into_iter() {
        let rc: RpcResult<()> = handle.spawn(tfunc()).await?;
        results.push(TestResult {
            name: name.to_string(),
            passed: rc.is_ok(),
            ..Default::default()
        });
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

pub async fn load_provider() -> Result<Provider, RpcError> {
    let mut conf = load_config()?;
    let values = conf.remove("values");
    let link_values = if let Some(toml::Value::Table(map)) = values {
        to_value_map(&map)?
    } else {
        SimpleValueMap::default()
    };
    let exe_path = conf
        .get("bin_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| {
            RpcError::ProviderInit("Must specifiy binary path in 'bin_path' in config file".into())
        })?;
    let actor_kp = KeyPair::new(KeyPairType::Module);
    eprintln!("actor_id:    {}", actor_kp.public_key());
    let provider_kp = KeyPair::new(KeyPairType::Service);
    eprintln!("provider_id: {}", provider_kp.public_key());

    let mut test_linkdef = LinkDefinition::default();
    test_linkdef.actor_id = actor_kp.public_key();
    test_linkdef.contract_id = conf
        .get("contract_id")
        .and_then(|v| v.as_str())
        .unwrap_or("wasmcloud:example")
        .to_string();
    test_linkdef.link_name = conf
        .get("link_name")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    test_linkdef.provider_id = provider_kp.public_key();
    test_linkdef.values = link_values;
    let prov = start_provider_test(conf, &exe_path, test_linkdef.clone()).await?;

    // give it time to startup
    let delay_time_sec = match prov.config.get("start_delay_sec") {
        Some(n) => {
            let n = n.as_integer().unwrap_or(0);
            if !(0..=60).contains(&n) {
                return Err(RpcError::InvalidParameter(format!(
                    "configuration value 'start_delay_sec' is too large: {}",
                    n
                )));
            }
            std::cmp::max(n as u64, DEFAULT_START_DELAY_SEC)
        }
        None => DEFAULT_START_DELAY_SEC,
    };
    tokio::time::sleep(std::time::Duration::from_secs(delay_time_sec)).await;

    // optionally, allow extra time to handle put_link
    if let Some(n) = prov.config.get("link_delay_sec") {
        if let Some(n) = n.as_integer() {
            if n > 0 {
                eprintln!("Pausing {} secs after put_link", n);
                tokio::time::sleep(std::time::Duration::from_secs(n as u64)).await;
            }
        } else {
            return Err(RpcError::InvalidParameter(format!(
                "configuration value 'link_delay_sec={}' is not a valid integer",
                n
            )));
        }
    }

    Ok(prov)
}
