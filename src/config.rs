//! Configuration: what the SDK reads, in what order, and what it defaults to.
//!
//! Precedence is always **explicit option > environment variable > default**, so a deployment can
//! set `PTN_HOST` and `PTN_API_KEY` and a test can override them in code.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::http::SharedHttpClient;
use crate::logger::LogSink;
use crate::payload::RedactHook;
use crate::snapshot::PayloadPolicy;

/// The PromptOn app the SDK talks to when `PTN_HOST` is not set.
pub const DEFAULT_HOST: &str = "https://app.prompton.ai";
/// The environment the SDK reads when nothing says otherwise.
pub const DEFAULT_ENVIRONMENT: &str = "production";
/// How long a snapshot is served from memory before a refresh is triggered.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(10);
/// The ceiling on the refresh backoff after repeated failures.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// How the SDK behaves with respect to the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Poll PromptOn, resolve locally, send monitoring logs.
    Live,
    /// Never touch the network: resolve from the disk cache and the bundle only.
    Offline,
    /// Never touch the network and capture monitoring logs in memory for assertions.
    Test,
}

impl Mode {
    /// Whether this mode may make HTTP calls.
    pub fn is_remote(self) -> bool {
        self == Mode::Live
    }
}

/// Where the disk cache lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskCache {
    /// A file named after the project and environment in the OS cache (or temp) directory.
    Default,
    /// An explicit path.
    Path(PathBuf),
    /// No disk cache at all.
    Disabled,
}

/// The monitoring-log buffer's thresholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// Send a batch this long after the first record was queued.
    pub flush_interval: Duration,
    /// Send as soon as this many records are queued (capped at 200 per request).
    pub flush_size: usize,
    /// Send as soon as the queue holds this many encoded bytes.
    pub flush_bytes: usize,
    /// The queue cap; above it the oldest records are dropped and counted.
    pub max_queue: usize,
    /// How many times one batch is retried before it is dropped and counted.
    pub max_attempts: u32,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            flush_interval: Duration::from_secs(2),
            flush_size: 100,
            flush_bytes: 1_000_000,
            max_queue: 10_000,
            max_attempts: 8,
        }
    }
}

/// The resolved configuration a [`crate::Client`] runs on.
#[derive(Clone)]
pub struct Config {
    /// The API base URL, `/api/v1` included and no trailing slash.
    pub base_url: String,
    /// The runtime key, `ptn_<project_slug>_…`.
    pub api_key: Option<String>,
    /// The environment this process reads.
    pub environment: String,
    /// The project slug, from the key or set explicitly.
    pub project: Option<String>,
    /// How long a snapshot is served before a refresh is triggered.
    pub cache_ttl: Duration,
    /// The per-request timeout.
    pub request_timeout: Duration,
    /// Where the disk cache lives.
    pub disk_cache: DiskCache,
    /// A snapshot file shipped inside the app, used when memory and disk are empty.
    pub bundle: Option<PathBuf>,
    /// Live, offline or test.
    pub mode: Mode,
    /// Send `sha256(end_user_ref)` instead of the raw reference.
    pub hash_end_user: bool,
    /// Whether to run the background poller.
    pub poll: bool,
    /// Whether to fetch the snapshot synchronously at start when no tier has one.
    pub fetch_on_start: bool,
    /// The monitoring-log buffer's thresholds.
    pub log: LogConfig,
    /// The policy for a use case whose snapshot entry carries none.
    pub payload_defaults: PayloadPolicy,
    /// The app's redaction hook.
    pub redact: Option<RedactHook>,
    /// The `User-Agent` sent with every request.
    pub user_agent: String,
    /// The transport, when the app supplied one.
    pub http_client: Option<SharedHttpClient>,
    /// Where the SDK's own log lines go.
    pub log_sink: Option<LogSink>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("environment", &self.environment)
            .field("project", &self.project)
            .field("cache_ttl", &self.cache_ttl)
            .field("request_timeout", &self.request_timeout)
            .field("disk_cache", &self.disk_cache)
            .field("bundle", &self.bundle)
            .field("mode", &self.mode)
            .field("hash_end_user", &self.hash_end_user)
            .field("poll", &self.poll)
            .field("log", &self.log)
            .field("payload_defaults", &self.payload_defaults)
            .field("redact", &self.redact.is_some())
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

impl Config {
    /// Whether the SDK may contact PromptOn: live mode with a key.
    pub fn remote_enabled(&self) -> bool {
        self.mode.is_remote() && self.api_key.is_some()
    }

