//! The payload policy the SDK applies to a monitoring log **before** it is enqueued.
//!
//! The server re-validates every rule here, but the SDK has to apply them first so the raw text
//! never travels over the network when the policy says it should not. The order matters, because
//! the steps interact:
//!
//! 1. the keep decision (sampling; errors and `stop_kind: length` are always kept),
//! 2. wrapping a string `input` as `{"text": …}` and a string `output` as `{"content": …}`,
//! 3. the mode — `none` drops the payload, `hash` replaces it with a digest, `full` truncates,
//! 4. the fixed 2048-byte cap on `error.message`,
//! 5. hashing `end_user_ref` when `hash_end_user` is set,
//! 6. the app's `redact` hook, last.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::sha256;
use crate::snapshot::{PayloadMode, PayloadPolicy};

/// The fixed cap on `error.message`, independent of `max_bytes`.
pub const ERROR_MESSAGE_MAX: usize = 2048;

const SAMPLE_SCALE: u32 = 10_000;

/// A user hook that gets the last word on every record before it is queued.
pub type RedactHook = Arc<dyn Fn(Value) -> Value + Send + Sync>;

/// The SDK-side inputs to the policy that do not come from the use-case document.
#[derive(Clone, Default)]
pub struct PayloadConfig {
    /// The policy used for a use case whose document entry carries none.
    pub defaults: PayloadPolicy,
    /// Send `sha256(end_user_ref)` instead of the raw reference.
    pub hash_end_user: bool,
    /// The app's redaction hook, applied last.
    pub redact: Option<RedactHook>,
}

impl std::fmt::Debug for PayloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayloadConfig")
            .field("defaults", &self.defaults)
            .field("hash_end_user", &self.hash_end_user)
            .field("redact", &self.redact.is_some())
            .finish()
    }
}

/// Applies `policy` (falling back to `config.defaults`) to one monitoring-log record.
pub fn apply(
    record: Map<String, Value>,
    policy: Option<&PayloadPolicy>,
    config: &PayloadConfig,
) -> Map<String, Value> {
    let policy = normalize_policy(policy, &config.defaults);
    let record = apply_mode(record, &policy);
    let record = cap_error_message(record);
    let record = hash_end_user(record, config);
    redact(record, config)
}

/// Merges a use-case document policy with the SDK defaults and clamps the values.
pub fn normalize_policy(policy: Option<&PayloadPolicy>, defaults: &PayloadPolicy) -> PayloadPolicy {
    let mut policy = policy.cloned().unwrap_or_else(|| defaults.clone());
    policy.sample_rate = policy.sample_rate.clamp(0.0, 1.0);
    if policy.max_bytes == 0 {
        policy.max_bytes = defaults.max_bytes;
    }
    policy
}

/// Whether this record's raw text is kept. Errors and truncated answers always are.
pub fn keep(record: &Map<String, Value>, sample_rate: f64) -> bool {
    if record.get("status").and_then(Value::as_str) == Some("error") {
        return true;
    }
    if record.get("stop_kind").and_then(Value::as_str) == Some("length") {
        return true;
    }
    if sample_rate >= 1.0 {
        return true;
    }
    if sample_rate <= 0.0 {
        return false;
    }
    let id = record.get("id").and_then(Value::as_str).unwrap_or("");
    bucket(id) < (sample_rate * f64::from(SAMPLE_SCALE)).round() as u32
}

/// The sampling bucket: the first 4 bytes of `sha256(id)` as an unsigned big-endian integer,
/// modulo 10000. The server computes the same number, so both sides agree without talking.
pub fn bucket(id: &str) -> u32 {
    let digest = sha256::digest(id.as_bytes());
    let head = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    head % SAMPLE_SCALE
}

// ---------------------------------------------------------------------------
// mode

