//! The official PromptOn SDK for Rust.
//!
//! PromptOn is the control plane for your app's LLM prompts. For each **use case** and
//! **environment** it holds one **pin**: a prompt version, one model, and its parameters. This SDK
//! fetches a **use-case document** of those pins, renders the pinned prompt with this call's variables, and
//! sends **monitoring logs** back in batches. Your app calls the provider itself, with its own key
//! and its own HTTP client — PromptOn is config-fetch, **not a proxy**, so it is never in the
//! request path, and if it is down your app keeps running on the last use-case document it received.
//!
//! ```no_run
//! use prompton::{CallMeta, Client, Completion, Result};
//!
//! let prompton = Client::from_env()?;                       // PTN_HOST, PTN_API_KEY
//! let use_case = prompton.use_case("greeting")?;             // model, params and prompt
//! let messages = use_case.messages(serde_json::json!({"name": "Ada"}))?;
//!
//! let answer = use_case.track(CallMeta::new().input_messages(messages.clone()), || {
//!     let text = my_provider_call(&use_case.model, &messages); // your key, your HTTP client
//!     Ok(Completion::new(text.clone(), Result::text(text).with_finish_reason("stop")))
//! })?;
//! # fn my_provider_call(_model: &Option<String>, _messages: &[prompton::Message]) -> String { String::new() }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # How it fails
//!
//! | when | what happens |
//! |---|---|
//! | a demand fetch times out, 5xx or 429 | the previous use-case document keeps serving; the caller sees nothing |
//! | PromptOn is down at start | the disk cache, then the bundle, answer `use_case` |
//! | nothing is cached anywhere | `use_case` returns [`Error::NotReady`] — the only error worth retrying |
//! | the use case has no live deployment | [`Error::Unresolved`] — a bug in the deployment, never a reason to use a hard-coded prompt |
//! | the prompt name is not pinned | [`Error::UnknownPrompt`], with the names that are |
//! | a monitoring log cannot be sent | it is retried, then dropped and counted; a model call never waits for it |
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

pub use crate::api::{LogsAck, RejectedRecord};
pub use crate::buffer::{FlushResult, LogStats, MAX_RECORDS_PER_REQUEST, MAX_REQUEST_BYTES};
pub use crate::config::{
    ClientBuilder, Config, DiskCache, LogConfig, Mode, DEFAULT_CACHE_TTL, DEFAULT_ENVIRONMENT,
    DEFAULT_HOST,
};
pub use crate::error::Error;
use crate::error::Result as SdkResult;
pub use crate::http::{
    HttpClient, HttpRequest, HttpResponse, Method, SharedHttpClient, TransportError, UreqClient,
};
pub use crate::logger::LogSink;
pub use crate::payload::{PayloadConfig, RedactHook};
pub use crate::record::{
    CallFailure, CallMeta, Completion, CostSource, ErrorKind, LogError, LogRecord, Result, Sdk,
    Status, Usage, SDK_NAME,
};
pub use crate::resolver::{Source, DEFAULT_PROMPT};
pub use crate::snapshot::{
    Deployment, InputVariable, Kind, Message, Model, PayloadMode, PayloadPolicy, PromptVersion,
    UseCaseDocument, UseCaseSpec, SCHEMA_VERSION,
};
pub use crate::stop_kind::StopKind;
pub use crate::store::UseCaseDocumentInfo;
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
use crate::resolver::{Resolution, ResolveOptions};
use crate::store::Store;

/// The SDK's version, as it appears in the `User-Agent` and in every record's `sdk` object.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One monitored tool or completion event. Provider-native JSON fields are preserved.
pub type TraceEvent = Map<String, Value>;

/// Trace event kind for one customer-tool attempt.
pub const EVENT_KIND_TOOL_ATTEMPT: &str = "tool_attempt";
/// Trace event kind for a model completion step.
pub const EVENT_KIND_COMPLETION: &str = "completion";

