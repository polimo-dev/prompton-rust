//! Shared test helpers: the conformance fixtures and a stub PromptOn server.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

/// Loads one conformance file.
pub fn conformance(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/conformance")
        .join(name);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("could not parse {}: {error}", path.display()))
}

/// A unique temporary directory for one test.
pub fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "prompton-rust-{}-{name}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One canned response the stub server hands out.
#[derive(Debug, Clone)]
pub struct StubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl StubResponse {
    pub fn json(status: u16, body: impl Into<String>) -> StubResponse {
        StubResponse {
            status,
            headers: vec![(
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> StubResponse {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn empty(status: u16) -> StubResponse {
        StubResponse {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }
}

/// What the stub server saw.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl RecordedRequest {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

type Handler = Arc<dyn Fn(&RecordedRequest, usize) -> StubResponse + Send + Sync>;

/// A one-thread HTTP/1.1 server that answers whatever the test tells it to.
pub struct StubServer {
    pub port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StubServer {
    /// Starts a server answering with `handler(request, request_index)`.
    pub fn start<F>(handler: F) -> StubServer
    where
        F: Fn(&RecordedRequest, usize) -> StubResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));
        let handler: Handler = Arc::new(handler);

        let handle = {
            let requests = requests.clone();
            let running = running.clone();
            std::thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).ok();
                            serve(stream, &requests, &handler);
                        }
                        Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        StubServer {
            port,
            requests,
            running,
            handle: Some(handle),
        }
    }

    /// The base URL to hand the SDK, `/api/v1` included.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/api/v1", self.port)
    }

    /// Everything the server has been asked so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// How many requests the server has answered.
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(stream: TcpStream, requests: &Arc<Mutex<Vec<RecordedRequest>>>, handler: &Handler) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok();
    }

    let request = RecordedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };

    let index = {
        let mut requests = requests.lock().unwrap();
        requests.push(request.clone());
        requests.len() - 1
    };

    let response = handler(&request, index);
    let mut out = stream;
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\n", response.body.len()));
    head.push_str("connection: close\r\n\r\n");
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(response.body.as_bytes());
    let _ = out.flush();
}

/// The `production` use-case document from the use-case conformance suite, whose ids are the ones the
/// golden monitoring-log records were built from.
pub fn greeting_document() -> Value {
    conformance("use_case.json")["documents"]["production"].clone()
}

/// A use-case document for the tests, with one chat use case and one prompt name per language.
pub fn document_json(environment: &str, project: &str, greeting: &str) -> String {
    serde_json::json!({
        "schema_version": 4,
        "project": project,
        "environment": environment,
        "use_cases": {
            "greeting": {
                "id": "0198f2a1-0000-7000-8000-00000000c001",
                "kind": "chat",
                "input_schema": [{"name": "name", "type": "string", "required": true}],
                "default_params": {"max_tokens": 512},
                "payload_policy": {"mode": "full", "sample_rate": 1.0, "max_bytes": 262144,
                                   "retention_days": 30, "encrypt": true}
            }
        },
        "deployments": {
            "greeting": {
                "id": "0198f2a1-0000-7000-8000-00000000d001",
                "revision": "v2026.09.30-3",
                "model_id": "0198f2a1-0000-7000-8000-00000000e001",
                "params": {"temperature": 0.2},
                "provider_options": {},
                "prompt_pins": {"default": "0198f2a1-0000-7000-8000-00000000a001"}
            }
        },
        "prompt_versions": {
            "0198f2a1-0000-7000-8000-00000000a001": {
                "id": "0198f2a1-0000-7000-8000-00000000a001",
                "prompt_id": "0198f2a1-0000-7000-8000-00000000b001",
                "number": 2,
                "engine": "liquid",
                "messages": [{"role": "user", "content": greeting}],
                "text_template": null
            }
        },
        "models": {
            "0198f2a1-0000-7000-8000-00000000e001": {
                "id": "0198f2a1-0000-7000-8000-00000000e001",
                "provider": "openrouter",
                "model_id": "openai/gpt-4o-mini",
                "display_name": "GPT-4o mini",
                "metadata": {},
                "provider_options": {"only": ["OpenAI"]},
                "capabilities": ["tools"],
                "status": "active"
            }
        }
    })
    .to_string()
}
