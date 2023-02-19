#![cfg(not(target_arch = "wasm32"))]
//!
//! simple test harness to load a capability provider and test it
//!

pub use log;

use crate::testing::TestResult;
use anyhow::anyhow;
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::lock::Mutex;
use nkeys::{KeyPair, KeyPairType};
use serde::{de, Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    env::var,
    error::Error,
    fs,
    io::Write,
    ops::Deref,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::OnceCell;
use wasmbus_rpc::{
    common::{Context, Message, SendOpts, Transport},
    core::{HealthCheckRequest, HealthCheckResponse, HostData, LinkDefinition, WasmCloudEntity},
    error::{RpcError, RpcResult},
    rpc_client::RpcClient,
};

pub type TestFunc = fn() -> BoxFuture<'static, RpcResult<()>>;

pub type SimpleValueMap = HashMap<String, String>;
pub type TomlMap = BTreeMap<String, toml::Value>;
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

const DEFAULT_START_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_millis(2000);
const DEFAULT_NATS_URL: &str = "127.0.0.1:4222";
// use a unique lattice prefix to separate test traffic
const TEST_LATTICE_PREFIX: &str = "TEST";
const TEST_HOST_ID: &str = "_TEST_";

static PROVIDERS: OnceCell<Mutex<HashMap<Config, Provider>>> = OnceCell::const_new();

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LogLevel(pub log::Level);

impl Deref for LogLevel {
    type Target = log::Level;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromStr for LogLevel {
    type Err = <log::Level as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<log::Level>().map(Self)
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl From<log::Level> for LogLevel {
    fn from(level: log::Level) -> Self {
        Self(level)
    }
}

impl From<LogLevel> for log::Level {
    fn from(LogLevel(level): LogLevel) -> Self {
        level
    }
}

/// Provider test config
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq)]
pub struct Config {
    /// Whether to enable Rust backtrace
    pub backtrace: bool,
    /// Contract ID
    pub contract_id: String,
    /// Lattice RPC prefix
    pub lattice_rpc_prefix: String,
    pub link_delay: Duration,
    pub link_name: String,
    /// [`LogLevel`] for capability provider set using "RUST_LOG" environment variable, defaults to [LogLevel::Info]
    pub log_level: LogLevel,
    /// URL at which NATS is accessible
    pub nats_url: String,
    /// URL at which Redis is accessible
    pub redis_url: String,
    pub start_delay: Duration,
    // NOTE: `BTreeMap`, unlike `HashMap` implements `Hash`
    pub values: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backtrace: false,
            contract_id: "wasmcloud:example".into(),
            lattice_rpc_prefix: TEST_LATTICE_PREFIX.into(),
            link_delay: Default::default(),
            link_name: "default".into(),
            log_level: log::Level::Info.into(),
            nats_url: DEFAULT_NATS_URL.into(),
            redis_url: "0.0.0.0:6379".into(),
            start_delay: DEFAULT_START_DELAY,
            values: Default::default(),
        }
    }
}

impl Config {
    /// Loads [Config] from path contained in environment variable `PROVIDER_TEST_CONFIG`,
    /// otherwise attempts to use `provider_test_config.toml` in working directory.
    pub fn load() -> Result<Self, RpcError> {
        let path = if let Ok(path) = var("PROVIDER_TEST_CONFIG") {
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
        let config = toml::from_str(&data).map_err(|e| {
            RpcError::ProviderInit(format!(
                "parse error in configuration file loaded from {}: {}",
                &path.display(),
                e
            ))
        })?;
        Ok(config)
    }
}

#[derive(Clone, Debug)]
pub struct Provider {
    inner: Arc<ProviderProcess>,
}

#[derive(Serialize)]
struct ShutdownMessage {
    /// The ID of the host that sent the message
    pub host_id: String,
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
    pub config: Config,
    pub nats_client: async_nats::Client,
    pub rpc_client: RpcClient,
    pub timeout_ms: std::sync::Mutex<u128>,
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
        ld.contract_id = self.config.contract_id.clone();
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
        let message = serde_json::to_vec(&ShutdownMessage {
            host_id: TEST_HOST_ID.to_string(),
        })
        .unwrap();
        let resp = self.rpc_client.request(shutdown_topic, message).await?;
        if !resp.is_empty() {
            eprintln!("shutdown response: {}", String::from_utf8_lossy(&resp));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
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
    ) -> Result<Vec<u8>, RpcError> {
        self.inner.send_rpc(message).await
    }

