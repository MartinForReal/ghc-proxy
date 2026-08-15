# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Fixed
- **`sonnet` now maps to Sonnet, not Opus.** The default model mappings folded
  every Claude spelling — Sonnet included — into the newest Opus. That is
  right for aliases actually named after a Claude version Copilot dropped, but
  `sonnet` and every `claude-sonnet-*` spelling now resolve to `claude-sonnet-5`
  instead, so a caller that asked for the mid tier gets its cost and rate
  limits rather than being silently upgraded to Opus.

### Removed
- Removed GitHub Models routing, catalog merging, token configuration, and setup
  now that the GitHub Models inference service has been retired. All model
  requests now use GitHub Copilot.

## [1.4.3] - 2026-08-09

### Added
- **Codex discovers Copilot context windows automatically.** Requests carrying
  Codex's `client_version` query receive its native model-catalog schema, with
  `context_window` and `max_context_window` taken from Copilot. Automatic setup
  now uses command-backed local authentication so Codex refreshes that catalog,
  and removes the stale global `model_context_window` override.

## [1.4.2] - 2026-08-09

### Added
- **The `[1m]` context variant works on any model.** Claude Desktop's
  `supports1m` offers a 1M-context variant beside the standard one and names it
  `<id>[1m]` on the wire. That suffix was only understood through a hand-written
  table of aliases, so any id missing from it answered 404 —
  `claude-haiku-4.5[1m]` and `sonnet[1m]` among them. The suffix is now stripped
  generically, but only after the configured mappings have been tried on the
  full id, so a hand-written `[1m]` entry still wins.

  The suffix also means something now. `context-1m-2025-08-07` used to be
  derived from the catalog alone, which put *every* request on the
  extended-context tier and left the standard variant with nothing to
  distinguish it. It is opt-in through the variant, still gated on the model
  actually advertising the window.
- `GET /v1/models` reports `context_window`, `max_output_tokens` and
  `supports_1m_context`, so an operator can see which ids are worth marking
  `supports1m` without fetching each model separately.
- **An opt-in one-hour prompt-cache TTL.** A `cache_control` breakpoint with no
  `ttl` gets the five-minute tier, which is short enough to be self-defeating on
  long turns: the entry is written during prefill, so a turn that itself runs
  longer than five minutes has already outlived its own cache by the time it
  finishes, and the next turn pays a full cold prefill of the whole
  conversation. Observed live — a 341s turn left a 353s gap to the next, whose
  prefix was byte-identical, and it still read zero cached tokens and
  re-prefilled 223K.

  `extend_cache_ttl` fills in `ttl = "1h"` where the client left it unset, and
  leaves an explicit `ttl` exactly as sent. Off by default: extended writes bill
  at a higher rate while reads cost the same, so on a conversation doing many
  small incremental writes between rare expiries it costs more than it saves.

### Fixed
- **Streams truncated during tool-argument silence.** Copilot withholds
  `input_json_delta` until a tool call's argument JSON is complete, so a healthy
  stream can go minutes without a byte. The upstream client set no TCP or HTTP/2
  keepalive, leaving the socket completely idle in that window — long enough for
  a load balancer, NAT or corporate middlebox to reap it. The truncated body
  reached Claude Code as "Server error mid-response". Two captured failures on
  `claude-opus-5` died at the same point after 55.8s and 85.5s of silence; the
  900s read timeout was never reached.

  The error source chain is now recorded too, since reqwest renders a dropped
  body as the fixed string "error decoding response body" and every distinct
  cause looked identical in the logs.
- **Upstream errors on the Anthropic surface arrived in OpenAI's shape.**
  `{"error": {"message": ...}}` carries neither the top-level `"type": "error"`
  nor the `error.type` Anthropic clients match on, so a well-formed rejection
  looked like a malformed response to the SDK. Seen when Claude Code's WebSearch
  subagent sends `web_search_20250305`, which Copilot does not implement. The
  status is mapped onto the matching Anthropic error type with the upstream
  message kept; an upstream already speaking Anthropic is forwarded untouched,
  and the OpenAI, Responses and embeddings paths still pass through verbatim.