/// Trace event status for an in-flight step.
pub const EVENT_STATUS_STARTED: &str = "started";
/// Trace event status for a completed successful step.
pub const EVENT_STATUS_OK: &str = "ok";
/// Trace event status for a failed step.
pub const EVENT_STATUS_ERROR: &str = "error";
/// Trace event status for a policy-denied step.
pub const EVENT_STATUS_DENIED: &str = "denied";
/// Trace event status for a cancelled step.
pub const EVENT_STATUS_CANCELLED: &str = "cancelled";
/// Trace event status for a timed-out step.
pub const EVENT_STATUS_TIMEOUT: &str = "timeout";
/// Trace event status for an expected step that was not observed.
pub const EVENT_STATUS_MISSING: &str = "missing";
/// Trace event status for a step that ended without complete evidence.
pub const EVENT_STATUS_INCOMPLETE: &str = "incomplete";

struct Inner {
    config: Arc<Config>,
    store: Arc<Store>,
    buffer: Arc<Buffer>,
    api: Arc<Api>,
    logger: Logger,
    threads: Mutex<Vec<JoinHandle<()>>>,
    prompt_cache: Mutex<HashMap<String, (Instant, Value)>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.store.stop();
        // Best effort: give the queue one last chance to reach PromptOn before the process exits.
        // `drain` rather than `flush`, so an armed `Retry-After` is left alone instead of being
        // sent into, and the exit stays quick.
        if self.config.mode == Mode::Live {
            let _ = self
                .buffer
                .drain(Some(Instant::now() + Duration::from_secs(5)));
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
    pub fn from_env() -> SdkResult<Client> {
        ClientBuilder::new().build()
    }

    /// Starts a client from an already-resolved [`Config`].
    pub fn from_config(config: Config) -> SdkResult<Client> {
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
                        "the first use-case document fetch failed ({error}); serving what is cached until a later demand fetch"
                    ));
                }
            }
            if config.poll {
                logger.say_once(
                    "poll-disabled",
                    "poll(true) is ignored; runtime config refresh is demand-driven per key",
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
                prompt_cache: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The resolved configuration.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    // -----------------------------------------------------------------------
    // use cases

    /// Reads a use case with the `default` prompt.
    pub fn use_case(&self, key: &str) -> SdkResult<UseCase> {
        self.use_case_with(key, &UseCaseOptions::default())
    }

    /// Reads a use case, picking a prompt by name.
    ///
    /// In live mode this gives the requested key one bounded chance to refresh when its cache is
    /// missing or stale. If that fetch fails or times out, the last cached value is used.
    pub fn use_case_with(&self, key: &str, options: &UseCaseOptions) -> SdkResult<UseCase> {
        let options = ResolveOptions {
            prompt: options.prompt.clone(),
        };
        self.inner.store.refresh_key_if_needed(key);
        let resolution = self.inner.store.resolve(key, &options);
        resolution.map(|resolution| UseCase::from_resolution(self.clone(), resolution))
    }

    /// The prompt names the live deployment pins, sorted.
    pub fn prompt_names(&self, use_case: &str) -> SdkResult<Vec<String>> {
        self.inner.store.refresh_key_if_needed(use_case);
        self.inner.store.prompt_names(use_case)
    }

    /// What the store currently holds: source, ETag, age and whether it is stale.
    pub fn use_cases_info(&self) -> UseCaseDocumentInfo {
        self.inner.store.info()
    }

    /// Compatibility no-op. Runtime lookup refreshes only the requested key through
    /// [`Client::use_case`]; bundle tooling should call [`Client::export_use_cases`] after the
    /// wanted keys have been cached.
    pub fn refresh(&self) -> SdkResult<()> {
        self.inner.store.refresh_now()
    }

    /// Writes the current use-case document (and its `.meta.json` sidecar) to `path`, ready to be committed
    /// as the app's bundle.
    pub fn export_use_cases(&self, path: impl AsRef<Path>) -> SdkResult<()> {
        self.inner.store.export(path.as_ref())
    }

    /// Installs a use-case document the app supplies, as `source: manual`. For tests and for
    /// apps that fetch the document themselves.
    pub fn set_use_cases(&self, document: &Value) -> SdkResult<()> {
        self.set_use_cases_as(document, Source::Manual)
    }

    /// Installs a use-case document the app supplies and says which tier it should be reported as.
    pub fn set_use_cases_as(&self, document: &Value, source: Source) -> SdkResult<()> {
        let (decoded, warnings) = UseCaseDocument::from_value(document)?;
        for warning in warnings {
            self.inner
                .logger
                .say(format!("use-case document: {warning}"));
        }
        let raw = serde_json::to_vec(document)?;
        self.inner.store.put_document(decoded, raw, source);
        Ok(())
    }

    /// Installs a use-case document from a file, as `source: manual`.
    pub fn set_use_cases_from_file(&self, path: impl AsRef<Path>) -> SdkResult<()> {
        let bytes = std::fs::read(path)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        self.set_use_cases(&value)
    }

    // -----------------------------------------------------------------------
    // the remote prompt client

    /// Asks the server to render a use case prompt, instead of rendering locally.
    ///
    /// This is the simple path and the smoke test: it always reflects the newest revision, with no
    /// use-case document involved. It is **not** for a hot loop — cache the use-case document and use
    /// [`Client::use_case`] there. A call without variables is cached for `cache_ttl` per (use
    /// case, prompt, environment) so the templates can be rendered locally, and while PromptOn is
    /// rate-limiting or failing the cached answer keeps being served.
    pub fn prompt_remote(&self, request: &RemotePromptRequest) -> SdkResult<RemotePrompt> {
        if !self.inner.config.remote_enabled() {
            return Err(Error::RemoteDisabled(
                "prompt_remote needs an API key and live mode".to_string(),
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
            if let Some(cached) = self.cached_prompt_response(&key, self.inner.config.cache_ttl) {
                return RemotePrompt::from_value(cached);
            }
        }

        let mut body = json!({
            "environment": environment,
        });
        if let Some(prompt) = &request.prompt {
            body["template"] = Value::String(prompt.clone());
        }
        if let Some(variables) = &request.variables {
            body["variables"] = variables.to_value();
        }

        match self.inner.api.prompt(&request.use_case, &body) {
            Ok(value) => {
                if cacheable {
                    if let Ok(mut cache) = self.inner.prompt_cache.lock() {
                        cache.insert(key, (Instant::now(), value.clone()));
                    }
                }
                RemotePrompt::from_value(value)
            }
            Err(failure) => {
                if failure.is_retryable() {
                    if let Some(cached) = self.cached_prompt_response(&key, Duration::MAX) {
                        self.inner.logger.say(format!(
                            "prompt render failed ({}); serving the cached answer for {}",
                            failure.error, request.use_case
                        ));
                        return RemotePrompt::from_value(cached);
                    }
                }
                Err(failure.error)
            }
        }
    }

    fn cached_prompt_response(&self, key: &str, max_age: Duration) -> Option<Value> {
        let cache = self.inner.prompt_cache.lock().ok()?;
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
    /// score the log later).
    pub fn log_id(&self) -> String {
        uuidv7::generate()
    }

    /// Queues one monitoring log and returns immediately.
    ///
    /// The record is validated (`use_case`, `model`, `status`, `started_at`), given an `id` and an
    /// `sdk` object when it has none, put through the use case's payload policy, and queued. It
    /// never blocks on the network.
    pub fn log(&self, record: LogRecord) -> SdkResult<()> {
        self.log_in(record, None)
    }

    /// Queues one monitoring log for a specific environment; batches never mix environments.
    pub fn log_in(&self, mut record: LogRecord, environment: Option<String>) -> SdkResult<()> {
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

    /// Submits observed tool/completion trace events synchronously. The SDK never calls customer
    /// tools; it only sends the events your app observed. Missing `event_id`, `observed_at` and
    /// `sdk` are filled before the request, and generated ids are kept in the supplied
    /// maps so caller retries reuse the same event ids.
    pub fn log_events(
        &self,
        events: &mut [TraceEvent],
        environment: Option<String>,
    ) -> SdkResult<LogsAck> {
        if events.is_empty() {
            return Ok(LogsAck::default());
        }
        if events.len() > 500 {
            return Err(Error::InvalidRecord(
                "log_events accepts at most 500 events".to_string(),
            ));
        }
        if !self.inner.config.remote_enabled() {
            return Err(Error::RemoteDisabled(
                "trace events require live mode and an API key".to_string(),
            ));
        }
        for (index, event) in events.iter_mut().enumerate() {
            fill_trace_event(index, event)?;
        }
        let env = environment.unwrap_or_else(|| self.inner.config.environment.clone());
        self.inner
            .api
            .post_events(&env, events)
            .map_err(|failure| failure.error)
    }

    /// Sends everything queued and waits for the answers, including the batches the background
    /// thread is already sending.
    ///
    /// This is what a script wants: it sends now, ignoring the size and time triggers *and* the
    /// retry pause the background thread honours — you asked explicitly, so the request goes out
    /// even inside an armed `Retry-After` window. A batch that fails stays queued with the same
    /// ids, and the error comes back.
    ///
    /// The shutdown paths ([`Client::shutdown`] and dropping the last clone) do not use this:
    /// they drain what can go out immediately and respect a `Retry-After` the server asked for.
    pub fn flush(&self) -> SdkResult<FlushResult> {
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
    fn track_resolution<T, F>(
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
                    Some(&completion.result),
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
                    failure.result.as_deref(),
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
                    Some(&LogError::new(ErrorKind::App, message)),
                );
                self.log_quietly(log);
                std::panic::resume_unwind(panic)
            }
        }
    }

    fn log_quietly(&self, record: LogRecord) {
        if let Err(error) = self.log(record) {
            self.inner
                .logger
                .say(format!("dropping a monitoring log: {error}"));
        }
    }

    /// Stops the background threads after a final best-effort drain of the monitoring-log queue.
    /// Dropping the last clone does this too.
    ///
    /// The drain is bounded: it never waits out or sends into an armed `Retry-After`, and it
    /// gives up after a few seconds, so shutting down while PromptOn is unhealthy is quick.
    pub fn shutdown(&self) {
        self.inner.store.stop();
        if self.inner.config.mode == Mode::Live {
            let _ = self
                .inner
                .buffer
                .drain(Some(Instant::now() + Duration::from_secs(5)));
        }
        self.inner.buffer.stop();
        if let Ok(mut threads) = self.inner.threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }

    /// How many use-case document requests the SDK has made. Tests assert that the cache TTL holds.
    pub fn use_case_fetch_count(&self) -> u64 {
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

fn fill_trace_event(index: usize, event: &mut TraceEvent) -> SdkResult<()> {
    if event
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        event.insert("event_id".to_string(), Value::String(uuidv7::generate()));
    }
    if event
        .get("observed_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        event.insert(
            "observed_at".to_string(),
            Value::String(record::now_iso8601()),
        );
    }
    if event
        .get("trace_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err(Error::InvalidRecord(format!(
            "trace event {index} needs trace_id"
        )));
    }
    let kind = event
        .get("event_kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(kind, EVENT_KIND_TOOL_ATTEMPT | EVENT_KIND_COMPLETION) {
        return Err(Error::InvalidRecord(format!(
            "trace event {index} has unsupported event_kind"
        )));
    }
    let status = event.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        status,
        EVENT_STATUS_STARTED
            | EVENT_STATUS_OK
            | EVENT_STATUS_ERROR
            | EVENT_STATUS_DENIED
            | EVENT_STATUS_CANCELLED
            | EVENT_STATUS_TIMEOUT
            | EVENT_STATUS_MISSING
            | EVENT_STATUS_INCOMPLETE
    ) {
        return Err(Error::InvalidRecord(format!(
            "trace event {index} has unsupported status"
        )));
    }
    event
        .entry("sdk".to_string())
        .or_insert_with(|| json!({"name": SDK_NAME, "version": VERSION}));
    Ok(())
}

