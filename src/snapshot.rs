//! The runtime prompt configuration document, decoded.
//!
//! The SDK reads schema version 1 only. Runtime HTTP fetches use the singular
//! `GET /api/v1/prompts/:key` detail shape. Offline bundles may aggregate those details into keyed
//! maps, but the lightweight `GET /api/v1/prompts` discovery list is not a loadable configuration.

use std::collections::BTreeMap;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::template::Engine;

/// The schema version this SDK reads.
pub const SCHEMA_VERSION: i64 = 1;

/// The default payload cap when a use case carries no `payload_policy`.
pub const DEFAULT_MAX_BYTES: usize = 262_144;

/// What a use case calls: chat completion, plain text completion, or an embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A chat completion: the prompt version carries `messages`.
    Chat,
    /// A single-string completion: the prompt version carries `text_template`.
    Text,
    /// An embedding call: no prompt at all.
    Embedding,
    /// A kind this SDK version does not know (v1 only adds fields).
    Other(String),
}

impl Serialize for Kind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Kind, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Kind::from_wire(Some(&raw)))
    }
}

impl Kind {
    /// The wire value.
    pub fn as_str(&self) -> &str {
        match self {
            Kind::Chat => "chat",
            Kind::Text => "text",
            Kind::Embedding => "embedding",
            Kind::Other(other) => other,
        }
    }

    fn from_wire(value: Option<&str>) -> Kind {
        match value {
            None | Some("chat") => Kind::Chat,
            Some("text") => Kind::Text,
            Some("embedding") => Kind::Embedding,
            Some(other) => Kind::Other(other.to_string()),
        }
    }
}

/// One chat message of a prompt version, before or after rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `system`, `user`, `assistant`, …
    pub role: String,
    /// Provider-native message `type`. The retired `slot` marker is rejected when rendered.
    pub message_type: Option<String>,
    /// The message text — a Liquid template before rendering, the final text after.
    pub content: String,
    /// The exact JSON content when it was not a string, or when it was an explicit null.
    pub content_value: Option<Value>,
    /// Whether the source JSON carried a content key.
    pub content_present: bool,
    /// The optional OpenAI-style `name`.
    pub name: Option<String>,
    /// Native tool result linkage.
    pub tool_call_id: Option<String>,
    /// Native assistant tool calls.
    pub tool_calls: Vec<Value>,
    /// Provider-native extra message fields.
    pub extra: Map<String, Value>,
}

impl Message {
    /// A message with no `name`.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Message {
        Message {
            role: role.into(),
            message_type: None,
            content: content.into(),
            content_value: None,
            content_present: true,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            extra: Map::new(),
        }
    }

    /// Returns the exact JSON content if one was decoded; otherwise the public string content.
    pub fn content_json(&self) -> Value {
        self.content_value
            .clone()
            .unwrap_or_else(|| Value::String(self.content.clone()))
    }
}

impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut len = self.extra.len();
        len += usize::from(self.message_type.is_some());
        len += usize::from(!self.role.is_empty());
        len += usize::from(self.content_present || !self.content.is_empty());
        len += usize::from(self.name.is_some());
        len += usize::from(self.tool_call_id.is_some());
        len += usize::from(!self.tool_calls.is_empty());
        let mut map = serializer.serialize_map(Some(len))?;
        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }
        if let Some(message_type) = &self.message_type {
            map.serialize_entry("type", message_type)?;
        }
        if !self.role.is_empty() {
            map.serialize_entry("role", &self.role)?;
        }
        if self.content_present || !self.content.is_empty() {
            map.serialize_entry("content", &self.content_json())?;
        }
        if let Some(name) = &self.name {
            map.serialize_entry("name", name)?;
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", tool_call_id)?;
        }
        if !self.tool_calls.is_empty() {
            map.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut raw = Map::<String, Value>::deserialize(deserializer)?;
        let role = raw
            .remove("role")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let message_type = raw
            .remove("type")
            .and_then(|v| v.as_str().map(str::to_string));
        let name = raw
            .remove("name")
            .and_then(|v| v.as_str().map(str::to_string));
        let tool_call_id = raw
            .remove("tool_call_id")
            .and_then(|v| v.as_str().map(str::to_string));
        let tool_calls = raw
            .remove("tool_calls")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let content_present = raw.contains_key("content");
        let content_value = raw.remove("content");
        let content = content_value
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(Message {
            role,
            message_type,
            content,
            content_value,
            content_present,
            name,
            tool_call_id,
            tool_calls,
            extra: raw,
        })
    }
}

/// One entry of a use case's `variables`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableDefinition {
    /// The variable name used in the template.
    pub name: String,
    /// `string`, `number`, `boolean`, `list` or `map`.
    pub r#type: String,
    /// Whether a render must supply it.
    pub required: bool,
    /// Free-form documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Example value shown to people and generated snippets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
}