- **The Gemini CLI setup produced a configuration that could not work.**
  `GOOGLE_GEMINI_BASE_URL` carried an API version, but the Gen AI SDK appends
  its own, so every request went to `/v1beta/v1beta/models/...` and 404ed before
  reaching a handler. The default model written was `gemini-2.5-pro`, which
  Copilot has never served, and there were no `gemini-*` mappings at all. A
  catch-all `gemini-` prefix now resolves to the pro tier, with the flash
  spellings listed separately so longest-match keeps a flash request on a flash
  model rather than silently upgrading it. Config version 5 seeds these into
  existing files, leaving any id the user already pointed somewhere untouched.
- **`count_tokens` stalled under a rate limit.** It inherited the shared retry
  helper, so a 429 cost up to 14s of backoff before falling through to the local
  estimate it would have produced immediately — and clients call it before every
  turn. It now issues a single request, and a 429 pauses upstream counting for
  60s instead of re-asking a limiter that has already refused.
- **The generated Codex config could not start.** `model_context_window` was
  never written, so Codex budgeted from its own table sized for OpenAI's public
  limits while Copilot serves the same slugs with different windows (1,050,000
  for `gpt-5.5`, 264,000 for `gpt-5-mini`); it is now read from the live catalog
  for whichever model ends up selected. An explicitly chosen `model` was
  overwritten on every run, and is now left alone. The proxy's `api_key` was not
  passed at all — and naming it through `env_key` does not work, because Codex
  fails a turn when the named variable is unset and the key can come from
  `config.yaml` with nothing ever exported, so it is written as a static
  `Authorization` header instead.

### Changed
- The four full-id `[1m]` alias entries were dropped from the default model
  table: prefix matching already resolved them and generic suffix stripping
  covers the rest. The bare aliases stay — stripping `4-8[1m]` leaves `4-8`,
  which nothing else maps. Existing installs are unaffected, since the
  migrations only insert keys and never remove them.

## [1.4.1] - 2026-08-03

### Fixed
- **Quota panel reported a spent allowance as untouched.** The per-response
  quota header carries a percentage rounded to a tenth, and the dashboard
  rendered it with `toFixed(0)` — so 99.5% displayed as "100%", beside a raw
  `10000000` entitlement. On a token-billed plan that tenth of a percent is
  10,000 AI units, so the panel could not show consumption at all.

  The panel now reads the absolute counts from `/usage` (refreshed every 60s,
  since unlike the rest of the page it costs an upstream call) and falls back to
  the header percentage — now at one decimal — when that call fails. Uncapped
  SKUs no longer draw a full bar, which claimed "100% left" for something that
  cannot run out.
- `GET /usage` now surfaces each SKU's `credits_used`. `entitlement - remaining`
  does not reproduce it: measured on a live enterprise seat that subtraction
  gave 43,774 against a reported 44,553.

### Added
- **The quota panel names the account it is reporting on**, reading
  `MartinForReal · enterprise · token-billed · resets 2026/9/1`. A quota figure
  with no account beside it is unreadable the moment more than one token is in
  play, and `token-billed` is what says the entitlement counts AI units rather
  than interactions. Hovering gives the time the upstream reported the figures,
  since they are polled rather than live. `GET /usage` gained the `login`,
  `token_based_billing` and `as_of` fields these come from.

## [1.4.0] - 2026-07-29

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
- **The Responses API over WebSocket.** Eight models advertise `ws:/responses`
  in the catalog — six of them alongside `/responses` and nothing else — and the
  proxy had no way to reach it. `GET /v1/responses` with an upgrade now exposes
  that transport, relayed to the matching upstream socket.

  The surface is undocumented — GitHub publishes no reference for the inference
  API at all, and the token response carries no websocket URL — so the protocol
  was established by probing: it lives on the same host as the HTTP API, speaks
  the same `response.*` events one per text frame, and takes a *flat* request
  frame with `model` at the top level. Nesting the body under `response` is
  rejected, as is omitting `type`. `scripts/ws_probe.py` and
  `scripts/ws_explore.py` are the tools that established this, kept for when it
  changes.

  A model that does not advertise the transport is refused with an error frame
  naming the alternative, rather than left waiting on the socket. Refusals and
  malformed frames are recorded with a `failure_kind` like every other path, so
  a failed attempt leaves something to diagnose from. WebSocket turns appear in
  the dashboard under `ws:/responses`, and their transcript renders through the
  same reassembly as SSE — only the transport differs.
