# Changelog

All notable changes to `prompton-sdk` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

## 0.5.1

- Patch source release metadata after the 0.5.0 source tag.
- Replace hex cache-key encoding with an MSRV-compatible clippy-clean loop.

- Retires message-slot expansion. A deployed message with `type: "slot"` now fails with
  `Message slots are not supported; compose conversation history in app code.`
- Documents the app-owned chat flow: render PromptOn-managed messages, append application chat
  history and the current user message, call the provider, and log that final message list.

## 0.5.0

- Runtime prompt configuration is now demand-driven: `Client::use_case` fetches only
  `GET /api/v1/prompts/{key}?environment=...` when that key is missing or stale.
- Startup and idle clients no longer fetch or poll configuration by default. Disk and bundle
  loading remain local-only fallback tiers.
- Config fetches are cached and rate-limited for 10 seconds, share an in-flight same-key request,
  use a 1-second fetch budget, and do not retry. Failures keep serving the last valid value, even
  when expired.
- `Client::refresh` is now a compatibility no-op for runtime clients; normal lookup never falls back to the bulk endpoint. Runtime key fetches persist immutable per-key disk snapshots for restart fallback.

## 0.4.1

- Patch runtime HTTP compatibility with the current server: SDK fetches use `GET /api/v1/prompts`, remote rendering uses `POST /api/v1/prompts/{key}/render`, and monitoring logs send `prompt_key`.

## 0.4.0 — schema 7 tools and trace events

### Added

- Reads deployed documents with schema versions 4 through 7, including `prompts`/`template_pins` aliases from the prompt contract fixture.
- Preserves native chat messages through slots, including `null` and array content, `tool_calls`, `tool_call_id`, and unknown provider fields.
- Merges canonical prompt `tools` into provider params, strips PromptOn-only `output_schema` and `output_examples`, and rejects conflicting legacy params.
- Adds `Client::log_events` for synchronous tool/completion trace events with stable generated `event_id`, `observed_at`, and `sdk`.

## 0.2.0 — vocabulary rename

### Changed

- Replaced the public local-resolution flow with `Client::use_case`,
  `Client::use_case_with`, `UseCase::messages`, `UseCase::text`, and `UseCase::track`.
- Renamed provider-call output to `Result`, with `Result::from_openai` and
  `Result::from_anthropic` helpers for common provider response shapes.
- Renamed monitoring record types to `LogRecord` and `LogError`, changed the runtime log endpoint
  to `POST /api/v1/logs`, and made the batch envelope `logs`.
- Renamed the server prompt endpoint to `POST /api/v1/use-cases/{key}/prompt`, with `params`,
  `provider_options`, `source`, and `prompt_names` in public response/log shapes.
- Raised the deployed use-case document schema to `schema_version: 4` and documented
  `use-cases.production.json` as the bundle filename.
- Regenerated the Rust conformance fixtures as `use_case.json` and `log_record.json`.

### Removed

- Removed old public compatibility names for local lookup, remote prompt rendering, tracking,
  source, provider result, and monitoring records.

## 0.1.0 — initial release

The first release of the PromptOn SDK for Rust. It reads snapshot schema version 3.

### Added

- `Client`: one handle per process, cheap to clone, safe to share across threads. Configured with
  `Client::from_env()` or `Client::builder()`, with the precedence explicit option > environment
  variable > default.
- **Use-case document store** with three tiers — memory, an atomically written disk cache with an
  ETag/`Last-Modified` sidecar, per-key runtime disk snapshots, and a bundle committed into the app — a 10-second memory cache, a
  background poller that refreshes with `If-None-Match`, `Retry-After` on `429`, and exponential
  backoff (×2 from the cache TTL, capped at five minutes) on `5xx`, timeouts and transport
  failures. A refresh never blocks or fails a model call, and a document for another environment
  or project is never used.
- **Local resolution** exactly as the runtime contract defines it, with `params` and
  `provider_options` layering, prompt selection by name and no fallback to `default`.
- **Template engine**: the Liquid subset PromptOn allows (`for`, `if`/`elsif`/`else`, `unless`,
  `assign`, `break`, `continue`, filters `size`, `join`, `default`), plus `raw` passthrough, a
  static whitelist check (`template::lint`) and detected-variable analysis (`template::variables`).
- **Remote prompt client** with a per-TTL cache for variable-less calls and
  a fallback to the cached answer while PromptOn is rate-limiting or failing.
- **Monitoring logs**: `Client::log`, `Client::flush` and the `Client::track` wrapper,
  behind a buffer that batches on size, bytes or time, sends at most 200 records per request, one
  environment per request, retries `429`/`5xx` with the same ids, splits a `413` in half, drops on
  other `4xx`, bounds the queue by dropping the oldest, and drains on shutdown.
- **Payload policy** applied before a record is queued: sampling, `hash` digests, `none`, and the
  contract's truncation arithmetic, UTF-8 safe.
- `StopKind` normalisation, app-generated UUIDv7 ids, a redaction hook and `hash_end_user`.
- **Test mode** (no HTTP, records captured for assertions) and **offline mode** (disk and bundle
  only).
- The cross-language conformance suite (`tests/conformance/`) and an environment-gated live test
  against a running PromptOn server (`tests/live_fixture.rs`).

### Fixed before release

Found by an adversarial review of the first cut of this crate, and fixed before 0.1.0 was tagged.
Each one now has a regression test.

- `error.kind` reached the wire as `http4xx` / `http5xx` instead of the contract's `http_4xx` /
  `http_5xx` (`#[serde(rename_all = "snake_case")]` puts no underscore before a digit), so the
  server rejected every monitoring log for a provider 4xx or 5xx — exactly the records that make
  error rates meaningful. The two variants now spell their wire value out, and
  `every_error_kind_matches_the_field_rule` pins all seven kinds in both directions against the
  conformance fixture. The live test now sends a 503 record as well as a 429.
- UUIDv7 generation used a non-atomic load/store on its RNG state, so concurrent callers could
  derive identical random words and emit duplicate ids. Because `id` is the batch idempotency key,
  those records were silently absorbed by the server as duplicates and lost. The step is now a
  single `fetch_add`, and `ids_are_unique_across_threads` generates 160,000 ids on 8 threads.
- Dropping the last `Client` while a monitoring-log batch was in its retry pause spun at 100% CPU
  until the backoff ladder ran out — over four minutes. Shutdown now neither waits out nor sends
  into an armed `Retry-After`, and gives up after five seconds.

### Changed before release

- `Client::shutdown` and dropping the last `Client` honour an armed `Retry-After` instead of
  sending into it. An explicit `Client::flush()` still sends now — you asked for it — and says so
  in its documentation.
- `LogStats::requests` counts every request the buffer made, not only the ones that succeeded,
  so send volume stays visible during an outage.

### Known deviations from the reference implementation

`tests/conformance/template.json` marks four cases `normative: false`. This SDK matches the
reference on `whitespace_control_renders` (the renderer honours `{%-`/`-%}`, `template::lint`
rejects them) and on `undefined_variable_in_if_condition` (a false condition swallows the
undefined variable). It deviates on two, both asserted in `every_non_normative_case_is_pinned`:

- an unknown filter is a render error rather than being applied — the whitelist is also enforced
  by `template::lint`, so such a template can never reach a use-case document;
- a map rendered into an output position produces compact JSON (`{"a":1}`) rather than Elixir's
  `inspect` output. Never rely on either.
