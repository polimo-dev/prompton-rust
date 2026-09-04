# prompton-sdk — the PromptOn SDK for Rust

PromptOn is the control plane for your app's LLM prompts. Every place your code calls a model
becomes a **use case**, and for each use case and environment PromptOn holds one **pin**: a prompt
version, one model, and its parameters. Your app fetches that configuration and then calls the
provider **itself**, with your own key and your own HTTP client.

PromptOn is config-fetch, **not a proxy**. It is never in the request path, it never sees your
provider key, and if it is down your app keeps running on the last snapshot it received. After each
call your app sends back a **monitoring log**, and those logs are how you see cost, latency, error
rate and stop reasons per use case.

```text
resolve("greeting")            → which model, which params, which prompt   (from memory, no HTTP)
render(vars)                   → the messages to send
with_generation(…, || …)       → your provider call, timed and logged
```

## Install

The crate is not on crates.io yet, so depend on the repository:

```toml
[dependencies]
prompton-sdk = { git = "https://github.com/polimo-dev/prompton-rust", branch = "main" }
```

Once it is published, the line becomes `prompton-sdk = "0.1"`. The crate is `prompton-sdk`; the
library you import is `prompton`. Rust 1.85 or newer.

## Quick start

```rust
use prompton::{CallMeta, Client, Completion, Outcome};

let prompton = Client::from_env()?;                                  // PTN_HOST, PTN_API_KEY
let call = prompton.resolve("greeting")?;                            // model, params, prompt
let messages = call.render_messages(serde_json::json!({"name": "Ada"}))?;
let answer = prompton.with_generation(&call, CallMeta::new().input_messages(messages.clone()), || {
    let text = my_provider.chat(&call.model, &messages);             // your key, your client
    Ok(Completion::new(text.clone(), Outcome::text(text).with_finish_reason("stop")))
})?;
```