- `scripts/ws_check.py` sweeps every model the catalog says supports the
  transport, discovering the list rather than hard-coding it, and asserts the
  event vocabulary, monotonic sequence numbers, termination, text and usage.
- `scripts/replay.py` replays a captured request at the upstream or back through
  the proxy and times the SSE stream event by event, with no stall watchdog.
- `scripts/protocol_check.py` asserts protocol conformance against a running
  proxy. The property that matters for a translating gateway is not "did it
  return 200" but "did it return the shape the *client* asked for", so every
  check is an assertion about the client-facing response: a Gemini caller must
  receive `candidates`, never the `choices` the upstream actually sent. Covers
  all four surfaces in both streaming and non-streaming modes, tool calls and
  tool-result round trips, stream terminators (`message_stop`, `[DONE]`,
  `response.completed`), token counting, and the error paths — an unknown model
  must not arrive as a `200` SSE stream that never produces an event.
- **What Copilot actually billed, per request.** Every response carries
  `copilot_usage.total_nano_aiu` — the AI units the turn cost — alongside a
  `token_details` breakdown giving the per-token rate for each token type
  charged. The total was verified against those rates on five responses. It is
  now recorded on every request across all six paths (four protocols ×
  streamed/not, plus WebSocket), totalled in `/api/stats` as `total_nano_aiu`,
  and is the dashboard's headline figure, replacing an estimate derived from
  published list prices that could not know a model is included at no charge.
  Reasoning tokens are recorded beside it.
- **Prompt-cache statistics** at `GET /api/cache` and on the dashboard: where
  input tokens came from (served from cache / written to cache / fresh), the
  disposition of recent requests, and a per-model hit rate with its own
  distribution bar. A single global number cannot tell you *which* conversation
  stopped matching its prefix, which is the failure that quietly multiplies the
  bill. What the cache was worth is reported as `saved_nano_aiu`, net of the
  write premium, because caching is not free — see below for where the rates
  come from.
- **Body capture can be toggled at runtime**, from the request browser or via
  `POST /api/config/debug`. It previously required restarting the proxy with
  `--debug`, by which point the request you wanted to inspect was gone; the flag
  is read live, so it applies from the next call. It is not written back to
  `config.yaml`, since capture puts prompts and any credentials they carry into
  memory and the log and should lapse on restart. `GET /health` now reports it.
  The `/api/config/` routes are guarded by the API key when one is configured —
  read-only dashboard endpoints stay open, but turning on body capture must not
  be something an unauthenticated caller can do.
