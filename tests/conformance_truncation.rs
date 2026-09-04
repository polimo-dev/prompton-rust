//! The cross-language payload-policy contract: every case in `conformance/truncation.json`.

mod support;

use prompton::payload::{apply, bucket, PayloadConfig};
use prompton::{PayloadMode, PayloadPolicy};
use serde_json::Value;

fn policy_of(value: &Value) -> PayloadPolicy {
    PayloadPolicy {
        mode: match value["mode"].as_str() {
            Some("hash") => PayloadMode::Hash,
            Some("none") => PayloadMode::None,
            _ => PayloadMode::Full,
        },
        sample_rate: value["sample_rate"].as_f64().unwrap_or(1.0),
        max_bytes: value["max_bytes"].as_u64().unwrap_or(262_144) as usize,
        retention_days: value["retention_days"].as_i64(),
        encrypt: value["encrypt"].as_bool().unwrap_or(false),
    }
}

#[test]
fn every_truncation_case() {
    let data = support::conformance("truncation.json");
    let cases = data["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 19,
        "expected the full suite, got {}",
        cases.len()
    );

    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let policy = policy_of(&case["policy"]);
        let config = PayloadConfig {
            defaults: PayloadPolicy::default(),
            hash_end_user: case["config"]["hash_end_user"].as_bool().unwrap_or(false),
            redact: None,
        };

        let record = case["log"].as_object().expect("log").clone();
        let actual = apply(record, Some(&policy), &config);

        assert_eq!(Value::Object(actual), case["expect"]["log"], "{name}");
    }
}

#[test]
fn every_sampling_bucket() {
    let data = support::conformance("truncation.json");
    for entry in data["sampling"]["buckets"].as_array().expect("buckets") {
        let id = entry["id"].as_str().expect("id");
        let expected = entry["bucket"].as_u64().expect("bucket") as u32;
        assert_eq!(bucket(id), expected, "bucket({id:?})");
    }
}

#[test]
fn the_limits_match_the_fixture() {
    let data = support::conformance("truncation.json");
    let limits = &data["limits"];
    assert_eq!(limits["max_bytes_default"].as_u64(), Some(262_144));
    assert_eq!(
        limits["error_message"].as_str(),
        Some("2048 bytes, fixed"),
        "the SDK caps error.message at {}",
        prompton::payload::ERROR_MESSAGE_MAX
    );
    assert_eq!(prompton::payload::ERROR_MESSAGE_MAX, 2048);
}
