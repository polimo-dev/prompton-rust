//! The HTTP transport: a tiny blocking interface the SDK talks to, plus the [`ureq`] client that
//! implements it.
//!
//! The trait exists so tests (and apps with their own connection pool, proxy or instrumentation)
//! can hand the SDK a different transport with [`crate::ClientBuilder::http_client`]. Retries,
//! backoff and caching live above this layer; an implementation only has to perform one request.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// The two verbs the runtime API uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET /use-cases`.
    Get,
    /// `POST /use-cases/{key}/prompt`, `POST /logs`.
    Post,
}

impl Method {
    /// The verb as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One outgoing request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// GET or POST.
    pub method: Method,
    /// The absolute URL, query string included.
    pub url: String,
    /// Headers to send; `authorization` and `user-agent` are already set by the SDK.
    pub headers: Vec<(String, String)>,
    /// The request body, for POST.
    pub body: Option<Vec<u8>>,
    /// How long the whole request may take.
    pub timeout: Duration,
}

/// One response. Status codes are data here — a 404 is not an error at this layer.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The HTTP status.
    pub status: u16,
    /// Response headers with lowercase names.
    pub headers: BTreeMap<String, String>,
    /// The raw response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A header value by lowercase name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The body parsed as JSON, when it is JSON.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// The request never got an answer: DNS, TCP, TLS or a timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// What happened, for the log line.
    pub message: String,
    /// Whether the failure was a timeout.
    pub timeout: bool,
}

impl TransportError {
    /// A transport failure with a message.
    pub fn new(message: impl Into<String>) -> TransportError {
        TransportError {
            message: message.into(),
            timeout: false,
        }
    }

    /// A timeout.
    pub fn timeout(message: impl Into<String>) -> TransportError {
        TransportError {
            message: message.into(),
            timeout: true,
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

/// Performs one HTTP request, blocking until it is done.
pub trait HttpClient: Send + Sync + 'static {
    /// Sends `request` and returns the response, whatever its status.
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The default transport, backed by [`ureq`].
pub struct UreqClient {
    agent: ureq::Agent,
}

impl UreqClient {
    /// A client whose requests time out after `timeout`.
    pub fn new(timeout: Duration) -> UreqClient {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .max_idle_connections_per_host(4)
            .build();
        UreqClient {
            agent: config.into(),
        }
    }
}

impl Default for UreqClient {
    fn default() -> UreqClient {
        UreqClient::new(Duration::from_secs(5))
    }
}

impl fmt::Debug for UreqClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqClient")
    }
}

impl HttpClient for UreqClient {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let result = match request.method {
            Method::Get => {
                let mut builder = self.agent.get(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                builder
                    .config()
                    .timeout_global(Some(request.timeout))
                    .build()
                    .call()
            }
            Method::Post => {
                let mut builder = self.agent.post(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                builder
                    .config()
                    .timeout_global(Some(request.timeout))
                    .build()
                    .send(request.body.unwrap_or_default())
            }
        };

        let mut response = result.map_err(|error| match error {
            ureq::Error::Timeout(_) => TransportError::timeout(error.to_string()),
            other => TransportError::new(other.to_string()),
        })?;

        let status = response.status().as_u16();
        let mut headers = BTreeMap::new();
        for (name, value) in response.headers() {
            if let Ok(value) = value.to_str() {
                headers.insert(name.as_str().to_ascii_lowercase(), value.to_string());
            }
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|error| TransportError::new(error.to_string()))?;

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// A shared transport handle.
pub type SharedHttpClient = Arc<dyn HttpClient>;

/// Parses a `Retry-After` header: either a number of seconds or an HTTP date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let target = parse_http_date(value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(target.saturating_sub(now)))
}

/// Parses an RFC 7231 HTTP date (`Fri, 04 Sep 2026 00:21:48 GMT`) into unix seconds.
pub fn parse_http_date(value: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let value = value.trim();
    let rest = value
        .split_once(", ")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let day: i64 = parts[0].parse().ok()?;
    let month = MONTHS.iter().position(|name| *name == parts[1])? as i64 + 1;
    let year: i64 = parts[2].parse().ok()?;
    let time: Vec<&str> = parts[3].split(':').collect();
    if time.len() != 3 {
        return None;
    }
    let hour: i64 = time[0].parse().ok()?;
    let minute: i64 = time[1].parse().ok()?;
    let second: i64 = time[2].parse().ok()?;

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Howard Hinnant's `days_from_civil`: a proleptic Gregorian date to days since the unix epoch.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after(" 12 "), Some(Duration::from_secs(12)));
    }

    #[test]
    fn parses_http_dates() {
        assert_eq!(
            parse_http_date("Fri, 04 Sep 2026 00:21:48 GMT"),
            Some(1_788_481_308)
        );
        assert_eq!(parse_http_date("nope"), None);
    }
}
