//! The cross-language resolution contract: every case in `conformance/prompt.json`.

mod support;

use prompton::{Client, Error, Kind, Source, UseCaseDocument, UseCaseOptions, Vars};
use serde_json::{json, Value};

#[test]
fn every_use_case_case() {
    let data = support::conformance("prompt.json");
    let documents = data["documents"].as_object().expect("documents");
    let cases = data["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 15,
        "expected the full suite, got {}",
        cases.len()
    );

    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let reference = case["document_ref"].as_str().expect("document_ref");
        UseCaseDocument::from_value(&documents[reference])
            .unwrap_or_else(|error| panic!("{name}: {reference} does not decode: {error}"));
        let client = Client::builder()
            .mode(prompton::Mode::Test)
            .without_disk_cache()
            .build()
            .unwrap();
        client
            .set_use_cases_as(&documents[reference], Source::Remote)
            .unwrap();

        let use_case = case["use_case"]
            .as_str()
            .or_else(|| case["prompt_key"].as_str())
            .expect("prompt_key");
        let options = UseCaseOptions {
            prompt: case["prompt"]
                .as_str()
                .or_else(|| case["template"].as_str())
                .map(str::to_string),
        };
        let expect = &case["expect"];

        let resolved = match client.use_case_with(use_case, &options) {
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
            .unwrap_or_else(|| panic!("{name}: expected error {expected}, use case succeeded"));
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
            expect["revision"].as_i64(),
            "{name}: revision"
        );
        assert_eq!(
            resolved.kind.as_str(),
            expect["kind"].as_str().unwrap(),
            "{name}: kind"
        );
        assert_eq!(
            resolved.prompt.as_deref(),
            expect["prompt"]
                .as_str()
                .or_else(|| expect["template"].as_str()),
            "{name}: prompt"
        );
        assert_eq!(
            &json!(resolved.prompt_names),
            if expect.get("prompt_names").is_some() {
                &expect["prompt_names"]
            } else {
                &expect["template_names"]
            },
            "{name}: prompt_names"
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
        ("unknown_use_case" | "unknown_prompt", Error::UnknownUseCase(key)) => {
            assert_eq!(Some(key.as_str()), expect["key"].as_str(), "{name}: key");
        }
        ("unresolved", Error::Unresolved(_)) => {}
        (
            "unknown_prompt" | "unknown_template",
            Error::UnknownPrompt {
                use_case,
                prompt,
                prompt_names,
            },
        ) => {
            assert_eq!(
                Some(use_case.as_str()),
                expect["key"].as_str(),
                "{name}: key"
            );
            assert_eq!(
                Some(prompt.as_str()),
                expect["prompt"]
                    .as_str()
                    .or_else(|| expect["template"].as_str()),
                "{name}"
            );
            assert_eq!(
                &json!(prompt_names),
                if expect.get("prompt_names").is_some() {
                    &expect["prompt_names"]
                } else {
                    &expect["template_names"]
                },
                "{name}"
            );
        }
        ("missing_variable", error) => {
            assert_eq!(
                error.missing_variable(),
                expect["variable"].as_str(),
                "{name}"
            );
        }
        (expected, error) => panic!("{name}: expected {expected}, got {error}"),
    }
}
