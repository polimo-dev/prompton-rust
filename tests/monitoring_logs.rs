//! The monitoring-log buffer: batching, the environment parameter, partial acceptance, retries,
//! the 413 split, the 4xx drop, the bounded queue, redaction, and the convenience wrapper.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prompton::{
    CallFailure, CallMeta, Client, Completion, ErrorKind, LogConfig, LogError, LogRecord, Message,
    Mode, Result, Status, TraceEvent, Usage, EVENT_KIND_COMPLETION, EVENT_KIND_TOOL_ATTEMPT,
    EVENT_STATUS_ERROR, EVENT_STATUS_OK,
};
use serde_json::{json, Value};
use support::{StubResponse, StubServer};

const ACCEPTED: &str = r#"{"accepted":1,"duplicates":0,"rejected":[]}"#;

fn quiet_builder(server: &StubServer) -> prompton::ClientBuilder {
    Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .poll(false)
        .fetch_on_start(false)
        .without_disk_cache()
        .log_sink(|_| {})
}

fn record(use_case: &str) -> LogRecord {
    let mut record = LogRecord::new(use_case, "openai/gpt-4o-mini", Status::Ok);
    record.latency_ms = Some(12);
    record.input = Some(json!({"text": "hi"}));
    record.output = Some(json!({"content": "hello"}));
    record
}

