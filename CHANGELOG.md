# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- **Live quota with no extra API call.** Copilot attaches per-SKU quota to every
  response (`x-quota-snapshot-chat`, `-completions`, `-premium_interactions`),
  carrying entitlement, overage, overage-permitted, percent remaining and reset
  date. Those are now parsed on every proxied response and surfaced on:
  - `GET /health` under `quota`
  - `GET /metrics` as `ghc_proxy_quota_percent_remaining`,
    `ghc_proxy_quota_entitlement` and `ghc_proxy_quota_overage`, labelled by `sku`
  - `GET /usage` under `live`, alongside the existing authoritative response

  Previously quota was only available from `/usage`, which costs a separate
  `/copilot_internal/user` request. The headers are undocumented, so a value
  that stops parsing is ignored rather than reported as zero, and the previous
  reading is left in place.
- **Observability for streams and failures.** Records now carry
  `upstream_idle_max_ms` (longest silence between upstream chunks, which is what
  assigns blame for a stall), `keepalive_probes`, `failure_kind`, `session_id`
  parsed from `metadata.user_id`, and `output_tokens_final` so a count that is
  still the `message_start` placeholder renders as `—` instead of being
  presented as fact. The dashboard gained the matching columns, a failures-only
  filter, and premium-request/cache-hit stats.
- `scripts/replay.py` replays a captured request at the upstream or back through
  the proxy and times the SSE stream event by event, with no stall watchdog.
