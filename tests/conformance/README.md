# PromptOn SDK conformance suite

These JSON files are the cross-language contract for PromptOn SDKs. Every SDK — Python, Node.js,
Go, Ruby, Java, Kotlin, Rust — copies this directory into its own test suite and asserts that it
reproduces the expected values byte for byte. When two SDKs disagree about how a prompt renders,
which model a snapshot resolves to, or how a monitoring log is truncated, an app that talks to
PromptOn from two languages gets two different answers. That is what these files prevent.

Nothing here is hand-written. `scripts/gen_conformance.exs` in this repository executes the Elixir
SDK and writes down what it actually produced:

```
mix run scripts/gen_conformance.exs
```

The Elixir SDK is the reference implementation, and the PromptOn server reuses its pure modules
(`SnapshotData`, `Resolver`, `Template`, `StopKind`) directly, so these fixtures also describe the
server. `test/prompton_sdk/conformance_test.exs` runs every case back through the SDK, which is
what keeps the files from drifting.

Each file records the commit it was generated from in `generated_from.commit`.

| File | What it pins down | Cases |
|---|---|---|
| `template.json` | Prompt rendering: the Liquid subset PromptOn allows | 72 render (4 non-normative), 10 lint, 5 detected-variables |
| `resolve.json` | Snapshot + use case (+ prompt name) → model, params, prompt version, rendered messages | 3 snapshots, 15 cases |
| `truncation.json` | The payload policy the SDK applies to a monitoring log before sending it | 19 cases + 5 sampling buckets |
| `stop_kind.json` | Provider `finish_reason` → PromptOn `stop_kind` | 22 cases |
| `generation_record.json` | Complete monitoring-log records and the batch envelope | 5 records |

## How to run the cases

Each file is a JSON object with a `cases` array. Load it, run the operation the file names, compare
with `expect`. Compare the whole structure, not field by field: an extra or missing key is a
failure. Map key order is not significant; array order is.

A case carrying `"normative": false` is reference behaviour that other SDKs need not reproduce; the
`note` says why. Everything else is required.

### template.json

For each case, render `template` with `variables` using the engine named in `engine`:

* `expect.output` — rendering succeeds and returns exactly that string.
* `expect.error` — rendering fails with that category. `missing_variable` also carries the
  reported `variable` name.

`lint_cases` exercise the static whitelist check (`expect.lint` is `"ok"`, or `"error"` plus the
`reasons` in order). `variables_cases` exercise "which input variables does this template read".
Both are optional for an SDK that only renders: the server rejects a non-conforming template when
the prompt version is committed, so a template that fails lint can never reach a snapshot.

### resolve.json

`snapshots` is a map of reference name → a complete schema-v3 snapshot document, exactly as
`GET /api/v1/snapshot?environment=…` returns it. For each case, decode
`snapshots[snapshot_ref]`, resolve `use_case` with the optional `prompt` name, and — when
`variables` is present — render the resulting prompt. This is precisely what `POST /api/v1/resolve`
does on the server.

### truncation.json

For each case, apply the payload policy in `policy` (and the SDK config in `config`, when present)
to `generation`, and compare with `expect.generation`.

### stop_kind.json

For each case, normalise `finish_reason` and compare with `stop_kind`; `truncated` is what your
"was the output cut off" helper must return.

### generation_record.json

These are golden shapes rather than executable cases: `records[].record` is a complete monitoring
log as `POST /api/v1/generations` accepts it, and `batch_envelope.request` wraps all of them in one
batch. Use them to check your record builder's output shape and your batch envelope. `field_rules`
lists what the server validates.

**A generation id must be a UUIDv7, not a UUIDv4.** The request-level validation accepts any UUID
string, but the column is a UUIDv7 type and a v4 id fails on write — the record comes back in
`rejected` with `record could not be stored`, which does not say why. Generate 48 bits of unix
milliseconds, the version nibble 7, then random bits.

**`started_at` in these records is a fixed timestamp.** The server rejects a record whose
`started_at` is more than 5 minutes in the future or more than 7 days in the past, so replace it
with a fresh value before posting these to a live server.