/// How much of a model call's `input`/`output` may be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PayloadMode {
    /// Store the payload, subject to sampling and truncation.
    Full,
    /// Store only the SHA-256 and byte size of each part.
    Hash,
    /// Store nothing; the narrow record still goes.
    None,
}

/// The use case's payload storage policy, as the document carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayloadPolicy {
    /// `full`, `hash` or `none`.
    pub mode: PayloadMode,
    /// The fraction of successful records whose payload is kept, 0.0–1.0.
    pub sample_rate: f64,
    /// The truncation budget every other limit is derived from.
    pub max_bytes: usize,
    /// How long the server keeps the payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<i64>,
    /// Whether the server encrypts the payload at rest.
    #[serde(default)]
    pub encrypt: bool,
}

impl Default for PayloadPolicy {
    fn default() -> PayloadPolicy {
        PayloadPolicy {
            mode: PayloadMode::Full,
            sample_rate: 1.0,
            max_bytes: DEFAULT_MAX_BYTES,
            retention_days: None,
            encrypt: false,
        }
    }
}

/// One use case: a single LLM call site in the app.
#[derive(Debug, Clone, PartialEq)]
pub struct UseCaseSpec {
    /// The use case id.
    pub id: Option<String>,
    /// The use case key (`diary_summary`, for example).
    pub key: String,
    /// Chat, text or embedding.
    pub kind: Kind,
    /// The declared prompt variables.
    pub variables: Vec<VariableDefinition>,
    /// Parameters every deployment of this use case starts from.
    pub default_params: Map<String, Value>,
    /// The payload storage policy, when the document carries one.
    pub payload_policy: Option<PayloadPolicy>,
}

/// One live deployment revision: a pin, not a router — one model plus one pinned prompt version
/// per prompt name.
#[derive(Debug, Clone, PartialEq)]
pub struct Deployment {
    /// The deployment id.
    pub id: Option<String>,
    /// The use case this deployment belongs to.
    pub use_case_key: String,
    /// The UTC-date deployment revision label, for example `v2026.09.30-1`.
    pub revision: Option<String>,
    /// The catalog id of the pinned model.
    pub model_id: Option<String>,
    /// Parameters layered over the use case's `default_params`.
    pub params: Map<String, Value>,
    /// Provider options layered over the model's `provider_options`.
    pub provider_options: Map<String, Value>,
    /// Prompt name to prompt version id. `{}` for an embedding use case.
    pub prompt_pins: BTreeMap<String, String>,
}

/// An immutable prompt version.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptVersion {
    /// The version id.
    pub id: String,
    /// The prompt this version belongs to.
    pub prompt_id: Option<String>,
    /// The version number, counting from 1.
    pub number: Option<i64>,
    /// Which template engine the version was committed with.
    pub engine: Engine,
    /// The chat messages, for a `chat` use case.
    pub messages: Option<Vec<Message>>,
    /// Provider-visible chat tools plus PromptOn authoring metadata.
    pub tools: Option<Map<String, Value>>,
    /// The single template string, for a `text` use case.
    pub text_template: Option<String>,
}

/// One model in the project's catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// The catalog id (a UUID).
    pub id: String,
    /// The provider name (`openrouter`, …).
    pub provider: Option<String>,
    /// The provider-side model string your app sends to the provider.
    pub model_id: Option<String>,
    /// The human-readable name.
    pub display_name: Option<String>,
    /// Free-form catalog metadata.
    pub metadata: Map<String, Value>,
    /// Provider options a deployment layers on top of.
    pub provider_options: Map<String, Value>,
    /// Advertised capabilities (`tools`, `streaming`, …).
    pub capabilities: Vec<String>,
    /// `active`, `deprecated`, …
    pub status: Option<String>,
}

/// A decoded use-case document.
#[derive(Debug, Clone, PartialEq)]
pub struct UseCaseDocument {
    /// The document's schema version.
    pub schema_version: i64,
    /// The project slug the document belongs to.
    pub project: Option<String>,
    /// The environment the document belongs to.
    pub environment: Option<String>,
    /// Use cases by key.
    pub use_cases: BTreeMap<String, UseCaseSpec>,
    /// Live deployments by use case key.
    pub deployments: BTreeMap<String, Deployment>,
    /// Prompt versions by id.
    pub prompt_versions: BTreeMap<String, PromptVersion>,
    /// Models by catalog id.
    pub models: BTreeMap<String, Model>,
}

