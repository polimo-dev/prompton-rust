//! Local resolution: snapshot + use case (+ prompt name) → what to send the provider.
//!
//! ```text
//! deployment       = snapshot.deployments[use_case]        # absent → unresolved (an error)
//! version          = snapshot.prompt_versions[deployment.prompt_pins[prompt or "default"]]
//! model            = snapshot.models[deployment.model_id]
//! params           = use_case.default_params <- deployment.params
//! provider_options = model.provider_options  <- deployment.provider_options
//! ```
//!
//! `<-` is a shallow merge where the right side wins and a `null` override is kept as `null`, not
//! deleted. The prompt name is the only selection axis and there is **no fallback to `default`**:
//! shipping English to a request that asked for `ko` is worse than an error.

use serde_json::{Map, Value};

use crate::error::Error;
use crate::snapshot::{InputVariable, Kind, Message, PayloadPolicy, UseCaseDocument};
use crate::template::{self, Engine, TemplateError, Vars};

/// The prompt name used when a call does not ask for one.
pub const DEFAULT_PROMPT: &str = "default";

/// Which tier the use-case document came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Fetched from PromptOn.
    Remote,
    /// Read from the local disk cache.
    Disk,
    /// Read from the use-case document bundled into the build.
    Bundle,
    /// Supplied by the app (test mode, or a hand-built use-case document).
    Manual,
}

impl Source {
    /// The wire value used in a monitoring log's `source`.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Remote => "remote",
            Source::Disk => "disk",
            Source::Bundle => "bundle",
            Source::Manual => "manual",
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Source> {
        match value {
            "remote" => Some(Source::Remote),
            "disk" => Some(Source::Disk),
            "bundle" => Some(Source::Bundle),
            "manual" => Some(Source::Manual),
            _ => None,
        }
    }
}

/// A rendered prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rendered {
    /// A chat use case: the messages to send.
    Messages(Vec<Message>),
    /// A text use case: the prompt string to send.
    Text(String),
    /// An embedding use case: there is no prompt.
    None,
}

/// Everything one call needs: which model to call, with which parameters, and which prompt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Resolution {
    /// The use case key that was resolved.
    pub use_case: String,
    /// Chat, text or embedding.
    pub kind: Kind,
    /// The live deployment revision's id.
    pub deployment_id: Option<String>,
    /// The live deployment revision number.
    pub deployment_revision: Option<i64>,
    /// The prompt name that was chosen; `None` for an embedding use case.
    pub prompt: Option<String>,
    /// Every prompt name the live revision pins.
    pub available_prompts: Vec<String>,
    /// The provider-side model string to send to the provider.
    pub model: Option<String>,
    /// The catalog id of that model.
    pub model_id: Option<String>,
    /// The provider name (`openrouter`, …).
    pub provider: Option<String>,
    /// `use_case.default_params <- deployment.params`, ready to send.
    pub params: Map<String, Value>,
    /// `model.provider_options <- deployment.provider_options`, ready to send.
    pub provider_options: Map<String, Value>,
    /// The pinned prompt version id.
    pub prompt_version_id: Option<String>,
    /// The pinned prompt version number.
    pub prompt_version_number: Option<i64>,
    /// The engine the pinned version was committed with.
    pub engine: Engine,
    /// The raw (unrendered) chat messages, for a chat use case.
    pub messages: Option<Vec<Message>>,
    /// The raw (unrendered) template, for a text use case.
    pub text: Option<String>,
    /// The use case's declared input variables.
    pub input_schema: Vec<InputVariable>,
    /// The use case's payload storage policy.
    pub payload_policy: Option<PayloadPolicy>,
    /// Which tier the use-case document came from.
    pub source: Source,
    /// The ETag of the use-case document this lookup came from.
    pub etag: Option<String>,
    /// Warnings such as `missing_model: <id>`; empty against a healthy use-case document.
    pub warnings: Vec<String>,
}

impl Resolution {
    /// Renders the pinned prompt with this call's variables.
    ///
    /// Chat use cases render every message, text use cases render the template, and an embedding
    /// use case has no prompt at all ([`Rendered::None`]).
    pub(crate) fn render(&self, vars: impl Into<Vars>) -> Result<Rendered, Error> {
        let vars = vars.into();
        match (&self.messages, &self.text) {
            (Some(messages), _) => Ok(Rendered::Messages(template::render_messages(
                messages,
                &vars,
                self.engine,
            )?)),
            (None, Some(text)) => Ok(Rendered::Text(template::render(text, &vars, self.engine)?)),
            (None, None) => Ok(Rendered::None),
        }
    }

