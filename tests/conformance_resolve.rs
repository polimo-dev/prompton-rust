//! The cross-language resolution contract: every case in `conformance/resolve.json`.

mod support;

use prompton::{Error, Kind, Rendered, ResolutionSource, ResolveOptions, SnapshotDocument, Vars};
use serde_json::{json, Value};

#[test]
fn every_resolve_case() {
    let data = support::conformance("resolve.json");
    let snapshots = data["snapshots"].as_object().expect("snapshots");
    let cases = data["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 15,
        "expected the full suite, got {}",
        cases.len()
    );

    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let reference = case["snapshot_ref"].as_str().expect("snapshot_ref");
        let document = SnapshotDocument::from_value(&snapshots[reference])
            .unwrap_or_else(|error| panic!("{name}: {reference} does not decode: {error}"))
            .0;

        let use_case = case["use_case"].as_str().expect("use_case");
        let options = ResolveOptions {
            prompt: case["prompt"].as_str().map(str::to_string),
        };
        let expect = &case["expect"];

        let resolution = match prompton::resolve_in(
            &document,
            use_case,
            &options,
            ResolutionSource::Remote,
            None,
        ) {
            Ok(resolution) => resolution,
            Err(error) => {
                assert_error(name, &error, expect);
                continue;
            }
        };

        if let Some(expected) = expect["error"].as_str() {
            // The only error left is a rendering one, which needs the variables.
            let variables = case.get("variables").cloned().unwrap_or(Value::Null);
            let error = resolution
                .render(Vars::from(variables))
                .err()
                .unwrap_or_else(|| {
                    panic!("{name}: expected error {expected}, resolution succeeded")
                });
            assert_error(name, &error, expect);
            continue;
        }

        assert_eq!(
            resolution.deployment_id.as_deref(),
            expect["deployment_id"].as_str(),
            "{name}: deployment_id"
        );
        assert_eq!(
            resolution.deployment_revision,
            expect["revision"].as_i64(),
            "{name}: revision"
        );
        assert_eq!(
            resolution.kind.as_str(),
            expect["kind"].as_str().unwrap(),
            "{name}: kind"
        );
        assert_eq!(
            resolution.prompt.as_deref(),
            expect["prompt"].as_str(),
            "{name}: prompt"
        );
        assert_eq!(
            json!(resolution.available_prompts),
            expect["prompts"],
            "{name}: prompts"
        );
        assert_eq!(
            resolution.model.as_deref(),
            expect["model"].as_str(),
            "{name}: model"
        );
        assert_eq!(
            resolution.model_id.as_deref(),
            expect["model_id"].as_str(),
            "{name}: model_id"
        );
        assert_eq!(
            resolution.provider.as_deref(),
            expect["provider"].as_str(),
            "{name}: provider"
        );
        assert_eq!(
            Value::Object(resolution.params.clone()),
            expect["effective_params"],
            "{name}: effective_params"
        );
        assert_eq!(
            Value::Object(resolution.provider_options.clone()),
            expect["effective_provider_options"],
            "{name}: effective_provider_options"
        );
        assert_eq!(
            json!(resolution.warnings),
            expect["warnings"],
            "{name}: warnings"
        );

        match &expect["prompt_version"] {
            Value::Null => assert_eq!(resolution.prompt_version_id, None, "{name}: prompt_version"),
            version => {
                assert_eq!(
                    resolution.prompt_version_id.as_deref(),
                    version["id"].as_str(),
                    "{name}: prompt_version.id"
                );
                assert_eq!(
                    resolution.prompt_version_number,
                    version["number"].as_i64(),
                    "{name}: prompt_version.number"
                );
            }
        }

        // Rendering: with variables the templates come back rendered, without them raw.
        let variables = case.get("variables").cloned();
        let rendered = match &variables {
            Some(variables) => resolution
                .render(Vars::from(variables.clone()))
                .unwrap_or_else(|error| panic!("{name}: render failed: {error}")),
            None => match (&resolution.messages, &resolution.text) {
                (Some(messages), _) => Rendered::Messages(messages.clone()),
                (None, Some(text)) => Rendered::Text(text.clone()),
                (None, None) => Rendered::None,
            },
        };

        match (&resolution.kind, rendered) {
            (Kind::Chat, Rendered::Messages(messages)) => {
                assert_eq!(
                    serde_json::to_value(&messages).unwrap(),
                    expect["messages"],
                    "{name}: messages"
                );
            }
            (Kind::Text, Rendered::Text(text)) => {
                assert_eq!(Value::String(text), expect["text"], "{name}: text");
            }
            (Kind::Embedding, Rendered::None) => {
                assert!(
                    expect.get("messages").is_none(),
                    "{name}: embedding messages"
                );
                assert!(expect.get("text").is_none(), "{name}: embedding text");
            }
            (kind, other) => {
                // The degraded snapshot resolves without a prompt version: no messages at all.
                assert!(
                    expect.get("messages").is_none() && expect.get("text").is_none(),
                    "{name}: unexpected {other:?} for kind {kind:?}"
                );
            }
        }
    }
}

fn assert_error(name: &str, error: &Error, expect: &Value) {
    let expected = expect["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: unexpected error {error}"));
    match (expected, error) {
        ("unknown_use_case", Error::UnknownUseCase(_)) => {}
        ("unresolved", Error::Unresolved(_)) => {}
        (
            "unknown_prompt",
            Error::UnknownPrompt {
                prompt,
                available_prompts,
                ..
            },
        ) => {
            assert_eq!(Some(prompt.as_str()), expect["prompt"].as_str(), "{name}");
            assert_eq!(
                json!(available_prompts),
                expect["available_prompts"],
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
