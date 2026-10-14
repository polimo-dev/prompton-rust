//! The cross-language resolution contract: every case in `conformance/prompt.json`.

mod support;

use prompton::{Client, Error, Kind, PromptDocument, PromptOptions, Source, Vars};
use serde_json::{json, Value};

#[test]
fn every_prompt_case() {
    let data = support::conformance("prompt.json");
    let documents = data["documents"].as_object().expect("documents");
    let cases = data["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 13,
        "expected the full suite, got {}",
        cases.len()
    );

    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let reference = case["document_ref"].as_str().expect("document_ref");
        PromptDocument::from_value(&documents[reference])
            .unwrap_or_else(|error| panic!("{name}: {reference} does not decode: {error}"));
        let client = Client::builder()
            .mode(prompton::Mode::Test)
            .without_disk_cache()
            .build()
            .unwrap();
        client
            .set_prompts_as(&documents[reference], Source::Remote)
            .unwrap();

        let prompt = case["prompt"]
            .as_str()
            .or_else(|| case["prompt_key"].as_str())
            .expect("prompt_key");
        let options = PromptOptions {
            template: case["prompt"]
                .as_str()
                .or_else(|| case["template"].as_str())
                .map(str::to_string),
        };
        let expect = &case["expect"];

        let resolved = match client.prompt_with(prompt, &options) {
            Ok(resolved) => resolved,
            Err(error) => {
                assert_error(name, &error, expect);
                continue;
            }
        };

        if let Some(expected) = expect["error"].as_str() {
            // The only error left is a rendering one, which needs the variables.
            let variables = case.get("variables").cloned().unwrap_or(Value::Null);
            let error = match resolved.kind {
                Kind::Chat => resolved.messages(Vars::from(variables)).map(|_| ()),
                Kind::Text => resolved.text(Vars::from(variables)).map(|_| ()),
                Kind::Embedding | Kind::Other(_) => Ok(()),
            }
            .err()
            .unwrap_or_else(|| panic!("{name}: expected error {expected}, prompt succeeded"));
            assert_error(name, &error, expect);
            continue;
        }

        assert_eq!(resolved.key, expect["key"].as_str().unwrap(), "{name}: key");
        assert_eq!(
            resolved.source.as_str(),
            expect["source"].as_str().unwrap(),
            "{name}: source"
        );
        assert_eq!(
            resolved.deployment_id.as_deref(),
            expect["deployment_id"].as_str(),
            "{name}: deployment_id"
        );
        assert_eq!(
            resolved.deployment_revision,
            expect["revision"].as_str().map(str::to_string),
            "{name}: revision"
        );
        assert_eq!(
            resolved.kind.as_str(),
            expect["kind"].as_str().unwrap(),
            "{name}: kind"
        );
        assert_eq!(
            resolved.template.as_deref(),
            expect["prompt"]
                .as_str()
                .or_else(|| expect["template"].as_str()),
            "{name}: prompt"
        );
        assert_eq!(
            &json!(resolved.template_names),
            &expect["template_names"],
            "{name}: template_names"
        );
        assert_eq!(
            resolved.model.as_deref(),
            expect["model"].as_str(),
            "{name}: model"
        );
        assert_eq!(
            resolved.model_id.as_deref(),
            expect["model_id"].as_str(),
            "{name}: model_id"
        );
        assert_eq!(
            resolved.provider.as_deref(),
            expect["provider"].as_str(),
            "{name}: provider"
        );
        assert_eq!(
            Value::Object(resolved.params.clone()),
            expect["params"],
            "{name}: params"
        );
        assert_eq!(
            Value::Object(resolved.provider_options.clone()),
            expect["provider_options"],
            "{name}: provider_options"
        );
        assert_eq!(
            json!(resolved.warnings),
            expect["warnings"],
            "{name}: warnings"
        );

        match &expect["prompt_version"] {
            Value::Null => assert_eq!(resolved.prompt_version_id, None, "{name}: prompt_version"),
            version => {
                assert_eq!(
                    resolved.prompt_version_id.as_deref(),
                    version["id"].as_str(),
                    "{name}: prompt_version.id"
                );
                assert_eq!(
                    resolved.prompt_version_number,
                    version["number"].as_i64(),
                    "{name}: prompt_version.number"
                );
            }
        }

        // Rendering: the public API exposes kind-specific `messages(vars)` and `text(vars)` paths.
        if let Some(variables) = case.get("variables").cloned() {
            match &resolved.kind {
                Kind::Chat => {
                    let messages = resolved
                        .messages(Vars::from(variables))
                        .unwrap_or_else(|error| panic!("{name}: messages failed: {error}"));
                    assert_eq!(
                        serde_json::to_value(&messages).unwrap(),
                        expect["messages"],
                        "{name}: messages"
                    );
                }
                Kind::Text => {
                    let text = resolved
                        .text(Vars::from(variables))
                        .unwrap_or_else(|error| panic!("{name}: text failed: {error}"));
                    assert_eq!(Value::String(text), expect["text"], "{name}: text");
                }
                Kind::Embedding | Kind::Other(_) => {
                    assert!(
                        expect.get("messages").is_none(),
                        "{name}: embedding messages"
                    );
                    assert!(expect.get("text").is_none(), "{name}: embedding text");
                }
            };
        }
    }
}

fn assert_error(name: &str, error: &Error, expect: &Value) {
    let expected = expect["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: unexpected error {error}"));
    match (expected, error) {
        ("unknown_prompt", Error::UnknownPrompt(key)) => {
            assert_eq!(Some(key.as_str()), expect["key"].as_str(), "{name}: key");
        }
        ("unresolved", Error::Unresolved(_)) => {}
        (
            "unknown_template",
            Error::UnknownTemplate {
                prompt,
                template,
                template_names,
            },
        ) => {
            assert_eq!(Some(prompt.as_str()), expect["key"].as_str(), "{name}: key");
            assert_eq!(
                Some(template.as_str()),
                expect["prompt"]
                    .as_str()
                    .or_else(|| expect["template"].as_str()),
                "{name}"
            );
            assert_eq!(&json!(template_names), &expect["template_names"], "{name}");
        }
        ("missing_variable", error) => {
            assert_eq!(
                error.missing_variable(),
                expect["variable"].as_str(),
                "{name}"
            );
        }
        ("render_error", Error::Template(prompton::TemplateError::Render(message))) => {
            if let Some(expected) = expect["message"].as_str() {
                assert_eq!(message, expected, "{name}");
            }
        }
        (expected, error) => panic!("{name}: expected {expected}, got {error}"),
    }
}
