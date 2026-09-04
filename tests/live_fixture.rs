//! The live integration test: the SDK against a running PromptOn server.
//!
//! It is ignored by the hermetic test suite and requires `PTN_API_KEY` (with `PTN_HOST`, default
//! `http://localhost:4000`) when explicitly selected:
//!
//! ```sh
//! PTN_HOST=http://localhost:4000 PTN_API_KEY=ptn_sdkfixture_… \
//!   cargo test --test live_fixture -- --ignored --nocapture
//! ```
//!
//! The fixture project has three use cases — `greeting` (chat, prompts `default` and `ko`),
//! `summarize` (text) and `embed` (embedding) — in the `production` and `staging` environments.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use prompton::{
    CallMeta, Client, Completion, CostSource, Error, HttpClient, HttpRequest, HttpResponse, Kind,
    RemotePromptRequest, Result, Status, TransportError, UreqClient, Usage, UseCaseOptions,
};
use serde_json::{json, Value};

/// Wraps the real transport and writes down what each call did, so the test can prove that the
/// second poll was conditional and answered `304`.
struct Recording {
    inner: UreqClient,
    calls: Arc<Mutex<Vec<(String, bool, u16)>>>,
}

impl HttpClient for Recording {
    fn execute(&self, request: HttpRequest) -> std::result::Result<HttpResponse, TransportError> {
        let url = request.url.clone();
        let conditional = request
            .headers
            .iter()
            .any(|(name, _)| name == "if-none-match");
        let response = self.inner.execute(request)?;
        self.calls
            .lock()
            .unwrap()
            .push((url, conditional, response.status));
        Ok(response)
    }
}

fn fixture() -> (String, String) {
    let key = std::env::var("PTN_API_KEY")
        .expect("PTN_API_KEY must be set when running the ignored live fixture test");
    assert!(
        !key.is_empty(),
        "PTN_API_KEY must not be empty when running the live fixture test"
    );
    let host = std::env::var("PTN_HOST").unwrap_or_else(|_| "http://localhost:4000".to_string());
    (host, key)
}

