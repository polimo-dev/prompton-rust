//! The live integration test: the SDK against a running PromptOn server.
//!
//! It runs only when `PTN_API_KEY` is set (with `PTN_HOST`, default `http://localhost:4000`), so
//! `cargo test` stays hermetic everywhere else:
//!
//! ```sh
//! PTN_HOST=http://localhost:4000 PTN_API_KEY=ptn_sdkfixture_… cargo test --test live_fixture
//! ```
//!
//! The fixture project has three use cases — `greeting` (chat, prompts `default` and `ko`),
//! `summarize` (text) and `embed` (embedding) — in the `production` and `staging` environments.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use prompton::{
    CallMeta, Client, Completion, CostSource, Error, GenerationRecord, HttpClient, HttpRequest,
    HttpResponse, Kind, Outcome, RemoteResolveRequest, ResolveOptions, Status, TransportError,
    UreqClient, Usage,
};
use serde_json::{json, Value};

/// Wraps the real transport and writes down what each call did, so the test can prove that the
/// second poll was conditional and answered `304`.
struct Recording {
    inner: UreqClient,
    calls: Arc<Mutex<Vec<(String, bool, u16)>>>,
}

impl HttpClient for Recording {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
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

fn fixture() -> Option<(String, String)> {
    let key = std::env::var("PTN_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())?;
    let host = std::env::var("PTN_HOST").unwrap_or_else(|_| "http://localhost:4000".to_string());
    Some((host, key))
}

#[test]
fn against_the_running_fixture_server() {
    let Some((host, key)) = fixture() else {
        eprintln!("skipping the live test: PTN_API_KEY is not set");
        return;
    };

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

    // --- the snapshot, and a conditional repoll -----------------------------
    let info = client.snapshot_info();
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
    assert_eq!(repoll.2, 304, "an unchanged snapshot answers 304");
    assert_eq!(client.snapshot_info().etag.as_deref(), Some(etag.as_str()));

    // The disk cache is written where it was asked for, and the sidecar carries the ETag.
    let sidecar: Value = serde_json::from_slice(
        &std::fs::read(dir.join("snapshot.json.meta.json")).expect("sidecar"),
    )
    .unwrap();
    assert_eq!(sidecar["etag"], Value::String(etag.clone()));
    assert_eq!(sidecar["project"], "sdkfixture");

    // --- local resolution against POST /resolve ----------------------------
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
        client.resolve("nope"),
        Err(Error::UnknownUseCase(_))
    ));
    match client.resolve_remote(&RemoteResolveRequest::new("nope")) {
        Err(Error::Http {
            status, details, ..
        }) => {
            assert_eq!(status, 404);
            assert_eq!(details["use_case"], "nope");
        }
        other => panic!("expected a 404 for an unknown use case, got {other:?}"),
    }

    match client.resolve_with("greeting", &ResolveOptions::prompt("fr")) {
        Err(Error::UnknownPrompt {
            available_prompts, ..
        }) => assert_eq!(available_prompts, vec!["default", "ko"]),
        other => panic!("expected UnknownPrompt, got {other:?}"),
    }
    match client.resolve_remote(&RemoteResolveRequest::new("greeting").prompt("fr")) {
        Err(Error::Http {
            status, details, ..
        }) => {
            assert_eq!(status, 404);
            assert_eq!(details["reason"], "unknown_prompt");
            assert_eq!(details["available_prompts"], json!(["default", "ko"]));
        }
        other => panic!("expected a 404 for an unpinned prompt, got {other:?}"),
    }

    let missing = client
        .resolve("greeting")
        .unwrap()
        .render(json!({}))
        .expect_err("the template needs a name");
    assert_eq!(missing.missing_variable(), Some("name"));
    match client
        .resolve_remote(&RemoteResolveRequest::new("greeting").variables(serde_json::Map::new()))
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
        .resolve_remote(&RemoteResolveRequest::new("greeting").environment("staging"))
        .expect("staging resolves");
    assert_eq!(staging.model.as_deref(), Some("openai/gpt-4o-mini"));
    assert!(
        staging.params.contains_key("temperature"),
        "{:?}",
        staging.params
    );