/// Options for reading a use case.
#[derive(Debug, Clone, Default)]
pub struct UseCaseOptions {
    /// The prompt name to pick; `None` means `default`.
    pub prompt: Option<String>,
}

impl UseCaseOptions {
    /// Picks a prompt by name.
    pub fn prompt(name: impl Into<String>) -> UseCaseOptions {
        UseCaseOptions {
            prompt: Some(name.into()),
        }
    }
}

/// A deployed use case: model configuration plus the pinned prompt.
#[derive(Debug, Clone)]
pub struct UseCase {
    client: Client,
    resolution: Resolution,
    /// The use case key.
    pub key: String,
    /// Chat, text or embedding.
    pub kind: Kind,
    /// The provider-side model string to send to the provider.
    pub model: Option<String>,
    /// The catalog id of that model.
    pub model_id: Option<String>,
    /// The provider name.
    pub provider: Option<String>,
    /// The layered parameters.
    pub params: Map<String, Value>,
    /// The layered provider options.
    pub provider_options: Map<String, Value>,
    /// The live deployment's id.
    pub deployment_id: Option<String>,
    /// The live deployment's UTC-date revision label.
    pub deployment_revision: Option<String>,
    /// The prompt name that was chosen.
    pub prompt: Option<String>,
    /// Every prompt name the live revision pins.
    pub prompt_names: Vec<String>,
    /// Which tier the document came from.
    pub source: Source,
    /// The pinned prompt version id.
    pub prompt_version_id: Option<String>,
    /// The pinned prompt version number.
    pub prompt_version_number: Option<i64>,
    /// The ETag of the use case document this came from.
    pub etag: Option<String>,
    /// Warnings such as `missing_model: <id>`.
    pub warnings: Vec<String>,
}