/// Why a use-case document could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The bytes are not JSON.
    #[error("use-case document is not valid JSON: {0}")]
    InvalidJson(String),
    /// The JSON is not a use-case document.
    #[error("invalid use-case document: {0}")]
    Invalid(String),
    /// Any document whose schema version is not the current public contract.
    #[error("unsupported use-case document schema_version {0} (this SDK reads {SCHEMA_VERSION})")]
    UnsupportedSchemaVersion(i64),
}

impl UseCaseDocument {
    /// Decodes the raw response bytes.
    pub fn from_json(bytes: &[u8]) -> Result<(UseCaseDocument, Vec<String>), DecodeError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|err| DecodeError::InvalidJson(err.to_string()))?;
        UseCaseDocument::from_value(&value)
    }

    /// Decodes an already-parsed JSON value.
    pub fn from_value(value: &Value) -> Result<(UseCaseDocument, Vec<String>), DecodeError> {
        let map = value
            .as_object()
            .ok_or_else(|| DecodeError::Invalid("top level must be an object".to_string()))?;

        let mut warnings = Vec::new();
        let schema_version = match map.get("schema_version") {
            Some(Value::Number(version)) => {
                let version = version.as_i64().ok_or_else(|| {
                    DecodeError::Invalid("schema_version must be an integer".to_string())
                })?;
                if version != SCHEMA_VERSION {
                    return Err(DecodeError::UnsupportedSchemaVersion(version));
                }
                version
            }
            Some(_) => {
                return Err(DecodeError::Invalid(
                    "schema_version must be an integer".to_string(),
                ));
            }
            None => {
                return Err(DecodeError::Invalid(
                    "schema_version is required".to_string(),
                ));
            }
        };

        let normalized;
        let map = if ["prompt", "deployment", "prompt_version", "model"]
            .iter()
            .any(|key| map.contains_key(*key))
        {
            normalized = normalize_schema1_single_prompt(map)?;
            &normalized
        } else {
            if let Some(Value::Array(_)) = map.get("prompts") {
                return Err(DecodeError::Invalid(
                    "prompt list responses are discovery-only and cannot be loaded as configuration"
                        .to_string(),
                ));
            }
            map
        };
        validate_schema1_references(map)?;

        let use_cases_raw = map
            .get("prompts")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                DecodeError::Invalid("prompts is required and must be an object".to_string())
            })?;

        let mut use_cases = BTreeMap::new();
        for (key, raw) in use_cases_raw {
            match raw.as_object() {
                Some(raw) => {
                    use_cases.insert(key.clone(), decode_use_case(key, raw, &mut warnings));
                }
                None => warnings.push(format!("prompt {key} is not an object")),
            }
        }

        let mut deployments = BTreeMap::new();
        if let Some(raw_map) = map.get("deployments") {
            match raw_map.as_object() {
                Some(raw_map) => {
                    for (key, raw) in raw_map {
                        match raw.as_object() {
                            Some(raw) => {
                                deployments.insert(key.clone(), decode_deployment(key, raw)?);
                            }
                            None => warnings.push(format!("deployment {key} is not an object")),
                        }
                    }
                }
                None if !raw_map.is_null() => {
                    warnings.push("deployments is not an object".to_string())
                }
                None => {}
            }
        }

        let mut prompt_versions = BTreeMap::new();
        if let Some(raw_map) = map.get("prompt_versions").and_then(Value::as_object) {
            for (id, raw) in raw_map {
                match raw.as_object() {
                    Some(raw) => {
                        let version = decode_prompt_version(id, raw);
                        prompt_versions.insert(version.id.clone(), version);
                    }
                    None => warnings.push(format!("prompt version {id} is not an object")),
                }
            }
        }

        let mut models = BTreeMap::new();
        if let Some(raw_map) = map.get("models").and_then(Value::as_object) {
            for (id, raw) in raw_map {
                match raw.as_object() {
                    Some(raw) => {
                        let model = decode_model(id, raw);
                        models.insert(model.id.clone(), model);
                    }
                    None => warnings.push(format!("model {id} is not an object")),
                }
            }
        }

        Ok((
            UseCaseDocument {
                schema_version,
                project: string_of(map.get("project")),
                environment: string_of(map.get("environment")),
                use_cases,
                deployments,
                prompt_versions,
                models,
            },
            warnings,
        ))
    }

    /// The deployment pinned for a use case, when there is one.
    pub fn deployment(&self, use_case_key: &str) -> Option<&Deployment> {
        self.deployments.get(use_case_key)
    }

    /// The prompt names the live deployment pins, sorted.
    pub fn prompt_names(&self, use_case_key: &str) -> Vec<String> {
        match self.deployments.get(use_case_key) {
            Some(deployment) => deployment.prompt_pins.keys().cloned().collect(),
            None => Vec::new(),
        }
    }
}

