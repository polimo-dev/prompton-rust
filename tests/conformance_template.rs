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
        // It is not ignored: `every_non_normative_case_is_pinned` below states what this SDK does
        // with each one.
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

/// The cases marked `normative: false` are reference-implementation behaviour other SDKs need not
/// reproduce - but "not required" must not become "not known", so every one of them is pinned to
/// what *this* SDK does. Two match the reference exactly; the other two are the deviations the
/// README and CHANGELOG name.
#[test]
fn every_non_normative_case_is_pinned() {
    let data = support::conformance("template.json");
    let mut seen: Vec<&str> = Vec::new();

    for case in data["cases"].as_array().expect("cases") {
        if case.get("normative") != Some(&Value::Bool(false)) {
            continue;
        }
        let name = case["name"].as_str().expect("name");
        let template = case["template"].as_str().expect("template");
        let vars = Vars::from(case["variables"].clone());
        let reference = case["expect"]["output"].as_str();
        let rendered = render(template, &vars, Engine::Liquid);
        seen.push(name);

        match name {
            // Deviation: an unknown filter is a render error here instead of being applied. The
            // whitelist is enforced by lint as well, so such a template cannot reach a snapshot.
            "nonnormative/unknown_filter_is_applied_at_render_time" => {
                assert!(
                    rendered.is_err(),
                    "{name}: expected a render error, got {rendered:?}"
                );
                let reasons = lint(template).expect_err("lint must reject the filter");
                assert!(
                    reasons.contains(&LintReason::DisallowedFilter("upcase".to_string())),
                    "{name}: {reasons:?}"
                );
            }
            // Matches the reference: the renderer honours whitespace control, lint rejects it.
            "nonnormative/whitespace_control_renders" => {
                assert_eq!(rendered.as_deref().ok(), reference, "{name}");
                assert!(lint(template).is_err(), "{name}: lint must reject it");
            }
            // Deviation: a map in an output position becomes compact JSON, not Elixir's inspect.
            "nonnormative/map_value_stringification" => {
                assert_eq!(rendered.as_deref().ok(), Some("{\"a\":1}"), "{name}");
                assert_ne!(
                    rendered.as_deref().ok(),
                    reference,
                    "{name}: still a deviation"
                );
            }
            // Matches the reference: a false condition swallows the undefined variable.
            "nonnormative/undefined_variable_in_if_condition" => {
                assert_eq!(rendered.as_deref().ok(), reference, "{name}");
            }
            other => panic!("{other} is a new non-normative case; decide and pin what it does"),
        }
    }

    assert_eq!(
        seen.len(),
        4,
        "the fixture has four non-normative cases: {seen:?}"
    );
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
