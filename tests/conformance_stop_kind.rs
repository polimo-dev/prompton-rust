//! The cross-language `stop_kind` contract: every case in `conformance/stop_kind.json`.

mod support;

use prompton::StopKind;

#[test]
fn every_stop_kind_case() {
    let data = support::conformance("stop_kind.json");
    let cases = data["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 22,
        "expected the full suite, got {}",
        cases.len()
    );

    for case in cases {
        let raw = case["finish_reason"].as_str();
        let source = case["source"].as_str().unwrap_or("");
        let expected = case["stop_kind"].as_str().expect("stop_kind");
        let truncated = case["truncated"].as_bool().expect("truncated");

        let kind = StopKind::normalize(raw);
        assert_eq!(kind.as_str(), expected, "{raw:?} ({source})");
        assert_eq!(kind.truncated(), truncated, "{raw:?} ({source})");

        // Normalisation is idempotent: the server re-normalises whatever the client sent.
        assert_eq!(
            StopKind::normalize(Some(kind.as_str())),
            kind,
            "{raw:?} is not idempotent"
        );
    }
}
