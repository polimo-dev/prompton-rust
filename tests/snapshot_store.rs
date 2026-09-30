//! The caching rules: the 10-second memory cache, conditional refreshes, rate limits, backoff,
//! and the disk and bundle tiers that keep an app running while PromptOn is down.

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prompton::{
    Client, Error, HttpClient, HttpRequest, HttpResponse, Mode, Source, TransportError,
};
use support::{document_json, StubResponse, StubServer};

const ETAG: &str = "\"sha256-1111\"";

fn ok_snapshot(greeting: &str) -> StubResponse {
    StubResponse::json(200, document_json("production", "demo", greeting))
        .with_header("etag", ETAG)
        .with_header("cache-control", "max-age=30")
}

#[derive(Default)]
struct ConcurrentFetchClient {
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    urls: Mutex<Vec<String>>,
}

impl ConcurrentFetchClient {
    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }
}

impl HttpClient for ConcurrentFetchClient {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.urls.lock().unwrap().push(request.url.clone());
        std::thread::sleep(Duration::from_millis(100));
        self.active.fetch_sub(1, Ordering::SeqCst);

        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert(
            "etag".to_string(),
            format!("\"etag-{}\"", self.calls.load(Ordering::SeqCst)),
        );
        let mut body = document_json("production", "demo", "Hello {{ name }}");
        if request.url.contains("/prompts/demand_other?") {
            body = body
                .replace("\"greeting\"", "\"demand_other\"")
                .replace("openai/gpt-4o-mini", "openai/other-model");
        }
        Ok(HttpResponse {
            status: 200,
            headers,
            body: body.into_bytes(),
        })
    }
}

fn client_with_transport(transport: Arc<dyn HttpClient>, ttl: Duration) -> Client {
    Client::builder()
        .base_url("http://127.0.0.1:1/api/v1")
        .api_key("ptn_demo_secret")
        .environment("production")
        .cache_ttl(Duration::from_secs(60))
        .demand_cache_ttl_for_tests(ttl)
        .request_timeout(Duration::from_secs(2))
        .without_disk_cache()
        .http_client(transport)
        .log_sink(|_| {})
        .build()
        .expect("client")
}

fn client_for(server: &StubServer, ttl: Duration) -> Client {
    Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .cache_ttl(Duration::from_secs(60))
        .demand_cache_ttl_for_tests(ttl)
        .request_timeout(Duration::from_secs(2))
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .expect("client")
}

#[test]
fn startup_and_idle_do_not_fetch_configuration() {
    let server = StubServer::start(|_, _| ok_snapshot("Hello {{ name }}"));
    let _client = client_for(&server, Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(server.request_count(), 0);
}

#[test]
fn every_use_case_inside_the_ttl_is_served_from_memory() {
    let server = StubServer::start(|_, _| ok_snapshot("Hello {{ name }}"));
    let client = client_for(&server, Duration::from_secs(60));

    for _ in 0..25 {
        let resolution = client.use_case("greeting").expect("use_case");
        assert_eq!(resolution.source, Source::Remote);
        assert_eq!(resolution.model.as_deref(), Some("openai/gpt-4o-mini"));
    }

    assert_eq!(
        server.request_count(),
        1,
        "the cache TTL must keep the SDK off the network"
    );
    assert_eq!(client.use_case_fetch_count(), 1);
}

#[test]
fn concurrent_same_key_requests_share_one_fetch() {
    let transport = Arc::new(ConcurrentFetchClient::default());
    let client = Arc::new(client_with_transport(
        transport.clone(),
        Duration::from_secs(60),
    ));

    let mut threads = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        threads.push(std::thread::spawn(move || {
            client.use_case("greeting").expect("use_case")
        }));
    }
    for thread in threads {
        let resolved = thread.join().expect("thread");
        assert_eq!(resolved.model.as_deref(), Some("openai/gpt-4o-mini"));
    }

    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(transport.urls().len(), 1);
}