fn logs(request: &support::RecordedRequest) -> Vec<Value> {
    request.json()["logs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_batch_is_sent_on_the_size_trigger_with_the_environment_parameter() {
    let server = StubServer::start(|_, _| StubResponse::json(202, ACCEPTED));
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 3,
            flush_interval: Duration::from_secs(60),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    for _ in 0..3 {
        client.log(record("greeting")).unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while server.request_count() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request for three records");
    assert!(
        requests[0].path.contains("environment=production"),
        "{}",
        requests[0].path
    );
    assert_eq!(requests[0].method, "POST");
    let batch = logs(&requests[0]);
    assert_eq!(batch.len(), 3);
    for entry in &batch {
        let id = entry["id"].as_str().unwrap();
        assert!(prompton::uuidv7::is_uuid_v7(id), "{id} must be a UUIDv7");
        assert_eq!(entry["sdk"]["name"], prompton::SDK_NAME);
        assert_eq!(entry["prompt_key"], "greeting");
    }
}

#[test]
fn flush_sends_everything_and_waits_for_the_answer() {
    let server = StubServer::start(|request, _| {
        let count = request.json()["logs"].as_array().unwrap().len();
        StubResponse::json(
            202,
            format!(r#"{{"accepted":{count},"duplicates":0,"rejected":[]}}"#),
        )
    });
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    for _ in 0..5 {
        client.log(record("greeting")).unwrap();
    }
    assert_eq!(
        server.request_count(),
        0,
        "nothing is sent before the trigger"
    );

    let outcome = client.flush().unwrap();
    assert_eq!(outcome.requests, 1);
    assert_eq!(outcome.accepted, 5);
    assert_eq!(outcome.queued, 0);
    assert_eq!(client.log_stats().accepted, 5);
}

#[test]
fn partial_acceptance_never_resends_what_was_accepted() {
    let server = StubServer::start(|_, _| {
        StubResponse::json(
            202,
            r#"{"accepted":1,"duplicates":0,"rejected":[{"index":0,"id":"x","code":"invalid_request","message":"id must be a UUID"}]}"#,
        )
    });
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    client.log(record("greeting")).unwrap();
    let outcome = client.flush().unwrap();

    assert_eq!(outcome.requests, 1);
    assert_eq!(outcome.rejected, 1);
    assert_eq!(outcome.queued, 0, "a rejected record is not requeued");
    assert_eq!(server.request_count(), 1);
    let stats = client.log_stats();
    assert_eq!(stats.accepted, 1);
    assert_eq!(stats.rejected, 1);
}

#[test]
fn a_429_retries_the_same_batch_with_the_same_ids_after_retry_after() {
    let server = StubServer::start(|_, index| {
        if index == 0 {
            StubResponse::json(429, r#"{"error":{"code":"rate_limited","message":"slow"}}"#)
                .with_header("retry-after", "1")
        } else {
            StubResponse::json(202, r#"{"accepted":2,"duplicates":0,"rejected":[]}"#)
        }
    });
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 2,
            flush_interval: Duration::from_millis(50),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    client.log(record("greeting")).unwrap();

    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        server.request_count(),
        1,
        "Retry-After must be waited out before the resend"
    );

    let deadline = Instant::now() + Duration::from_secs(4);
    while server.request_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the batch is resent once, not more");
    let first: Vec<&str> = logs(&requests[0])
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .map(|id| Box::leak(id.to_string().into_boxed_str()) as &str)
        .collect();
    let second: Vec<&str> = logs(&requests[1])
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .map(|id| Box::leak(id.to_string().into_boxed_str()) as &str)
        .collect();
    assert_eq!(first, second, "the resend must carry the same ids");
    assert_eq!(client.log_stats().accepted, 2);
}

#[test]
fn a_413_splits_the_batch_in_half() {
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = sizes.clone();
    let server = StubServer::start(move |request, _| {
        let count = request.json()["logs"].as_array().unwrap().len();
        seen.lock().unwrap().push(count);
        if count > 2 {
            StubResponse::json(
                413,
                r#"{"error":{"code":"payload_too_large","message":"body over 5 MB"}}"#,
            )
        } else {
            StubResponse::json(
                202,
                format!(r#"{{"accepted":{count},"duplicates":0,"rejected":[]}}"#),
            )
        }
    });

    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 4,
            flush_interval: Duration::from_millis(30),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    for _ in 0..4 {
        client.log(record("greeting")).unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(4);
    while client.log_stats().accepted < 4 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(*sizes.lock().unwrap(), vec![4, 2, 2]);
    assert_eq!(client.log_stats().accepted, 4);
}

#[test]
fn any_other_4xx_drops_the_batch_without_retrying() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let server = StubServer::start(|_, _| {
        StubResponse::json(
            401,
            r#"{"error":{"code":"unauthorized","message":"no key"}}"#,
        )
    });

    let client = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_wrong")
        .environment("production")
        .poll(false)
        .fetch_on_start(false)
        .without_disk_cache()
        .log_sink(move |line| sink.lock().unwrap().push(line.to_string()))
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    assert!(client.flush().is_err());
    client.log(record("greeting")).unwrap();
    assert!(client.flush().is_err());

    assert_eq!(
        server.request_count(),
        2,
        "each batch is tried exactly once"
    );
    assert_eq!(client.log_stats().dropped_undeliverable, 2);
    assert_eq!(client.log_stats().queued, 0);
    assert_eq!(
        client.log_stats().requests,
        2,
        "a request that failed still counts as a request"
    );

    let said = lines.lock().unwrap();
    let complaints: Vec<_> = said
        .iter()
        .filter(|line| line.contains("will not be retried"))
        .collect();
    assert_eq!(complaints.len(), 1, "said once, not per batch: {said:?}");
}

#[test]
fn the_queue_is_bounded_and_drops_the_oldest() {
    let server = StubServer::start(|_, _| StubResponse::json(202, ACCEPTED));
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            max_queue: 5,
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    for index in 0..20 {
        let mut record = record("greeting");
        record.trace_id = Some(format!("call-{index}"));
        client.log(record).unwrap();
    }

    let stats = client.log_stats();
    assert_eq!(stats.queued, 5);
    assert_eq!(stats.dropped_oldest, 15);

    client.flush().unwrap();
    let sent = logs(&server.requests()[0]);
    assert_eq!(sent.len(), 5);
    assert_eq!(sent[0]["trace_id"], "call-15", "the newest records survive");
}

#[test]
fn every_request_carries_exactly_one_environment() {
    let server = StubServer::start(|_, _| StubResponse::json(202, ACCEPTED));
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    client
        .log_in(record("greeting"), Some("staging".to_string()))
        .unwrap();
    client.log(record("greeting")).unwrap();
    client.flush().unwrap();

    let paths: Vec<String> = server
        .requests()
        .iter()
        .map(|request| request.path.clone())
        .collect();
    assert_eq!(paths.len(), 3, "three environments changes, three requests");
    assert!(paths[0].contains("environment=production"));
    assert!(paths[1].contains("environment=staging"));
    assert!(paths[2].contains("environment=production"));
}

#[test]
fn one_record_over_the_request_limit_is_dropped_and_counted() {
    let server = StubServer::start(|_, _| StubResponse::json(202, ACCEPTED));
    let client = quiet_builder(&server).build().unwrap();

    // The payload policy shrinks input and output, so the only way to be over the limit after it
    // has run is a field the policy does not touch, such as metadata.
    let mut huge = record("greeting");
    huge.metadata = Some(
        json!({"blob": "x".repeat(prompton::MAX_REQUEST_BYTES + 1)})
            .as_object()
            .cloned()
            .unwrap(),
    );
    client.log(huge).unwrap();

    assert_eq!(client.log_stats().dropped_too_large, 1);
    assert_eq!(client.log_stats().queued, 0);
}

#[test]
fn redaction_runs_last_and_end_user_refs_can_be_hashed() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .hash_end_user(true)
        .redact(|mut value| {
            if let Some(output) = value.get_mut("output") {
                *output = json!({"content": "[redacted]"});
            }
            value
        })
        .log_sink(|_| {})
        .build()
        .unwrap();

    let mut record = record("greeting");
    record.end_user_ref = Some("user-42".to_string());
    client.log(record).unwrap();

    let logged = client.captured_logs();
    assert_eq!(logged[0]["output"], json!({"content": "[redacted]"}));
    assert_eq!(
        logged[0]["end_user_ref"],
        "6d894aa3ee802549d7f340e7c1cf0d1c1cb14cd84f768d92ffaa6785337c4997"
    );
}

#[test]
fn closed_transport_generation_logs_are_suppressed_before_redaction() {
    let redacted = Arc::new(AtomicUsize::new(0));
    let counter = redacted.clone();
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .redact(move |value| {
            counter.fetch_add(1, Ordering::Relaxed);
            value
        })
        .log_sink(|_| {})
        .build()
        .unwrap();

    let mut entry = record("greeting");
    entry.status = Status::Error;
    entry.error = Some(LogError::new(
        ErrorKind::Transport,
        "failed to send request: %Req.TransportError{reason: :closed}",
    ));

    client.log(entry).unwrap();

    assert_eq!(
        redacted.load(Ordering::Relaxed),
        0,
        "closed transport records are dropped before the redact hook"
    );
    assert!(
        client.captured_logs().is_empty(),
        "closed transport records should not reach the test buffer"
    );
}

#[test]
fn only_exact_closed_transport_generation_logs_are_suppressed() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();

    let mut transport_other = record("greeting");
    transport_other.status = Status::Error;
    transport_other.error = Some(LogError::new(ErrorKind::Transport, "connection refused"));
    client.log(transport_other).unwrap();

    let mut app_closed = record("greeting");
    app_closed.status = Status::Error;
    app_closed.error = Some(LogError::new(
        ErrorKind::App,
        "failed to send request: %Req.TransportError{reason: :closed}",
    ));
    client.log(app_closed).unwrap();

    assert_eq!(client.captured_logs().len(), 2);
}

#[test]
fn test_mode_makes_no_http_calls_and_captures_records() {
    let server = StubServer::start(|_, _| StubResponse::json(202, ACCEPTED));
    let client = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    client.set_use_cases(&support::greeting_document()).unwrap();

    let resolution = client.use_case("greeting").unwrap();
    resolution
        .track(CallMeta::new(), || {
            Ok(Completion::new((), Result::text("hi")))
        })
        .unwrap();

    assert_eq!(
        server.request_count(),
        0,
        "test mode never touches the network"
    );
    let logged = client.captured_logs();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0]["prompt_key"], "greeting");
    client.clear_captured_logs();
    assert!(client.captured_logs().is_empty());
}

#[test]
fn the_wrapper_times_the_call_and_returns_what_the_closure_returned() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    client.set_use_cases(&support::greeting_document()).unwrap();
    let resolution = client.use_case("greeting").unwrap();

    let messages = resolution.messages(json!({"name": "Ada"})).unwrap();
    let meta = CallMeta::new()
        .variables(json!({"name": "Ada"}))
        .input_messages(messages.clone())
        .trace_id("job:1");

    let answer = resolution
        .track(meta, || {
            std::thread::sleep(Duration::from_millis(30));
            Ok(Completion::new(
                "Hello, Ada!".to_string(),
                Result::text("Hello, Ada!")
                    .with_finish_reason("stop")
                    .with_usage(Usage::tokens(38, 9)),
            ))
        })
        .unwrap();
    assert_eq!(answer.value, "Hello, Ada!");

    let logged = client.captured_logs();
    let record = &logged[0];
    assert_eq!(record["status"], "ok");
    assert_eq!(record["stop_kind"], "stop");
    assert_eq!(record["trace_id"], "job:1");
    assert!(record["latency_ms"].as_i64().unwrap() >= 30);
    assert_eq!(
        record["input"]["messages"][1]["content"],
        "Say hello to Ada."
    );
    assert_eq!(record["usage"]["input_tokens"], 38);
    assert_eq!(record["source"], "manual");
    let _ = Message::new("user", "unused");
}