    /// The disk-cache path, when one is configured.
    pub fn disk_cache_path(&self) -> Option<PathBuf> {
        match &self.disk_cache {
            DiskCache::Disabled => None,
            DiskCache::Path(path) => Some(path.clone()),
            DiskCache::Default => Some(default_cache_path(
                self.project.as_deref().unwrap_or("default"),
                &self.environment,
            )),
        }
    }
}

/// Builds a [`crate::Client`].
///
/// ```no_run
/// use prompton::Client;
/// use std::time::Duration;
///
/// let client = Client::builder()
///     .api_key("ptn_myproject_…")
///     .environment("staging")
///     .cache_ttl(Duration::from_secs(10))
///     .build()?;
/// # Ok::<(), prompton::Error>(())
/// ```
#[derive(Default)]
pub struct ClientBuilder {
    host: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    environment: Option<String>,
    project: Option<String>,
    cache_ttl: Option<Duration>,
    request_timeout: Option<Duration>,
    disk_cache: Option<DiskCache>,
    bundle: Option<PathBuf>,
    mode: Option<Mode>,
    hash_end_user: Option<bool>,
    poll: Option<bool>,
    fetch_on_start: Option<bool>,
    log: Option<LogConfig>,
    payload_defaults: Option<PayloadPolicy>,
    redact: Option<RedactHook>,
    user_agent: Option<String>,
    http_client: Option<SharedHttpClient>,
    log_sink: Option<LogSink>,
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientBuilder")
    }
}

impl ClientBuilder {
    /// A builder with nothing set: every value comes from the environment or the default.
    pub fn new() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// The PromptOn host, without `/api/v1` (which the SDK appends). Overrides `PTN_HOST`.
    pub fn host(mut self, host: impl Into<String>) -> ClientBuilder {
        self.host = Some(host.into());
        self
    }

    /// The full API base URL including `/api/v1`, for a gateway that does not sit at the root.
    pub fn base_url(mut self, base_url: impl Into<String>) -> ClientBuilder {
        self.base_url = Some(base_url.into());
        self
    }

    /// The runtime key. Overrides `PTN_API_KEY`.
    pub fn api_key(mut self, api_key: impl Into<String>) -> ClientBuilder {
        self.api_key = Some(api_key.into());
        self
    }

    /// The environment to read. Overrides `PTN_ENVIRONMENT`; defaults to `production`.
    pub fn environment(mut self, environment: impl Into<String>) -> ClientBuilder {
        self.environment = Some(environment.into());
        self
    }

    /// The project slug, when it cannot be read from the key.
    pub fn project(mut self, project: impl Into<String>) -> ClientBuilder {
        self.project = Some(project.into());
        self
    }

    /// How long a snapshot is served from memory before a refresh is triggered (default 10 s).
    pub fn cache_ttl(mut self, cache_ttl: Duration) -> ClientBuilder {
        self.cache_ttl = Some(cache_ttl);
        self
    }

