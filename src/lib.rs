//! The official PromptOn SDK for Rust.
//!
//! PromptOn is the control plane for your app's LLM prompts. For each **use case** and
//! **environment** it holds one **pin**: a prompt version, one model, and its parameters. This SDK
//! fetches a **snapshot** of those pins, renders the pinned prompt with this call's variables, and
//! sends **monitoring logs** back in batches. Your app calls the provider itself, with its own key
//! and its own HTTP client — PromptOn is config-fetch, **not a proxy**, so it is never in the
//! request path, and if it is down your app keeps running on the last snapshot it received.
//!
//! ```no_run
//! use prompton::{CallMeta, Client, Completion, Outcome};
//!
//! let prompton = Client::from_env()?;                       // PTN_HOST, PTN_API_KEY
//! let call = prompton.resolve("greeting")?;                  // which model, params and prompt
//! let messages = call.render_messages(serde_json::json!({"name": "Ada"}))?;
//!
//! let answer = prompton.with_generation(&call, CallMeta::new().input_messages(messages.clone()), || {
//!     let text = my_provider_call(&call.model, &messages);   // your key, your HTTP client
//!     Ok(Completion::new(text.clone(), Outcome::text(text).with_finish_reason("stop")))
//! })?;
//! # fn my_provider_call(_model: &Option<String>, _messages: &[prompton::Message]) -> String { String::new() }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # How it fails
//!
//! | when | what happens |
//! |---|---|
//! | a poll times out, 5xx or 429 | the previous snapshot keeps serving; the caller sees nothing |
//! | PromptOn is down at start | the disk cache, then the bundle, answer `resolve` |
//! | nothing is cached anywhere | `resolve` returns [`Error::NotReady`] — the only error worth retrying |
//! | the use case has no live deployment | [`Error::Unresolved`] — a bug in the deployment, never a reason to use a hard-coded prompt |
//! | the prompt name is not pinned | [`Error::UnknownPrompt`], with the names that are |
//! | a monitoring log cannot be sent | it is retried, then dropped and counted; a generation never waits for it |
//!
//! # Configuration
//!
//! Every option follows **explicit option > environment variable > default**; see
//! [`ClientBuilder`]. The two that matter are `PTN_HOST` (default `https://app.prompton.ai`; the
//! SDK appends `/api/v1`) and `PTN_API_KEY`. Without a key the SDK makes no remote calls at all
//! and serves whatever the disk cache or the bundle holds.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod api;
mod buffer;
mod config;
mod error;
mod http;
mod logger;
pub mod payload;
mod record;
mod resolver;
mod sha256;
mod snapshot;
mod stop_kind;
mod store;
pub mod template;
pub mod uuidv7;

pub use crate::api::{GenerationsAck, RejectedRecord};
pub use crate::buffer::{FlushOutcome, LogStats, MAX_RECORDS_PER_REQUEST, MAX_REQUEST_BYTES};
pub use crate::config::{
    ClientBuilder, Config, DiskCache, LogConfig, Mode, DEFAULT_CACHE_TTL, DEFAULT_ENVIRONMENT,
    DEFAULT_HOST,
};
pub use crate::error::{Error, Result};
pub use crate::http::{
    HttpClient, HttpRequest, HttpResponse, Method, SharedHttpClient, TransportError, UreqClient,
};
pub use crate::logger::LogSink;
pub use crate::payload::{PayloadConfig, RedactHook};
pub use crate::record::{
    CallFailure, CallMeta, Completion, CostSource, ErrorKind, GenerationError, GenerationRecord,
    Outcome, Sdk, Status, Usage, SDK_NAME,
};
pub use crate::resolver::{
    resolve as resolve_in, Rendered, Resolution, ResolutionSource, ResolveOptions, DEFAULT_PROMPT,
};
pub use crate::snapshot::{
    Deployment, InputVariable, Kind, Message, Model, PayloadMode, PayloadPolicy, PromptVersion,
    SnapshotDocument, UseCase, SCHEMA_VERSION,
};
pub use crate::stop_kind::StopKind;
pub use crate::store::SnapshotInfo;
pub use crate::template::{Engine, LintReason, TemplateError, Vars};

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::api::Api;
use crate::buffer::Buffer;
use crate::logger::Logger;
use crate::store::Store;