#[test]
fn track_logs_app_composed_chat_messages() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    client.set_use_cases(&support::greeting_document()).unwrap();
    let resolution = client.use_case("greeting").unwrap();
    let mut final_messages = resolution.messages(json!({"name": "Ada"})).unwrap();
    final_messages.push(Message::new("user", "Earlier app-owned turn"));
    final_messages.push(Message::new("user", "What should I do next?"));

    resolution
        .track(
            CallMeta::new()
                .variables(json!({"name": "Ada"}))
                .input_messages(final_messages.clone()),
            || {
                Ok(Completion::new(
                    "Use the app-composed messages.",
                    Result::text("Use the app-composed messages."),
                ))
            },
        )
        .unwrap();

    let logged = client.captured_logs();
    let messages = logged[0]["input"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), final_messages.len());
    assert_eq!(
        messages.last().unwrap()["content"],
        "What should I do next?"
    );
}

#[test]
fn track_preserves_native_message_content_presence() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    client.set_use_cases(&support::greeting_document()).unwrap();
    let resolution = client.use_case("greeting").unwrap();
    let messages = vec![
        serde_json::from_value::<Message>(
            json!({"role":"assistant","type":"native","name":"helper","reasoning":"opaque"}),
        )
        .unwrap(),
        serde_json::from_value::<Message>(json!({"role":"assistant","content":null})).unwrap(),
        serde_json::from_value::<Message>(json!({"role":"assistant","content":""})).unwrap(),
        serde_json::from_value::<Message>(json!({"role":"tool","content":[]})).unwrap(),
    ];
    assert!(!messages[0].content_present);
    assert!(messages[0].content.is_empty());
    let encoded_messages = serde_json::to_value(&messages).unwrap();
    assert!(
        encoded_messages[0].get("content").is_none(),
        "{encoded_messages:?}"
    );

    resolution
        .track(CallMeta::new().input_messages(messages), || {
            Ok(Completion::new("ok", Result::text("ok")))
        })
        .unwrap();

    let logged = client.captured_logs();
    let messages = logged[0]["input"]["messages"].as_array().unwrap();
    assert!(messages[0].get("content").is_none(), "{:?}", messages[0]);
    assert_eq!(messages[0]["type"], "native");
    assert_eq!(messages[0]["name"], "helper");
    assert_eq!(messages[0]["reasoning"], "opaque");
    assert!(messages[1].get("content").unwrap().is_null());
    assert_eq!(messages[2]["content"], "");
    assert_eq!(messages[3]["content"], json!([]));
}