fn decode_use_case(key: &str, raw: &Map<String, Value>, warnings: &mut Vec<String>) -> UseCaseSpec {
    let mut variables = Vec::new();
    if let Some(entries) = raw.get("variables").and_then(Value::as_array) {
        for entry in entries {
            match entry.as_object() {
                Some(entry) => variables.push(VariableDefinition {
                    name: string_of(entry.get("name")).unwrap_or_default(),
                    r#type: string_of(entry.get("type")).unwrap_or_else(|| "string".to_string()),
                    required: entry
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    description: string_of(entry.get("description")),
                    example: entry.get("example").cloned(),
                }),
                None => warnings.push(format!("variables entry of {key} is not an object")),
            }
        }
    }

    UseCaseSpec {
        id: string_of(raw.get("id")),
        key: key.to_string(),
        kind: Kind::from_wire(raw.get("kind").and_then(Value::as_str)),
        variables,
        default_params: object_of(raw.get("default_params")),
        payload_policy: raw
            .get("payload_policy")
            .and_then(Value::as_object)
            .map(decode_payload_policy),
    }
}

fn decode_payload_policy(raw: &Map<String, Value>) -> PayloadPolicy {
    PayloadPolicy {
        mode: match raw.get("mode").and_then(Value::as_str) {
            Some("hash") => PayloadMode::Hash,
            Some("none") => PayloadMode::None,
            _ => PayloadMode::Full,
        },
        sample_rate: raw
            .get("sample_rate")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .clamp(0.0, 1.0),
        max_bytes: raw
            .get("max_bytes")
            .and_then(Value::as_u64)
            .filter(|bytes| *bytes > 0)
            .map(|bytes| bytes as usize)
            .unwrap_or(DEFAULT_MAX_BYTES),
        retention_days: raw.get("retention_days").and_then(Value::as_i64),
        encrypt: raw.get("encrypt").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn validate_schema1_references(map: &Map<String, Value>) -> Result<(), DecodeError> {
    let deployments = required_collection_object(map, "deployments")?;
    let prompt_versions = required_collection_object(map, "prompt_versions")?;
    let models = required_collection_object(map, "models")?;

    for (key, raw) in prompt_versions {
        let raw = raw.as_object().ok_or_else(|| {
            DecodeError::Invalid(format!("prompt_versions.{key} must be an object"))
        })?;
        let prompt = required_string(raw.get("prompt"), &format!("prompt_versions.{key}.prompt"))?;
        let number =
            strict_positive_i64(raw.get("number"), &format!("prompt_versions.{key}.number"))?;
        let expected = format!("{prompt}@{number}");
        if key != &expected {
            return Err(DecodeError::Invalid(format!(
                "prompt_versions key {key} must match embedded prompt version {expected}"
            )));
        }
    }

    for (key, raw) in models {
        let raw = raw
            .as_object()
            .ok_or_else(|| DecodeError::Invalid(format!("models.{key} must be an object")))?;
        let provider = required_string(raw.get("provider"), &format!("models.{key}.provider"))?;
        let model = required_string(raw.get("model"), &format!("models.{key}.model"))?;
        let expected = format!("{provider}:{model}");
        if key != &expected {
            return Err(DecodeError::Invalid(format!(
                "models key {key} must match embedded model {expected}"
            )));
        }
    }

    for (key, raw) in deployments {
        let raw = raw
            .as_object()
            .ok_or_else(|| DecodeError::Invalid(format!("deployments.{key} must be an object")))?;
        let number = strict_positive_i64(
            raw.get("prompt_version"),
            &format!("deployments.{key}.prompt_version"),
        )?;
        let provider =
            required_string(raw.get("provider"), &format!("deployments.{key}.provider"))?;
        let model = required_string(raw.get("model"), &format!("deployments.{key}.model"))?;
        let prompt_version_key = format!("{key}@{number}");
        if !prompt_versions.contains_key(&prompt_version_key) {
            return Err(DecodeError::Invalid(format!(
                "deployment {key} references missing prompt version {prompt_version_key}"
            )));
        }
        let model_key = format!("{provider}:{model}");
        if !models.contains_key(&model_key) {
            return Err(DecodeError::Invalid(format!(
                "deployment {key} references missing model {model_key}"
            )));
        }
    }

    Ok(())
}

fn normalize_schema1_single_prompt(
    map: &Map<String, Value>,
) -> Result<Map<String, Value>, DecodeError> {
    for plural in ["prompts", "deployments", "prompt_versions", "models"] {
        if map.contains_key(plural) {
            return Err(DecodeError::Invalid(format!(
                "schema 1 single prompt document must not include {plural}"
            )));
        }
    }

    let prompt = required_object(map, "prompt")?;
    let deployment = required_object(map, "deployment")?;
    let prompt_version = required_object(map, "prompt_version")?;
    let model = required_object(map, "model")?;

    let key = required_string(prompt.get("key"), "prompt.key")?;
    let version_prompt = required_string(prompt_version.get("prompt"), "prompt_version.prompt")?;
    if version_prompt != key {
        return Err(DecodeError::Invalid(format!(
            "prompt_version.prompt {version_prompt} must match prompt.key {key}"
        )));
    }

    let version_number =
        strict_positive_i64(prompt_version.get("number"), "prompt_version.number")?;
    let deployment_version = strict_positive_i64(
        deployment.get("prompt_version"),
        "deployment.prompt_version",
    )?;
    if deployment_version != version_number {
        return Err(DecodeError::Invalid(format!(
            "deployment.prompt_version {deployment_version} must match prompt_version.number {version_number}"
        )));
    }

    let deployment_provider = required_string(deployment.get("provider"), "deployment.provider")?;
    let deployment_model = required_string(deployment.get("model"), "deployment.model")?;
    let model_provider = required_string(model.get("provider"), "model.provider")?;
    let model_name = required_string(model.get("model"), "model.model")?;
    if deployment_provider != model_provider || deployment_model != model_name {
        return Err(DecodeError::Invalid(format!(
            "deployment provider/model {deployment_provider}:{deployment_model} must match model {model_provider}:{model_name}"
        )));
    }

    let mut normalized = map.clone();
    normalized.remove("prompt");
    normalized.remove("deployment");
    normalized.remove("prompt_version");
    normalized.remove("model");

    normalized.insert(
        "prompts".to_string(),
        Value::Object(singleton_object(key, Value::Object(prompt.clone()))),
    );
    normalized.insert(
        "deployments".to_string(),
        Value::Object(singleton_object(key, Value::Object(deployment.clone()))),
    );
    normalized.insert(
        "prompt_versions".to_string(),
        Value::Object(singleton_object(
            &format!("{key}@{version_number}"),
            Value::Object(prompt_version.clone()),
        )),
    );
    normalized.insert(
        "models".to_string(),
        Value::Object(singleton_object(
            &format!("{model_provider}:{model_name}"),
            Value::Object(model.clone()),
        )),
    );
    Ok(normalized)
}

fn required_object<'a>(
    map: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, DecodeError> {
    match map.get(key) {
        Some(Value::Object(raw)) => Ok(raw),
        Some(_) => Err(DecodeError::Invalid(format!("{key} must be an object"))),
        None => Err(DecodeError::Invalid(format!("{key} is required"))),
    }
}

fn singleton_object(key: &str, value: Value) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(key.to_string(), value);
    map
}

fn required_collection_object<'a>(
    map: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, DecodeError> {
    match map.get(key) {
        Some(Value::Object(raw)) => Ok(raw),
        Some(_) => Err(DecodeError::Invalid(format!("{key} must be an object"))),
        None => Err(DecodeError::Invalid(format!("{key} is required"))),
    }
}

fn required_string<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str, DecodeError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        Some(Value::String(_)) => Err(DecodeError::Invalid(format!(
            "{field} must be a non-empty string"
        ))),
        Some(_) => Err(DecodeError::Invalid(format!("{field} must be a string"))),
        None => Err(DecodeError::Invalid(format!("{field} is required"))),
    }
}