    // --- monitoring logs: 202, then duplicates on a resend -----------------
    let resolution = client.resolve("greeting").unwrap();
    let messages = resolution.render_messages(json!({"name": "Ada"})).unwrap();

    let ids: Vec<String> = (0..3).map(|_| client.generation_id()).collect();
    let mut first = GenerationRecord::from_resolution(&resolution, Status::Ok);
    first.id = ids[0].clone();
    first.input = Some(json!({"variables": {"name": "Ada"}, "messages": messages}));
    first.output = Some(json!({"content": "Hello, Ada!"}));
    first.finish_reason = Some("stop".to_string());
    first.stop_kind = Some(prompton::StopKind::Stop);
    first.latency_ms = Some(842);
    first.trace_id = Some("prompton-rust:live".to_string());
    first.usage = Some(Usage::tokens(38, 6).with_cost(0.000012, CostSource::Provider));

    let mut second = GenerationRecord::from_resolution(&resolution, Status::Error);
    second.id = ids[1].clone();
    second.error = Some(prompton::GenerationError::http(
        429,
        "rate limited by upstream provider",
    ));
    second.latency_ms = Some(1503);

    // A provider 5xx: `error.kind` has to reach the wire as `http_5xx`. `rate_limited` above is
    // spelled the same whether or not the SDK gets the underscore rule right, so only this record
    // proves the vocabulary the server enforces.
    let mut third = GenerationRecord::from_resolution(&resolution, Status::Error);
    third.id = ids[2].clone();
    third.error = Some(prompton::GenerationError::http(
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
    let outcome = client.flush().expect("the batch is accepted");
    assert_eq!(outcome.accepted, 3, "{outcome:?}");
    assert_eq!(
        outcome.rejected, 0,
        "the server refused a record: {outcome:?}"
    );

    client.log(first).unwrap();
    client.log(second).unwrap();
    client.log(third).unwrap();
    let resend = client.flush().expect("the resend is accepted");
    assert_eq!(resend.duplicates, 3, "the same ids must be absorbed");
    assert_eq!(resend.accepted, 0);

    // --- the convenience wrapper, end to end -------------------------------
    let answer = client
        .with_generation(
            &resolution,
            CallMeta::new()
                .variables(json!({"name": "Ada"}))
                .input_messages(messages)
                .trace_id("prompton-rust:wrapper"),
            || {
                Ok(Completion::new(
                    "Hello, Ada!".to_string(),
                    Outcome::text("Hello, Ada!")
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
    let options = ResolveOptions {
        prompt: prompt.map(str::to_string),
    };
    let local = client
        .resolve_with(use_case, &options)
        .unwrap_or_else(|error| panic!("{use_case}: local resolution failed: {error}"));

    let mut request = RemoteResolveRequest::new(use_case);
    request.prompt = prompt.map(str::to_string);
    if local.kind != Kind::Embedding {
        request.variables = Some(variables.clone().into());
    }
    let remote = client
        .resolve_remote(&request)
        .unwrap_or_else(|error| panic!("{use_case}: server resolution failed: {error}"));

    assert_eq!(local.kind, remote.kind, "{use_case}: kind");
    assert_eq!(local.model, remote.model, "{use_case}: model");
    assert_eq!(local.model_id, remote.model_id, "{use_case}: model_id");
    assert_eq!(local.provider, remote.provider, "{use_case}: provider");
    assert_eq!(local.params, remote.params, "{use_case}: effective_params");
    assert_eq!(
        local.provider_options, remote.provider_options,
        "{use_case}: effective_provider_options"
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
        local.available_prompts, remote.available_prompts,
        "{use_case}: prompts"
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
            let rendered = local.render_messages(variables).unwrap();
            assert_eq!(
                Some(rendered.as_slice()),
                remote.messages.as_deref(),
                "{use_case}: the local render must match the server's"
            );
        }
        Kind::Text => {
            let rendered = local.render_text(variables).unwrap();
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