#[test]
#[ignore = "requires a running PromptOn fixture server"]
fn against_the_running_fixture_server() {
    let (host, key) = fixture();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(Recording {
        inner: UreqClient::new(Duration::from_secs(10)),
        calls: calls.clone(),
    });
    let dir = support::temp_dir("live");

    let client = Client::builder()
        .host(&host)
        .api_key(&key)
        .environment("production")
        .cache_ttl(Duration::from_millis(50))
        .poll(false)
        .disk_cache_path(dir.join("snapshot.json"))
        .http_client(transport)
        .build()
        .expect("the SDK should start against the fixture server");

    // --- the use-case document, and a conditional repoll --------------------
    let info = client.use_cases_info();
    assert_eq!(info.environment.as_deref(), Some("production"));
    assert_eq!(info.project.as_deref(), Some("sdkfixture"));
    assert_eq!(info.schema_version, Some(prompton::SCHEMA_VERSION));
    let etag = info.etag.clone().expect("the server sends an ETag");
    assert!(etag.contains("sha256-"), "{etag}");

    client.refresh().expect("a second fetch");
    let repoll = calls
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("a recorded call");
    assert!(repoll.1, "the repoll must send If-None-Match");
    assert_eq!(repoll.2, 304, "an unchanged use-case document answers 304");
    assert_eq!(client.use_cases_info().etag.as_deref(), Some(etag.as_str()));

    // The disk cache is written where it was asked for, and the sidecar carries the ETag.
    let sidecar: Value = serde_json::from_slice(
        &std::fs::read(dir.join("snapshot.json.meta.json")).expect("sidecar"),
    )
    .unwrap();
    assert_eq!(sidecar["etag"], Value::String(etag.clone()));
    assert_eq!(sidecar["project"], "sdkfixture");

    // --- local use-case rendering against the remote prompt endpoint ----------------------------
    same_as_server(&client, "greeting", None, json!({"name": "Ada"}));
    same_as_server(&client, "greeting", Some("ko"), json!({"name": "아다"}));
    same_as_server(
        &client,
        "summarize",
        None,
        json!({"items": ["alpha", "beta", "gamma"]}),
    );
    same_as_server(&client, "embed", None, json!({}));

    // --- the error cases ---------------------------------------------------
    assert!(matches!(
        client.use_case("nope"),
        Err(Error::UnknownUseCase(_))
    ));
    match client.prompt_remote(&RemotePromptRequest::new("nope")) {
        Err(Error::Http {
            status, details, ..
        }) => {
            assert_eq!(status, 404);
            assert_eq!(details["key"], "nope");
        }
        other => panic!("expected a 404 for an unknown use case, got {other:?}"),
    }

    match client.use_case_with("greeting", &UseCaseOptions::prompt("fr")) {
        Err(Error::UnknownPrompt { prompt_names, .. }) => {
            assert_eq!(prompt_names, vec!["default", "ko"])
        }
        other => panic!("expected UnknownPrompt, got {other:?}"),
    }
    match client.prompt_remote(&RemotePromptRequest::new("greeting").prompt("fr")) {
        Err(Error::Http {
            status, details, ..
        }) => {
            assert_eq!(status, 404);
            assert_eq!(details["reason"], "unknown_prompt");
            assert_eq!(details["prompt_names"], json!(["default", "ko"]));
        }
        other => panic!("expected a 404 for an unpinned prompt, got {other:?}"),
    }

    let missing = client
        .use_case("greeting")
        .unwrap()
        .messages(json!({}))
        .expect_err("the template needs a name");
    assert_eq!(missing.missing_variable(), Some("name"));
    match client
        .prompt_remote(&RemotePromptRequest::new("greeting").variables(serde_json::Map::new()))
    {
        Err(Error::Http {
            status, details, ..
        }) => {
            assert_eq!(status, 400);
            assert_eq!(details["missing_variable"], "name");
        }
        other => panic!("expected a 400 for a missing variable, got {other:?}"),
    }

    // An unknown environment is a 404 that names it.
    let other_environment = Client::builder()
        .host(&host)
        .api_key(&key)
        .environment("canary")
        .poll(false)
        .fetch_on_start(false)
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    match other_environment.refresh() {
        Err(Error::Http {
            status, details, ..
        }) => {
            assert_eq!(status, 404);
            assert_eq!(details["environment"], "canary");
        }
        other => panic!("expected a 404 for an unknown environment, got {other:?}"),
    }

    // --- staging is a different pin ---------------------------------------
    let staging = client
        .prompt_remote(&RemotePromptRequest::new("greeting").environment("staging"))
        .expect("staging use case works");
    assert_eq!(staging.model.as_deref(), Some("openai/gpt-4o-mini"));
    assert!(
        staging.params.contains_key("temperature"),
        "{:?}",
        staging.params
    );

    // --- monitoring logs: 202, then duplicates on a resend -----------------
    let resolution = client.use_case("greeting").unwrap();
    let messages = resolution.messages(json!({"name": "Ada"})).unwrap();

    let ids: Vec<String> = (0..3).map(|_| client.log_id()).collect();
    let mut first = resolution.log_record(Status::Ok);
    first.id = ids[0].clone();
    first.input = Some(json!({"variables": {"name": "Ada"}, "messages": messages}));
    first.output = Some(json!({"content": "Hello, Ada!"}));
    first.finish_reason = Some("stop".to_string());
    first.stop_kind = Some(prompton::StopKind::Stop);
    first.latency_ms = Some(842);
    first.trace_id = Some("prompton-rust:live".to_string());
    first.usage = Some(Usage::tokens(38, 6).with_cost(0.000012, CostSource::Provider));

    let mut second = resolution.log_record(Status::Error);
    second.id = ids[1].clone();
    second.error = Some(prompton::LogError::http(
        429,
        "rate limited by upstream provider",
    ));
    second.latency_ms = Some(1503);

    // A provider 5xx: `error.kind` has to reach the wire as `http_5xx`. `rate_limited` above is
    // spelled the same whether or not the SDK gets the underscore rule right, so only this record
    // proves the vocabulary the server enforces.
    let mut third = resolution.log_record(Status::Error);
    third.id = ids[2].clone();
    third.error = Some(prompton::LogError::http(
        503,
        "upstream provider is unavailable",
    ));
    third.latency_ms = Some(212);
    assert_eq!(
        serde_json::to_value(&third).unwrap()["error"]["kind"],
        json!("http_5xx")
    );

    client.log(first.clone()).unwrap();
    client.log(second.clone()).unwrap();
    client.log(third.clone()).unwrap();
    let flushed = client.flush().expect("the batch is accepted");
    assert_eq!(flushed.accepted, 3, "{flushed:?}");
    assert_eq!(
        flushed.rejected, 0,
        "the server refused a record: {flushed:?}"
    );

    client.log(first).unwrap();
    client.log(second).unwrap();
    client.log(third).unwrap();
    let resend = client.flush().expect("the resend is accepted");
    assert_eq!(resend.duplicates, 3, "the same ids must be absorbed");
    assert_eq!(resend.accepted, 0);

    // --- the convenience wrapper, end to end -------------------------------
    let answer = resolution
        .track(
            CallMeta::new()
                .variables(json!({"name": "Ada"}))
                .input_messages(messages)
                .trace_id("prompton-rust:wrapper"),
            || {
                Ok(Completion::new(
                    "Hello, Ada!".to_string(),
                    Result::text("Hello, Ada!")
                        .with_finish_reason("stop")
                        .with_usage(Usage::tokens(38, 6)),
                ))
            },
        )
        .unwrap();
    assert_eq!(answer.value, "Hello, Ada!");
    let wrapped = client.flush().expect("the wrapper's record is accepted");
    assert_eq!(wrapped.accepted, 1);

    let stats = client.log_stats();
    assert_eq!(stats.accepted, 4);
    assert_eq!(stats.duplicates, 3);
    assert_eq!(stats.dropped_undeliverable, 0);
    assert_eq!(stats.queued, 0);
}

