//! The cross-language monitoring-log shape: every record in `conformance/log_record.json`.
//!
//! These are golden shapes rather than executable cases, so this file checks three things: that
//! every record round-trips through [`LogRecord`] unchanged, that the required fields and
//! the UUIDv7 rule hold, and that the SDK's own record builder produces the same shape for the
//! same inputs.

mod support;

use prompton::{
    CallMeta, Client, Completion, CostSource, ErrorKind, LogError, LogRecord, Message, Mode,
    Result, Usage,
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
    let data = support::conformance("log_record.json");
    let records = data["records"].as_array().expect("records");
    assert_eq!(records.len(), 5);

    for entry in records {
        let name = entry["name"].as_str().unwrap_or("<unnamed>");
        let record: LogRecord = serde_json::from_value(entry["record"].clone())
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

/// `error.kind` is a closed vocabulary and the server rejects anything outside it, so every kind
/// the SDK can emit is pinned against `field_rules["error.kind"]` — the wire spelling, both ways,
/// and the status mapping that picks one.
#[test]
fn every_error_kind_matches_the_field_rule() {
    let data = support::conformance("log_record.json");
    let rule = data["field_rules"]["error.kind"]
        .as_str()
        .expect("the error.kind rule");
    let allowed: Vec<&str> = rule.split('|').map(str::trim).collect();
    assert_eq!(
        allowed,
        vec![
            "http_4xx",
            "http_5xx",
            "rate_limited",
            "timeout",
            "transport",
            "parse",
            "app"
        ]
    );

    let kinds = [
        ErrorKind::Http4xx,
        ErrorKind::Http5xx,
        ErrorKind::RateLimited,
        ErrorKind::Timeout,
        ErrorKind::Transport,
        ErrorKind::Parse,
        ErrorKind::App,
    ];
    assert_eq!(kinds.len(), allowed.len());

    for (kind, wire) in kinds.iter().zip(allowed.iter()) {
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            json!(wire),
            "{kind:?} must serialise as {wire}"
        );
        let parsed: ErrorKind = serde_json::from_value(json!(wire))
            .unwrap_or_else(|error| panic!("{wire} must deserialise: {error}"));
        assert_eq!(parsed, *kind);

        let error = LogError::new(*kind, "x");
        assert_eq!(serde_json::to_value(&error).unwrap()["kind"], json!(wire));
    }

    // The status mapping the wrapper uses, and the record it ends up putting on the wire.
    assert_eq!(ErrorKind::from_status(404), ErrorKind::Http4xx);
    assert_eq!(ErrorKind::from_status(429), ErrorKind::RateLimited);
    assert_eq!(ErrorKind::from_status(503), ErrorKind::Http5xx);

    let mut record = LogRecord::new("greeting", "openai/gpt-4o-mini", prompton::Status::Error);
    record.error = Some(LogError::http(503, "upstream is down"));
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["error"]["kind"], json!("http_5xx"));
    assert_eq!(value["error"]["status"], json!(503));
}

#[test]
fn the_batch_envelope_is_one_object_with_a_logs_array() {
    let data = support::conformance("log_record.json");
    let request = &data["batch_envelope"]["request"];
    let logs = request["logs"].as_array().expect("logs");
    assert_eq!(logs.len(), 5);
    assert!(logs.len() <= prompton::MAX_RECORDS_PER_REQUEST);

    let envelope = json!({ "logs": logs });
    assert_eq!(envelope.as_object().unwrap().len(), 1);
}

#[test]
fn the_field_rules_hold() {
    let data = support::conformance("log_record.json");
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

    let mut record = LogRecord::new("greeting", "openai/gpt-4o-mini", prompton::Status::Ok);
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
    let data = support::conformance("log_record.json");
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
        .set_use_cases_as(&support::greeting_document(), prompton::Source::Remote)
        .unwrap();

    let resolution = client.use_case("greeting").unwrap();
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

    let outcome = Result::text("Hello, Ada! Lovely to see you.")
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
    let outcome = Result {
        model_used: Some("openai/gpt-4o-mini".to_string()),
        upstream_provider: Some("OpenAI".to_string()),
        is_byok: Some(false),
        ..outcome
    };

    resolution
        .track(meta, || Ok(Completion::new((), outcome.clone())))
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
    let data = support::conformance("log_record.json");
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
        .set_use_cases_as(&support::greeting_document(), prompton::Source::Remote)
        .unwrap();
    let resolution = client.use_case("greeting").unwrap();

    let meta = CallMeta::new()
        .variables(json!({"name": "Ada"}))
        .input_messages(vec![
            Message::new("system", "You are a friendly greeter. Answer in one line."),
            Message::new("user", "Say hello to Ada."),
        ])
        .trace_id("oban:8843")
        .sequence(2);

    let failure = resolution
        .track::<(), _>(meta, || {
            Err(prompton::CallFailure {
                error: LogError {
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
