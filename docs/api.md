---
title: API Reference
---

# API Reference

[Home](index.md) · [Getting Started](getting-started.md) ·
[Configuration](configuration.md) · **API Reference** ·
[Claude Code & Codex](claude-code.md)

---

All endpoints listen on `http://<address>:<port>` (default
`http://127.0.0.1:8314`). By default no API key is required by the proxy itself —
authentication to GitHub Copilot is handled internally. You can optionally
require a key on the LLM endpoints; see [Authentication](#authentication) below.

## Endpoints

| Method & path | Description |
|---------------|-------------|
| `POST /v1/chat/completions` | OpenAI chat completions (also `/chat/completions`) |
| `POST /v1/responses` | OpenAI Responses API for Codex (also `/responses`) |
| `POST /v1/messages` | Anthropic Messages API |
| `POST /v1/messages/count_tokens` | Anthropic token counting (real BPE, local estimate fallback) |
| `POST /v1beta/models/{model}:generateContent` | Gemini generate content |
| `POST /v1beta/models/{model}:streamGenerateContent` | Gemini streaming (SSE) |
| `POST /v1beta/models/{model}:countTokens` | Gemini token counting |
| `POST /v1/embeddings` | Embeddings (also `/embeddings`) |
| `GET /v1/models` | List available models (also `/models`, `/api/models`) |
| `GET /v1/models/{model}` | Retrieve a single model (also `/models/{model}`) |
| `GET /v1/models/full/` | Raw upstream model catalog with capabilities |
| `GET /usage` | Copilot plan and quota usage |
| `GET /health` | Liveness/readiness probe |
| `GET /` | Web analytics dashboard |
| `GET /metrics/dashboard` | Metrics dashboard UI |
| `GET /metrics` | OpenMetrics exposition endpoint |
| `GET /requests` | Request browser |
| `GET /api/stats` | Dashboard statistics (JSON) |
| `GET /api/requests` | Recent requests (JSON) |
| `GET /api/audit` | Filtered audit records |
| `GET /api/audit/summary` | Aggregated audit summary |
| `POST /api/config/reload` | Reload `config.yaml` without restart |
| `GET /openapi.json` | OpenAPI v3 specification of the LLM endpoints |

Streaming (SSE) is supported on the chat, responses, and messages endpoints by
setting `"stream": true` in the request body. The Gemini surface streams via the
dedicated `:streamGenerateContent` action.

## Health check

`GET /health` answers without contacting the upstream, so it is cheap enough for
a service supervisor or container probe to poll frequently. It is never guarded
by the optional API key.

```bash
curl http://127.0.0.1:8314/health
```

```json
{
  "status": "ok",
  "ready": true,
  "version": "1.3.0",
  "uptime_seconds": 128,
  "copilot_token": { "present": true, "expires_in_seconds": 1487 },
  "models_loaded": 77,
  "requests_served": 42,
  "auth_required": false
}
```

`ready` is true once a Copilot token has been obtained **and** the model catalog
has loaded. A degraded proxy still answers `200` with `ready: false` so probes
can distinguish "process alive" from "able to serve traffic". Add `?strict=true`
to get `503 Service Unavailable` instead when the proxy is not ready.

## Retrieve a model

`GET /v1/models/{model}` returns a single catalog entry in the OpenAI shape,
including the raw `capabilities` and `supported_endpoints` reported upstream.
Model aliases from `model_mappings` are resolved, so `/v1/models/opus` returns
the mapped Copilot model. Unknown ids return `404` with an OpenAI-style error
body.

```bash
curl http://127.0.0.1:8314/v1/models/claude-opus-4.8
```

## Token counting

`POST /v1/messages/count_tokens` forwards to the upstream Anthropic
`count_tokens` endpoint for models that expose the native `/v1/messages`
surface, returning exact counts. For every other model (and whenever the
upstream call fails) the proxy falls back to a local tiktoken estimate using the
tokenizer advertised in the model catalog. Estimated responses are marked:

```json
{ "input_tokens": 812, "estimated": true }
```

## OpenAI SDK

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8314/v1", api_key="not-needed")
resp = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(resp.choices[0].message.content)
```

## Anthropic SDK

```python
import anthropic

client = anthropic.Anthropic(base_url="http://127.0.0.1:8314", api_key="not-needed")
msg = client.messages.create(
    model="claude-sonnet-4",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}],
)
print(msg.content)
```

The proxy serves Anthropic requests directly from Copilot's native
`/v1/messages` endpoint when the model supports it, and otherwise translates
them through chat completions transparently.

## Gemini

```bash
curl "http://127.0.0.1:8314/v1beta/models/gemini-2.5-pro:generateContent" \
  -H "Content-Type: application/json" \
  -d '{"contents": [{"role": "user", "parts": [{"text": "Hello!"}]}]}'
