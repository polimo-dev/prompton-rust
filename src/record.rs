//! Monitoring-log records: the JSON `POST /api/v1/logs` accepts, and the types the
//! convenience wrapper builds one from.
//!
//! Send one record per model call your app made, **including failures** — error rates and
//! truncation rates are meaningless without them. Never log secrets: no provider keys, no
//! `PTN_API_KEY`, no user PII beyond `end_user_ref`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::resolver::Resolution;
use crate::snapshot::Message;
use crate::stop_kind::StopKind;
use crate::template::Vars;

/// The name this SDK reports in every record's `sdk` object.
pub const SDK_NAME: &str = "prompton-rust";

/// Whether the provider call succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The call returned an answer.
    Ok,
    /// The call failed, or the app could not use the answer.
    Error,
}

/// The canonical error kinds the server understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The provider answered 4xx (other than 429).
    // `rename_all = "snake_case"` puts no underscore before a digit, so both of these have to
    // spell out the contract's wire value. Getting it wrong makes the server reject the record.
    #[serde(rename = "http_4xx")]
    Http4xx,
    /// The provider answered 5xx.
    #[serde(rename = "http_5xx")]
    Http5xx,
    /// The provider answered 429, or its own rate limiter fired.
    RateLimited,
    /// The call timed out.
    Timeout,
    /// DNS, TCP or TLS failure: the request never got an answer.
    Transport,
    /// The answer arrived but could not be parsed or validated.
    Parse,
    /// Anything else, including a panic in the app's own code.
    App,
}

impl ErrorKind {
    /// The kind implied by an HTTP status: 429 is `rate_limited`, other 4xx `http_4xx`, 5xx
    /// `http_5xx`.
    pub fn from_status(status: u16) -> ErrorKind {
        match status {
            429 => ErrorKind::RateLimited,
            400..=499 => ErrorKind::Http4xx,
            500..=599 => ErrorKind::Http5xx,
            _ => ErrorKind::App,
        }
    }
}

/// What went wrong with a provider call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogError {
    /// One of the seven canonical kinds.
    pub kind: ErrorKind,
    /// The HTTP status, when the failure was an HTTP one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// A short message. Capped at 2048 bytes before sending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl LogError {
    /// An error of `kind` with a message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> LogError {
        LogError {
            kind,
            status: None,
            message: Some(message.into()),
        }
    }

    /// An HTTP failure: the kind is derived from the status.
    pub fn http(status: u16, message: impl Into<String>) -> LogError {
        LogError {
            kind: ErrorKind::from_status(status),
            status: Some(status),
            message: Some(message.into()),
        }
    }
}

/// Where a cost figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostSource {
    /// The provider reported the cost.
    Provider,
    /// The SDK or the app computed it from the model catalog.
    Catalog,
    /// Nobody knows.
    Unknown,
}

/// Token counts and cost for one call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Completion tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Cost in US dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Where `cost_usd` came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_source: Option<CostSource>,
    /// The provider's raw usage object. Blanked by the server above 16 KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

impl Usage {
    /// Token counts with no cost.
    pub fn tokens(input_tokens: i64, output_tokens: i64) -> Usage {
        Usage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            ..Usage::default()
        }
    }

    /// Adds a cost and its source.
    pub fn with_cost(mut self, cost_usd: f64, source: CostSource) -> Usage {
        self.cost_usd = Some(cost_usd);
        self.cost_source = Some(source);
        self
    }
}

/// Which SDK produced a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sdk {
    /// The SDK name, `prompton-rust` for this crate.
    pub name: String,
    /// The SDK version.
    pub version: String,
}