fn apply_mode(mut record: Map<String, Value>, policy: &PayloadPolicy) -> Map<String, Value> {
    if policy.mode == PayloadMode::None || !keep(&record, policy.sample_rate) {
        record.remove("input");
        record.remove("output");
        return record;
    }

    record = wrap_strings(record);
    match policy.mode {
        PayloadMode::Hash => hash_payload(record),
        _ => truncate_payload(record, policy.max_bytes),
    }
}

fn wrap_strings(mut record: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::String(text)) = record.get("input") {
        record.insert("input".to_string(), json!({ "text": text }));
    }
    if let Some(Value::String(content)) = record.get("output") {
        record.insert("output".to_string(), json!({ "content": content }));
    }
    record
}

fn hash_payload(mut record: Map<String, Value>) -> Map<String, Value> {
    for key in ["input", "output"] {
        let value = match record.get(key) {
            None | Some(Value::Null) => continue,
            Some(value) => value.clone(),
        };
        let json = canonical_json(&value);
        record.insert(
            key.to_string(),
            json!({"sha256": sha256::hex(json.as_bytes()), "bytes": json.len(), "hashed": true}),
        );
    }
    record
}

fn truncate_payload(mut record: Map<String, Value>, max_bytes: usize) -> Map<String, Value> {
    if let Some(input) = record.remove("input") {
        if let Some(input) = truncate_input(input, max_bytes) {
            record.insert("input".to_string(), input);
        }
    }
    if let Some(output) = record.remove("output") {
        if let Some(output) = truncate_output(output, max_bytes) {
            record.insert("output".to_string(), output);
        }
    }
    record
}

fn truncate_input(input: Value, max_bytes: usize) -> Option<Value> {
    let mut map = match input {
        Value::Object(map) => map,
        Value::Null => return None,
        other => return Some(other),
    };

    let per_message = (max_bytes / 8).max(64);
    let variable_limit = (max_bytes / 4).max(64);

    let mut truncated = false;
    if let Some(Value::Array(messages)) = map.get("messages").cloned() {
        let (messages, hit) = truncate_messages(Value::Array(messages), per_message, max_bytes);
        truncated |= hit;
        map.insert("messages".to_string(), messages);
    }
    if let Some(Value::String(text)) = map.get("text").cloned() {
        let (text, hit) = truncate_string(&text, max_bytes);
        truncated |= hit;
        map.insert("text".to_string(), Value::String(text));
    }
    if let Some(variables) = map.get("variables").cloned() {
        if !variables.is_null() {
            let json = canonical_json(&variables);
            if json.len() > variable_limit {
                truncated = true;
                map.insert(
                    "variables".to_string(),
                    json!({"truncated": true, "sha256": sha256::hex(json.as_bytes()),
                           "bytes": json.len()}),
                );
            }
        }
    }

    if truncated {
        map.insert("truncated".to_string(), Value::Bool(true));
    }
    Some(Value::Object(map))
}

fn truncate_output(output: Value, max_bytes: usize) -> Option<Value> {
    let mut map = match output {
        Value::Object(map) => map,
        Value::Null => return None,
        other => return Some(other),
    };
    let limit = (max_bytes / 4).max(64);
    let mut truncated = false;

    if let Some(Value::String(content)) = map.get("content").cloned() {
        let (content, hit) = truncate_string(&content, limit);
        truncated |= hit;
        map.insert("content".to_string(), Value::String(content));
    }

    if let Some(Value::Array(calls)) = map.get("tool_calls").cloned() {
        let (calls, hit) = truncate_tool_calls(calls, limit);
        truncated |= hit;
        map.insert("tool_calls".to_string(), Value::Array(calls));
    }

    if truncated {
        map.insert("truncated".to_string(), Value::Bool(true));
    }
    Some(Value::Object(map))
}