impl UseCase {
    fn from_resolution(client: Client, resolution: Resolution) -> UseCase {
        UseCase {
            client,
            key: resolution.use_case.clone(),
            kind: resolution.kind.clone(),
            model: resolution.model.clone(),
            model_id: resolution.model_id.clone(),
            provider: resolution.provider.clone(),
            params: resolution.params.clone(),
            provider_options: resolution.provider_options.clone(),
            deployment_id: resolution.deployment_id.clone(),
            deployment_revision: resolution.deployment_revision.clone(),
            prompt: resolution.prompt.clone(),
            prompt_names: resolution.available_prompts.clone(),
            source: resolution.source,
            prompt_version_id: resolution.prompt_version_id.clone(),
            prompt_version_number: resolution.prompt_version_number,
            etag: resolution.etag.clone(),
            warnings: resolution.warnings.clone(),
            resolution,
        }
    }

    /// Renders a chat prompt, or fails when this use case is not a chat one.
    pub fn messages(&self, vars: impl Into<Vars>) -> SdkResult<Vec<Message>> {
        self.resolution.render_messages(vars)
    }

    /// Renders a text prompt, or fails when this use case is not a text one.
    pub fn text(&self, vars: impl Into<Vars>) -> SdkResult<String> {
        self.resolution.render_text(vars)
    }