impl Default for Sdk {
    fn default() -> Sdk {
        Sdk {
            name: SDK_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// One monitoring log: what your app did, for one model call.
///
/// Only `id`, `use_case`, `model`, `status` and `started_at` are required; `id` is filled with a
/// fresh UUIDv7 when you leave it empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    /// The idempotency key: a UUIDv7 the app generates before the provider call.
    pub id: String,
    /// The use case key.
    pub use_case: String,
    /// The provider-side model string that was requested.
    pub model: String,
    /// Whether the call succeeded.
    pub status: Status,
    /// When the call started, ISO 8601 with an offset.
    pub started_at: String,

    /// `chat`, `text` or `embedding`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The deployment revision that resolved this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// That revision's number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_revision: Option<i64>,
    /// The prompt name that was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The pinned prompt version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_version_id: Option<String>,
    /// The catalog id of the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Which tier the configuration came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The provider that was called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model the provider actually served, when it differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    /// The upstream provider a router picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_provider: Option<String>,
    /// The parameters the call was made with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Map<String, Value>>,
    /// `{"variables", "messages"}` or `{"text"}` — or a pre-hashed `{"sha256", "bytes"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// `{"content", "tool_calls"}` — or a pre-hashed `{"sha256", "bytes"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// The provider's raw finish reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// The normalised stop kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_kind: Option<StopKind>,
    /// What went wrong, for `status: error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<LogError>,
    /// Tokens and cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Wall-clock duration of the provider call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
    /// A correlation id of the app's choosing (a job id, a request id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// The position of this call inside a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    /// A stable, non-identifying reference to the end user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_user_ref: Option<String>,
    /// Free-form tags. At most 2 KB, or the record is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Map<String, Value>>,
    /// Free-form app data. At most 4 KB, or the record is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    /// Which SDK sent the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<Sdk>,
}

impl LogRecord {
    /// A record with the five required fields; `id` is a fresh UUIDv7 and `started_at` is now.
    pub fn new(use_case: impl Into<String>, model: impl Into<String>, status: Status) -> LogRecord {
        LogRecord {
            id: crate::uuidv7::generate(),
            use_case: use_case.into(),
            model: model.into(),
            status,
            started_at: now_iso8601(),
            kind: None,
            deployment_id: None,
            deployment_revision: None,
            prompt: None,
            prompt_version_id: None,
            model_id: None,
            source: None,
            provider: None,
            model_used: None,
            upstream_provider: None,
            params: None,
            input: None,
            output: None,
            finish_reason: None,
            stop_kind: None,
            error: None,
            usage: None,
            latency_ms: None,
            trace_id: None,
            sequence: None,
            end_user_ref: None,
            context: None,
            metadata: None,
            sdk: Some(Sdk::default()),
        }
    }

    /// Copies the resolution evidence — deployment, prompt, model and source — into the record.
    pub(crate) fn from_resolution(resolution: &Resolution, status: Status) -> LogRecord {
        let mut record = LogRecord::new(
            resolution.use_case.clone(),
            resolution.model.clone().unwrap_or_default(),
            status,
        );
        record.apply_resolution(resolution);
        record
    }

    /// Fills the resolution-derived fields of an existing record.
    pub(crate) fn apply_resolution(&mut self, resolution: &Resolution) {
        if self.use_case.is_empty() {
            self.use_case = resolution.use_case.clone();
        }
        if self.model.is_empty() {
            if let Some(model) = &resolution.model {
                self.model = model.clone();
            }
        }
        self.kind
            .get_or_insert_with(|| resolution.kind.as_str().to_string());
        if self.deployment_id.is_none() {
            self.deployment_id = resolution.deployment_id.clone();
        }
        if self.deployment_revision.is_none() {
            self.deployment_revision = resolution.deployment_revision;
        }
        if self.prompt.is_none() {
            self.prompt = resolution.prompt.clone();
        }
        if self.prompt_version_id.is_none() {
            self.prompt_version_id = resolution.prompt_version_id.clone();
        }
        if self.provider.is_none() {
            self.provider = resolution.provider.clone();
        }
        self.source
            .get_or_insert_with(|| resolution.source.as_str().to_string());
        if self.params.is_none() && !resolution.params.is_empty() {
            self.params = Some(resolution.params.clone());
        }
    }

    /// Checks the fields the server requires. Called by `log`, so an invalid record never queues.
    pub fn validate(&self) -> std::result::Result<(), crate::Error> {
        for (field, value) in [
            ("use_case", self.use_case.as_str()),
            ("model", self.model.as_str()),
            ("started_at", self.started_at.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(crate::Error::InvalidRecord(format!("{field} is required")));
            }
        }
        Ok(())
    }

    /// The record as a JSON object, with every absent field omitted.
    pub fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }
}

/// What the provider call produced, for the [`crate::UseCase::track`] wrapper.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Result {
    /// The answer text.
    pub content: Option<String>,
    /// The tool calls the model asked for, in the provider's own shape.
    pub tool_calls: Option<Value>,
    /// The provider's raw finish reason.
    pub finish_reason: Option<String>,
    /// An explicit stop kind, when the adapter already knows it.
    pub stop_kind: Option<StopKind>,
    /// Tokens and cost.
    pub usage: Usage,
    /// The model the provider actually served.
    pub model_used: Option<String>,
    /// The upstream provider a router picked.
    pub upstream_provider: Option<String>,
    /// Whether the call used the app's own provider key through a router.
    pub is_byok: Option<bool>,
}

