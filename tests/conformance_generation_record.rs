//! The cross-language monitoring-log shape: every record in `conformance/generation_record.json`.
//!
//! These are golden shapes rather than executable cases, so this file checks three things: that
//! every record round-trips through [`GenerationRecord`] unchanged, that the required fields and
//! the UUIDv7 rule hold, and that the SDK's own record builder produces the same shape for the
//! same inputs.

mod support;

use prompton::{
    CallMeta, Client, Completion, CostSource, ErrorKind, GenerationError, GenerationRecord,
    Message, Mode, Outcome, Usage,
};
use serde_json::{json, Map, Value};

/// Drops keys whose value is null, so a record that spells out `"raw": null` compares equal to
/// one that leaves the key out. The server treats them the same.
fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        other => other.clone(),
    }
}

#[test]
fn every_golden_record_round_trips() {
    let data = support::conformance("generation_record.json");
    let records = data["records"].as_array().expect("records");
    assert_eq!(records.len(), 5);

    for entry in records {
        let name = entry["name"].as_str().unwrap_or("<unnamed>");
        let record: GenerationRecord = serde_json::from_value(entry["record"].clone())
            .unwrap_or_else(|error| panic!("{name} does not deserialise: {error}"));

        record
            .validate()
            .unwrap_or_else(|error| panic!("{name} is not valid: {error}"));
        assert!(
            prompton::uuidv7::is_uuid_v7(&record.id),
            "{name}: the id must be a UUIDv7"
        );

        let round_trip = serde_json::to_value(&record).unwrap();
        assert_eq!(
            without_nulls(&round_trip),
            without_nulls(&entry["record"]),
            "{name}"
        );
    }
}

#[test]
fn the_batch_envelope_is_one_object_with_a_generations_array() {
    let data = support::conformance("generation_record.json");
    let request = &data["batch_envelope"]["request"];
    let generations = request["generations"].as_array().expect("generations");
    assert_eq!(generations.len(), 5);
    assert!(generations.len() <= prompton::MAX_RECORDS_PER_REQUEST);

    let envelope = json!({ "generations": generations });
    assert_eq!(envelope.as_object().unwrap().len(), 1);
}

#[test]
fn the_field_rules_hold() {
    let data = support::conformance("generation_record.json");
    let required: Vec<&str> = data["field_rules"]["required"]
        .as_array()
        .expect("required")
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        required,
        vec!["id", "use_case", "model", "status", "started_at"]
    );

    let mut record = GenerationRecord::new("greeting", "openai/gpt-4o-mini", prompton::Status::Ok);
    assert!(record.validate().is_ok());
    for blank in ["use_case", "model", "started_at"] {
        let mut broken = record.clone();
        match blank {
            "use_case" => broken.use_case = String::new(),
            "model" => broken.model = String::new(),
            _ => broken.started_at = String::new(),
        }
        assert!(broken.validate().is_err(), "{blank} must be required");
    }
    record.id = "d2b0f1e4-6f5d-4a1e-9f3a-0b0c0d0e0f10".to_string();
    assert!(
        !prompton::uuidv7::is_uuid_v7(&record.id),
        "a v4 id must not pass for a v7 one"
    );
}

/// The wrapper's own output, for the inputs the `chat/success` fixture was built from.
#[test]
fn the_record_builder_matches_the_golden_chat_success_shape() {
    let data = support::conformance("generation_record.json");
    let golden = data["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "chat/success")
        .expect("chat/success")["record"]
        .clone();

    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .build()
        .unwrap();
    client
        .set_snapshot_as(
            &support::greeting_snapshot(),
            prompton::ResolutionSource::Remote,
        )
        .unwrap();

    let resolution = client.resolve("greeting").unwrap();
    let messages = vec![
        Message::new("system", "You are a friendly greeter. Answer in one line."),
        Message::new("user", "Say hello to Ada."),
    ];

    let meta = CallMeta::new()
        .variables(json!({"name": "Ada"}))
        .input_messages(messages)
        .end_user_ref("user-42")
        .trace_id("oban:8842")
        .sequence(1)
        .context(json!({"language": "en", "plan": "pro"}))
        .metadata(json!({"attempt": 1, "job_id": 8842}));

    let outcome = Outcome::text("Hello, Ada! Lovely to see you.")
        .with_finish_reason("stop")
        .with_usage(
            Usage {
                input_tokens: Some(38),
                output_tokens: Some(9),
                raw: Some(json!({"completion_tokens": 9, "prompt_tokens": 38, "total_tokens": 47})),
                ..Usage::default()
            }
            .with_cost(0.000112, CostSource::Provider),
        );
    let outcome = Outcome {
        model_used: Some("openai/gpt-4o-mini".to_string()),
        upstream_provider: Some("OpenAI".to_string()),
        is_byok: Some(false),
        ..outcome
    };

    client
        .with_generation(&resolution, meta, || {
            Ok(Completion::new((), outcome.clone()))
        })
        .unwrap();

    let logged = client.captured_logs();
    assert_eq!(logged.len(), 1);
    let built = normalise(logged[0].clone());
    let expected = normalise(golden.as_object().unwrap().clone());
    assert_eq!(Value::Object(built), Value::Object(expected));
}

/// The wrapper's output for a failed call, against the `chat/error_without_output` shape.
#[test]
fn the_record_builder_matches_the_golden_error_shape() {
    let data = support::conformance("generation_record.json");
    let golden = data["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "chat/error_without_output")
        .expect("chat/error_without_output")["record"]
        .clone();

    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .build()
        .unwrap();
    client
        .set_snapshot_as(
            &support::greeting_snapshot(),
            prompton::ResolutionSource::Remote,
        )
        .unwrap();
    let resolution = client.resolve("greeting").unwrap();

    let meta = CallMeta::new()
        .variables(json!({"name": "Ada"}))
        .input_messages(vec![
            Message::new("system", "You are a friendly greeter. Answer in one line."),
            Message::new("user", "Say hello to Ada."),
        ])
        .trace_id("oban:8843")
        .sequence(2);

    let failure = client
        .with_generation::<(), _>(&resolution, meta, || {
            Err(prompton::CallFailure {
                error: GenerationError {
                    kind: ErrorKind::RateLimited,
                    status: Some(429),
                    message: Some("rate limited by upstream provider".to_string()),
                },
                outcome: None,
                source: None,
            })
        })
        .unwrap_err();
    assert_eq!(failure.error.kind, ErrorKind::RateLimited);

    let logged = client.captured_logs();
    let built = normalise(logged[0].clone());
    let expected = normalise(golden.as_object().unwrap().clone());
    assert_eq!(Value::Object(built), Value::Object(expected));
}

/// Replaces the fields that are new on every run, and the SDK's own name, so two records built at
/// different times and by different SDKs can be compared field for field.
fn normalise(mut record: Map<String, Value>) -> Map<String, Value> {
    record.insert("id".to_string(), json!("<id>"));
    record.insert("started_at".to_string(), json!("<started_at>"));
    record.insert("latency_ms".to_string(), json!(0));
    record.insert(
        "sdk".to_string(),
        json!({"name": "<sdk>", "version": "<v>"}),
    );
    without_nulls(&Value::Object(record))
        .as_object()
        .cloned()
        .unwrap()
}