    /// sets the time period for an expected response to rpc messages,
    /// after which an RpcError::Timeout will occur.
    fn set_timeout(&self, interval: Duration) {
        let lock = self.timeout_ms.try_lock();
        if let Ok(mut rg) = lock {
            *rg = interval.as_millis()
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

/// Starts a capability provider from its par file for testing
pub async fn start_provider_test(
    config: Config,
    path: impl Into<PathBuf>,
    ld: LinkDefinition,
) -> Result<Provider, RpcError> {
    let path = path.into();

    let file = fs::File::open(&path)?;
    // generate a fake host key, which we will use for cluster issuer and host id
    let host_key = KeyPair::new(KeyPairType::Server);
    eprintln!("host_id:     {}", host_key.public_key());
    eprintln!("contract_id: {}", &ld.contract_id);
    let actor_id = ld.actor_id.clone();

    eprintln!("log_level:   {}", &config.log_level.as_str());

    let mut host_data = HostData::default();
    host_data.host_id = TEST_HOST_ID.to_string(); // host_key.public_key();
    host_data.lattice_rpc_prefix = config.lattice_rpc_prefix.clone();
    host_data.link_name = ld.link_name.clone();
    host_data.lattice_rpc_url = config.nats_url.clone();
    host_data.provider_key = ld.provider_id.clone();
    host_data.invocation_seed = host_key.seed().unwrap();
    host_data.cluster_issuers = vec![host_key.public_key()];
    host_data.link_definitions = vec![ld];
    host_data.structured_logging = false;

    let buf = serde_json::to_vec(&host_data).map_err(|e| RpcError::Ser(e.to_string()))?;
    let mut encoded = base64::encode_config(&buf, base64::STANDARD_NO_PAD);
    encoded.push_str("\r\n");

    // provider's stdout is piped through our stdout
    let mut proc = std::process::Command::new(&path)
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .env("RUST_LOG", config.log_level.as_str())
        .env("RUST_BACKTRACE", if config.backtrace { "1" } else { "0" })
        .spawn()
        .map_err(|e| {
            RpcError::Other(format!(
                "launching provider bin at {}: {}",
                &path.display(),
                e
            ))
        })?;

    let mut stdin = proc
        .stdin
        .take()
        .ok_or_else(|| RpcError::ProviderInit("failed to open child stdin".into()))?;

    stdin.write_all(encoded.as_bytes())?;

    // Connect to nats
    let nats_client = host_data.nats_connect().await?;
    wasmbus_rpc::provider::init_host_bridge_for_test(nats_client.clone(), &host_data)?;

    let rpc_client_timeout = host_data
        .default_rpc_timeout_ms
        .map(Duration::from_millis)
        .or(Some(DEFAULT_RPC_TIMEOUT));
    let rpc_client = RpcClient::new(
        nats_client.clone(),
        host_key.public_key(),
        rpc_client_timeout,
        Arc::new(host_key),
    );

    let timeout_ms = std::sync::Mutex::new(DEFAULT_RPC_TIMEOUT.as_millis());
    let inner = Arc::new(ProviderProcess {
        file,
        path,
        proc,
        actor_id,
        config,
        nats_client,
        rpc_client,
        host_data,
        timeout_ms,
    });
    Ok(Provider { inner })
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

pub struct TestCase {
    pub name: String,
    pub func: TestFunc,
    pub bin_path: PathBuf,
    pub config: Config,
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
    tests: impl IntoIterator<Item = TestCase>,
) -> Result<Vec<TestResult>, Box<dyn Error>> {
    let mut results: Vec<TestResult> = Vec::new();
    let handle = tokio::runtime::Handle::current();
    for TestCase {
        name,
        func,
        bin_path,
        config,
    } in tests
    {
        let rc: RpcResult<()> = handle.spawn(func()).await?;
        results.push(TestResult {
            name,
            passed: rc.is_ok(),
            ..Default::default()
        });
        let _ = test_provider(bin_path, config).await.shutdown().await; // send shutdown message
    }
    Ok(results)
}

pub async fn test_provider(bin_path: impl Into<PathBuf>, config: Config) -> Provider {
    let mut providers = PROVIDERS
        .get_or_init(|| async { Default::default() })
        .await
        .lock()
        .await;
    if let Some(prov) = providers.get(&config) {
        return prov.clone();
    }
    let prov = load_provider(bin_path, config.clone())
        .await
        .expect("failed to load provider");
    providers.insert(config, prov.clone());
    prov
}

pub async fn load_provider(
    bin_path: impl Into<PathBuf>,
    config: Config,
) -> Result<Provider, RpcError> {
    let actor_kp = KeyPair::new(KeyPairType::Module);
    eprintln!("actor_id:    {}", actor_kp.public_key());
    let provider_kp = KeyPair::new(KeyPairType::Service);
    eprintln!("provider_id: {}", provider_kp.public_key());

    let config_b64 = if !config.values.is_empty() {
        let json = serde_json::to_string(&config.values)
            .map_err(|e| RpcError::Ser(format!("invalid 'values' map: {}", e)))?;
        Some(base64::encode_config(&json, base64::STANDARD_NO_PAD))
    } else {
        None
    };
    let values = config
        .values
        .clone()
        .into_iter()
        .chain(config_b64.map(|v| ("config_b64".into(), v)))
        .collect();

    let mut test_linkdef = LinkDefinition::default();
    test_linkdef.actor_id = actor_kp.public_key();
    test_linkdef.contract_id = config.contract_id.clone();
    test_linkdef.link_name = config.link_name.clone();
    test_linkdef.provider_id = provider_kp.public_key();
    test_linkdef.values = values;
    let prov = start_provider_test(config.clone(), bin_path, test_linkdef.clone()).await?;

    // give it time to startup
    tokio::time::sleep(prov.config.start_delay).await;

    // optionally, allow extra time to handle put_link
    if !config.link_delay.is_zero() {
        eprintln!("Pausing {:?} secs after put_link", config.link_delay);
        tokio::time::sleep(config.link_delay).await;
    }
    Ok(prov)
}