impl Result {
    /// A provider result carrying only the answer text.
    pub fn text(content: impl Into<String>) -> Result {
        Result {
            content: Some(content.into()),
            ..Result::default()
        }
    }

    /// Builds a result from an OpenAI-compatible response object.
    ///
    /// The adapter accepts any `Serialize` value and reads the common chat-completion shape:
    /// `choices[0].message.content`, `choices[0].message.tool_calls`, `choices[0].finish_reason`,
    /// `usage.prompt_tokens`, `usage.completion_tokens`, and `model`.
    pub fn from_openai(answer: impl Serialize) -> Result {
        let value = serde_json::to_value(answer).unwrap_or(Value::Null);
        let first_choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let message = first_choice.and_then(|choice| choice.get("message"));
        let usage = value.get("usage").unwrap_or(&Value::Null);

        Result {
            content: message
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_calls: message
                .and_then(|message| message.get("tool_calls"))
                .filter(|tool_calls| !tool_calls.is_null())
                .cloned(),
            finish_reason: first_choice
                .and_then(|choice| choice.get("finish_reason"))
                .and_then(Value::as_str)
                .map(str::to_string),
            usage: Usage {
                input_tokens: usage.get("prompt_tokens").and_then(Value::as_i64),
                output_tokens: usage.get("completion_tokens").and_then(Value::as_i64),
                raw: (!usage.is_null()).then(|| usage.clone()),
                ..Usage::default()
            },
            model_used: value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Result::default()
        }
    }

    /// Builds a result from an Anthropic-compatible response object.
    ///
    /// The adapter reads text blocks from `content`, `stop_reason`, `usage.input_tokens`,
    /// `usage.output_tokens`, and `model`.
    pub fn from_anthropic(answer: impl Serialize) -> Result {
        let value = serde_json::to_value(answer).unwrap_or(Value::Null);
        let content = value
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            block.get("text").and_then(Value::as_str)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .filter(|text| !text.is_empty());
        let usage = value.get("usage").unwrap_or(&Value::Null);

        Result {
            content,
            finish_reason: value
                .get("stop_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            usage: Usage {
                input_tokens: usage.get("input_tokens").and_then(Value::as_i64),
                output_tokens: usage.get("output_tokens").and_then(Value::as_i64),
                raw: (!usage.is_null()).then(|| usage.clone()),
                ..Usage::default()
            },
            model_used: value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Result::default()
        }
    }

    /// Sets the provider's raw finish reason (the stop kind is derived from it).
    pub fn with_finish_reason(mut self, finish_reason: impl Into<String>) -> Result {
        self.finish_reason = Some(finish_reason.into());
        self
    }

    /// Sets the usage.
    pub fn with_usage(mut self, usage: Usage) -> Result {
        self.usage = usage;
        self
    }

    /// Sets the tool calls.
    pub fn with_tool_calls(mut self, tool_calls: Value) -> Result {
        self.tool_calls = Some(tool_calls);
        self
    }

    /// The stop kind: the explicit one when set, otherwise derived from `finish_reason`.
    pub fn effective_stop_kind(&self) -> Option<StopKind> {
        match (self.stop_kind, &self.finish_reason) {
            (Some(kind), _) => Some(StopKind::normalize(Some(kind.as_str()))),
            (None, Some(reason)) => Some(StopKind::normalize(Some(reason))),
            (None, None) => None,
        }
    }
}