#[test]
fn concurrent_different_keys_fetch_independently() {
    let transport = Arc::new(ConcurrentFetchClient::default());
    let client = Arc::new(client_with_transport(
        transport.clone(),
        Duration::from_secs(60),
    ));

    let greeting = {
        let client = client.clone();
        std::thread::spawn(move || client.use_case("greeting").expect("greeting"))
    };
    let other = {
        let client = client.clone();
        std::thread::spawn(move || client.use_case("demand_other").expect("other"))
    };

    assert_eq!(
        greeting.join().expect("greeting").model.as_deref(),
        Some("openai/gpt-4o-mini")
    );
    assert_eq!(
        other.join().expect("other").model.as_deref(),
        Some("openai/other-model")
    );

    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        transport.max_active(),
        2,
        "different keys should not share a global fetch lock"
    );
}

#[test]
fn another_key_fetch_does_not_mutate_a_fresh_key() {
    let other_doc = document_json("production", "demo", "Hello {{ name }}")
        .replace("\"greeting\"", "\"demand_other\"")
        .replace("openai/gpt-4o-mini", "openai/other-model");
    let server = StubServer::start(move |request, _| {
        if request.path.starts_with("/api/v1/prompts/demand_other?") {
            return StubResponse::json(200, other_doc.clone()).with_header("etag", "\"other\"");
        }
        ok_snapshot("Hello {{ name }}")
    });
    let client = client_for(&server, Duration::from_secs(60));

    let greeting = client.use_case("greeting").expect("greeting");
    let other = client.use_case("demand_other").expect("other");
    let greeting_again = client.use_case("greeting").expect("greeting again");

    assert_eq!(greeting.model.as_deref(), Some("openai/gpt-4o-mini"));
    assert_eq!(greeting_again.model, greeting.model);
    assert_eq!(other.model.as_deref(), Some("openai/other-model"));
}

#[test]
fn a_refresh_after_the_ttl_is_conditional_and_a_304_changes_nothing() {
    let server = StubServer::start(|request, index| {
        if index == 0 {
            return ok_snapshot("Hello {{ name }}");
        }
        assert_eq!(
            request.headers.get("if-none-match").map(String::as_str),
            Some(ETAG),
            "a refresh must be conditional"
        );
        StubResponse::empty(304).with_header("etag", ETAG)
    });

    let client = client_for(&server, Duration::from_millis(80));
    let first = client.use_case("greeting").expect("use_case");
    assert_eq!(first.etag.as_deref(), Some(ETAG));

    std::thread::sleep(Duration::from_millis(400));

    let later = client.use_case("greeting").expect("use_case");
    assert_eq!(later.etag.as_deref(), Some(ETAG));
    assert_eq!(later.source, Source::Remote);
    assert!(
        server.request_count() >= 2,
        "the stale lookup should have refreshed at least once"
    );
    let info = client.use_cases_info();
    assert!(!info.stale, "a 304 confirms the document is current");
    assert_eq!(info.failures, 0);
}

#[test]
fn a_new_document_replaces_the_old_one() {
    let server = StubServer::start(|_, index| {
        if index == 0 {
            ok_snapshot("Hello {{ name }}")
        } else {
            StubResponse::json(200, document_json("production", "demo", "Hi {{ name }}!"))
                .with_header("etag", "\"sha256-2222\"")
        }
    });

    let client = client_for(&server, Duration::from_millis(60));
    assert_eq!(
        rendered(&client),
        "Hello Ada",
        "the first document should be in force"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rendered(&client), "Hi Ada!");
    assert_eq!(
        client.use_cases_info().etag.as_deref(),
        Some("\"sha256-2222\"")
    );
}

fn rendered(client: &Client) -> String {
    let resolution = client.use_case("greeting").expect("use_case");
    resolution
        .messages(serde_json::json!({"name": "Ada"}))
        .expect("render")[0]
        .content
        .clone()
}