#[test]
fn a_failed_call_is_logged_with_its_usage_and_the_error_propagates() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    client.set_use_cases(&support::greeting_document()).unwrap();
    let resolution = client.use_case("greeting").unwrap();

    let failure = resolution
        .track::<String, _>(CallMeta::new(), || {
            Err(
                CallFailure::new(ErrorKind::Parse, "unexpected end of JSON input").with_result(
                    Result::text("{\"greeting\":")
                        .with_finish_reason("length")
                        .with_usage(Usage::tokens(38, 512)),
                ),
            )
        })
        .unwrap_err();
    assert_eq!(failure.error.kind, ErrorKind::Parse);

    let logged = client.captured_logs();
    let record = &logged[0];
    assert_eq!(record["status"], "error");
    assert_eq!(record["error"]["kind"], "parse");
    assert_eq!(record["stop_kind"], "length");
    assert_eq!(record["usage"]["output_tokens"], 512);
    assert_eq!(record["output"]["content"], "{\"greeting\":");
}

#[test]
fn a_panic_is_logged_as_an_app_error_and_then_resumed() {
    let client = Client::builder()
        .mode(Mode::Test)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    client.set_use_cases(&support::greeting_document()).unwrap();
    let resolution = client.use_case("greeting").unwrap();

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
        let resolution = resolution.clone();
        move || resolution.track::<(), _>(CallMeta::new(), || panic!("the provider client blew up"))
    }));
    std::panic::set_hook(previous);

    assert!(outcome.is_err(), "the panic must reach the caller");
    let logged = client.captured_logs();
    assert_eq!(logged[0]["status"], "error");
    assert_eq!(logged[0]["error"]["kind"], "app");
    assert_eq!(logged[0]["error"]["message"], "the provider client blew up");
}