Get a runtime key with the [PromptOn CLI](https://github.com/polimo-dev/prompton-cli):
`prompton api-keys issue`. Then `export PTN_API_KEY=ptn_<project>_…` and, if you are not on the
hosted app, `export PTN_HOST=https://prompton.example`.

`cargo run --example quickstart` runs the whole flow — with a real server if `PTN_API_KEY` is set,
and on an inline snapshot if it is not.

### Using it from an async runtime

`resolve`, `render`, `log` and `with_generation` (around your own async-free closure) do **no**
network I/O of their own: they read memory and queue work, so they are safe to call straight from an
async task. `flush`, `refresh`, `resolve_remote` and `export_snapshot` block, so call them from
`tokio::task::spawn_blocking` or on a thread of your own. Polling and sending happen on the SDK's
own two threads and never touch your runtime.

## Configuration

Every option follows **explicit option > environment variable > default**.

| Option (`Client::builder()`) | Environment | Default | What it does |
|---|---|---|---|
| `.host(url)` | `PTN_HOST` | `https://app.prompton.ai` | The PromptOn app. The SDK appends `/api/v1` |
| `.base_url(url)` | `PTN_BASE_URL` | — | The full API base, when a gateway does not sit at the root |
| `.api_key(key)` | `PTN_API_KEY` | none | `ptn_<project_slug>_…`. Without it the SDK makes **no** remote calls and serves the disk cache and bundle only, saying so once |
| `.environment(name)` | `PTN_ENVIRONMENT` | `production` | Which environment this process reads |
| `.project(slug)` | `PTN_PROJECT` | from the key | Names the default disk-cache file and guards against another project's snapshot |
| `.cache_ttl(duration)` | — | 10 s | How long a snapshot is served from memory before a refresh; also the base of the failure backoff |
| `.request_timeout(duration)` | — | 5 s | Per-request timeout |
| `.disk_cache_path(path)` / `.without_disk_cache()` | — | OS cache dir | `<cache>/prompton/<project>.<environment>.snapshot.json`, written atomically with a `.meta.json` sidecar |
| `.bundle(path)` | — | none | A snapshot file shipped inside the app, used when memory and disk are empty |
| `.mode(Mode::Live \| Offline \| Test)` | — | `Live` | `Offline` never touches the network; `Test` also captures records in memory |
| `.poll(bool)` | — | `true` | The background poller. With it off, a refresh is triggered by the next resolve after the TTL |
| `.fetch_on_start(bool)` | — | `true` | A cold start with nothing cached fetches once, synchronously, before `build()` returns |
| `.log_config(LogConfig { … })` | — | 2 s / 100 records / 1 MB / 10 000 queued / 8 attempts | The monitoring-log buffer's triggers and limits |
| `.payload_defaults(policy)` | — | `full`, 1.0, 256 KB | The payload policy for a use case whose snapshot entry carries none |
| `.hash_end_user(bool)` | — | `false` | Send `sha256(end_user_ref)` instead of the raw reference |
| `.redact(fn)` | — | none | A hook that gets the last word on every record before it is queued |
| `.http_client(client)` | — | `ureq` | Any `HttpClient`: a proxy, an instrumented client, a test double |
| `.log_sink(fn)` | — | stderr | Where the SDK's own log lines go |

## Resilience — what happens when PromptOn is down

The single most important behaviour of this SDK: **a generation never fails because PromptOn did.**
Configuration goes stale in the worst case, never absent.

- **Poll, do not fetch per request.** `GET /snapshot` with `If-None-Match` every 10 seconds by
  default. A `304` costs nothing, so a short interval is cheap. Every `resolve` inside the TTL is
  answered from memory with no HTTP call at all.
- **Refresh in the background.** A refresh never blocks a generation, and while one is in flight —
  or after it fails — the previous document keeps serving.
- **Rate limits and failures.** On `429` the SDK waits out `Retry-After` (falling back to
  `error.details.retry_after`, then to backoff) and does not contact the server before it has
  elapsed. On `5xx`, timeouts and transport failures it backs off exponentially, ×2 from the cache
  TTL up to five minutes. The caller sees none of this.
- **Three tiers, in order: memory → disk → bundle → remote.** The disk cache is written atomically
  (temp file, then rename) with a sidecar holding the ETag and `Last-Modified`; several processes
  on one host may share it, a reader tolerates a concurrent rename, and a corrupt or partial file is
  ignored rather than fatal. The bundle is a snapshot committed into the repository
  (`client.export_snapshot("snapshot.production.json")`, refreshed by your build) — in a serverless
  or scale-to-zero runtime it is the primary cold-start fallback, not a nicety.
- **No external services, ever.** No database, no Redis, no shared store: memory, one local file and
  the bundled file are the only tiers. Instances never coordinate; each keeps its own copy, which
  ETag polling makes cheap.
- **The environment and project guards.** A snapshot for another environment or another project is
  never used, wherever it came from — a `staging` process must not boot on a `production` bundle.
- **Monitoring logs never block a generation.** They are batched, retried, bounded, and dropped with
  a counter rather than allowed to grow without limit.

**Prove it before you ship**: run your app with `PTN_HOST` pointed somewhere unreachable and confirm
that generations still happen on the cached snapshot.

## How it fails

| when | what you see |
|---|---|
| a poll times out, or answers `5xx` or `429` | nothing: the previous snapshot keeps serving, and the SDK says so on its log sink |
| PromptOn is down at start | the disk cache, then the bundle, answer `resolve`; `resolution_source` records which |
| nothing is cached anywhere | `Error::NotReady` — the only error worth retrying |
| the use case is not in the snapshot | `Error::UnknownUseCase` |
| the use case has no live deployment here | `Error::Unresolved` — a bug in the deployment, **never** a reason to fall back to a hard-coded prompt |
| the prompt name is not pinned | `Error::UnknownPrompt { available_prompts, … }` — there is no silent fallback to `default` |
| a variable the template needs is missing | `Error::Template(TemplateError::MissingVariable(name))`, with `error.missing_variable()` |
| a monitoring log cannot be sent | it is retried with the same ids, then dropped and counted in `client.log_stats()` |
| the queue is full | the **oldest** records are dropped and counted |

## Monitoring logs

`with_generation` times your provider call, builds the record and queues it. `log` takes a record
you built yourself (after a streaming call, say), and `flush` sends the queue now and waits — call
it before a short-lived process exits.

```rust
let result = prompton.with_generation(
    call,
    CallMeta::new()
        .variables(serde_json::json!({"name": "Ada"}))
        .input_messages(messages)
        .end_user_ref("user-42")
        .trace_id("job:8842")
        .context(serde_json::json!({"language": "en", "plan": "pro"})),
    || match call_the_provider() {
        Ok(text) => Ok(Completion::new(
            text.clone(),
            Outcome::text(text)
                .with_finish_reason("stop")
                .with_usage(Usage::tokens(38, 9).with_cost(0.000112, CostSource::Provider)),
        )),
        Err(status) => Err(CallFailure::http(status, "the provider refused")),
    },
);
prompton.flush()?;
```

A record carries what your app did, and nothing it should not — **never log secrets**: no provider
keys, no `PTN_API_KEY`, no user PII beyond `end_user_ref`.

| field | filled by | notes |
|---|---|---|
| `id` | the SDK | a UUIDv7, generated before the call; the idempotency key. A resend is counted as a duplicate, never stored twice |
| `use_case`, `model`, `status`, `started_at` | the SDK | the four required fields besides `id` |
| `kind` | the resolution | `chat`, `text` or `embedding` |
| `deployment_id`, `deployment_revision`, `prompt`, `prompt_version_id` | the resolution | which pin produced this call |
| `resolution_source` | the store | `remote`, `disk`, `bundle` or `manual` |
| `provider`, `model_used`, `upstream_provider` | the resolution and your outcome | who actually served it |
| `params` | the resolution (plus `CallMeta::params`) | what the call was made with |
| `input` | `CallMeta` | `{variables, messages}` or `{text}`, truncated by the payload policy |
| `output` | your `Outcome` | `{content, tool_calls}`, truncated by the payload policy |
| `finish_reason`, `stop_kind` | your `Outcome` | `stop_kind` is normalised: `stop`, `length`, `tool_call`, `content_filter`, `other`. Only `length` means the answer was cut off |
| `error` | your `CallFailure` | `kind` is one of `http_4xx`, `http_5xx`, `rate_limited`, `timeout`, `transport`, `parse`, `app`; the message is capped at 2 KB |
| `usage` | your `Outcome` | `input_tokens`, `output_tokens`, `cost_usd`, `cost_source`, `raw` |
| `latency_ms` | the wrapper | wall-clock duration of your closure |
| `trace_id`, `sequence`, `end_user_ref` | `CallMeta` | correlation. `end_user_ref` can be hashed with `hash_end_user` |
| `context`, `metadata` | `CallMeta` | free-form, ≤ 2 KB and ≤ 4 KB or the server rejects the record |
| `sdk` | the SDK | `{"name": "prompton-rust", "version": …}` |

Failures matter as much as successes: error rates and truncation rates are meaningless without
them, so send `status: "error"` records too.

### The payload policy

Before a record is queued the SDK applies the use case's `payload_policy` from the snapshot, so raw
text never travels when the policy says it should not: the keep decision (sampling on a hash of the
id; errors and `stop_kind: length` are always kept), then `none` (drop `input`/`output`), `hash`
(replace them with `{sha256, bytes}`) or `full` (truncate to the contract's limits, UTF-8 safe),
then the 2 KB cap on `error.message`, then `hash_end_user`, then your `redact` hook, last.

## Testing your app

```rust
use prompton::{Client, Mode};

let prompton = Client::builder().mode(Mode::Test).without_disk_cache().build()?;
prompton.set_snapshot(&my_test_snapshot())?;

// … exercise the code under test …

let logged = prompton.captured_logs();
assert_eq!(logged[0]["status"], "ok");
assert_eq!(logged[0]["input"]["variables"]["name"], "Ada");
```

`Mode::Test` makes no HTTP calls and captures every record for assertions; `Mode::Offline` also
makes no HTTP calls but resolves from the disk cache and the bundle, which is what you want in CI.

## Conformance

`tests/conformance/` is the cross-language contract every PromptOn SDK reproduces: template
rendering, resolution, payload truncation, `stop_kind` normalisation and golden monitoring-log
records. `cargo test` executes every case. `tests/live_fixture.rs` runs the same SDK against a real
server and is skipped unless `PTN_API_KEY` is set:

```sh
cargo test
PTN_HOST=http://localhost:4000 PTN_API_KEY=ptn_sdkfixture_… cargo test --test live_fixture
```

## License

Copyright 2026 Polimo. Licensed under the Apache License, Version 2.0 — see [LICENSE](LICENSE).

PromptOn is a trademark of Polimo. The license does not grant permission to use the PromptOn name or
logo; forks and derived services must use a different name.