## The exact semantics

### Missing variable

A variable is *missing* when the key is absent from the variables map. A key that is present with a
`null` value is **not** missing: it renders as the empty string, and the `default` filter replaces
it. This is the distinction that trips people up:

| Variables | `{{ x }}` | `{{ x \| default: "fb" }}` |
|---|---|---|
| `{}` | error `missing_variable` | error `missing_variable` |
| `{"x": null}` | `""` | `"fb"` |
| `{"x": ""}` | `""` | `"fb"` |
| `{"x": "v"}` | `"v"` | `"v"` |

A missing variable is an error at **output positions** (`{{ … }}`), in a `for` enumerable, in an
`unless` condition, and as an `assign` source. It is **not** checked in a branch that does not
execute — `{% if mode == "a" %}{{ only_for_a }}{% endif %}` with `mode = "b"` renders empty and
raises nothing. For nested access the reported name is the dotted path: `{{ user.name }}` against
`{"user": {}}` reports `user.name`.

### Value rendering

Strings render as they are, with **no HTML escaping**. Integers render without a decimal point,
floats with one (`2.0` renders as `"2.0"`). `true`/`false` render as those words. `null` renders as
the empty string. A list renders as its elements concatenated with no separator, which is the
Liquid rule and almost never what you want — use `join`. Rendering a map into an output position is
unspecified; do not do it.

### The allowed subset

Tags: `for` (with `else`, `break`, `continue`, and `forloop.*`), `if`/`elsif`/`else`, `unless`,
`assign`. Filters: `size`, `join`, `default`. Everything else — `include`, `capture`, `case`,
`raw`, `comment`, `cycle`, `render`, `tablerow`, `increment`, `liquid` — is a parse error, and
whitespace control (`{%-`, `-%}`, `{{-`, `-}}`) is rejected by lint.

The filter whitelist is enforced by lint and by the server at commit time, **not** by the renderer.
The reference implementation happily applies `upcase` at render time. An SDK that raises instead is
equally correct, because such a template can never reach a snapshot.

`engine: "raw"` returns the source verbatim without parsing it. It exists for prompts whose text
genuinely contains `{{` or `{%`.

### Merge order

```
effective_params            = use_case.default_params  <- deployment.params
effective_provider_options  = model.provider_options   <- deployment.provider_options
```

Both are **shallow** merges where the right side wins. A nested map on the right replaces the left
side whole; it is not merged into it. An override value of `null` is kept as `null`, not deleted —
apps rely on sending `"only": null` to clear a provider restriction.

### Resolution

A deployment revision is a **pin**, not a router. It is one model plus one pinned prompt version
per prompt name. At request time the only selection axis is the prompt name (default `"default"`),
and the environment is a request parameter that decides which snapshot you fetched.

* Unknown use case key → `unknown_use_case`.
* Use case exists but has no deployment in this environment → `unresolved`.
* The deployment pins no version under the requested name → `unknown_prompt`. **There is no
  fallback to `"default"`**: shipping English to a request that asked for `"ko"` is worse than an
  error. The expectation carries `available_prompts`, which is what the server's 404 lists.
* Use case of kind `embedding` has no prompt at all: `prompt` and `prompt_version` are `null`, and
  a prompt name passed with the request is ignored rather than rejected.
* If the snapshot references a prompt version or model id it does not contain, resolution still
  succeeds with those fields `null` and a warning. A healthy server never emits such a snapshot.

### Truncation arithmetic

`max_bytes` comes from the use case's `payload_policy` and defaults to **262144**. Everything else
is derived from it. Sizes are byte counts: a message `content` is measured on the string, and
everything else on its JSON encoding with no whitespace.

| Field | Cap |
|---|---|
| one message's `content` | `max(max_bytes / 8, 64)` |
| `input.messages` (whole list, JSON) | `max_bytes` |
| `input.text` | `max_bytes` |
| `input.variables` (JSON) | `max(max_bytes / 4, 64)` |
| `output.content` | `max(max_bytes / 4, 64)` |
| `output.tool_calls` (JSON) | `max(max_bytes / 4, 64)` |
| `error.message` | 2048, fixed, independent of `max_bytes` |