/// Resolves locally and on the server and compares the two answers field for field.
fn same_as_server(client: &Client, use_case: &str, prompt: Option<&str>, variables: Value) {
    let options = UseCaseOptions {
        prompt: prompt.map(str::to_string),
    };
    let local = client
        .use_case_with(use_case, &options)
        .unwrap_or_else(|error| panic!("{use_case}: local resolution failed: {error}"));

    let mut request = RemotePromptRequest::new(use_case);
    request.prompt = prompt.map(str::to_string);
    if local.kind != Kind::Embedding {
        request.variables = Some(variables.clone().into());
    }
    let remote = client
        .prompt_remote(&request)
        .unwrap_or_else(|error| panic!("{use_case}: server resolution failed: {error}"));

    assert_eq!(local.kind, remote.kind, "{use_case}: kind");
    assert_eq!(local.model, remote.model, "{use_case}: model");
    assert_eq!(local.model_id, remote.model_id, "{use_case}: model_id");
    assert_eq!(local.provider, remote.provider, "{use_case}: provider");
    assert_eq!(local.params, remote.params, "{use_case}: params");
    assert_eq!(
        local.provider_options, remote.provider_options,
        "{use_case}: provider_options"
    );
    assert_eq!(
        local.deployment_id, remote.deployment_id,
        "{use_case}: deployment id"
    );
    assert_eq!(
        local.deployment_revision, remote.deployment_revision,
        "{use_case}: deployment revision"
    );
    assert_eq!(local.prompt, remote.prompt, "{use_case}: prompt");
    assert_eq!(
        local.prompt_names, remote.prompt_names,
        "{use_case}: prompt_names"
    );
    assert_eq!(remote.key, use_case, "{use_case}: key");
    assert_eq!(
        remote.source,
        prompton::Source::Remote,
        "{use_case}: source"
    );
    assert_eq!(
        local.prompt_version_id, remote.prompt_version_id,
        "{use_case}: prompt version"
    );
    assert_eq!(
        remote.warnings,
        Vec::<String>::new(),
        "{use_case}: warnings"
    );

    match local.kind {
        Kind::Chat => {
            let rendered = local.messages(variables).unwrap();
            assert_eq!(
                Some(rendered.as_slice()),
                remote.messages.as_deref(),
                "{use_case}: the local render must match the server's"
            );
        }
        Kind::Text => {
            let rendered = local.text(variables).unwrap();
            assert_eq!(
                Some(rendered.as_str()),
                remote.text.as_deref(),
                "{use_case}: the local render must match the server's"
            );
        }
        _ => {
            assert!(remote.messages.is_none(), "{use_case}: embedding messages");
            assert!(remote.text.is_none(), "{use_case}: embedding text");
            assert!(local.prompt.is_none());
        }
    }
}