/// The SDK's version, as it appears in the `User-Agent` and in every record's `sdk` object.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Inner {
    config: Arc<Config>,
    store: Arc<Store>,
    buffer: Arc<Buffer>,
    api: Arc<Api>,
    logger: Logger,
    threads: Mutex<Vec<JoinHandle<()>>>,
    resolve_cache: Mutex<HashMap<String, (Instant, Value)>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.store.stop();
        // Best effort: give the queue one last chance to reach PromptOn before the process exits.
        if self.config.mode == Mode::Live {
            let _ = self
                .buffer
                .flush(Some(Instant::now() + Duration::from_secs(5)));
        }
        self.buffer.stop();
        if let Ok(mut threads) = self.threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
}

/// The SDK handle: one per process, cheap to clone, safe to share across threads.
///
/// Dropping the last clone stops the background threads after one final best-effort flush of the
/// monitoring-log queue.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("environment", &self.inner.config.environment)
            .field("project", &self.inner.config.project)
            .field("mode", &self.inner.config.mode)
            .finish()
    }
}

impl Client {
    /// A builder for the options that are not read from the environment.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// A client configured entirely from `PTN_HOST`, `PTN_API_KEY`, `PTN_ENVIRONMENT` and
    /// `PTN_PROJECT`.
    pub fn from_env() -> Result<Client> {
        ClientBuilder::new().build()
    }

    /// Starts a client from an already-resolved [`Config`].
    pub fn from_config(config: Config) -> Result<Client> {
        let logger = match &config.log_sink {
            Some(sink) => Logger::to_sink(sink.clone()),
            None => Logger::stderr(),
        };
        let config = Arc::new(config);

        let http = match &config.http_client {
            Some(client) => client.clone(),
            None => Arc::new(UreqClient::new(config.request_timeout)),
        };
        let api = Arc::new(Api {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            user_agent: config.user_agent.clone(),
            timeout: config.request_timeout,
            http,
        });

        let store = Arc::new(Store::new(config.clone(), api.clone(), logger.clone()));
        let buffer = Arc::new(Buffer::new(config.clone(), api.clone(), logger.clone()));

        store.load_local();

        if config.api_key.is_none() && config.mode == Mode::Live {
            logger.say_once(
                "no-api-key",
                "no PTN_API_KEY: running from the disk cache and the bundle only, with no remote calls",
            );
        }

        let mut threads = Vec::new();
        if config.remote_enabled() {
            if config.fetch_on_start && store.current().is_none() {
                // A cold start with nothing cached: fetch once, then never block again.
                if let Err(error) = store.refresh_now() {
                    logger.say(format!(
                        "the first snapshot fetch failed ({error}); serving what is cached and retrying in the background"
                    ));
                }
            }
            if config.poll {
                let poller = store.clone();
                threads.push(
                    std::thread::Builder::new()
                        .name("prompton-poller".to_string())
                        .spawn(move || Store::run_poller(poller))
                        .map_err(Error::Io)?,
                );
            }
            let worker = buffer.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("prompton-logs".to_string())
                    .spawn(move || Buffer::run_worker(worker))
                    .map_err(Error::Io)?,
            );
        }