#[test]
fn a_429_is_honoured_and_the_caller_never_sees_it() {
    let server = StubServer::start(|_, index| {
        if index == 0 {
            ok_snapshot("Hello {{ name }}")
        } else {
            StubResponse::json(
                429,
                r#"{"error":{"code":"rate_limited","message":"slow down","details":{"retry_after":60}}}"#,
            )
            .with_header("retry-after", "60")
        }
    });

    let client = client_for(&server, Duration::from_millis(50));
    assert!(client.use_case("greeting").is_ok());
    std::thread::sleep(Duration::from_millis(60));

    assert!(
        client.use_case("greeting").is_ok(),
        "the caller sees nothing"
    );
    assert_eq!(
        server.request_count(),
        2,
        "Retry-After must hold the SDK back: one fetch, one 429, then silence"
    );
    let info = client.use_cases_info();
    assert!(info.stale);
    assert_eq!(info.failures, 1);
}

#[test]
fn repeated_5xx_backs_off_and_keeps_serving_the_previous_document() {
    let server = StubServer::start(|_, index| {
        if index == 0 {
            ok_snapshot("Hello {{ name }}")
        } else {
            StubResponse::json(
                500,
                r#"{"error":{"code":"internal_error","message":"boom"}}"#,
            )
        }
    });

    let client = client_for(&server, Duration::from_millis(50));
    assert_eq!(rendered(&client), "Hello Ada");
    std::thread::sleep(Duration::from_millis(60));

    assert_eq!(
        rendered(&client),
        "Hello Ada",
        "the last good document stays"
    );
    let count = server.request_count();
    // 50 ms, then 50, 100, 200, 400 … — a fixed interval would have made ~10 requests by now.
    assert_eq!(count, 2, "one initial fetch and one demand failure");
    assert!(client.use_cases_info().failures >= 1);
}

/// A transport that never answers in time, to prove a timeout is treated like any other failure.
struct TimingOutClient {
    calls: Arc<AtomicUsize>,
}

impl HttpClient for TimingOutClient {
    fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(TransportError::timeout("timed out after 5s"))
    }
}

struct SlowSuccessfulClient {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl HttpClient for SlowSuccessfulClient {
    fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("etag".to_string(), "\"late\"".to_string());
        Ok(HttpResponse {
            status: 200,
            headers,
            body: document_json("production", "demo", "Late {{ name }}").into_bytes(),
        })
    }
}