fn truncate_messages(messages: Value, per_message: usize, total: usize) -> (Value, bool) {
    let list = match messages {
        Value::Array(list) => list,
        other => return (other, false),
    };

    let mut truncated = false;
    let list: Vec<Value> = list
        .into_iter()
        .map(|message| {
            let (message, hit) = truncate_message(message, per_message);
            truncated |= hit;
            message
        })
        .collect();

    if json_size(&Value::Array(list.clone())) <= total {
        return (Value::Array(list), truncated);
    }
    (Value::Array(fit_messages(list, total)), true)
}

/// The total is over budget: first empty the middle messages into byte-count stubs (the system
/// prompt and the newest turn are always preserved), and only if that is not enough drop the
/// middle entirely and leave one marker message behind.
fn fit_messages(messages: Vec<Value>, limit: usize) -> Vec<Value> {
    let stubbed = stub_middle(&messages, limit);
    if json_size(&Value::Array(stubbed.clone())) <= limit {
        stubbed
    } else {
        drop_middle(messages, limit)
    }
}

fn stub_middle(messages: &[Value], limit: usize) -> Vec<Value> {
    let count = messages.len();
    let mut running = json_size(&Value::Array(messages.to_vec()));
    let mut out = Vec::with_capacity(count);

    for (index, message) in messages.iter().enumerate() {
        if index > 0 && index + 1 < count && running > limit {
            let bytes = message_content_bytes(message);
            let mut stub = message.as_object().cloned().unwrap_or_default();
            stub.insert(
                "content".to_string(),
                Value::String(format!("…[truncated {bytes} bytes]…")),
            );
            stub.insert("truncated".to_string(), Value::Bool(true));
            let stub = Value::Object(stub);
            running = running - json_size(message) + json_size(&stub);
            out.push(stub);
        } else {
            out.push(message.clone());
        }
    }
    out
}

fn drop_middle(messages: Vec<Value>, limit: usize) -> Vec<Value> {
    let mut messages = messages;
    if messages.is_empty() {
        return Vec::new();
    }
    let first = messages.remove(0);
    let rest = messages;

    let marker = |dropped: usize| {
        json!({"role": "system", "content": format!("…[{dropped} messages truncated]…"),
               "truncated": true})
    };

    let base = json_size(&Value::Array(vec![first.clone(), marker(rest.len())]));
    if base <= limit {
        let kept_tail = tail_within(&rest, limit - base);
        let dropped = rest.len() - kept_tail.len();
        let mut out = vec![first, marker(dropped)];
        out.extend(kept_tail);
        return out;
    }

    match shrink_first(&first) {
        Some(smaller) => {
            let mut all = vec![smaller];
            all.extend(rest);
            drop_middle(all, limit)
        }
        None => {
            let marker = marker(rest.len() + 1);
            if json_size(&Value::Array(vec![marker.clone()])) <= limit {
                vec![marker]
            } else {
                Vec::new()
            }
        }
    }
}

/// Halves the first message's content, or strips it down to its role. `None` when there is
/// nothing left to shrink, which is the recursion's stop condition.
fn shrink_first(message: &Value) -> Option<Value> {
    let bytes = message_content_bytes(message);
    if bytes == 0 {
        let mut minimal = Map::new();
        if let Some(role) = message.get("role").cloned() {
            minimal.insert("role".to_string(), role);
        }
        minimal.insert("truncated".to_string(), Value::Bool(true));
        let minimal = Value::Object(minimal);
        return if &minimal == message {
            None
        } else {
            Some(minimal)
        };
    }
    let (smaller, _) = truncate_message(message.clone(), bytes / 2);
    Some(smaller)
}

fn tail_within(messages: &[Value], budget: usize) -> Vec<Value> {
    let mut left = budget;
    let mut kept = Vec::new();
    for message in messages.iter().rev() {
        let size = json_size(message) + 1;
        if size <= left {
            left -= size;
            kept.push(message.clone());
        } else {
            break;
        }
    }
    kept.reverse();
    kept
}