#[test]
fn shutdown_drains_the_queue() {
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = sent.clone();
    let server = StubServer::start(move |request, _| {
        let count = request.json()["logs"].as_array().unwrap().len();
        counter.fetch_add(count, Ordering::Relaxed);
        StubResponse::json(
            202,
            format!(r#"{{"accepted":{count},"duplicates":0,"rejected":[]}}"#),
        )
    });

    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            ..LogConfig::default()
        })
        .build()
        .unwrap();
    for _ in 0..7 {
        client.log(record("greeting")).unwrap();
    }
    client.shutdown();

    assert_eq!(sent.load(Ordering::Relaxed), 7);
}

/// Dropping the last clone must not wait out — or send into — a retry pause. The bug this pins
/// down span the whole backoff ladder at 100% CPU inside `Drop`, so a CLI or a container exiting
/// during a PromptOn outage hung for minutes.
#[test]
fn dropping_a_client_mid_backoff_returns_at_once() {
    let server = StubServer::start(|_, _| {
        StubResponse::json(503, r#"{"error":{"code":"unavailable","message":"down"}}"#)
    });
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            max_attempts: 8,
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    assert!(client.flush().is_err(), "the stub answers 503");
    let after_flush = server.request_count();
    assert_eq!(after_flush, 1);
    assert_eq!(client.log_stats().requests, 1);
    assert_eq!(client.log_stats().queued, 1, "the batch stays queued");

    let start = Instant::now();
    drop(client);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "drop took {elapsed:?}; shutdown must not wait out the backoff"
    );
    assert_eq!(
        server.request_count(),
        after_flush,
        "shutdown must not send inside the retry pause the server asked for"
    );
}

