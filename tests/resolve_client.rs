//! The remote prompt client: the simple path, its 10-second cache, and what it does when PromptOn
//! is rate-limiting or failing.

mod support;

use std::time::Duration;

use prompton::{Client, Error, Kind, RemotePromptRequest};
use serde_json::json;
use support::{StubResponse, StubServer};

fn prompt_body(prompt: &str, content: &str) -> String {
    json!({
        "key": "greeting",
        "kind": "chat",
        "deployment": {"id": "0198f2a1-0000-7000-8000-00000000d001", "revision": 3},
        "template": prompt,
        "template_names": ["default", "ko"],
        "model_id": "0198f2a1-0000-7000-8000-00000000e001",
        "model": "openai/gpt-4o-mini",
        "provider": "openrouter",
        "params": {"max_tokens": 512, "temperature": 0.2},
        "provider_options": {"only": ["OpenAI"]},
        "prompt_version": {"id": "0198f2a1-0000-7000-8000-00000000a001", "number": 2},
        "messages": [{"role": "user", "content": content}],
        "warnings": [],
        "source": "remote",
        "etag": "sha256-1111"
    })
    .to_string()
}

fn client_for(server: &StubServer, ttl: Duration) -> Client {
    Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .cache_ttl(ttl)
        .poll(false)
        .fetch_on_start(false)
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap()
}

#[test]
fn a_variable_less_answer_is_cached_and_rendered_locally() {
    let server = StubServer::start(|request, _| {
        assert!(
            request.path.ends_with("/prompts/greeting/render"),
            "{}",
            request.path
        );
        assert_eq!(request.json()["environment"], "production");
        StubResponse::json(200, prompt_body("default", "Say hello to {{ name }}."))
    });
    let client = client_for(&server, Duration::from_secs(60));

    let answer = client
        .prompt_remote(&RemotePromptRequest::new("greeting"))
        .unwrap();
    assert_eq!(answer.key, "greeting");
    assert_eq!(answer.kind, Kind::Chat);
    assert_eq!(answer.model.as_deref(), Some("openai/gpt-4o-mini"));
    assert_eq!(answer.prompt_names, vec!["default", "ko"]);
    assert_eq!(answer.deployment_revision, Some(3));
    assert_eq!(answer.source, prompton::Source::Remote);

    let rendered = answer.messages(json!({"name": "Ada"})).unwrap();
    assert_eq!(rendered[0].content, "Say hello to Ada.");

    for _ in 0..5 {
        client
            .prompt_remote(&RemotePromptRequest::new("greeting"))
            .unwrap();
    }
    assert_eq!(server.request_count(), 1, "the answer is cached per TTL");
}

#[test]
fn a_call_with_variables_is_never_cached() {
    let server = StubServer::start(|request, _| {
        assert_eq!(request.json()["variables"]["name"], "Ada");
        StubResponse::json(200, prompt_body("default", "Say hello to Ada."))
    });
    let client = client_for(&server, Duration::from_secs(60));

    for _ in 0..3 {
        let answer = client
            .prompt_remote(&RemotePromptRequest::new("greeting").variables(json!({"name": "Ada"})))
            .unwrap();
        assert_eq!(
            answer.messages.as_ref().unwrap()[0].content,
            "Say hello to Ada."
        );
    }
    assert_eq!(server.request_count(), 3, "rendering is always fresh");
}

#[test]
fn the_prompt_name_is_part_of_the_cache_key() {
    let server = StubServer::start(|request, _| {
        let prompt = request.json()["template"]
            .as_str()
            .unwrap_or("default")
            .to_string();
        StubResponse::json(200, prompt_body(&prompt, "…"))
    });
    let client = client_for(&server, Duration::from_secs(60));

    client
        .prompt_remote(&RemotePromptRequest::new("greeting"))
        .unwrap();
    let ko = client
        .prompt_remote(&RemotePromptRequest::new("greeting").prompt("ko"))
        .unwrap();
    assert_eq!(ko.prompt.as_deref(), Some("ko"));
    assert_eq!(server.request_count(), 2);

    client
        .prompt_remote(&RemotePromptRequest::new("greeting").prompt("ko"))
        .unwrap();
    assert_eq!(server.request_count(), 2, "the second ko call is cached");
}

#[test]
fn a_rate_limit_or_a_5xx_is_answered_from_the_cache() {
    let server = StubServer::start(|_, index| {
        if index == 0 {
            StubResponse::json(200, prompt_body("default", "Say hello to {{ name }}."))
        } else if index == 1 {
            StubResponse::json(429, r#"{"error":{"code":"rate_limited","message":"slow"}}"#)
        } else {
            StubResponse::json(
                500,
                r#"{"error":{"code":"internal_error","message":"boom"}}"#,
            )
        }
    });
    let client = client_for(&server, Duration::from_millis(30));

    client
        .prompt_remote(&RemotePromptRequest::new("greeting"))
        .unwrap();
    std::thread::sleep(Duration::from_millis(60));

    let after_429 = client
        .prompt_remote(&RemotePromptRequest::new("greeting"))
        .expect("the cached answer keeps serving");
    assert_eq!(after_429.model.as_deref(), Some("openai/gpt-4o-mini"));

    std::thread::sleep(Duration::from_millis(60));
    let after_500 = client
        .prompt_remote(&RemotePromptRequest::new("greeting"))
        .expect("still serving");
    assert_eq!(after_500.deployment_revision, Some(3));
    assert_eq!(server.request_count(), 3);
}

#[test]
fn a_404_is_reported_with_its_details() {
    let server = StubServer::start(|_, _| {
        StubResponse::json(
            404,
            r#"{"error":{"code":"not_found","message":"no prompt named \"fr\"","details":{"reason":"unknown_template","template":"fr","template_names":["default","ko"]}}}"#,
        )
    });
    let client = client_for(&server, Duration::from_secs(60));

    match client.prompt_remote(&RemotePromptRequest::new("greeting").prompt("fr")) {
        Err(Error::Http {
            status,
            code,
            details,
            ..
        }) => {
            assert_eq!(status, 404);
            assert_eq!(code.as_deref(), Some("not_found"));
            assert_eq!(details["reason"], "unknown_template");
            assert_eq!(details["template_names"], json!(["default", "ko"]));
        }
        other => panic!("expected a 404, got {other:?}"),
    }
}

#[test]
fn offline_and_test_modes_refuse_to_call_out() {
    for mode in [prompton::Mode::Offline, prompton::Mode::Test] {
        let client = Client::builder()
            .mode(mode)
            .api_key("ptn_demo_secret")
            .environment("production")
            .without_disk_cache()
            .log_sink(|_| {})
            .build()
            .unwrap();
        assert!(matches!(
            client.prompt_remote(&RemotePromptRequest::new("greeting")),
            Err(Error::RemoteDisabled(_))
        ));
        assert!(matches!(client.refresh(), Err(Error::RemoteDisabled(_))));
    }
}