```

The model is taken from the URL path and translated per your
[mappings](configuration.md#model-mappings). Gemini requests are translated
through chat completions, so any Copilot model works. Streaming uses the
`:streamGenerateContent` action and emits `data:` SSE lines.

## Authentication

By default the proxy accepts all local requests. Set `api_key` in `config.yaml`
(or `GHC_PROXY_API_KEY`) to require a key on the LLM endpoints. The key is
accepted from any of the standard provider headers and compared in constant time:

```bash
curl http://127.0.0.1:8314/v1/messages          -H "x-api-key: KEY" ...
curl http://127.0.0.1:8314/v1/chat/completions  -H "Authorization: Bearer KEY" ...
curl "http://127.0.0.1:8314/v1beta/models/gemini-2.5-pro:generateContent" -H "x-goog-api-key: KEY" ...
```

The dashboard, metrics, and static pages stay open so local monitoring works
without a key. Unauthenticated requests to protected endpoints return `401`.

## cURL

```bash
# Chat completions
curl http://127.0.0.1:8314/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hello!"}]}'

# List models
curl http://127.0.0.1:8314/v1/models

# Usage / quota
curl http://127.0.0.1:8314/usage
```

## Model discovery

`GET /v1/models` returns the OpenAI-style list. For full capability data —
context-window limits, supported endpoints, vision, tokenizer — use:

```bash
curl http://127.0.0.1:8314/v1/models/full/
```

This is the authoritative source for which models support a 1M-token context
window (those advertising `max_context_window_tokens` greater than 200,000).

## Audit API

`GET /api/audit` returns recent request records with their extracted audit
fields. All filters are optional and combine with AND:

| Parameter | Effect |
|-----------|--------|
| `endpoint` | Substring match on the endpoint path |
| `status` | Exact HTTP status code |
| `tool_name` | Keeps records whose request offered a matching tool |
| `agent` | `true`/`false` — agent- vs user-initiated requests |
| `model` | Substring match on the requested or translated model |
| `page`, `per_page` | Pagination (`per_page` is clamped to 500) |

`GET /api/audit/summary` aggregates the same records into top tools, stop-reason
counts, estimated cost, and prompt-cache hit rate.

## Notable behaviors

- **GitHub Models routing** — when enabled (default), requests whose translated
  model id uses the `publisher/model` form (e.g. `openai/gpt-4o`) are routed to
  the [GitHub Models](https://models.github.ai) inference API instead of Copilot,
  authenticated with a token that has the `models: read` permission.
  See [Configuration](configuration.md#github-models).
- **Model translation** — model names are rewritten per your
  [mappings](configuration.md#model-mappings) before being forwarded.
- **1M context** — for Anthropic-native requests, the proxy forwards the
  `anthropic-beta: context-1m-2025-08-07` header for models whose catalog
  advertises an extended context window.
- **Retry with backoff** — upstream connection errors are retried with
  exponential backoff; retryable upstream HTTP errors are also retried
  (`max_connection_retries`).
- **Orphaned tool-result recovery** — when the upstream rejects a request for an
  orphaned `tool_use_id`, the proxy retries with the offending tool results
  stripped.
- **Adaptive-thinking migration** — when an upstream model rejects
  `thinking.type = "enabled"`, the proxy automatically retries using the
  adaptive format.
- **Upstream errors are never disguised as streams** — a non-2xx upstream
  response on a `"stream": true` request is returned as a normal error response
  with the upstream status code, not as a `200` SSE body.
- **Interrupted streams are reported, not silently truncated** — if the upstream
  connection drops mid-response, the proxy emits a protocol-appropriate
  terminator (`data: {"error": …}` + `[DONE]` for OpenAI, `event: error` for
  Anthropic and Responses, `finishReason: "OTHER"` for Gemini) and records the
  request as `502`. Anthropic streams that end without a `finish_reason` are
  also closed with `message_stop`, so clients never block on a half-open
  message or keep a partial answer as a completed turn.
- **Byte-exact streaming** — SSE parsing buffers raw bytes and decodes only
  complete lines, so multi-byte characters split across network chunks are
  never mangled into `U+FFFD` or dropped.
- **SSE keepalive** — a `: keepalive` comment is emitted after 15 seconds of
  silence so extended thinking does not trip the ~60 second idle timeout
  enforced by the upstream load balancer. Comments are ignored by every
  spec-compliant SSE client.
- **`anthropic-beta` passthrough** — the client's beta flags are forwarded and
  merged with the ones the proxy derives (`context-1m-2025-08-07` for extended
  context models, `context-management-2025-06-27` when the request uses
  `context_management`).
- **MCP tool results** — a `tool_result` whose `content` is an array of blocks
  is normalized before translation: text blocks are joined and image blocks
  become `image_url` data URLs. Images nested there also enable the vision
  header.
- **Parameter migration** — a model that rejects `max_tokens` in favour of
  `max_completion_tokens` is retried once with the renamed parameter, and a
  missing `max_tokens` is filled from the model catalog.
- **Cost estimates use the served model** — the `estimated_cost_usd` field and
  the `ghc_proxy_estimated_cost_usd_total` metric price requests using the model
  actually sent upstream (after translation), not the alias the client asked
  for.
- **Graceful shutdown** — on Ctrl-C (or `SIGTERM` on Unix) the proxy stops
  accepting connections and lets in-flight requests and SSE streams finish.
- **Content filtering** — system-prompt add/remove and tool-result suffix
  removal are applied per your configuration.