- `upstream_read_timeout_seconds` (default `900`, `0` disables, env
  `GHC_PROXY_UPSTREAM_READ_TIMEOUT`). Bounds the silence *between* reads from an
  upstream response rather than the total duration, so long streaming answers
  are unaffected. Without it a half-open connection yields no data, no error and
  no end-of-stream: the request hangs forever and the stream-interrupted
  handling never runs. The default clears the longest silence measured against
  the real upstream (329.5s, while a tool call's argument JSON was buffered) with
  room to spare.

### Changed
- **The cache panel is per model, and says why a cell is empty.** The panel
  opened with one aggregate stacked bar, which cannot say *which* model stopped
  matching — the question a collapsing hit rate raises. The bar is now a
  Distribution column, one per model, and what remains at the top is the colour
  key those bars need. Below the table, a footnote separates the two reasons a
  row comes back empty: a prompt too short to be eligible at all, and one long
  enough but never sent before, since nothing can be read back on first sight.

  **`Written` only appears when something was written.** Only the Anthropic
  surface with an explicit `cache_control` marker ever bills a cache write.
  Every other surface caches implicitly: the first call reads nothing, the next
  reads the whole prefix back, and no write is billed. Measured across twelve
  calls with a 27k-token prefix, exactly one reported a write. The column and
  its swatch are hidden outright on a workload that never writes, rather than
  showing a wall of zeros for an event that cannot happen.

  Measured while investigating: `claude-haiku-4.5` cached a 6902-token prefix
  but not a 4082-token one, so a prompt in the low thousands legitimately
  produces an empty row. Every surface — chat, messages, responses, Gemini and
  WebSocket, streamed and not — was confirmed to carry
  `copilot_usage.token_details`, so an empty row is never the proxy failing to
  read what upstream reported.
- **Cache savings come from the model's own rates, not a price list.** The
  per-model saving was `list price × (1 − 0.1)` for reads and
  `× (1.25 − 1)` for writes — one hard-coded discount applied to every model.
  Copilot states its actual per-token rate for every token type it charges, in
  `copilot_usage.token_details`, and the entries are not the same on every
  model: `claude-haiku-4.5` prices cache writes above its input rate,
  `gemini-3.5-flash` prices them at zero, `gpt-5.5` does not price them at all,
  and `gpt-4o-mini` prices nothing because Copilot includes it. The old maths
  claimed a saving on models that are free.

  `/api/cache` now reports `saved_nano_aiu` in the same AI units the rest of
  the dashboard bills in, computed per request from the rates that request
  reported, and drops `saved_usd` / `write_premium_usd` / `net_saved_usd`.

  A `null` `saved_nano_aiu` now means no response reported rates to compute one
  from, distinct from `0`, which is a real figure: nothing was cached, so
  nothing was saved.
- **Failed attempts no longer count as consumption.** `request_count` counted
  every record, so a burst of rejected calls inflated the request total, added
  an empty-named row to the per-model cache table, and dragged every cache
  disposition toward "uncached" — a rate computed over requests that consumed
  nothing. Statistics now cover requests that produced an answer, `/api/stats`
  reports `failed_requests` separately, and the overview links to them. Token
  and billing totals still count either way: a stream cut off partway consumed
  what it consumed, and hiding that would understate the bill.

  The predicate — non-2xx, or a `failure_kind` on an otherwise successful
  status — now has one definition shared by the statistics, the failures-only
  filter and the dashboard, instead of three.
- **Dashboard restructured around consumption.** The landing page opened with
  eight identically-weighted stat cards followed by the full 78-row model
  catalogue, so the number that tracks spend had no more prominence than
  `bytes_received`, and reference data dominated a page you open to check usage.
  It now leads with what Copilot billed in AI units, input tokens (with the
  cache-hit share) and output tokens (with the reasoning share); quota bars per
  SKU sit directly beneath, since that is the same quantity seen from the other
  end. Traffic, proxy health and the prompt-cache panel follow, with the model
  catalogue folded away and the request list on its own tab.
  - The three pages share one stylesheet at `/app.css` instead of three drifting
    inline copies, and carry the same persistent nav with a live readiness and
    version indicator — previously each page offered only a lone
    `← Dashboard` link.
  - **Wide screens are used.** The old 1180px cap left a 2560px monitor half
    empty while the twelve-column request table still scrolled sideways. Width
    is now per-page: the request table gets 2000px and the metric list 1600px,
    since both are data a wide screen genuinely helps you read, while the
    overview keeps a 1280px measure because a handful of large numbers only
    drift apart when stretched. Message text keeps a 100ch measure so prose
    stays readable at full width; tool payloads are code and stay unconstrained.
  - `/health` data (per-SKU quota, Copilot token expiry, uptime, readiness,
    models loaded) was reachable but shown nowhere; it is now on the overview.
    The request list stays on its own tab — the overview only reports how many
    requests failed and links across.
  - **Debug bodies render as a conversation.** `--debug` stores request and
    response bodies verbatim, which is the right thing to store and the wrong
    thing to read: finding what the model actually said meant scrolling past
    tool schemas, content-filter blocks and, for streams, every SSE frame. The
    detail view now reconstructs the exchange — system/user/assistant turns,
    tool calls and results as collapsible cards, token and stop-reason chips —
    and reassembles streamed fragments into the message they describe. All
    three wire formats are handled, and none of them agree: Anthropic numbers
    content blocks and tags every delta, chat completions hides tool arguments
    inside `choices[].delta.tool_calls[]`, and the Responses API uses a flat
    `response.*` vocabulary where the delta is a bare string. Reasoning is
    picked up under every name the families use (`reasoning_text`,
    `reasoning_content`, `response.reasoning_summary_text.delta`) rather than
    dropped, and a turn that produced no visible output says whether it ran out
    of token budget instead of reporting an empty stream. The raw bodies remain
    one click away on a `Raw` tab.
  - A third tab shows what the wire actually carried, which is the question when
    a stream stalls, repeats, or ends somewhere unexpected. For a stream it
    lists **every SSE frame** — sequence number, event name and a one-line gist,
    each expandable to the full payload — so the `[DONE]` sentinel, a missing
    `message_stop` or a duplicated index is visible instead of buried in a
    scroll of raw text. For a non-streaming completion it lists **every
    top-level field**, including the ones the conversation view has no place for
    (`content_filter_results`, `prompt_filter_results`, `system_fingerprint`,
    `service_tier`, `copilot_usage`).
  - The request table's full nanosecond ISO timestamp consumed a third of the
    row width; it renders as local wall-clock time with the exact value in the
    tooltip, and the table scrolls horizontally rather than letting the panel
    clip columns off the right edge, with the expanded detail pinned to the left
    so it stays readable while the table scrolls. Empty results say so instead
    of showing a header row with nothing under it.
  - **Each request now says what happened, not just what was said.** The
    response section opened with a row of bare chips: the numbers were there,
    but not their meaning. It now leads with the outcome, normalized across
    surfaces and explained — `length`, `max_tokens`, `max_output_tokens` and
    `MAX_TOKENS` are the same event on four different APIs, and all four mean
    the answer on screen is not the whole answer. Endings are colour-coded by
    whether they are normal (`end_turn`, `stop`, `completed`), truncated, or a
    refusal or filter block.

    Beneath it sit the facts each surface actually reports, omitted when
    absent: reasoning effort, service tier, prompt-cache retention, response
    verbosity and output item types on the Responses API; the backend build
    fingerprint on chat completions, which is worth noticing because a change
    there can shift results for identical input; cache-write TTL split and
    inference region on Anthropic; rejected speculative tokens, which are
    billed. Content-filter verdicts appear only when something was actually
    flagged — an all-clear on four categories on every request is noise.
  - The `Probes` column is gone. Keepalive only runs on the Anthropic streaming
    path, so four of the five request paths rendered a dash that read as "zero
    probes sent" when it meant "not measured here". The count now rides in the
    `Idle` tooltip, which is where it was useful anyway — the pair is what
    assigns blame for a stall.
  - **Protocol-specific parameters moved out of the list and into the detail.**
    The three surfaces agree on very little, so a table column for any of them
    is structurally empty for the other two: `New` (cache writes) only exists on
    Anthropic, and `Session` only when the client encodes one into
    `metadata.user_id` the way Claude Code does. Both columns are gone; the
    detail now carries a panel naming the surface and listing what that request
    actually set — `top_k`, `stop_sequences`, `cache_control` marks and
    `thinking` for Anthropic; `seed`, penalties, `response_format` and
    `logprobs` for chat completions; `reasoning.effort`, `max_output_tokens`,
    `store` and `truncation` for the Responses API; `generationConfig` and
    `safetySettings` for Gemini. Absent parameters are omitted rather than shown
    as blanks. A Gemini request is labelled `Gemini → OpenAI Chat Completions
    (translated)`, because the proxy rewrites it before forwarding and the
    recorded body is the rewritten one — `topK` and `safetySettings` have no
    counterpart and are dropped, which the panel says instead of leaving the
    reader to assume they took effect. Gemini's `contents`/`parts`,
    `functionCall` and `functionResponse` shapes render like every other
    surface's.
- **`auto_upgrade` now defaults to `true`**, including for config files written
  before the setting existed. The proxy checks GitHub releases on startup and
  replaces its own binary when a newer version is published; the replacement
  takes effect on the next start. Disable with `auto_upgrade: false`,
  `--no-auto-upgrade`, or `GHC_PROXY_AUTO_UPGRADE=0` — worth doing when the
  binary is managed by a package manager, or lives in a build output directory
  that `cargo build`/`cargo clean` also writes to.
- **Default model mappings point at Opus 5 and Sonnet 5.** The catalog now
  carries `claude-opus-5` and `claude-sonnet-5`; both are generally available and
  match `claude-opus-4.8` on every published capability — 1M context, 64k
  output, billing multiplier 1, vision. `claude-opus-5` is the new built-in
  target, and the table gained the `opus5` / `5[1m]` aliases along with the
  `claude-opus-5*`, `claude-sonnet-5*` and `claude-sonnet-4.6` prefixes.

  **Existing config files are only added to, never rewritten.** A mapping
  already on disk keeps pointing where it was told to, even when it holds what
  used to be the built-in default. A value equal to an old default is
  indistinguishable from a version pinned on purpose, and pinning is common —
  a config in the wild carried `claude-opus-4-7: claude-opus-4.7` beside
  `haiku: claude-opus-4.7`, both of which a "lift stale defaults" pass would
  silently retarget. Run `--setup`, or `--default`, to adopt the new defaults.
- `config_version` bumped to `4`, so existing `config.yaml` files gain the new
  aliases on the next start.
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
- **Recorded request and response sizes on `/v1/messages` measured the wrong
  bytes, and cost a full serialisation each to get.** Both paths built a byte
  count by serialising the whole JSON tree —
  `serde_json::to_vec(&current).map(|v| v.len())` — inside their four-attempt
  retry loops, and the response was serialised twice more: once for a debug log
  that discards it unless logging is on, once to measure it.

  The numbers were also not the ones every other endpoint records.
  `request_size` measured the proxy's version of the request, after system
  prompt injection, the tool-result suffix and the model rename, so the
  dashboard's "Sent" total was adding two definitions together. `response_size`
  measured a re-serialisation of the parsed tree, which differs from the wire
  bytes in whitespace, key order and number formatting.

  Both now follow the pattern the rest of the file already used: the client's
  `body.len()` taken once, and the response read as text once, then measured,
  logged and parsed from that single copy. A 103-byte request returning 865
  bytes now records exactly 103 and 865.
- **Output token totals were not comparable across surfaces.** The two
  conventions for reasoning tokens disagree about whether they are already
  counted: a Responses turn reports `input 11 + output 17 == total 28` with 10
  of that output being reasoning, while the translated Gemini surface reports
  `prompt 5 + completion 1 + reasoning 97 == total 103`. Taken at face value the
  dashboard showed a reasoning share of 262%. `output_tokens` is now normalized
  to the true total on both, the way `input_tokens` already was.
- **Gemini requests lost parameters that had exact counterparts.** The
  translation to chat completions carried `temperature`, `topP`,
  `maxOutputTokens` and `stopSequences` and dropped the rest of
  `generationConfig` in silence, so a client's `seed`, `candidateCount`,
  `presencePenalty`, `frequencyPenalty` or `responseMimeType` had no effect and
  nothing said why. They now map to `seed`, `n`, `presence_penalty`,
  `frequency_penalty` and `response_format` (with `responseSchema` carried over
  as a `json_schema` format). `topK` and `safetySettings` genuinely have no
  counterpart in the chat completions schema and are still dropped — the
  dashboard names them rather than describing the loss in general terms, and a
  test asserts they are not invented.
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