/// What the app knows about a call that the resolution does not.
#[derive(Debug, Clone, Default)]
pub struct CallMeta {
    /// Use this id instead of a fresh one (pre-issued with [`crate::Client::log_id`]).
    pub id: Option<String>,
    /// The variables the prompt was rendered with.
    pub variables: Option<Vars>,
    /// The final messages that were sent, after the app attached its own history.
    pub input_messages: Option<Vec<Message>>,
    /// The final prompt string that was sent, for a text use case.
    pub input_text: Option<String>,
    /// A stable, non-identifying reference to the end user.
    pub end_user_ref: Option<String>,
    /// A correlation id of the app's choosing.
    pub trace_id: Option<String>,
    /// The position of this call inside a trace.
    pub sequence: Option<i64>,
    /// Free-form tags (≤ 2 KB).
    pub context: Option<Map<String, Value>>,
    /// Free-form app data (≤ 4 KB).
    pub metadata: Option<Map<String, Value>>,
    /// Parameters actually used, layered over the resolution's.
    pub params: Option<Map<String, Value>>,
}

impl CallMeta {
    /// An empty meta.
    pub fn new() -> CallMeta {
        CallMeta::default()
    }

    /// Records the variables the prompt was rendered with.
    pub fn variables(mut self, variables: impl Into<Vars>) -> CallMeta {
        self.variables = Some(variables.into());
        self
    }

    /// Records the messages that were sent.
    pub fn input_messages(mut self, messages: Vec<Message>) -> CallMeta {
        self.input_messages = Some(messages);
        self
    }

    /// Records the prompt string that was sent.
    pub fn input_text(mut self, text: impl Into<String>) -> CallMeta {
        self.input_text = Some(text.into());
        self
    }

    /// Sets the end user reference.
    pub fn end_user_ref(mut self, reference: impl Into<String>) -> CallMeta {
        self.end_user_ref = Some(reference.into());
        self
    }

    /// Sets the trace id.
    pub fn trace_id(mut self, trace_id: impl Into<String>) -> CallMeta {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Sets the sequence number inside the trace.
    pub fn sequence(mut self, sequence: i64) -> CallMeta {
        self.sequence = Some(sequence);
        self
    }

    /// Sets the free-form context tags.
    pub fn context(mut self, context: Value) -> CallMeta {
        self.context = context.as_object().cloned();
        self
    }

    /// Sets the free-form metadata.
    pub fn metadata(mut self, metadata: Value) -> CallMeta {
        self.metadata = metadata.as_object().cloned();
        self
    }
}

/// A successful provider call: the value your code wants, plus what to log.
#[derive(Debug, Clone)]
pub struct Completion<T> {
    /// Whatever your closure produced.
    pub value: T,
    /// What to record about the call.
    pub result: Result,
}

impl<T> Completion<T> {
    /// Pairs a value with its provider result.
    pub fn new(value: T, result: Result) -> Completion<T> {
        Completion { value, result }
    }
}

/// A failed provider call. `result` is kept when the provider answered but the app could not use
/// the answer, so a parse failure still counts as spend and as a quality signal.
#[derive(Debug)]
pub struct CallFailure {
    /// What went wrong.
    pub error: LogError,
    /// What the provider returned before the failure, when anything did. Boxed to keep the error
    /// small enough to return by value without cost.
    pub result: Option<Box<Result>>,
    /// The app's own error, carried through untouched.
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl CallFailure {
    /// A failure of `kind` with a message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> CallFailure {
        CallFailure {
            error: LogError::new(kind, message),
            result: None,
            source: None,
        }
    }

    /// An HTTP failure from the provider.
    pub fn http(status: u16, message: impl Into<String>) -> CallFailure {
        CallFailure {
            error: LogError::http(status, message),
            result: None,
            source: None,
        }
    }

    /// Keeps the usage and output the provider did return.
    pub fn with_result(mut self, result: Result) -> CallFailure {
        self.result = Some(Box::new(result));
        self
    }

    /// Carries the app's own error through.
    pub fn with_source(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> CallFailure {
        self.source = Some(Box::new(source));
        self
    }
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider call failed ({:?}): {}",
            self.error.kind,
            self.error.message.as_deref().unwrap_or("no message")
        )
    }
}