        Ok(Client {
            inner: Arc::new(Inner {
                config,
                store,
                buffer,
                api,
                logger,
                threads: Mutex::new(threads),
                resolve_cache: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The resolved configuration.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    // -----------------------------------------------------------------------
    // resolution

    /// Resolves a use case with the `default` prompt.
    pub fn resolve(&self, use_case: &str) -> Result<Resolution> {
        self.resolve_with(use_case, &ResolveOptions::default())
    }

    /// Resolves a use case, picking a prompt by name.
    ///
    /// This never touches the network: it reads the snapshot in memory. When that document is
    /// older than `cache_ttl` a refresh is triggered — on the background poller, or on a one-shot
    /// thread when the poller is off — and this call returns the document it already has.
    pub fn resolve_with(&self, use_case: &str, options: &ResolveOptions) -> Result<Resolution> {
        let resolution = self.inner.store.resolve(use_case, options);
        if !self.inner.config.poll
            && self.inner.config.remote_enabled()
            && self.inner.store.needs_refresh()
        {
            Store::refresh_in_background(self.inner.store.clone());
        }
        resolution
    }

    /// The prompt names the live deployment pins, sorted.
    pub fn prompt_names(&self, use_case: &str) -> Result<Vec<String>> {
        let entry = self.inner.store.current().ok_or_else(|| {
            Error::NotReady(format!(
                "no snapshot for environment {:?}",
                self.inner.config.environment
            ))
        })?;
        if !entry.document.use_cases.contains_key(use_case) {
            return Err(Error::UnknownUseCase(use_case.to_string()));
        }
        Ok(entry.document.prompt_names(use_case))
    }

    /// What the store currently holds: source, ETag, age and whether it is stale.
    pub fn snapshot_info(&self) -> SnapshotInfo {
        self.inner.store.info()
    }

    /// Fetches the snapshot once, synchronously. For scripts and cold starts; the background
    /// poller does this on its own every `cache_ttl`.
    pub fn refresh(&self) -> Result<()> {
        self.inner.store.refresh_now()
    }

    /// Writes the current snapshot (and its `.meta.json` sidecar) to `path`, ready to be committed
    /// as the app's bundle.
    pub fn export_snapshot(&self, path: impl AsRef<Path>) -> Result<()> {
        self.inner.store.export(path.as_ref())
    }

    /// Installs a snapshot the app supplies, as `resolution_source: manual`. For tests and for
    /// apps that fetch the document themselves.
    pub fn set_snapshot(&self, document: &Value) -> Result<()> {
        self.set_snapshot_as(document, ResolutionSource::Manual)
    }

    /// Installs a snapshot the app supplies and says which tier it should be reported as.
    pub fn set_snapshot_as(&self, document: &Value, source: ResolutionSource) -> Result<()> {
        let (decoded, warnings) = SnapshotDocument::from_value(document)?;
        for warning in warnings {
            self.inner.logger.say(format!("snapshot: {warning}"));
        }
        let raw = serde_json::to_vec(document)?;
        self.inner.store.put_document(decoded, raw, source);
        Ok(())
    }

    /// Installs a snapshot from a file, as `resolution_source: manual`.
    pub fn set_snapshot_from_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = std::fs::read(path)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        self.set_snapshot(&value)
    }

    // -----------------------------------------------------------------------
    // the /resolve client

    /// Asks the server to resolve, instead of resolving locally.
    ///
    /// This is the simple path and the smoke test: it always reflects the newest revision, with no
    /// snapshot involved. It is **not** for a hot loop — cache the snapshot and use
    /// [`Client::resolve`] there. A call without variables is cached for `cache_ttl` per (use
    /// case, prompt, environment) so the templates can be rendered locally, and while PromptOn is
    /// rate-limiting or failing the cached answer keeps being served.
    pub fn resolve_remote(&self, request: &RemoteResolveRequest) -> Result<RemoteResolution> {
        if !self.inner.config.remote_enabled() {
            return Err(Error::RemoteDisabled(
                "resolve_remote needs an API key and live mode".to_string(),
            ));
        }

        let environment = request
            .environment
            .clone()
            .unwrap_or_else(|| self.inner.config.environment.clone());
        let key = format!(
            "{}\u{1}{}\u{1}{}",
            request.use_case,
            request.prompt.as_deref().unwrap_or(DEFAULT_PROMPT),
            environment
        );
        let cacheable = request.variables.is_none();

        if cacheable {
            if let Some(cached) = self.cached_resolve(&key, self.inner.config.cache_ttl) {
                return RemoteResolution::from_value(cached);
            }
        }

        let mut body = json!({
            "use_case": request.use_case,
            "environment": environment,
        });
        if let Some(prompt) = &request.prompt {
            body["prompt"] = Value::String(prompt.clone());
        }
        if let Some(variables) = &request.variables {
            body["variables"] = variables.to_value();
        }

        match self.inner.api.resolve(&body) {
            Ok(value) => {
                if cacheable {
                    if let Ok(mut cache) = self.inner.resolve_cache.lock() {
                        cache.insert(key, (Instant::now(), value.clone()));
                    }
                }
                RemoteResolution::from_value(value)
            }
            Err(failure) => {
                if failure.is_retryable() {
                    if let Some(cached) = self.cached_resolve(&key, Duration::MAX) {
                        self.inner.logger.say(format!(
                            "resolve failed ({}); serving the cached answer for {}",
                            failure.error, request.use_case
                        ));
                        return RemoteResolution::from_value(cached);
                    }
                }
                Err(failure.error)
            }
        }
    }

    fn cached_resolve(&self, key: &str, max_age: Duration) -> Option<Value> {
        let cache = self.inner.resolve_cache.lock().ok()?;
        let (at, value) = cache.get(key)?;
        if at.elapsed() <= max_age {
            Some(value.clone())
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // monitoring logs

    /// A fresh UUIDv7, for an app that wants the id before the call (to store its own row, or to
    /// score the generation later).
    pub fn generation_id(&self) -> String {
        uuidv7::generate()
    }

    /// Queues one monitoring log and returns immediately.
    ///
    /// The record is validated (`use_case`, `model`, `status`, `started_at`), given an `id` and an
    /// `sdk` object when it has none, put through the use case's payload policy, and queued. It
    /// never blocks on the network.
    pub fn log(&self, record: GenerationRecord) -> Result<()> {
        self.log_in(record, None)
    }

    /// Queues one monitoring log for a specific environment; batches never mix environments.
    pub fn log_in(&self, mut record: GenerationRecord, environment: Option<String>) -> Result<()> {
        if record.id.trim().is_empty() {
            record.id = uuidv7::generate();
        }
        if record.started_at.trim().is_empty() {
            record.started_at = record::now_iso8601();
        }
        if record.sdk.is_none() {
            record.sdk = Some(Sdk::default());
        }
        record.validate()?;

        let policy = self.inner.store.payload_policy(&record.use_case);

        let map = payload::apply(record.to_map(), policy.as_ref(), &self.payload_config());
        self.inner.buffer.enqueue(map, environment);
        Ok(())
    }

    fn payload_config(&self) -> PayloadConfig {
        PayloadConfig {
            defaults: self.inner.config.payload_defaults.clone(),
            hash_end_user: self.inner.config.hash_end_user,
            redact: self.inner.config.redact.clone(),
        }
    }

    /// Sends everything queued and waits for the answers, including the batches the background
    /// thread is already sending.
    ///
    /// This is what a script or a shutdown path wants: it sends now, ignoring the size and time
    /// triggers and the retry pause the background thread honours. A batch that fails stays
    /// queued with the same ids, and the error comes back.
    pub fn flush(&self) -> Result<FlushOutcome> {
        self.inner.buffer.flush(None)
    }

    /// What the monitoring-log buffer has done so far: queued, sent, accepted, dropped.
    pub fn log_stats(&self) -> LogStats {
        self.inner.buffer.stats()
    }

    /// The records captured in test and offline mode, in the order they were logged.
    pub fn captured_logs(&self) -> Vec<Map<String, Value>> {
        self.inner.buffer.captured()
    }

    /// Forgets the captured records.
    pub fn clear_captured_logs(&self) {
        self.inner.buffer.clear_captured();
    }

    /// Times a provider call, builds the monitoring log, and queues it.
    ///
    /// The closure returns exactly what this function returns, so nothing about the app's control
    /// flow changes. A `Ok(Completion)` is logged as `status: ok`; a [`CallFailure`] as
    /// `status: error`, keeping the usage and output when the provider answered but the app could
    /// not use the answer; a panic is logged as `error.kind: app` and then resumed.
    pub fn with_generation<T, F>(
        &self,
        resolution: &Resolution,
        meta: CallMeta,
        call: F,
    ) -> std::result::Result<Completion<T>, CallFailure>
    where
        F: FnOnce() -> std::result::Result<Completion<T>, CallFailure>,
    {
        let id = meta.id.clone().unwrap_or_else(uuidv7::generate);
        let started_at = record::now_iso8601();
        let start = Instant::now();

        let result = catch_unwind(AssertUnwindSafe(call));
        let timing = record::Timing {
            id,
            started_at,
            latency_ms: start.elapsed().as_millis() as i64,
        };

        match result {
            Ok(Ok(completion)) => {
                let log = record::build_record(
                    resolution,
                    &meta,
                    timing,
                    Status::Ok,
                    Some(&completion.outcome),
                    None,
                );
                self.log_quietly(log);
                Ok(completion)
            }
            Ok(Err(failure)) => {
                let log = record::build_record(
                    resolution,
                    &meta,
                    timing,
                    Status::Error,
                    failure.outcome.as_deref(),
                    Some(&failure.error),
                );
                self.log_quietly(log);
                Err(failure)
            }
            Err(panic) => {
                let message = panic_message(panic.as_ref());
                let log = record::build_record(
                    resolution,
                    &meta,
                    timing,
                    Status::Error,
                    None,
                    Some(&GenerationError::new(ErrorKind::App, message)),
                );
                self.log_quietly(log);
                std::panic::resume_unwind(panic)
            }
        }
    }

    fn log_quietly(&self, record: GenerationRecord) {
        if let Err(error) = self.log(record) {
            self.inner
                .logger
                .say(format!("dropping a monitoring log: {error}"));
        }
    }

    /// Stops the background threads after a final flush. Dropping the last clone does this too.
    pub fn shutdown(&self) {
        self.inner.store.stop();
        if self.inner.config.mode == Mode::Live {
            let _ = self
                .inner
                .buffer
                .flush(Some(Instant::now() + Duration::from_secs(5)));
        }
        self.inner.buffer.stop();
        if let Ok(mut threads) = self.inner.threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }

    /// How many snapshot requests the SDK has made. Tests assert that the cache TTL holds.
    pub fn snapshot_fetch_count(&self) -> u64 {
        self.inner.store.fetch_count()
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "the provider call panicked".to_string()
    }
}

/// A request for the server-side `/resolve` endpoint.
#[derive(Debug, Clone, Default)]
pub struct RemoteResolveRequest {
    /// The use case key.
    pub use_case: String,
    /// The prompt name; `None` means `default`.
    pub prompt: Option<String>,
    /// The environment; `None` means the client's.
    pub environment: Option<String>,
    /// The variables to render with. Leave them out to get the raw templates back (and to let the
    /// answer be cached).
    pub variables: Option<Vars>,
}

impl RemoteResolveRequest {
    /// A request for one use case's `default` prompt.
    pub fn new(use_case: impl Into<String>) -> RemoteResolveRequest {
        RemoteResolveRequest {
            use_case: use_case.into(),
            ..RemoteResolveRequest::default()
        }
    }

    /// Picks a prompt by name.
    pub fn prompt(mut self, prompt: impl Into<String>) -> RemoteResolveRequest {
        self.prompt = Some(prompt.into());
        self
    }

    /// Reads another environment than the client's.
    pub fn environment(mut self, environment: impl Into<String>) -> RemoteResolveRequest {
        self.environment = Some(environment.into());
        self
    }

    /// Asks the server to render with these variables.
    pub fn variables(mut self, variables: impl Into<Vars>) -> RemoteResolveRequest {
        self.variables = Some(variables.into());
        self
    }
}

/// What `POST /resolve` answered.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteResolution {
    /// The use case key.
    pub use_case: String,
    /// Chat, text or embedding.
    pub kind: Kind,
    /// The live deployment's id.
    pub deployment_id: Option<String>,
    /// The live deployment's revision number.
    pub deployment_revision: Option<i64>,
    /// The prompt name that was used.
    pub prompt: Option<String>,
    /// Every prompt name the live revision pins.
    pub available_prompts: Vec<String>,
    /// The provider-side model string.
    pub model: Option<String>,
    /// The catalog id of that model.
    pub model_id: Option<String>,
    /// The provider name.
    pub provider: Option<String>,
    /// The layered parameters.
    pub params: Map<String, Value>,
    /// The layered provider options.
    pub provider_options: Map<String, Value>,
    /// The pinned prompt version id.
    pub prompt_version_id: Option<String>,
    /// The pinned prompt version number.
    pub prompt_version_number: Option<i64>,
    /// The messages: rendered when variables were sent, raw templates otherwise.
    pub messages: Option<Vec<Message>>,
    /// The text: rendered when variables were sent, the raw template otherwise.
    pub text: Option<String>,
    /// Server-side warnings, such as `missing_model: <id>`.
    pub warnings: Vec<String>,
    /// The snapshot ETag this answer was resolved from.
    pub etag: Option<String>,
    /// The response as it arrived.
    pub raw: Value,
}

impl RemoteResolution {
    fn from_value(value: Value) -> Result<RemoteResolution> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::Transport("resolve returned a non-object body".to_string()))?
            .clone();

        let string = |key: &str| -> Option<String> {
            object.get(key).and_then(Value::as_str).map(str::to_string)
        };
        let map = |key: &str| -> Map<String, Value> {
            object
                .get(key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        };

        Ok(RemoteResolution {
            use_case: string("use_case").unwrap_or_default(),
            kind: object
                .get("kind")
                .and_then(Value::as_str)
                .map(|kind| match kind {
                    "text" => Kind::Text,
                    "embedding" => Kind::Embedding,
                    "chat" => Kind::Chat,
                    other => Kind::Other(other.to_string()),
                })
                .unwrap_or(Kind::Chat),
            deployment_id: object
                .get("deployment")
                .and_then(|deployment| deployment.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            deployment_revision: object
                .get("deployment")
                .and_then(|deployment| deployment.get("revision"))
                .and_then(Value::as_i64),
            prompt: string("prompt"),
            available_prompts: object
                .get("prompts")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            model: string("model"),
            model_id: string("model_id"),
            provider: string("provider"),
            params: map("effective_params"),
            provider_options: map("effective_provider_options"),
            prompt_version_id: object
                .get("prompt_version")
                .and_then(|version| version.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            prompt_version_number: object
                .get("prompt_version")
                .and_then(|version| version.get("number"))
                .and_then(Value::as_i64),
            messages: object
                .get("messages")
                .and_then(|messages| serde_json::from_value(messages.clone()).ok()),
            text: string("text"),
            warnings: object
                .get("warnings")
                .and_then(Value::as_array)
                .map(|warnings| {
                    warnings
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            etag: string("etag"),
            raw: value,
        })
    }

    /// Renders the templates this answer carries, locally, with the Liquid subset.
    ///
    /// Only meaningful for an answer fetched **without** variables; when the server rendered
    /// already, there is nothing left to substitute.
    pub fn render(&self, vars: impl Into<Vars>) -> Result<Rendered> {
        let vars = vars.into();
        match (&self.messages, &self.text) {
            (Some(messages), _) => Ok(Rendered::Messages(template::render_messages(
                messages,
                &vars,
                Engine::Liquid,
            )?)),
            (None, Some(text)) => Ok(Rendered::Text(template::render(
                text,
                &vars,
                Engine::Liquid,
            )?)),
            (None, None) => Ok(Rendered::None),
        }
    }
}