A string over its cap keeps its head and tail and loses the middle:

```
<head>\n…[truncated N bytes]…\n<tail>
```

where `N = original_bytes - limit`. The budget left after the marker is split 60% head / 40% tail,
then each side is trimmed back to a UTF-8 character boundary, so the result is never longer than
the cap and never contains a split multi-byte character. Every map that lost bytes gets
`"truncated": true`.

`input.variables` is never partially cut. Over its cap the whole map is replaced by
`{"truncated": true, "sha256": <hex>, "bytes": <n>}`.

`input.messages` over the total cap is handled in two steps. First the middle messages are emptied
into `…[truncated N bytes]…` stubs from the front, stopping as soon as the list fits — the first
message (the system prompt) and the last message (the newest turn) are always preserved, so a later
middle message can survive intact. If stubbing is not enough, the middle is dropped entirely and
replaced by one `…[N messages truncated]…` marker message, with as many original messages from the
tail as still fit.

`output.tool_calls` over its cap shrinks each call's `arguments` string to an equal share of the
remaining budget, halving that share until the encoded list fits. If the share would fall below 32
bytes the whole list is replaced by `[{"truncated": true, "bytes": <n>}]`.

Ordering matters, because the steps interact: keep-decision, then string wrapping, then the mode
(`none` / `hash` / `full`), then the `error.message` cap, then `end_user_ref` hashing, then the
app's `redact` hook last.

### Sampling

```
bucket(id) = first 4 bytes of sha256(id), read as an unsigned big-endian 32-bit integer, mod 10000
keep       = bucket(id) < round(sample_rate * 10000)
```

The decision is a pure function of the record id, so a resend makes the same decision and the
server reaches the same answer independently. Records with `status == "error"` and records with
`stop_kind == "length"` are kept regardless of the rate — an error you cannot see is worse than a
storage bill, and a truncated answer is the one you most need the text of.

### Payload modes

* `full` — truncate as above.
* `hash` — replace `input` and `output` with `{"sha256": <hex>, "bytes": <n>, "hashed": true}`. The
  digest is taken over the canonical JSON of the value **after** string wrapping, so a string input
  hashes `{"text":"…"}` and not the bare string. The server recognises this wrapper and stores the
  hash without ever seeing the text.
* `none` — drop `input` and `output` entirely. The narrow record still goes.

### stop_kind

| Raw `finish_reason` | `stop_kind` |
|---|---|
| `stop`, `end_turn`, `stop_sequence` | `stop` |
| `length`, `max_tokens` | `length` |
| `tool_calls`, `tool_use`, `tool_call` | `tool_call` |
| `content_filter` | `content_filter` |
| anything else, empty, or absent | `other` |

Comparison lowercases and trims, so Google's `STOP` and `MAX_TOKENS` map correctly. Normalisation
is idempotent: feeding a `stop_kind` back in returns itself, which matters because the server
re-normalises whatever the client sent.

Two traps. Google's `SAFETY` and `RECITATION` map to `other`, not `content_filter` — only the
literal string `content_filter` lands there. And `tool_calls` is **not** a truncation: only
`length` sets `truncated`, and the truncation rate, the evaluator and the alerts all depend on
that.

### Server-side field caps

`truncation.json` has a `server_field_caps` section for two rules the SDK does **not** apply. The
ingest endpoint blanks `params` over 4 KB and `usage.raw` over 16 KB rather than rejecting the
record, and appends the field name to `metadata.truncated_fields`. By contrast `context` over 2 KB
and `metadata` over 4 KB reject the whole record. Do not try to shrink `params` or `usage.raw` in
an SDK; do keep `context` and `metadata` small.

## Source

Generated from the PromptOn Elixir SDK, <https://github.com/polimo-dev/prompton-elixir>. The exact
commit is in `generated_from.commit` in each file, and `generated_from.sdk_version` is the SDK
version those expectations came from.