/// The same, through the explicit shutdown path.
#[test]
fn shutdown_mid_backoff_returns_at_once() {
    let server =
        StubServer::start(|_, _| StubResponse::empty(429).with_header("Retry-After", "120"));
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    assert!(client.flush().is_err());
    let after_flush = server.request_count();

    let start = Instant::now();
    client.shutdown();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?} against a Retry-After of 120s"
    );
    assert_eq!(server.request_count(), after_flush);
}

#[test]
fn logging_is_safe_from_many_threads_at_once() {
    let server = StubServer::start(|request, _| {
        let count = request.json()["logs"].as_array().unwrap().len();
        StubResponse::json(
            202,
            format!(r#"{{"accepted":{count},"duplicates":0,"rejected":[]}}"#),
        )
    });
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 50,
            flush_interval: Duration::from_millis(50),
            max_queue: 10_000,
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                client.log(record("greeting")).unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    client.flush().unwrap();

    let stats = client.log_stats();
    assert_eq!(stats.accepted + stats.dropped_oldest, 400);
    assert_eq!(stats.queued, 0);
}

#[test]
fn a_batch_that_never_gets_through_is_dropped_after_its_attempts() {
    let server = StubServer::start(|_, _| {
        StubResponse::json(
            503,
            r#"{"error":{"code":"unavailable","message":"storage failure"}}"#,
        )
    });
    let client = quiet_builder(&server)
        .log_config(LogConfig {
            flush_size: 1000,
            flush_interval: Duration::from_secs(600),
            max_attempts: 2,
            ..LogConfig::default()
        })
        .build()
        .unwrap();

    client.log(record("greeting")).unwrap();
    for _ in 0..3 {
        assert!(client.flush().is_err(), "503 keeps failing");
    }

    let stats = client.log_stats();
    assert_eq!(stats.dropped_undeliverable, 1, "dropped after the attempts");
    assert_eq!(stats.queued, 0, "and not left in the queue for ever");
    assert_eq!(
        server.request_count(),
        3,
        "one send per attempt, then no more"
    );
}

#[test]
fn log_events_posts_events_envelope_and_fills_stable_fields() {
    let server = StubServer::start(|_, _| {
        StubResponse::json(
            202,
            r#"{"accepted":0,"duplicates":0,"rejected":[],"events":{"accepted":1,"duplicates":0,"rejected":[]}}"#,
        )
    });
    let client = quiet_builder(&server).build().unwrap();
    let mut event = TraceEvent::new();
    event.insert("trace_id".to_string(), json!("trace-1"));
    event.insert("event_kind".to_string(), json!(EVENT_KIND_TOOL_ATTEMPT));
    event.insert("status".to_string(), json!(EVENT_STATUS_OK));
    event.insert("tool_call_id".to_string(), json!("call_1"));
    event.insert("tool_name".to_string(), json!("search"));
    event.insert("arguments".to_string(), json!({"q":"diary"}));
    event.insert("result".to_string(), json!([{"text":"found"}]));
    let mut events = vec![event];

    let ack = client.log_events(&mut events, None).unwrap();
    assert_eq!(ack.accepted, 1);
    let first_id = events[0]["event_id"].clone();
    assert!(!first_id.as_str().unwrap_or("").is_empty());
    assert!(!events[0]["observed_at"].as_str().unwrap_or("").is_empty());
    assert_eq!(events[0]["sdk"]["version"], prompton::VERSION);

    client.log_events(&mut events, None).unwrap();
    assert_eq!(events[0]["event_id"], first_id);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let envelope = requests[0].json();
    assert_eq!(envelope["logs"].as_array().unwrap().len(), 0);
    assert_eq!(envelope["events"].as_array().unwrap().len(), 1);
    assert!(requests[0].path.contains("environment=production"));
}

#[test]
fn log_events_suppresses_closed_transport_completion_errors() {
    let server = StubServer::start(|_, _| StubResponse::json(202, ACCEPTED));
    let client = quiet_builder(&server).build().unwrap();
    let mut event = TraceEvent::new();
    event.insert("trace_id".to_string(), json!("trace-closed"));
    event.insert("event_kind".to_string(), json!(EVENT_KIND_COMPLETION));
    event.insert("status".to_string(), json!(EVENT_STATUS_ERROR));
    event.insert(
        "completion_output".to_string(),
        json!("failed to call LLM: failed to send request: %Req.TransportError{reason: :closed}"),
    );
    let mut events = vec![event];

    let ack = client.log_events(&mut events, None).unwrap();

    assert_eq!(ack, prompton::LogsAck::default());
    assert!(
        !events[0]["event_id"].as_str().unwrap_or("").is_empty(),
        "validation/fill still happens before filtering"
    );
    assert_eq!(server.request_count(), 0, "all filtered means no network");
}

#[test]
fn log_events_filters_closed_transport_and_sends_the_rest_in_order() {
    let server = StubServer::start(|request, _| {
        let count = request.json()["events"].as_array().unwrap().len();
        StubResponse::json(
            202,
            format!(
                r#"{{"accepted":0,"duplicates":0,"rejected":[],"events":{{"accepted":{count},"duplicates":0,"rejected":[]}}}}"#
            ),
        )
    });
    let client = quiet_builder(&server).build().unwrap();
    let mut events = vec![
        TraceEvent::from_iter([
            ("trace_id".to_string(), json!("trace-a")),
            ("event_kind".to_string(), json!(EVENT_KIND_COMPLETION)),
            ("status".to_string(), json!(EVENT_STATUS_ERROR)),
            ("completion_output".to_string(), json!("ordinary failure")),
        ]),
        TraceEvent::from_iter([
            ("trace_id".to_string(), json!("trace-closed")),
            ("event_kind".to_string(), json!(EVENT_KIND_COMPLETION)),
            ("status".to_string(), json!(EVENT_STATUS_ERROR)),
            (
                "completion_output".to_string(),
                json!("%Req.TransportError{reason: :closed}"),
            ),
        ]),
        TraceEvent::from_iter([
            ("trace_id".to_string(), json!("trace-b")),
            ("event_kind".to_string(), json!(EVENT_KIND_COMPLETION)),
            ("status".to_string(), json!(EVENT_STATUS_ERROR)),
            ("completion_output".to_string(), json!("connection refused")),
        ]),
    ];

    let ack = client.log_events(&mut events, None).unwrap();

    assert_eq!(ack.accepted, 2);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body = requests[0].json();
    let sent = body["events"].as_array().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["trace_id"], "trace-a");
    assert_eq!(sent[1]["trace_id"], "trace-b");
}