fn truncate_message(message: Value, limit: usize) -> (Value, bool) {
    let mut map = match message {
        Value::Object(map) => map,
        other => return (other, false),
    };

    match map.get("content").cloned() {
        Some(Value::String(content)) => {
            let (content, truncated) = truncate_string(&content, limit);
            map.insert("content".to_string(), Value::String(content));
            if truncated {
                map.insert("truncated".to_string(), Value::Bool(true));
            }
            (Value::Object(map), truncated)
        }
        None | Some(Value::Null) => (Value::Object(map), false),
        Some(other) => {
            let json = canonical_json(&other);
            if json.len() > limit {
                let (content, _) = truncate_string(&json, limit);
                map.insert("content".to_string(), Value::String(content));
                map.insert("truncated".to_string(), Value::Bool(true));
                (Value::Object(map), true)
            } else {
                (Value::Object(map), false)
            }
        }
    }
}

fn message_content_bytes(message: &Value) -> usize {
    match message.get("content") {
        None | Some(Value::Null) => 0,
        Some(Value::String(content)) => content.len(),
        Some(other) => canonical_json(other).len(),
    }
}

fn truncate_tool_calls(calls: Vec<Value>, limit: usize) -> (Vec<Value>, bool) {
    let encoded = Value::Array(calls.clone());
    if json_size(&encoded) <= limit {
        return (calls, false);
    }

    let blanked: Vec<Value> = calls.iter().map(|call| put_arguments(call, "")).collect();
    let overhead = json_size(&Value::Array(blanked));
    let budget = limit.saturating_sub(overhead) / calls.len().max(1);
    (shrink_tool_calls(&calls, budget, limit), true)
}

fn shrink_tool_calls(calls: &[Value], budget: usize, limit: usize) -> Vec<Value> {
    if budget < 32 {
        return vec![json!({"truncated": true, "bytes": json_size(&Value::Array(calls.to_vec()))})];
    }
    let shrunk: Vec<Value> = calls
        .iter()
        .map(|call| match arguments_of(call) {
            Some(arguments) => {
                let (arguments, _) = truncate_string(arguments, budget);
                put_arguments(call, &arguments)
            }
            None => call.clone(),
        })
        .collect();

    if json_size(&Value::Array(shrunk.clone())) <= limit {
        shrunk
    } else {
        shrink_tool_calls(calls, budget / 2, limit)
    }
}

fn arguments_of(call: &Value) -> Option<&str> {
    call.get("function")?.get("arguments")?.as_str()
}

fn put_arguments(call: &Value, arguments: &str) -> Value {
    let mut call_map = match call.as_object() {
        Some(map) => map.clone(),
        None => return call.clone(),
    };
    let mut function = match call_map.get("function").and_then(Value::as_object) {
        Some(function) if function.get("arguments").and_then(Value::as_str).is_some() => {
            function.clone()
        }
        _ => return call.clone(),
    };
    function.insert(
        "arguments".to_string(),
        Value::String(arguments.to_string()),
    );
    call_map.insert("function".to_string(), Value::Object(function));
    Value::Object(call_map)
}

// ---------------------------------------------------------------------------
// strings

/// UTF-8-safe truncation that keeps the head and the tail. The result is never longer than
/// `limit` bytes and never contains a split character.
pub fn truncate_string(input: &str, limit: usize) -> (String, bool) {
    if input.len() <= limit {
        return (input.to_string(), false);
    }

    let marker = format!("\n…[truncated {} bytes]…\n", input.len() - limit);
    if marker.len() > limit {
        return (trim_trailing_partial(&input.as_bytes()[..limit]), true);
    }

    let budget = limit - marker.len();
    let head_len = budget * 6 / 10;
    let tail_len = budget - head_len;
    let bytes = input.as_bytes();

    let head = trim_trailing_partial(&bytes[..head_len]);
    let tail = trim_leading_partial(&bytes[bytes.len() - tail_len..]);
    (format!("{head}{marker}{tail}"), true)
}

