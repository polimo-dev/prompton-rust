//! The cross-language template contract: every case in `conformance/template.json`.

mod support;

use prompton::template::{lint, render, variables};
use prompton::{Engine, LintReason, TemplateError, Vars};
use serde_json::Value;

#[test]
fn every_render_case() {
    let data = support::conformance("template.json");
    let cases = data["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 70,
        "expected the full suite, got {}",
        cases.len()
    );

    let mut executed = 0usize;
    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        // A case marked normative: false is reference behaviour other SDKs need not reproduce.
        if case.get("normative") == Some(&Value::Bool(false)) {
            continue;
        }
        executed += 1;

        let template = case["template"].as_str().expect("template");
        let engine = match case["engine"].as_str() {
            Some("raw") => Engine::Raw,
            _ => Engine::Liquid,
        };
        let vars = Vars::from(case["variables"].clone());
        let expect = &case["expect"];

        match render(template, &vars, engine) {
            Ok(output) => {
                let expected = expect["output"].as_str().unwrap_or_else(|| {
                    panic!("{name}: expected an error {expect}, rendered {output:?}")
                });
                assert_eq!(output, expected, "{name}");
            }
            Err(error) => {
                let expected = expect["error"].as_str().unwrap_or_else(|| {
                    panic!("{name}: expected output {expect}, failed with {error}")
                });
                assert_eq!(error.category(), expected, "{name}: {error}");
                if let Some(variable) = expect["variable"].as_str() {
                    assert_eq!(
                        error,
                        TemplateError::MissingVariable(variable.to_string()),
                        "{name}"
                    );
                }
            }
        }
    }

    assert!(executed >= 68, "only {executed} normative cases ran");
}

#[test]
fn every_lint_case() {
    let data = support::conformance("template.json");
    for case in data["lint_cases"].as_array().expect("lint_cases") {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let template = case["template"].as_str().expect("template");
        let expect = &case["expect"];

        match lint(template) {
            Ok(()) => assert_eq!(expect["lint"], "ok", "{name}: expected {expect}"),
            Err(reasons) => {
                assert_eq!(expect["lint"], "error", "{name}: unexpected {reasons:?}");
                let expected: Vec<(String, String)> = expect["reasons"]
                    .as_array()
                    .expect("reasons")
                    .iter()
                    .map(|reason| {
                        (
                            reason["kind"].as_str().unwrap().to_string(),
                            reason["value"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect();
                let actual: Vec<(String, String)> = reasons
                    .iter()
                    .map(|reason| (reason.kind().to_string(), reason.value().to_string()))
                    .collect();
                assert_eq!(actual, expected, "{name}");
            }
        }
    }
}

#[test]
fn every_detected_variables_case() {
    let data = support::conformance("template.json");
    for case in data["variables_cases"].as_array().expect("variables_cases") {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let template = case["template"].as_str().expect("template");
        let expected: Vec<String> = case["expect"]["variables"]
            .as_array()
            .expect("variables")
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        assert_eq!(variables(template), expected, "{name}");
    }
}

#[test]
fn the_allowed_sets_match_the_fixture() {
    let data = support::conformance("template.json");
    let filters: Vec<&str> = data["allowed_filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    for filter in filters {
        assert!(
            lint(&format!("{{{{ x | {filter} }}}}")).is_ok(),
            "{filter} should be allowed"
        );
    }
    assert_eq!(
        lint("{{ x | upcase }}").unwrap_err(),
        vec![LintReason::DisallowedFilter("upcase".to_string())]
    );
}
