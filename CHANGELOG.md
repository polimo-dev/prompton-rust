# Changelog

All notable changes to `prompton-sdk` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 — initial release

The first release of the PromptOn SDK for Rust. It reads snapshot schema version 3.

### Added

- `Client`: one handle per process, cheap to clone, safe to share across threads. Configured with
  `Client::from_env()` or `Client::builder()`, with the precedence explicit option > environment
  variable > default.
- **Snapshot store** with three tiers — memory, an atomically written disk cache with an
  ETag/`Last-Modified` sidecar, and a bundle committed into the app — a 10-second memory cache, a
  background poller that refreshes with `If-None-Match`, `Retry-After` on `429`, and exponential
  backoff (×2 from the cache TTL, capped at five minutes) on `5xx`, timeouts and transport
  failures. A refresh never blocks or fails a generation, and a document for another environment
  or project is never used.
- **Local resolution** exactly as the runtime contract defines it, with `params` and
  `provider_options` layering, prompt selection by name and no fallback to `default`.
- **Template engine**: the Liquid subset PromptOn allows (`for`, `if`/`elsif`/`else`, `unless`,
  `assign`, `break`, `continue`, filters `size`, `join`, `default`), plus `raw` passthrough, a
  static whitelist check (`template::lint`) and detected-variable analysis (`template::variables`).
- **`/resolve` client** (`Client::resolve_remote`) with a per-TTL cache for variable-less calls and
  a fallback to the cached answer while PromptOn is rate-limiting or failing.
- **Monitoring logs**: `Client::log`, `Client::flush` and the `Client::with_generation` wrapper,
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