fn strict_positive_i64(value: Option<&Value>, field: &str) -> Result<i64, DecodeError> {
    match value {
        Some(Value::Number(number)) => match number.as_i64() {
            Some(number) if number > 0 => Ok(number),
            _ => Err(DecodeError::Invalid(format!(
                "{field} must be a positive integer"
            ))),
        },
        Some(_) => Err(DecodeError::Invalid(format!(
            "{field} must be a positive integer"
        ))),
        None => Err(DecodeError::Invalid(format!("{field} is required"))),
    }
}

fn decode_deployment(key: &str, raw: &Map<String, Value>) -> Result<Deployment, DecodeError> {
    let number = strict_positive_i64(
        raw.get("prompt_version"),
        &format!("deployment {key} prompt_version"),
    )?;
    let mut prompt_pins = BTreeMap::new();
    prompt_pins.insert("default".to_string(), format!("{key}@{number}"));

    Ok(Deployment {
        id: None,
        use_case_key: key.to_string(),
        revision: revision_of(raw.get("revision"))?,
        model_id: readable_model_key(raw),
        params: object_of(raw.get("params")),
        provider_options: object_of(raw.get("provider_options")),
        prompt_pins,
    })
}

fn decode_prompt_version(id: &str, raw: &Map<String, Value>) -> PromptVersion {
    let messages = raw
        .get("messages")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_object())
                .filter_map(|entry| serde_json::from_value(Value::Object(entry.clone())).ok())
                .collect::<Vec<_>>()
        });

    PromptVersion {
        id: readable_prompt_version_key(id, raw),
        prompt_id: string_of(raw.get("prompt")),
        number: raw.get("number").and_then(Value::as_i64),
        engine: Engine::from_wire(raw.get("engine").and_then(Value::as_str)),
        messages,
        tools: raw.get("tools").and_then(Value::as_object).cloned(),
        text_template: string_of(raw.get("text_template")),
    }
}