    /// Times a provider call, builds the monitoring log, and queues it.
    pub fn track<T, F>(
        &self,
        meta: CallMeta,
        call: F,
    ) -> std::result::Result<Completion<T>, CallFailure>
    where
        F: FnOnce() -> std::result::Result<Completion<T>, CallFailure>,
    {
        self.client.track_resolution(&self.resolution, meta, call)
    }

    /// Starts a manual log record from this use case's deployment evidence.
    pub fn log_record(&self, status: Status) -> LogRecord {
        LogRecord::from_resolution(&self.resolution, status)
    }
}

/// A request for the server-side prompt endpoint.
#[derive(Debug, Clone, Default)]
pub struct RemotePromptRequest {
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

impl RemotePromptRequest {
    /// A request for one use case's `default` prompt.
    pub fn new(use_case: impl Into<String>) -> RemotePromptRequest {
        RemotePromptRequest {
            use_case: use_case.into(),
            ..RemotePromptRequest::default()
        }
    }

    /// Picks a prompt by name.
    pub fn prompt(mut self, prompt: impl Into<String>) -> RemotePromptRequest {
        self.prompt = Some(prompt.into());
        self
    }

    /// Reads another environment than the client's.
    pub fn environment(mut self, environment: impl Into<String>) -> RemotePromptRequest {
        self.environment = Some(environment.into());
        self
    }