impl std::error::Error for CallFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// What the wrapper measured about one call.
pub(crate) struct Timing {
    pub id: String,
    pub started_at: String,
    pub latency_ms: i64,
}

/// Builds the record the wrapper logs.
pub(crate) fn build_record(
    resolution: &Resolution,
    meta: &CallMeta,
    timing: Timing,
    status: Status,
    outcome: Option<&Result>,
    error: Option<&LogError>,
) -> LogRecord {
    let mut record = LogRecord::new(
        resolution.use_case.clone(),
        resolution.model.clone().unwrap_or_default(),
        status,
    );
    record.id = timing.id;
    record.started_at = timing.started_at;
    record.latency_ms = Some(timing.latency_ms);
    record.sdk = Some(Sdk::default());
    record.apply_resolution(resolution);

    if let Some(params) = &meta.params {
        record.params = Some(crate::resolver::merge(&resolution.params, params));
    }

    let mut input = Map::new();
    if let Some(variables) = &meta.variables {
        input.insert("variables".to_string(), variables.to_value());
    }
    if let Some(messages) = &meta.input_messages {
        input.insert(
            "messages".to_string(),
            serde_json::to_value(messages).unwrap_or(Value::Null),
        );
    }
    if let Some(text) = &meta.input_text {
        input.insert("text".to_string(), Value::String(text.clone()));
    }
    if !input.is_empty() {
        record.input = Some(Value::Object(input));
    }

    record.context = Some(meta.context.clone().unwrap_or_default());
    let mut metadata = meta.metadata.clone().unwrap_or_default();
    record.trace_id = meta.trace_id.clone();
    record.sequence = meta.sequence;
    record.end_user_ref = meta.end_user_ref.clone();

    if let Some(outcome) = outcome {
        let mut output = Map::new();
        if let Some(content) = &outcome.content {
            output.insert("content".to_string(), Value::String(content.clone()));
        }
        if let Some(tool_calls) = &outcome.tool_calls {
            output.insert("tool_calls".to_string(), tool_calls.clone());
        }
        if !output.is_empty() {
            record.output = Some(Value::Object(output));
        }

        record.finish_reason = outcome.finish_reason.clone();
        record.stop_kind = outcome.effective_stop_kind();
        record.model_used = outcome.model_used.clone();
        record.upstream_provider = outcome.upstream_provider.clone();
        if let Some(is_byok) = outcome.is_byok {
            metadata.insert("is_byok".to_string(), Value::Bool(is_byok));
        }

        let mut usage = outcome.usage.clone();
        if usage.cost_source.is_none() {
            usage.cost_source = Some(CostSource::Unknown);
        }
        record.usage = Some(usage);
    } else {
        record.usage = Some(Usage {
            cost_source: Some(CostSource::Unknown),
            ..Usage::default()
        });
    }

    record.metadata = Some(metadata);
    record.error = error.cloned();
    record
}

/// Now, as an ISO 8601 UTC timestamp with milliseconds.
pub fn now_iso8601() -> String {
    iso8601(SystemTime::now())
}

/// Formats a `SystemTime` as an ISO 8601 UTC timestamp with milliseconds.
pub fn iso8601(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs() as i64;
    let millis = duration.subsec_millis();

    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the unix epoch to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_timestamps() {
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_millis(1_756_944_231_222)),
            "2025-09-04T00:03:51.222Z"
        );
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn requires_the_five_fields() {
        let mut record = LogRecord::new("greeting", "openai/gpt-4o-mini", Status::Ok);
        assert!(record.validate().is_ok());
        record.model = String::new();
        assert!(record.validate().is_err());
    }

    #[test]
    fn omits_absent_fields() {
        let record = LogRecord::new("greeting", "m", Status::Ok);
        let map = record.to_map();
        assert!(!map.contains_key("error"));
        assert!(!map.contains_key("output"));
        assert_eq!(map["status"], Value::String("ok".to_string()));
        assert_eq!(map["sdk"]["name"], Value::String(SDK_NAME.to_string()));
    }
}