    /// Renders a chat prompt, or fails when this use case is not a chat one.
    pub(crate) fn render_messages(&self, vars: impl Into<Vars>) -> Result<Vec<Message>, Error> {
        match self.render(vars)? {
            Rendered::Messages(messages) => Ok(messages),
            _ => Err(Error::Template(TemplateError::Render(format!(
                "use case {} has no chat messages to render",
                self.use_case
            )))),
        }
    }

    /// Renders a text prompt, or fails when this use case is not a text one.
    pub(crate) fn render_text(&self, vars: impl Into<Vars>) -> Result<String, Error> {
        match self.render(vars)? {
            Rendered::Text(text) => Ok(text),
            _ => Err(Error::Template(TemplateError::Render(format!(
                "use case {} has no text template to render",
                self.use_case
            )))),
        }
    }
}

/// Options for a single resolution.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolveOptions {
    /// The prompt name to pick; `None` means `default`. Ignored for an embedding use case.
    pub prompt: Option<String>,
}

/// Resolves `use_case` against `document`.
pub(crate) fn resolve(
    document: &UseCaseDocument,
    use_case_key: &str,
    options: &ResolveOptions,
    source: Source,
    etag: Option<&str>,
) -> Result<Resolution, Error> {
    let use_case = document
        .use_cases
        .get(use_case_key)
        .ok_or_else(|| Error::UnknownUseCase(use_case_key.to_string()))?;

    let deployment = document
        .deployments
        .get(use_case_key)
        .ok_or_else(|| Error::Unresolved(use_case_key.to_string()))?;

    let available_prompts: Vec<String> = deployment.prompt_pins.keys().cloned().collect();

    let (prompt_name, version_id) = if use_case.kind == Kind::Embedding {
        (None, None)
    } else {
        let name = options
            .prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_PROMPT.to_string());
        match deployment.prompt_pins.get(&name) {
            Some(version_id) => (Some(name), Some(version_id.clone())),
            None => {
                return Err(Error::UnknownPrompt {
                    use_case: use_case_key.to_string(),
                    prompt: name,
                    prompt_names: available_prompts,
                })
            }
        }
    };

    let mut warnings = Vec::new();
    let version = match &version_id {
        Some(id) => match document.prompt_versions.get(id) {
            Some(version) => Some(version),
            None => {
                warnings.push(format!("missing_prompt_version: {id}"));
                None
            }
        },
        None => None,
    };

    let model = match &deployment.model_id {
        Some(id) => match document.models.get(id) {
            Some(model) => Some(model),
            None => {
                warnings.push(format!("missing_model: {id}"));
                None
            }
        },
        None => None,
    };

    let messages = match (&use_case.kind, version) {
        (Kind::Chat, Some(version)) => version.messages.clone(),
        _ => None,
    };
    let text = match (&use_case.kind, version) {
        (Kind::Text, Some(version)) => version.text_template.clone(),
        _ => None,
    };

    let empty = Map::new();
    Ok(Resolution {
        use_case: use_case_key.to_string(),
        kind: use_case.kind.clone(),
        deployment_id: deployment.id.clone(),
        deployment_revision: deployment.revision,
        prompt: prompt_name,
        available_prompts,
        model: model.and_then(|model| model.model_id.clone()),
        model_id: model.map(|model| model.id.clone()),
        provider: model.and_then(|model| model.provider.clone()),
        params: merge(&use_case.default_params, &deployment.params),
        provider_options: merge(
            model.map(|model| &model.provider_options).unwrap_or(&empty),
            &deployment.provider_options,
        ),
        prompt_version_id: version.map(|version| version.id.clone()),
        prompt_version_number: version.and_then(|version| version.number),
        engine: version.map(|version| version.engine).unwrap_or_default(),
        messages,
        text,
        input_schema: use_case.input_schema.clone(),
        payload_policy: use_case.payload_policy.clone(),
        source,
        etag: etag.map(str::to_string),
        warnings,
    })
}