fn trim_trailing_partial(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    for _ in 0..4 {
        if let Ok(text) = std::str::from_utf8(&bytes[..end]) {
            return text.to_string();
        }
        if end == 0 {
            break;
        }
        end -= 1;
    }
    String::new()
}

fn trim_leading_partial(bytes: &[u8]) -> String {
    let mut start = 0usize;
    while start < bytes.len() && (0x80..0xC0).contains(&bytes[start]) {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

// ---------------------------------------------------------------------------
// tail of the pipeline

fn cap_error_message(mut record: Map<String, Value>) -> Map<String, Value> {
    let message = record
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(message) = message {
        if message.len() > ERROR_MESSAGE_MAX {
            let (message, _) = truncate_string(&message, ERROR_MESSAGE_MAX);
            if let Some(Value::Object(error)) = record.get_mut("error") {
                error.insert("message".to_string(), Value::String(message));
            }
        }
    }
    record
}

fn hash_end_user(mut record: Map<String, Value>, config: &PayloadConfig) -> Map<String, Value> {
    if !config.hash_end_user {
        return record;
    }
    let reference = match record.get("end_user_ref") {
        None | Some(Value::Null) => return record,
        Some(Value::String(reference)) => reference.clone(),
        Some(other) => other.to_string(),
    };
    record.insert(
        "end_user_ref".to_string(),
        Value::String(sha256::hex(reference.as_bytes())),
    );
    record
}

fn redact(record: Map<String, Value>, config: &PayloadConfig) -> Map<String, Value> {
    let hook = match &config.redact {
        Some(hook) => hook.clone(),
        None => return record,
    };

    let input = Value::Object(record.clone());
    match catch_unwind(AssertUnwindSafe(|| hook(input))) {
        Ok(Value::Object(redacted)) => redacted,
        // A hook that returns something other than an object, or panics, must not leak the raw
        // text: drop the payload and keep the narrow record.
        _ => {
            let mut record = record;
            record.remove("input");
            record.remove("output");
            record
        }
    }
}

/// The canonical JSON encoding used for every size and digest in this module: no whitespace and
/// object keys in sorted order, which is what the server hashes too.
pub fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn json_size(value: &Value) -> usize {
    canonical_json(value).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn wraps_string_payloads() {
        let out = apply(
            record(json!({"id": "a", "status": "ok", "input": "hi", "output": "yo"})),
            Some(&PayloadPolicy::default()),
            &PayloadConfig::default(),
        );
        assert_eq!(out["input"], json!({"text": "hi"}));
        assert_eq!(out["output"], json!({"content": "yo"}));
    }

    #[test]
    fn keeps_errors_at_a_zero_sample_rate() {
        let policy = PayloadPolicy {
            sample_rate: 0.0,
            ..PayloadPolicy::default()
        };
        let out = apply(
            record(json!({"id": "a", "status": "error", "input": {"text": "hi"}})),
            Some(&policy),
            &PayloadConfig::default(),
        );
        assert!(out.contains_key("input"));

        let out = apply(
            record(json!({"id": "a", "status": "ok", "input": {"text": "hi"}})),
            Some(&policy),
            &PayloadConfig::default(),
        );
        assert!(!out.contains_key("input"));
    }

    #[test]
    fn truncation_is_utf8_safe_and_within_the_cap() {
        let text = "한글".repeat(200);
        let (out, truncated) = truncate_string(&text, 64);
        assert!(truncated);
        assert!(out.len() <= 64, "{} bytes", out.len());
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn redact_runs_last_and_a_panic_drops_the_payload() {
        let config = PayloadConfig {
            redact: Some(Arc::new(|_| panic!("boom"))),
            ..PayloadConfig::default()
        };
        let out = apply(
            record(json!({"id": "a", "status": "ok", "input": {"text": "secret"}})),
            None,
            &config,
        );
        assert!(!out.contains_key("input"));
    }
}