- `upstream_read_timeout_seconds` (default `900`, `0` disables, env
  `GHC_PROXY_UPSTREAM_READ_TIMEOUT`). Bounds the silence *between* reads from an
  upstream response rather than the total duration, so long streaming answers
  are unaffected. Without it a half-open connection yields no data, no error and
  no end-of-stream: the request hangs forever and the stream-interrupted
  handling never runs. The default clears the longest silence measured against
  the real upstream (329.5s, while a tool call's argument JSON was buffered) with
  room to spare.

### Changed
- **`auto_upgrade` now defaults to `true`**, including for config files written
  before the setting existed. The proxy checks GitHub releases on startup and
  replaces its own binary when a newer version is published; the replacement
  takes effect on the next start. Disable with `auto_upgrade: false`,
  `--no-auto-upgrade`, or `GHC_PROXY_AUTO_UPGRADE=0` — worth doing when the
  binary is managed by a package manager, or lives in a build output directory
  that `cargo build`/`cargo clean` also writes to.
- `config_version` bumped to `3`, so existing `config.yaml` files gain
  `upstream_read_timeout_seconds` and the new `auto_upgrade` default on the next
  start.
- **Config schema upgrades apply automatically.** When a release introduces new
  `config.yaml` properties (signalled by a `config_version` bump), the missing
  keys are now filled with their defaults and written back to `config.yaml` on
  the next start, instead of requiring an explicit `--update-config` run.
  Existing values are preserved, and up-to-date files are never rewritten. The
  Opus 4.8 alias backfill is now scoped to pre-v2 files so it cannot overwrite
  customized mappings in current ones

### Fixed
- **Non-2xx upstreams on `/v1/messages` reached the client as an empty `200`
  stream.** `messages_direct` only intercepted `400`; a `401`, `403`, `429` or
  `5xx` fell through and was wrapped in a `200 text/event-stream` whose body was
  a JSON error object, so the client waited on a stream that never produced an
  event and reported a stall instead of the auth or rate-limit failure that
  actually happened. This was the one path Claude Code takes. All five streaming
  paths now gate on a single `is_streamable_status()` predicate
- **A client that disconnected mid-stream left no record.** axum drops the
  response body on disconnect, which drops the generator, so the `store.add`
  after the loop never ran. Recording now happens from `Drop`
- Pre-flight failures (token refresh, rate gate, connect errors) returned early
  without recording; they are captured with a `failure_kind`
- **Keepalive probes could be silenced exactly during a stall.** The boundary
  flag was set from the last chunk, so a TCP split mid-event left it stuck until
  a new chunk arrived — and an upstream that went quiet right then produced no
  probes at all. Partial events are now held back so the downstream always sits
  on an event boundary, and the Anthropic path sends `event: ping` rather than
  an SSE comment, since comments are discarded by the parser and never reset a
  client's idle watchdog
- **Token counts were read from one bucket.** Anthropic's `input_tokens`,
  `cache_read_input_tokens` and `cache_creation_input_tokens` are disjoint, so a
  fully-cached Claude Code turn reported single digits for a 348,483-token
  prompt. The three protocols slice the total differently and now have separate
  extractors; cost reprices the cached buckets instead of charging every input
  token at the full rate
- **Reassembling one large SSE event was quadratic.** The line buffer rescanned
  the whole retained buffer on every chunk, so a single event that arrived in
  many pieces was re-read from the start each time. A 4 MB event delivered in
  4 KB chunks took **7.9 seconds** of pure CPU; the search now resumes where the
  previous one stopped, bringing it to milliseconds. Correctness was never
  affected — only the cost of getting there.
- The line buffer could grow without bound if an upstream never emitted a
  newline. A single line is now capped at 64 MB.
- Stopped scraping the Arch User Repository for the latest VS Code version. The
  AUR maintainers asked projects to stop; `dynamic_vscode_version` now uses
  Microsoft's own `update.code.visualstudio.com` release API and ignores
  non-`major.minor.patch` builds.
- Removed `scripts/__pycache__` from version control and added the matching
  `.gitignore` entries.

## [1.3.0] - 2026-07-27

### Added
- `GET /health` liveness/readiness probe reporting version, uptime, Copilot
  token status and remaining lifetime, loaded model count, requests served, and
  whether API-key auth is enabled. Answers without contacting upstream and is
  never guarded by the API key; `?strict=true` returns `503` when not ready
- `GET /v1/models/{model}` (and `/models/{model}`) OpenAI-compatible single
  model retrieval, including `capabilities` and `supported_endpoints`. Model
  aliases from `model_mappings` are resolved; unknown ids return `404`
- Graceful shutdown on Ctrl-C and `SIGTERM`, draining in-flight requests and SSE
  streams instead of dropping them
- `ghc_proxy_uptime_seconds` gauge on `/metrics`
- `model=` filter on `GET /api/audit`, matching the requested or translated model
- `/health` and `/v1/models/{model}` documented in `openapi.json`
- **SSE keepalive.** Streams emit a `: keepalive` comment after 15s of silence
  so an idle connection is not dropped at the ~60s timeout GitHub's upstream
  load balancer and most intermediaries enforce. Extended thinking can easily
  exceed that with no tokens emitted, which previously surfaced as
  `user_request_timeout` or a stream that died mid-answer. The comment is only
  injected at an event boundary, so the verbatim passthrough paths are never
  spliced mid-event
- **`anthropic-beta` request headers are forwarded.** The client's flags are
  preserved and merged with the ones the proxy derives, instead of being
  overwritten by the 1M-context flag
- Missing `max_tokens` on `/v1/chat/completions` is filled from the model
  catalog's `capabilities.limits.max_output_tokens`, which several Copilot
  models reject the request without
- Requesting a `/responses`-only model on `/v1/chat/completions` now returns an
  actionable `unsupported_api_for_model` error naming the right endpoint,
  instead of an opaque upstream 400

### Fixed
- **`context_management` was silently stripped, disabling Claude Code's context
  editing.** The field was not in the Anthropic passthrough allowlist, and the
  upstream rejects it with a misleading `Extra inputs are not permitted` 400
  unless the `context-management-2025-06-27` beta is requested. It is now
  forwarded and the matching beta flag is derived automatically
- **`tool_result` content arrays were forwarded raw, breaking every MCP tool
  that returns anything but a bare string.** Anthropic allows `content` to be an
  array of blocks, which MCP servers routinely use; passing it through made the
  upstream reject the whole request with
  `type has to be either 'image_url' or 'text'`. Text blocks are now joined and
  image blocks rewritten as `image_url` data URLs
- **Images nested inside a `tool_result` did not set `Copilot-Vision-Request`.**
  `has_image` only inspected top-level content blocks, so screenshots returned
  by MCP tools were sent as non-vision requests
- `image` blocks with a `url` source (rather than `base64`) are now translated
  instead of being turned into an empty data URL
- Models that replaced `max_tokens` with `max_completion_tokens` (`gpt-5.3-codex`
  and friends) previously failed outright; the parameter is renamed and the
  request retried once, on both the streaming and non-streaming paths
- **Stopped scraping the Arch User Repository for the latest VS Code version.**
  The AUR maintainers asked proxies of this kind to stop, having become the
  single most-requested endpoint on their service. `dynamic_vscode_version` now
  uses Microsoft's own `update.code.visualstudio.com` release API and ignores
  non-`major.minor.patch` builds
- The local `/v1/messages/count_tokens` estimate ignored per-message framing and
  tool schemas, under-reporting badly enough that Claude Code compacted too late
  and then hit a hard `prompt token count exceeds the limit` failure. Tool
  definitions are now counted as serialized JSON, plus per-message and
  per-request overhead
- **`/v1/messages/count_tokens` always failed with HTTP 400.** The handler only
  tried the upstream when the catalog advertised `/v1/messages/count_tokens`,
  which Copilot never does, and then returned an error. It now forwards to the
  upstream for any model exposing the native `/v1/messages` surface (returning
  exact counts) and falls back to a local tiktoken estimate — marked
  `"estimated": true` — instead of erroring. This unblocks Claude Code, which
  calls the endpoint before every request
- **Streaming `/v1/responses` returned upstream errors as a `200` SSE body.**
  A 401/429/5xx from upstream was wrapped in a success stream, so clients saw a
  broken response instead of the real failure. Non-2xx upstream responses are
  now surfaced with their status code, matching the other streaming paths
- **Cost estimates were wrong for mapped models and for `gpt-4o`.** Costs were
  computed from the client-supplied model name rather than the model actually
  served, so an alias such as `opus` priced at the fallback rate (~38x too low).
  The `gpt-4o` rate was also unreachable because the broader `gpt-4` arm matched
  first. Rates now key off the translated model, order specific families before
  general ones, strip `publisher/` prefixes for GitHub Models ids, and cover the
  gpt-4.1/gpt-5/o-series/Gemini families
- Dashboard pagination could overflow `usize` (panicking the handler) and an
  unbounded `per_page` forced a full clone of the request store; `page`/`per_page`
  are now parsed with saturating arithmetic and clamped to 500
- `error.log` grew without limit; it is now rotated to `error.log.1` past 8 MB
- The upstream HTTP client had no connect timeout, so a dead upstream could wedge
  a request indefinitely (30s connect timeout, 90s pool idle timeout; no overall
  request timeout so SSE streams are unaffected)
- The model catalog was fetched twice at startup because the periodic refresh
  timer fired immediately on its first tick
- `/metrics` and `/api/audit*` cloned every retained record (including captured
  request/response bodies in debug mode) on each call; aggregation and filtering
  now happen in place under the store lock
- Removed a dead `is_github_models` binding in the translated Anthropic path

#### Streaming integrity
- **Streaming responses could be silently corrupted mid-text.** Every SSE chunk
  was decoded with `String::from_utf8_lossy` as it arrived, but upstream chunks
  do not respect UTF-8 character boundaries. Any multi-byte character (CJK,
  emoji, `—`, box drawing) split across two chunks became `U+FFFD`, and because
  the damaged payload then failed to parse as JSON it was **dropped entirely**,
  deleting text from the middle of a response with no error. The corrupted or
  missing text was then replayed into the next request as conversation history.
  SSE parsing now buffers raw bytes and only decodes complete lines
  (`util::SseLineBuffer`), matching the `TextDecoderStream` + `TextLineStream`
  pipeline the TypeScript `copilot-api` proxy gets from its platform
- **Truncated streams were delivered as if they were complete.** A transport
  error mid-stream only did `break`, so the client received a partial answer
  with no terminator and the request was still recorded as `200`. All five
  streaming paths now detect the interruption, emit a protocol-appropriate
  terminator, and record the request as `502`:
  - OpenAI: `data: {"error": …, "code": "stream_interrupted"}` followed by `[DONE]`
  - Anthropic: `event: error` with an `api_error` payload
  - Responses: `event: error` with `code: "stream_interrupted"`
  - Gemini: final chunk with `finishReason: "OTHER"`
- **Anthropic streams that ended without a `finish_reason` never sent
  `message_stop`.** Claude Code either blocks waiting for it or keeps the
  partial text as a finished assistant turn. `AnthropicStreamState::finish()`
  now closes any open content block and emits `message_delta` + `message_stop`
- A final SSE event arriving without a trailing newline was discarded; the line
  buffer is now flushed when the stream ends
- `/v1/chat/completions` streaming dropped any `data:` payload that failed to
  parse as JSON instead of forwarding it
- `/v1/responses` streaming now also flags a stream that ends before
  `response.completed`
- Direct-Anthropic streaming extracted `stop_reason` and `tools_called` by
  re-scanning the captured response body, so those audit fields were only ever
  populated in debug mode; they are now collected while the stream is parsed
- SSE `data:` parsing accepts `data:{…}` (no space) per the SSE spec, not only
  `data: {…}`

### Changed
- `/api/audit/summary` now rounds cost figures to 4 decimals so small per-request
  costs are no longer reported as `0.0`
- Default `vscode_version` bumped to `1.130.0`
- The release workflow now fails if the tag does not match the `Cargo.toml`
  version. `v1.2.3` shipped with `version = "1.2.2"`, so binaries from that
  release report `1.2.2` and `auto_upgrade` re-downloaded it on every start

## [1.2.3] - 2026-07-20

### Fixed
- `--setup --claudecode` no longer overwrites unrelated Claude Code settings
  ([#24](https://github.com/MartinForReal/ghc-proxy/pull/24))

### Known issue
- Released with `Cargo.toml` still at `1.2.2`, so binaries from this release
  identify themselves as `1.2.2`. With `auto_upgrade` enabled they re-download
  this release on every start. Upgrade to 1.3.0 to clear it.

## [1.2.2] - 2026-07-14

### Added
- Setup wizard now configures the GitHub Models token
  - New "GitHub Models" step lets you enable/disable `publisher/model` routing
  - Checks whether the resolved GitHub token already has models access; if not, guides you to create a fine-grained PAT with the `models: read` permission
  - Validates the pasted token against the GitHub Models catalog before saving it to `github_models.token`
  - Optionally captures an organization to attribute inference to

### Fixed
- Fixed GitHub Device Flow failing with `invalid_scope` (`The scopes requested are invalid: models.`)
  - The Copilot OAuth app does not support a `models` classic OAuth scope, so requesting it broke all authentication
  - Device Flow now requests only the supported `read:user copilot` scopes
  - GitHub Models access now requires a dedicated token with the `models: read` permission (fine-grained PAT) via `github_models.token`; documentation updated accordingly

## [1.2.1] - 2026-07-02

### Fixed
- Fixed `/v1/messages` (Claude Code) incorrectly routing to GitHub Models when model name contained `/`
  - `/v1/messages` uses Anthropic Messages API which is not supported by GitHub Models
  - Now always routes to Copilot upstream regardless of model format
- Fixed `/v1/responses` (Codex) incorrectly routing to GitHub Models
  - Added explicit validation to reject GitHub Models models with error message recommending `/v1/chat/completions`
- Fixed 7 clippy warnings:
  - Collapsed nested `if-let` in `anthropic.rs` (lines 683-688)
  - Replaced `sort_by` with `sort_by_key` in `server.rs` (lines 1838, 1843)
  - Replaced `len() > 0` with `!is_empty()` in `server.rs` (line 1849)
  - Added `#[allow(dead_code)]` for unused utility functions: `is_prompt_cache_eligible`, `extract_prompt_cache_hit`, `filter_tools_by_frequency`

### Updated
- **Documentation**: Updated all docs to reflect current implementation
  - README.md: Added CLI options reference, version numbers, and architecture overview
  - docs/configuration.md: Added `config_version` and `auto_upgrade` fields documentation
  - docs/getting-started.md: Added startup endpoint details
  - docs/api.md: Added metrics, audit, and reload endpoints with adaptive-thinking behavior notes
  - docs/claude-code.md: Clarified API key "add if missing, don't overwrite" behavior

### Performance
- Confirmed model mapping lookups are in-memory with O(log n) BTreeMap performance (~5-10µs per request)
  - No optimization needed; current implementation is production-ready

## [1.2.0] - Previous Release

Previous releases information would go here.