    /// Asks the server to render with these variables.
    pub fn variables(mut self, variables: impl Into<Vars>) -> RemotePromptRequest {
        self.variables = Some(variables.into());
        self
    }
}

/// What `POST /prompts/{key}/render` answered.
#[derive(Debug, Clone, PartialEq)]
pub struct RemotePrompt {
    /// The use case key.
    pub key: String,
    /// Chat, text or embedding.
    pub kind: Kind,
    /// The live deployment's id.
    pub deployment_id: Option<String>,
    /// The live deployment's UTC-date revision label.
    pub deployment_revision: Option<String>,
    /// The prompt name that was used.
    pub prompt: Option<String>,
    /// Every prompt name the live revision pins.
    pub prompt_names: Vec<String>,
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
    /// Which tier the server resolved from.
    pub source: Source,
    /// The use-case document ETag this answer was resolved from.
    pub etag: Option<String>,
    /// The response as it arrived.
    pub raw: Value,
}

impl RemotePrompt {
    fn from_value(value: Value) -> SdkResult<RemotePrompt> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::Transport("prompt returned a non-object body".to_string()))?
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

        Ok(RemotePrompt {
            key: string("key").unwrap_or_default(),
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
                .and_then(revision_of),
            prompt: string("template").or_else(|| string("prompt")),
            prompt_names: object
                .get("template_names")
                .or_else(|| object.get("prompt_names"))
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
            params: crate::resolver::merge_tools(
                map("params"),
                object.get("tools").and_then(Value::as_object),
            )?,
            provider_options: map("provider_options"),
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
            source: object
                .get("source")
                .and_then(Value::as_str)
                .and_then(Source::from_str)
                .unwrap_or(Source::Remote),
            etag: string("etag"),
            raw: value,
        })
    }

    /// Renders the chat messages this answer carries, or fails when it is not a chat answer.
    pub fn messages(&self, vars: impl Into<Vars>) -> SdkResult<Vec<Message>> {
        let vars = vars.into();
        match &self.messages {
            Some(messages) => Ok(template::render_messages(messages, &vars, Engine::Liquid)?),
            None => Err(Error::Template(TemplateError::Render(format!(
                "use case {} has no chat messages to render",
                self.key
            )))),
        }
    }

    /// Renders the text template this answer carries, or fails when it is not a text answer.
    pub fn text(&self, vars: impl Into<Vars>) -> SdkResult<String> {
        let vars = vars.into();
        match &self.text {
            Some(text) => Ok(template::render(text, &vars, Engine::Liquid)?),
            None => Err(Error::Template(TemplateError::Render(format!(
                "use case {} has no text template to render",
                self.key
            )))),
        }
    }
}

fn revision_of(value: &Value) -> Option<String> {
    match value {
        Value::String(string) => Some(string.clone()),
        Value::Number(number) => number.as_i64().map(|value| format!("v2026.09.30-{value}")),
        _ => None,
    }
}