fn decode_model(_id: &str, raw: &Map<String, Value>) -> Model {
    Model {
        id: readable_model_key(raw).unwrap_or_default(),
        provider: string_of(raw.get("provider")),
        model_id: string_of(raw.get("model")),
        display_name: string_of(raw.get("display_name")),
        metadata: object_of(raw.get("metadata")),
        provider_options: object_of(raw.get("provider_options")),
        capabilities: raw
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        status: string_of(raw.get("status")),
    }
}

fn readable_prompt_version_key(_id: &str, raw: &Map<String, Value>) -> String {
    match (
        string_of(raw.get("prompt")),
        raw.get("number")
            .and_then(Value::as_i64)
            .filter(|number| *number > 0),
    ) {
        (Some(prompt), Some(number)) => format!("{prompt}@{number}"),
        _ => String::new(),
    }
}

fn readable_model_key(raw: &Map<String, Value>) -> Option<String> {
    match (string_of(raw.get("provider")), string_of(raw.get("model"))) {
        (Some(provider), Some(model)) => Some(format!("{provider}:{model}")),
        _ => None,
    }
}

fn string_of(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(string)) => Some(string.clone()),
        _ => None,
    }
}

fn revision_of(value: Option<&Value>) -> Result<Option<String>, DecodeError> {
    match value {
        Some(Value::String(string)) => Ok(Some(string.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(DecodeError::Invalid(
            "deployment revision must be a string".to_string(),
        )),
    }
}

fn object_of(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "schema_version": 1,
            "project": "demo",
            "environment": "production",
            "prompts": {
                "greeting": {
                    "key": "greeting",
                    "kind": "chat",
                    "variables": [{"name": "name", "type": "string", "required": true,
                                   "description": "Display name", "example": "Ada"}],
                    "default_params": {"max_tokens": 512},
                    "payload_policy": {"mode": "hash", "sample_rate": 0.5, "max_bytes": 1024,
                                       "retention_days": 30, "encrypt": true}
                }
            },
            "deployments": {
                "greeting": {"revision": "v2026.09.30-3", "provider": "openrouter",
                             "model": "openai/gpt-4o-mini", "prompt_version": 2,
                             "params": {"temperature": 0.4}, "provider_options": {}}
            },
            "prompt_versions": {
                "greeting@2": {"prompt": "greeting", "number": 2, "engine": "liquid",
                       "messages": [{"role": "user", "content": "hi {{ name }}"}],
                       "text_template": null}
            },
            "models": {
                "openrouter:openai/gpt-4o-mini": {"provider": "openrouter", "model": "openai/gpt-4o-mini",
                       "provider_options": {"only": ["OpenAI"]}, "capabilities": ["tools"],
                       "status": "active"}
            }
        })
    }

    #[test]
    fn decodes_schema_one_aggregate_document() {
        let (doc, warnings) = UseCaseDocument::from_value(&document()).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(doc.environment.as_deref(), Some("production"));
        assert_eq!(doc.use_cases["greeting"].kind, Kind::Chat);
        assert_eq!(doc.prompt_names("greeting"), vec!["default"]);
        assert_eq!(
            doc.use_cases["greeting"].variables[0]
                .description
                .as_deref(),
            Some("Display name")
        );
        assert_eq!(
            doc.use_cases["greeting"].variables[0].example,
            Some(json!("Ada"))
        );
        assert_eq!(
            doc.use_cases["greeting"]
                .payload_policy
                .as_ref()
                .unwrap()
                .mode,
            PayloadMode::Hash
        );
        assert_eq!(
            doc.models["openrouter:openai/gpt-4o-mini"]
                .model_id
                .as_deref(),
            Some("openai/gpt-4o-mini")
        );
    }

    #[test]
    fn refuses_non_current_schema_versions() {
        let mut value = document();
        value["schema_version"] = json!(3);
        assert_eq!(
            UseCaseDocument::from_value(&value),
            Err(DecodeError::UnsupportedSchemaVersion(3))
        );
    }

    #[test]
    fn accepts_schema_one_shapes_and_refuses_future_or_list() {
        assert!(UseCaseDocument::from_value(&document()).is_ok());
        assert!(UseCaseDocument::from_value(&readable_document()).is_ok());
        assert!(UseCaseDocument::from_value(&readable_single_document("greeting")).is_ok());

        let mut value = document();
        value["schema_version"] = json!(10);
        assert_eq!(
            UseCaseDocument::from_value(&value),
            Err(DecodeError::UnsupportedSchemaVersion(10))
        );

        let list = json!({
            "schema_version": 1,
            "project": "demo",
            "environment": "production",
            "prompts": [{"key": "greeting", "kind": "chat"}]
        });
        assert!(matches!(
            UseCaseDocument::from_value(&list),
            Err(DecodeError::Invalid(message)) if message.contains("discovery-only")
        ));
    }

    fn readable_document() -> Value {
        json!({
            "schema_version": 1,
            "project": "demo",
            "environment": "production",
            "prompts": {
                "greeting": {"kind": "chat", "default_params": {"max_tokens": 64}}
            },
            "deployments": {
                "greeting": {
                    "revision": "v2026.09.30-3",
                    "provider": "openrouter",
                    "model": "openai/gpt-4o-mini",
                    "prompt_version": 2,
                    "params": {"temperature": 0.2}
                }
            },
            "prompt_versions": {
                "greeting@2": {
                    "prompt": "greeting",
                    "number": 2,
                    "engine": "liquid",
                    "messages": [{"role": "user", "content": "hi"}]
                }
            },
            "models": {
                "openrouter:openai/gpt-4o-mini": {
                    "provider": "openrouter",
                    "model": "openai/gpt-4o-mini",
                    "status": "active"
                }
            }
        })
    }

    #[test]
    fn decodes_schema_one_readable_resource_keys() {
        let value = readable_document();

        let (doc, warnings) = UseCaseDocument::from_value(&value).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(doc.schema_version, 1);
        assert_eq!(doc.deployments["greeting"].id, None);
        assert_eq!(
            doc.deployments["greeting"].model_id.as_deref(),
            Some("openrouter:openai/gpt-4o-mini")
        );
        assert_eq!(
            doc.deployments["greeting"].prompt_pins["default"],
            "greeting@2"
        );
        assert_eq!(
            doc.prompt_versions["greeting@2"].prompt_id.as_deref(),
            Some("greeting")
        );
        assert_eq!(
            doc.models["openrouter:openai/gpt-4o-mini"]
                .model_id
                .as_deref(),
            Some("openai/gpt-4o-mini")
        );
    }

    fn readable_single_document(key: &str) -> Value {
        json!({
            "schema_version": 1,
            "project": "demo",
            "environment": "production",
            "prompt": {
                "key": key,
                "kind": "chat",
                "default_params": {"max_tokens": 64}
            },
            "deployment": {
                "revision": "v2026.09.30-3",
                "provider": "openrouter",
                "model": "openai/gpt-4o-mini",
                "prompt_version": 2,
                "params": {"temperature": 0.2}
            },
            "prompt_version": {
                "prompt": key,
                "number": 2,
                "engine": "liquid",
                "messages": [{"role": "user", "content": "hi"}]
            },
            "model": {
                "provider": "openrouter",
                "model": "openai/gpt-4o-mini",
                "status": "active"
            }
        })
    }

    #[test]
    fn decodes_schema_one_single_prompt_document() {
        let value = readable_single_document("greeting");

        let (doc, warnings) = UseCaseDocument::from_value(&value).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(doc.schema_version, 1);
        assert_eq!(doc.use_cases["greeting"].key, "greeting");
        assert_eq!(
            doc.deployments["greeting"].model_id.as_deref(),
            Some("openrouter:openai/gpt-4o-mini")
        );
        assert_eq!(
            doc.deployments["greeting"].prompt_pins["default"],
            "greeting@2"
        );
        assert_eq!(
            doc.prompt_versions["greeting@2"].prompt_id.as_deref(),
            Some("greeting")
        );
    }

    #[test]
    fn decodes_schema_one_decision_kind_as_an_unknown_kind() {
        let mut value = readable_single_document("route");
        value["prompt"]["kind"] = json!("decision");
        value["prompt_version"]["kind"] = json!("decision");

        let (doc, warnings) = UseCaseDocument::from_value(&value).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            doc.use_cases["route"].kind,
            Kind::Other("decision".to_string())
        );
    }

    #[test]
    fn schema_one_rejects_mixed_or_mismatched_single_prompt_documents() {
        let mut mixed = readable_single_document("greeting");
        mixed["prompts"] = json!({});
        assert!(matches!(
            UseCaseDocument::from_value(&mixed),
            Err(DecodeError::Invalid(message)) if message.contains("must not include prompts")
        ));

        let mut bad_prompt = readable_single_document("greeting");
        bad_prompt["prompt_version"]["prompt"] = json!("other");
        assert!(matches!(
            UseCaseDocument::from_value(&bad_prompt),
            Err(DecodeError::Invalid(message)) if message.contains("prompt_version.prompt other")
        ));

        let mut bad_version = readable_single_document("greeting");
        bad_version["deployment"]["prompt_version"] = json!(3);
        assert!(matches!(
            UseCaseDocument::from_value(&bad_version),
            Err(DecodeError::Invalid(message)) if message.contains("deployment.prompt_version 3")
        ));

        let mut bad_model = readable_single_document("greeting");
        bad_model["deployment"]["model"] = json!("openai/missing");
        assert!(matches!(
            UseCaseDocument::from_value(&bad_model),
            Err(DecodeError::Invalid(message)) if message.contains("must match model")
        ));
    }

    #[test]
    fn schema_one_requires_all_aggregate_collections_and_rejects_partial_mixed_shape() {
        for key in ["deployments", "prompt_versions", "models"] {
            let mut missing = readable_document();
            missing.as_object_mut().unwrap().remove(key);
            assert!(matches!(
                UseCaseDocument::from_value(&missing),
                Err(DecodeError::Invalid(message)) if message.contains(&format!("{key} is required"))
            ));

            let mut array = readable_document();
            array[key] = json!([]);
            assert!(matches!(
                UseCaseDocument::from_value(&array),
                Err(DecodeError::Invalid(message)) if message.contains(&format!("{key} must be an object"))
            ));
        }

        let empty = json!({
            "schema_version": 1,
            "project": "demo",
            "environment": "production",
            "prompts": {},
            "deployments": {},
            "prompt_versions": {},
            "models": {}
        });
        let (doc, warnings) = UseCaseDocument::from_value(&empty).unwrap();
        assert!(warnings.is_empty());
        assert!(doc.use_cases.is_empty());

        let mut partial_mixed = readable_document();
        partial_mixed["deployment"] = json!({});
        assert!(matches!(
            UseCaseDocument::from_value(&partial_mixed),
            Err(DecodeError::Invalid(message)) if message.contains("must not include prompts")
        ));
    }

    #[test]
    fn schema_one_requires_strict_readable_refs() {
        let mut string_pin = readable_document();
        string_pin["deployments"]["greeting"]["prompt_version"] = json!("2");
        assert!(matches!(
            UseCaseDocument::from_value(&string_pin),
            Err(DecodeError::Invalid(message)) if message.contains("deployments.greeting.prompt_version")
        ));

        let mut fractional_pin = readable_document();
        fractional_pin["deployments"]["greeting"]["prompt_version"] = json!(2.5);
        assert!(matches!(
            UseCaseDocument::from_value(&fractional_pin),
            Err(DecodeError::Invalid(message)) if message.contains("positive integer")
        ));

        let mut bad_version_key = readable_document();
        let version = bad_version_key["prompt_versions"]
            .as_object_mut()
            .unwrap()
            .remove("greeting@2")
            .unwrap();
        bad_version_key["prompt_versions"]
            .as_object_mut()
            .unwrap()
            .insert("other@2".to_string(), version);
        assert!(matches!(
            UseCaseDocument::from_value(&bad_version_key),
            Err(DecodeError::Invalid(message)) if message.contains("prompt_versions key other@2")
        ));

        let mut missing_model = readable_document();
        missing_model["deployments"]["greeting"]["model"] = json!("openai/missing");
        assert!(matches!(
            UseCaseDocument::from_value(&missing_model),
            Err(DecodeError::Invalid(message)) if message.contains("missing model openrouter:openai/missing")
        ));

        let mut colon_model = readable_document();
        colon_model["deployments"]["greeting"]["model"] = json!("vendor:family/model");
        let model = json!({"provider": "openrouter", "model": "vendor:family/model"});
        colon_model["models"].as_object_mut().unwrap().clear();
        colon_model["models"]
            .as_object_mut()
            .unwrap()
            .insert("openrouter:vendor:family/model".to_string(), model);
        assert!(UseCaseDocument::from_value(&colon_model).is_ok());
    }

    #[test]
    fn refuses_missing_or_non_integer_schema_versions() {
        let mut missing = document();
        missing.as_object_mut().unwrap().remove("schema_version");
        assert_eq!(
            UseCaseDocument::from_value(&missing),
            Err(DecodeError::Invalid(
                "schema_version is required".to_string()
            ))
        );

        let mut text = document();
        text["schema_version"] = json!("1");
        assert_eq!(
            UseCaseDocument::from_value(&text),
            Err(DecodeError::Invalid(
                "schema_version must be an integer".to_string()
            ))
        );
    }

    #[test]
    fn refuses_numeric_deployment_revisions() {
        let mut value = document();
        value["deployments"]["greeting"]["revision"] = json!(3);
        assert_eq!(
            UseCaseDocument::from_value(&value),
            Err(DecodeError::Invalid(
                "deployment revision must be a string".to_string()
            ))
        );
    }
}