#[test]
fn log_events_validates_required_fields() {
    let server = StubServer::start(|_, _| {
        StubResponse::json(
            202,
            r#"{"accepted":0,"duplicates":0,"rejected":[],"events":{"accepted":1,"duplicates":0,"rejected":[]}}"#,
        )
    });
    let client = quiet_builder(&server).build().unwrap();
    let mut missing_trace = vec![TraceEvent::from_iter([
        (
            "event_kind".to_string(),
            json!(prompton::EVENT_KIND_COMPLETION),
        ),
        ("status".to_string(), json!(EVENT_STATUS_OK)),
    ])];
    assert!(client.log_events(&mut missing_trace, None).is_err());

    let mut bad_kind = vec![TraceEvent::from_iter([
        ("trace_id".to_string(), json!("t")),
        ("event_kind".to_string(), json!("weird")),
        ("status".to_string(), json!(EVENT_STATUS_OK)),
    ])];
    assert!(client.log_events(&mut bad_kind, None).is_err());

    let mut too_many = (0..501)
        .map(|_| {
            TraceEvent::from_iter([
                ("trace_id".to_string(), json!("t")),
                (
                    "event_kind".to_string(),
                    json!(prompton::EVENT_KIND_COMPLETION),
                ),
                ("status".to_string(), json!(EVENT_STATUS_OK)),
            ])
        })
        .collect::<Vec<_>>();
    assert!(client.log_events(&mut too_many, None).is_err());
}