#[test]
fn a_slow_custom_transport_falls_back_after_one_second_and_cannot_late_overwrite() {
    let dir = support::temp_dir("slow-success");
    let bundle = dir.join("use-cases.production.json");
    std::fs::write(
        &bundle,
        document_json("production", "demo", "Bundle {{ name }}"),
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let client = Client::builder()
        .base_url("http://127.0.0.1:1/api/v1")
        .api_key("ptn_demo_secret")
        .environment("production")
        .bundle(&bundle)
        .without_disk_cache()
        .request_timeout(Duration::from_secs(2))
        .http_client(Arc::new(SlowSuccessfulClient {
            calls: calls.clone(),
            delay: Duration::from_millis(1200),
        }))
        .log_sink(|_| {})
        .build()
        .unwrap();

    let start = std::time::Instant::now();
    assert_eq!(rendered(&client), "Bundle Ada");
    assert!(
        start.elapsed() < Duration::from_millis(1150),
        "runtime fetch should fall back after the 1s SDK budget, elapsed {:?}",
        start.elapsed()
    );

    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(rendered(&client), "Bundle Ada");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct SlowFailingClient {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl HttpClient for SlowFailingClient {
    fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Err(TransportError::timeout("slow failure"))
    }
}

#[test]
fn failed_fetch_gate_is_measured_from_attempt_start() {
    let dir = support::temp_dir("attempt-start");
    let bundle = dir.join("use-cases.production.json");
    std::fs::write(
        &bundle,
        document_json("production", "demo", "Bundle {{ name }}"),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let client = Client::builder()
        .base_url("http://127.0.0.1:1/api/v1")
        .api_key("ptn_demo_secret")
        .environment("production")
        .bundle(&bundle)
        .without_disk_cache()
        .request_timeout(Duration::from_secs(2))
        .demand_cache_ttl_for_tests(Duration::from_millis(100))
        .http_client(Arc::new(SlowFailingClient {
            calls: calls.clone(),
            delay: Duration::from_millis(120),
        }))
        .log_sink(|_| {})
        .build()
        .unwrap();

    assert_eq!(rendered(&client), "Bundle Ada");
    assert_eq!(rendered(&client), "Bundle Ada");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the second attempt should be allowed once attempt_start + gate has passed"
    );
}

#[test]
fn per_key_disk_cache_survives_restart_with_independent_documents() {
    let other_doc = document_json("production", "demo", "Hello {{ name }}")
        .replace("\"greeting\"", "\"demand_other\"")
        .replace("openai/gpt-4o-mini", "openai/other-model");
    let server = StubServer::start(move |request, _| {
        if request.path.starts_with("/api/v1/prompts/demand_other?") {
            return StubResponse::json(200, other_doc.clone()).with_header("etag", "\"other\"");
        }
        ok_snapshot("Hello {{ name }}")
    });
    let dir = support::temp_dir("per-key-disk");
    let disk = dir.join("snapshot.json");
    let first = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .disk_cache_path(&disk)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert_eq!(
        first.use_case("greeting").unwrap().model.as_deref(),
        Some("openai/gpt-4o-mini")
    );
    assert_eq!(
        first.use_case("demand_other").unwrap().model.as_deref(),
        Some("openai/other-model")
    );
    drop(first);

    let restarted = Client::builder()
        .base_url("http://127.0.0.1:1/api/v1")
        .api_key("ptn_demo_secret")
        .environment("production")
        .disk_cache_path(&disk)
        .request_timeout(Duration::from_millis(20))
        .log_sink(|_| {})
        .build()
        .unwrap();
    let greeting = restarted.use_case("greeting").unwrap();
    let other = restarted.use_case("demand_other").unwrap();
    assert_eq!(greeting.source, Source::Disk);
    assert_eq!(other.source, Source::Disk);
    assert_eq!(greeting.model.as_deref(), Some("openai/gpt-4o-mini"));
    assert_eq!(other.model.as_deref(), Some("openai/other-model"));
}

#[test]
fn a_timeout_leaves_the_bundle_in_place_and_never_fails_a_use_case_lookup() {
    let dir = support::temp_dir("bundle");
    let bundle = dir.join("use-cases.production.json");
    std::fs::write(
        &bundle,
        document_json("production", "demo", "Hi {{ name }}"),
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let client = Client::builder()
        .base_url("http://127.0.0.1:1/api/v1")
        .api_key("ptn_demo_secret")
        .environment("production")
        .cache_ttl(Duration::from_secs(60))
        .demand_cache_ttl_for_tests(Duration::from_millis(50))
        .bundle(&bundle)
        .without_disk_cache()
        .http_client(Arc::new(TimingOutClient {
            calls: calls.clone(),
        }))
        .log_sink(|_| {})
        .build()
        .unwrap();

    std::thread::sleep(Duration::from_millis(250));
    let resolution = client.use_case("greeting").expect("the bundle answers");
    assert_eq!(resolution.source, Source::Bundle);
    assert!(calls.load(Ordering::Relaxed) >= 1, "it kept trying");
    assert!(client.use_cases_info().stale);
}

#[test]
fn the_tiers_are_memory_then_disk_then_bundle_then_remote() {
    let dir = support::temp_dir("tiers");
    let disk = dir.join("cache.json");
    let bundle = dir.join("bundle.json");
    std::fs::write(&disk, document_json("production", "demo", "from disk")).unwrap();
    std::fs::write(&bundle, document_json("production", "demo", "from bundle")).unwrap();

    // Disk wins over the bundle.
    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .disk_cache_path(&disk)
        .bundle(&bundle)
        .log_sink(|_| {})
        .build()
        .unwrap();
    let resolution = client.use_case("greeting").unwrap();
    assert_eq!(resolution.source, Source::Disk);
    assert_eq!(resolution.messages(()).unwrap()[0].content, "from disk");

    // With no disk cache the bundle answers.
    std::fs::remove_file(&disk).unwrap();
    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .disk_cache_path(&disk)
        .bundle(&bundle)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert_eq!(client.use_case("greeting").unwrap().source, Source::Bundle);

    // With neither, resolution fails with a clear message and nothing else.
    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    match client.use_case("greeting") {
        Err(Error::NotReady(message)) => {
            assert!(message.contains("production"), "{message}");
        }
        other => panic!("expected NotReady, got {other:?}"),
    }
}

#[test]
fn a_document_for_another_environment_or_project_is_never_used() {
    let dir = support::temp_dir("guard");
    let disk = dir.join("cache.json");

    std::fs::write(&disk, document_json("staging", "demo", "staging text")).unwrap();
    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .disk_cache_path(&disk)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert!(
        matches!(client.use_case("greeting"), Err(Error::NotReady(_))),
        "a staging document must not answer a production process"
    );

    std::fs::write(&disk, document_json("production", "other", "other project")).unwrap();
    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .project("demo")
        .disk_cache_path(&disk)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert!(
        matches!(client.use_case("greeting"), Err(Error::NotReady(_))),
        "another project's document must not answer either"
    );
}

#[test]
fn a_corrupt_or_partial_file_is_ignored_rather_than_fatal() {
    let dir = support::temp_dir("corrupt");
    let disk = dir.join("cache.json");
    std::fs::write(&disk, b"{\"schema_version\": 3, \"use_ca").unwrap();

    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .disk_cache_path(&disk)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert!(matches!(
        client.use_case("greeting"),
        Err(Error::NotReady(_))
    ));

    // A v1/v2 document is refused the same way.
    std::fs::write(&disk, br#"{"schema_version": 2, "use_cases": {}}"#).unwrap();
    let client = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .disk_cache_path(&disk)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert!(matches!(
        client.use_case("greeting"),
        Err(Error::NotReady(_))
    ));
}

#[test]
fn export_writes_the_full_snapshot_to_disk_with_a_sidecar() {
    let dir = support::temp_dir("diskwrite");
    let disk = dir.join("nested").join("snapshot.json");
    let server = StubServer::start(|_, _| ok_snapshot("Hello {{ name }}"));

    let client = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .cache_ttl(Duration::from_secs(60))
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert!(client.use_case("greeting").is_ok());
    client.export_use_cases(&disk).expect("export");

    let written = std::fs::read(&disk).expect("the disk cache was written");
    let document: serde_json::Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(document["environment"], "production");

    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("nested/snapshot.json.meta.json")).unwrap())
            .unwrap();
    assert_eq!(sidecar["etag"], ETAG);
    assert_eq!(sidecar["environment"], "production");
    assert_eq!(sidecar["project"], "demo");

    let leftovers: Vec<_> = std::fs::read_dir(dir.join("nested"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "the temp file must be renamed away");

    // A second client boots from that file with no server at all.
    drop(client);
    let offline = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .disk_cache_path(&disk)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert_eq!(offline.use_case("greeting").unwrap().source, Source::Disk);
}

#[test]
fn refresh_is_noop_and_export_writes_current_document() {
    let dir = support::temp_dir("export");
    let bundle = dir.join("use-cases.production.json");
    let server = StubServer::start(|_, _| ok_snapshot("Hello {{ name }}"));

    let client = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .poll(false)
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();

    client.refresh().expect("refresh is a no-op");
    assert_eq!(server.request_count(), 0);
    assert!(client.use_case("greeting").is_ok());

    client.export_use_cases(&bundle).expect("export");
    assert!(bundle.exists());
    assert!(dir.join("use-cases.production.json.meta.json").exists());

    let from_bundle = Client::builder()
        .mode(Mode::Offline)
        .environment("production")
        .without_disk_cache()
        .bundle(&bundle)
        .log_sink(|_| {})
        .build()
        .unwrap();
    assert_eq!(
        from_bundle.use_case("greeting").unwrap().source,
        Source::Bundle
    );
}

#[test]
fn without_an_api_key_no_remote_call_is_made_and_it_is_said_once() {
    let dir = support::temp_dir("nokey");
    let disk = dir.join("cache.json");
    std::fs::write(&disk, document_json("production", "demo", "from disk")).unwrap();

    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = lines.clone();
    let server = StubServer::start(|_, _| ok_snapshot("Hello {{ name }}"));

    let client = Client::builder()
        .base_url(server.base_url())
        .environment("production")
        .disk_cache_path(&disk)
        .log_sink(move |line| sink.lock().unwrap().push(line.to_string()))
        .build()
        .unwrap();

    assert!(client.use_case("greeting").is_ok());
    assert_eq!(server.request_count(), 0, "no key, no remote calls");
    client.refresh().expect("refresh is a no-op without a key");

    let said = lines.lock().unwrap();
    let mentions: Vec<_> = said
        .iter()
        .filter(|line| line.contains("no PTN_API_KEY"))
        .collect();
    assert_eq!(mentions.len(), 1, "said exactly once: {said:?}");
}

#[test]
fn resolving_is_safe_from_many_threads_at_once() {
    let server = StubServer::start(|_, _| ok_snapshot("Hello {{ name }}"));
    let client = client_for(&server, Duration::from_millis(20));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..200 {
                let resolution = client.use_case("greeting").expect("use_case");
                assert_eq!(
                    resolution.deployment_revision,
                    Some("v2026.09.30-3".to_string())
                );
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn without_the_poller_a_stale_use_case_lookup_refreshes_on_demand() {
    let server = StubServer::start(|_, index| {
        if index == 0 {
            ok_snapshot("Hello {{ name }}")
        } else {
            StubResponse::json(200, document_json("production", "demo", "Hi {{ name }}!"))
                .with_header("etag", "\"sha256-3333\"")
        }
    });

    let client = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_demo_secret")
        .environment("production")
        .cache_ttl(Duration::from_secs(60))
        .demand_cache_ttl_for_tests(Duration::from_millis(40))
        .poll(false)
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .unwrap();

    assert_eq!(rendered(&client), "Hello Ada");
    assert_eq!(
        server.request_count(),
        1,
        "one fetch at start, then nothing"
    );

    std::thread::sleep(Duration::from_millis(80));
    // The stale use_case call performs the refresh before resolving.
    assert_eq!(rendered(&client), "Hi Ada!");
    assert_eq!(
        server.request_count(),
        2,
        "the stale read triggered a demand refresh"
    );
}

#[test]
fn fetch_uses_current_prompts_endpoint_and_prompt_document_shape() {
    let fixture = support::conformance("prompt.json");
    let production = fixture["documents"]["production"].clone();
    let server = StubServer::start(move |request, _| {
        assert!(
            request.path.starts_with("/api/v1/prompts/greeting?"),
            "{}",
            request.path
        );
        StubResponse::json(200, production.to_string()).with_header("etag", ETAG)
    });
    let client = Client::builder()
        .base_url(server.base_url())
        .api_key("ptn_sdkfixture_secret")
        .environment("production")
        .cache_ttl(Duration::from_secs(60))
        .request_timeout(Duration::from_secs(2))
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .expect("client");
    let resolution = client.use_case("greeting").expect("use_case");
    assert_eq!(resolution.model.as_deref(), Some("openai/gpt-4o-mini"));
    assert!(!resolution.prompt_names.is_empty());
}