    /// The per-request timeout (default 5 s).
    pub fn request_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.request_timeout = Some(timeout);
        self
    }

    /// Writes the disk cache to an explicit path.
    pub fn disk_cache_path(mut self, path: impl Into<PathBuf>) -> ClientBuilder {
        self.disk_cache = Some(DiskCache::Path(path.into()));
        self
    }

    /// Turns the disk cache off. Memory and the bundle are then the only tiers.
    pub fn without_disk_cache(mut self) -> ClientBuilder {
        self.disk_cache = Some(DiskCache::Disabled);
        self
    }

    /// A snapshot file shipped inside the app, used when memory and disk are empty.
    pub fn bundle(mut self, path: impl Into<PathBuf>) -> ClientBuilder {
        self.bundle = Some(path.into());
        self
    }

    /// Live (default), offline or test.
    pub fn mode(mut self, mode: Mode) -> ClientBuilder {
        self.mode = Some(mode);
        self
    }

    /// Sends `sha256(end_user_ref)` instead of the raw reference.
    pub fn hash_end_user(mut self, hash_end_user: bool) -> ClientBuilder {
        self.hash_end_user = Some(hash_end_user);
        self
    }

    /// Turns the background poller off; refreshes then happen on the next resolve after the TTL.
    pub fn poll(mut self, poll: bool) -> ClientBuilder {
        self.poll = Some(poll);
        self
    }

    /// Whether a cold start (nothing in memory, on disk or in the bundle) fetches the snapshot
    /// synchronously before [`ClientBuilder::build`] returns. On by default, so the first resolve after
    /// a cold start has something to work with; turn it off in a process that must never block on
    /// PromptOn at boot.
    pub fn fetch_on_start(mut self, fetch_on_start: bool) -> ClientBuilder {
        self.fetch_on_start = Some(fetch_on_start);
        self
    }

    /// The monitoring-log buffer's thresholds.
    pub fn log_config(mut self, log: LogConfig) -> ClientBuilder {
        self.log = Some(log);
        self
    }

    /// The payload policy for a use case whose snapshot entry carries none.
    pub fn payload_defaults(mut self, defaults: PayloadPolicy) -> ClientBuilder {
        self.payload_defaults = Some(defaults);
        self
    }

    /// A hook that gets the last word on every record before it is queued.
    pub fn redact(
        mut self,
        redact: impl Fn(serde_json::Value) -> serde_json::Value + Send + Sync + 'static,
    ) -> ClientBuilder {
        self.redact = Some(Arc::new(redact));
        self
    }

    /// Overrides the `User-Agent`.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> ClientBuilder {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// Uses a different HTTP transport (a test double, a proxy, an instrumented client).
    pub fn http_client(mut self, client: SharedHttpClient) -> ClientBuilder {
        self.http_client = Some(client);
        self
    }

    /// Sends the SDK's own log lines somewhere other than stderr.
    pub fn log_sink(mut self, sink: impl Fn(&str) + Send + Sync + 'static) -> ClientBuilder {
        self.log_sink = Some(Arc::new(sink));
        self
    }

    /// Resolves the configuration without starting a client.
    pub fn build_config(self) -> Result<Config, crate::Error> {
        let api_key = self
            .api_key
            .or_else(|| non_empty_env("PTN_API_KEY"))
            .filter(|key| !key.trim().is_empty());

        let base_url = match self.base_url.or_else(|| non_empty_env("PTN_BASE_URL")) {
            Some(base_url) => base_url.trim_end_matches('/').to_string(),
            None => {
                let host = self
                    .host
                    .or_else(|| non_empty_env("PTN_HOST"))
                    .unwrap_or_else(|| DEFAULT_HOST.to_string());
                format!("{}/api/v1", host.trim_end_matches('/'))
            }
        };

        let environment = self
            .environment
            .or_else(|| non_empty_env("PTN_ENVIRONMENT"))
            .unwrap_or_else(|| DEFAULT_ENVIRONMENT.to_string());
        if environment.trim().is_empty() {
            return Err(crate::Error::Config(
                "environment must not be empty".to_string(),
            ));
        }

        let project = self
            .project
            .or_else(|| non_empty_env("PTN_PROJECT"))
            .or_else(|| api_key.as_deref().and_then(project_from_key));

        Ok(Config {
            base_url,
            api_key,
            environment,
            project,
            cache_ttl: self.cache_ttl.unwrap_or(DEFAULT_CACHE_TTL),
            request_timeout: self.request_timeout.unwrap_or(Duration::from_secs(5)),
            disk_cache: self.disk_cache.unwrap_or(DiskCache::Default),
            bundle: self.bundle,
            mode: self.mode.unwrap_or(Mode::Live),
            hash_end_user: self.hash_end_user.unwrap_or(false),
            poll: self.poll.unwrap_or(true),
            fetch_on_start: self.fetch_on_start.unwrap_or(true),
            log: self.log.unwrap_or_default(),
            payload_defaults: self.payload_defaults.unwrap_or_default(),
            redact: self.redact,
            user_agent: self
                .user_agent
                .unwrap_or_else(|| format!("prompton-rust/{}", env!("CARGO_PKG_VERSION"))),
            http_client: self.http_client,
            log_sink: self.log_sink,
        })
    }

    /// Resolves the configuration and starts the client (loading disk and bundle, and starting the
    /// background threads).
    pub fn build(self) -> Result<crate::Client, crate::Error> {
        crate::Client::from_config(self.build_config()?)
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The project slug encoded in a runtime key: `ptn_<project_slug>_<random>`.
pub fn project_from_key(api_key: &str) -> Option<String> {
    let rest = api_key.strip_prefix("ptn_")?;
    let (slug, _) = rest.rsplit_once('_')?;
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

/// The default disk-cache path: `<os cache dir>/prompton/<project>.<environment>.snapshot.json`.
pub fn default_cache_path(project: &str, environment: &str) -> PathBuf {
    let base = cache_root();
    base.join("prompton")
        .join(format!("{project}.{environment}.snapshot.json"))
}

fn cache_root() -> PathBuf {
    if let Some(dir) = non_empty_env("XDG_CACHE_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(home) = non_empty_env("HOME") {
        let home = PathBuf::from(home);
        if cfg!(target_os = "macos") {
            return home.join("Library").join("Caches");
        }
        return home.join(".cache");
    }
    if let Some(local) = non_empty_env("LOCALAPPDATA") {
        return PathBuf::from(local);
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_project_from_the_key() {
        assert_eq!(
            project_from_key("ptn_sdkfixture_6yfe6v2ipbld676gved6w5jjcuqcq4fu").as_deref(),
            Some("sdkfixture")
        );
        assert_eq!(project_from_key("nope"), None);
    }

    #[test]
    fn appends_the_api_prefix_to_the_host() {
        let config = ClientBuilder::new()
            .host("http://localhost:4000/")
            .mode(Mode::Offline)
            .build_config()
            .unwrap();
        assert_eq!(config.base_url, "http://localhost:4000/api/v1");
    }

    #[test]
    fn precedence_is_option_then_environment_then_default() {
        // One test owns the process environment, so nothing races with it.
        std::env::remove_var("PTN_ENVIRONMENT");
        let config = ClientBuilder::new()
            .mode(Mode::Offline)
            .build_config()
            .unwrap();
        assert_eq!(config.environment, DEFAULT_ENVIRONMENT);

        std::env::set_var("PTN_ENVIRONMENT", "from-env");
        let config = ClientBuilder::new()
            .mode(Mode::Offline)
            .build_config()
            .unwrap();
        assert_eq!(config.environment, "from-env");

        let config = ClientBuilder::new()
            .environment("explicit")
            .mode(Mode::Offline)
            .build_config()
            .unwrap();
        assert_eq!(config.environment, "explicit");
        std::env::remove_var("PTN_ENVIRONMENT");
    }
}