/// A shallow merge where the right side wins; a `null` on the right is kept, not deleted.
pub fn merge(left: &Map<String, Value>, right: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = left.clone();
    for (key, value) in right {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> UseCaseDocument {
        let value = json!({
            "schema_version": 4,
            "project": "demo",
            "environment": "production",
            "use_cases": {
                "greeting": {"id": "u1", "kind": "chat", "default_params": {"max_tokens": 512},
                             "input_schema": [], "payload_policy": null},
                "embed": {"id": "u2", "kind": "embedding", "default_params": {"dimensions": 256},
                          "input_schema": []},
                "draft": {"id": "u3", "kind": "chat", "default_params": {}, "input_schema": []}
            },
            "deployments": {
                "greeting": {"id": "d1", "revision": 3, "model_id": "m1",
                             "params": {"temperature": 0.2}, "provider_options": {"sort": null},
                             "prompt_pins": {"default": "v1", "ko": "v2"}},
                "embed": {"id": "d2", "revision": 2, "model_id": "m2", "params": {},
                          "provider_options": {}, "prompt_pins": {}}
            },
            "prompt_versions": {
                "v1": {"id": "v1", "number": 2, "engine": "liquid",
                       "messages": [{"role": "user", "content": "Say hello to {{ name }}."}]},
                "v2": {"id": "v2", "number": 1, "engine": "liquid",
                       "messages": [{"role": "user", "content": "{{ name }}님 안녕."}]}
            },
            "models": {
                "m1": {"id": "m1", "provider": "openrouter", "model_id": "openai/gpt-4o-mini",
                       "provider_options": {"only": ["OpenAI"]}},
                "m2": {"id": "m2", "provider": "openrouter", "model_id": "openai/text-embedding-3-small",
                       "provider_options": {}}
            }
        });
        UseCaseDocument::from_value(&value).unwrap().0
    }

    fn resolve_key(key: &str, options: ResolveOptions) -> Result<Resolution, Error> {
        resolve(&document(), key, &options, Source::Remote, Some("sha256-x"))
    }

    #[test]
    fn merges_params_and_provider_options() {
        let resolution = resolve_key("greeting", ResolveOptions::default()).unwrap();
        assert_eq!(resolution.params["max_tokens"], json!(512));
        assert_eq!(resolution.params["temperature"], json!(0.2));
        assert_eq!(resolution.provider_options["only"], json!(["OpenAI"]));
        assert_eq!(resolution.provider_options["sort"], json!(null));
        assert_eq!(resolution.available_prompts, vec!["default", "ko"]);
    }

    #[test]
    fn renders_the_pinned_prompt() {
        let resolution = resolve_key(
            "greeting",
            ResolveOptions {
                prompt: Some("ko".to_string()),
            },
        )
        .unwrap();
        let rendered = resolution.render(json!({"name": "아다"})).unwrap();
        match rendered {
            Rendered::Messages(messages) => {
                assert_eq!(messages[0].content, "아다님 안녕.".to_string());
            }
            other => panic!("expected messages, got {other:?}"),
        }
    }

    #[test]
    fn an_unpinned_prompt_name_is_an_error() {
        let error = resolve_key(
            "greeting",
            ResolveOptions {
                prompt: Some("fr".to_string()),
            },
        )
        .unwrap_err();
        match error {
            Error::UnknownPrompt {
                prompt,
                prompt_names,
                ..
            } => {
                assert_eq!(prompt, "fr");
                assert_eq!(prompt_names, vec!["default", "ko"]);
            }
            other => panic!("expected UnknownPrompt, got {other:?}"),
        }
    }

    #[test]
    fn embedding_ignores_the_prompt_name() {
        let resolution = resolve_key(
            "embed",
            ResolveOptions {
                prompt: Some("ko".to_string()),
            },
        )
        .unwrap();
        assert_eq!(resolution.prompt, None);
        assert_eq!(resolution.prompt_version_id, None);
        assert_eq!(resolution.render(()).unwrap(), Rendered::None);
    }

    #[test]
    fn a_use_case_without_a_deployment_is_unresolved() {
        assert!(matches!(
            resolve_key("draft", ResolveOptions::default()),
            Err(Error::Unresolved(_))
        ));
        assert!(matches!(
            resolve_key("nope", ResolveOptions::default()),
            Err(Error::UnknownUseCase(_))
        ));
    }
}
