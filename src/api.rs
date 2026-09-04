//! The three runtime endpoints, one function each.
//!
//! Everything above this module (the store, the buffer) decides *when* to call; this module only
//! knows how to shape one call and how to read the answer, including the two things the retry
//! policies need: the HTTP status and `Retry-After`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::error::Error;
use crate::http::{parse_retry_after, HttpRequest, HttpResponse, Method, SharedHttpClient};

/// The answer to a conditional deployed-use-case document request.
#[derive(Debug, Clone)]
pub(crate) enum UseCaseFetch {
    /// `304`: the document we already hold is current.
    NotModified {
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// `200`: a new document.
    Fetched {
        body: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// One record the server refused, with the rest of the batch still accepted.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RejectedRecord {
    /// The record's position in the batch that was sent.
    pub index: usize,
    /// The record's id, when it had a usable one.
    #[serde(default)]
    pub id: Option<String>,
    /// `invalid_request` or `conflict`.
    #[serde(default)]
    pub code: String,
    /// Why it was refused.
    #[serde(default)]
    pub message: String,
}

/// What `POST /logs` answered.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct LogsAck {
    /// How many records this request stored.
    #[serde(default)]
    pub accepted: usize,
    /// How many ids the project had already stored.
    #[serde(default)]
    pub duplicates: usize,
    /// Per-record failures.
    #[serde(default)]
    pub rejected: Vec<RejectedRecord>,
}

/// A call that did not succeed, with what the retry policies need to decide what to do next.
#[derive(Debug)]
pub(crate) struct RemoteFailure {
    /// The HTTP status, or `None` when the request never got an answer.
    pub status: Option<u16>,
    /// `Retry-After`, from the header or from `error.details.retry_after`.
    pub retry_after: Option<Duration>,
    /// The error to hand the caller.
    pub error: Error,
}

impl RemoteFailure {
    pub fn transport(message: String) -> RemoteFailure {
        RemoteFailure {
            status: None,
            retry_after: None,
            error: Error::Transport(message),
        }
    }

    /// Whether waiting and trying again could help.
    pub fn is_retryable(&self) -> bool {
        match self.status {
            None => true,
            Some(429) => true,
            Some(status) => (500..=599).contains(&status),
        }
    }
}

/// The runtime API client: base URL, key, and one HTTP transport.
pub(crate) struct Api {
    pub base_url: String,
    pub api_key: Option<String>,
    pub user_agent: String,
    pub timeout: Duration,
    pub http: SharedHttpClient,
}

impl Api {
    pub fn snapshot(
        &self,
        environment: &str,
        etag: Option<&str>,
    ) -> Result<UseCaseFetch, RemoteFailure> {
        let mut headers = self.headers(false);
        if let Some(etag) = etag {
            headers.push(("if-none-match".to_string(), etag.to_string()));
        }

        let response = self.send(HttpRequest {
            method: Method::Get,
            url: format!(
                "{}/use-cases?environment={}",
                self.base_url,
                encode(environment)
            ),
            headers,
            body: None,
            timeout: self.timeout,
        })?;

        let etag = response.header("etag").map(str::to_string);
        let last_modified = response.header("last-modified").map(str::to_string);

        match response.status {
            200 => Ok(UseCaseFetch::Fetched {
                body: response.body,
                etag,
                last_modified,
            }),
            304 => Ok(UseCaseFetch::NotModified {
                etag,
                last_modified,
            }),
            _ => Err(self.failure(response)),
        }
    }

    pub fn prompt(&self, use_case: &str, body: &Value) -> Result<Value, RemoteFailure> {
        let response = self.send(HttpRequest {
            method: Method::Post,
            url: format!("{}/use-cases/{}/prompt", self.base_url, encode(use_case)),
            headers: self.headers(true),
            body: Some(serde_json::to_vec(body).unwrap_or_default()),
            timeout: self.timeout,
        })?;

        if response.status == 200 {
            response.json().ok_or_else(|| RemoteFailure {
                status: Some(200),
                retry_after: None,
                error: Error::Transport("prompt returned a body that is not JSON".to_string()),
            })
        } else {
            Err(self.failure(response))
        }
    }

    pub fn post_logs(
        &self,
        environment: &str,
        records: &[Map<String, Value>],
    ) -> Result<LogsAck, RemoteFailure> {
        let body = json!({ "logs": records });
        let response = self.send(HttpRequest {
            method: Method::Post,
            url: format!("{}/logs?environment={}", self.base_url, encode(environment)),
            headers: self.headers(true),
            body: Some(serde_json::to_vec(&body).unwrap_or_default()),
            timeout: self.timeout,
        })?;

        if (200..300).contains(&response.status) {
            Ok(response
                .json()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default())
        } else {
            Err(self.failure(response))
        }
    }

    fn headers(&self, json_body: bool) -> Vec<(String, String)> {
        let mut headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), self.user_agent.clone()),
        ];
        if json_body {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        if let Some(key) = &self.api_key {
            headers.push(("authorization".to_string(), format!("Bearer {key}")));
        }
        headers
    }

    fn send(&self, request: HttpRequest) -> Result<HttpResponse, RemoteFailure> {
        self.http
            .execute(request)
            .map_err(|error| RemoteFailure::transport(error.message))
    }

    fn failure(&self, response: HttpResponse) -> RemoteFailure {
        let body = response.json();
        let error = body
            .as_ref()
            .and_then(|body| body.get("error"))
            .cloned()
            .unwrap_or(Value::Null);

        let code = error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| String::from_utf8_lossy(&response.body).trim().to_string());
        let details = error.get("details").cloned().unwrap_or(Value::Null);

        let retry_after = response
            .header("retry-after")
            .and_then(parse_retry_after)
            .or_else(|| {
                details
                    .get("retry_after")
                    .and_then(Value::as_u64)
                    .map(Duration::from_secs)
            });

        RemoteFailure {
            status: Some(response.status),
            retry_after,
            error: Error::Http {
                status: response.status,
                code,
                message,
                details,
            },
        }
    }
}

/// Percent-encodes a query-string value.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("production"), "production");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn reads_a_rejected_list() {
        let ack: LogsAck = serde_json::from_value(json!({
            "accepted": 1, "duplicates": 0,
            "rejected": [{"index": 0, "id": "x", "code": "invalid_request", "message": "id must be a UUID"}]
        }))
        .unwrap();
        assert_eq!(ack.accepted, 1);
        assert_eq!(ack.rejected[0].index, 0);
    }
}
