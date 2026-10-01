//! The use-case document: what `GET /api/v1/prompts?environment=…` returns, decoded.
//!
//! One request returns everything live in one environment — every deployment, the prompt versions
//! and models they pin, and the use case metadata — and the SDK reads it locally. The
//! SDK reads schema versions 4 through 7: stale disk caches, old bundles, missing versions and
//! unsupported future schema versions are refused.

use std::collections::BTreeMap;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::template::Engine;

/// The schema version this SDK reads.
pub const SCHEMA_VERSION: i64 = 7;

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

/// One entry of a use case's `input_schema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputVariable {
    /// The variable name used in the template.
    pub name: String,
    /// `string`, `number`, `boolean`, `list` or `map`.
    pub r#type: String,
    /// Whether a render must supply it.
    pub required: bool,
    /// Free-form documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    /// The declared input variables.
    pub input_schema: Vec<InputVariable>,
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
    /// Any document whose schema version is not exactly v4.
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
                if !(4..=SCHEMA_VERSION).contains(&version) {
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
                ))
            }
        };

        let use_cases_raw = map
            .get("use_cases")
            .or_else(|| map.get("prompts"))
            .and_then(Value::as_object)
            .ok_or_else(|| {
                DecodeError::Invalid("use_cases is required and must be an object".to_string())
            })?;

        let mut use_cases = BTreeMap::new();
        for (key, raw) in use_cases_raw {
            match raw.as_object() {
                Some(raw) => {
                    use_cases.insert(key.clone(), decode_use_case(key, raw, &mut warnings));
                }
                None => warnings.push(format!("use case {key} is not an object")),
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
    let mut input_schema = Vec::new();
    if let Some(entries) = raw.get("input_schema").and_then(Value::as_array) {
        for entry in entries {
            match entry.as_object() {
                Some(entry) => input_schema.push(InputVariable {
                    name: string_of(entry.get("name")).unwrap_or_default(),
                    r#type: string_of(entry.get("type")).unwrap_or_else(|| "string".to_string()),
                    required: entry
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    description: string_of(entry.get("description")),
                }),
                None => warnings.push(format!("input_schema entry of {key} is not an object")),
            }
        }
    }

    UseCaseSpec {
        id: string_of(raw.get("id")),
        key: key.to_string(),
        kind: Kind::from_wire(raw.get("kind").and_then(Value::as_str)),
        input_schema,
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

fn decode_deployment(key: &str, raw: &Map<String, Value>) -> Result<Deployment, DecodeError> {
    let mut prompt_pins = BTreeMap::new();
    if let Some(pins) = raw
        .get("prompt_pins")
        .or_else(|| raw.get("template_pins"))
        .and_then(Value::as_object)
    {
        for (name, version_id) in pins {
            if let Some(version_id) = version_id.as_str() {
                prompt_pins.insert(name.clone(), version_id.to_string());
            }
        }
    }

    Ok(Deployment {
        id: string_of(raw.get("id")),
        use_case_key: string_of(raw.get("use_case_key")).unwrap_or_else(|| key.to_string()),
        revision: revision_of(raw.get("revision"))?,
        model_id: string_of(raw.get("model_id")),
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
        id: string_of(raw.get("id")).unwrap_or_else(|| id.to_string()),
        prompt_id: string_of(raw.get("prompt_id")),
        number: raw.get("number").and_then(Value::as_i64),
        engine: Engine::from_wire(raw.get("engine").and_then(Value::as_str)),
        messages,
        tools: raw.get("tools").and_then(Value::as_object).cloned(),
        text_template: string_of(raw.get("text_template")),
    }
}

fn decode_model(id: &str, raw: &Map<String, Value>) -> Model {
    Model {
        id: string_of(raw.get("id")).unwrap_or_else(|| id.to_string()),
        provider: string_of(raw.get("provider")),
        model_id: string_of(raw.get("model_id")),
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
            "schema_version": 4,
            "project": "demo",
            "environment": "production",
            "use_cases": {
                "greeting": {
                    "id": "u1", "kind": "chat",
                    "input_schema": [{"name": "name", "type": "string", "required": true}],
                    "default_params": {"max_tokens": 512},
                    "payload_policy": {"mode": "hash", "sample_rate": 0.5, "max_bytes": 1024,
                                       "retention_days": 30, "encrypt": true}
                }
            },
            "deployments": {
                "greeting": {"id": "d1", "revision": "v2026.09.30-3", "model_id": "m1",
                             "params": {"temperature": 0.4}, "provider_options": {},
                             "prompt_pins": {"default": "v1", "ko": "v2"}}
            },
            "prompt_versions": {
                "v1": {"id": "v1", "number": 2, "engine": "liquid",
                       "messages": [{"role": "user", "content": "hi {{ name }}"}],
                       "text_template": null}
            },
            "models": {
                "m1": {"id": "m1", "provider": "openrouter", "model_id": "openai/gpt-4o-mini",
                       "provider_options": {"only": ["OpenAI"]}, "capabilities": ["tools"],
                       "status": "active"}
            }
        })
    }

    #[test]
    fn decodes_a_v4_document() {
        let (doc, warnings) = UseCaseDocument::from_value(&document()).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(doc.environment.as_deref(), Some("production"));
        assert_eq!(doc.use_cases["greeting"].kind, Kind::Chat);
        assert_eq!(doc.prompt_names("greeting"), vec!["default", "ko"]);
        assert_eq!(
            doc.use_cases["greeting"]
                .payload_policy
                .as_ref()
                .unwrap()
                .mode,
            PayloadMode::Hash
        );
        assert_eq!(
            doc.models["m1"].model_id.as_deref(),
            Some("openai/gpt-4o-mini")
        );
    }

    #[test]
    fn refuses_older_schema_versions() {
        let mut value = document();
        value["schema_version"] = json!(3);
        assert_eq!(
            UseCaseDocument::from_value(&value),
            Err(DecodeError::UnsupportedSchemaVersion(3))
        );
    }

    #[test]
    fn accepts_schema_versions_through_seven_and_refuses_future() {
        for version in [5, 6, 7] {
            let mut value = document();
            value["schema_version"] = json!(version);
            assert!(UseCaseDocument::from_value(&value).is_ok());
        }
        let mut value = document();
        value["schema_version"] = json!(8);
        assert_eq!(
            UseCaseDocument::from_value(&value),
            Err(DecodeError::UnsupportedSchemaVersion(8))
        );
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
        text["schema_version"] = json!("4");
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
