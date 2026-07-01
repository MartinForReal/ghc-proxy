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
| `POST /v1/messages/count_tokens` | Anthropic token counting (real BPE) |
| `POST /v1beta/models/{model}:generateContent` | Gemini generate content |
| `POST /v1beta/models/{model}:streamGenerateContent` | Gemini streaming (SSE) |
| `POST /v1beta/models/{model}:countTokens` | Gemini token counting |
| `POST /v1/embeddings` | Embeddings (also `/embeddings`) |
| `GET /v1/models` | List available models (also `/models`, `/api/models`) |
| `GET /v1/models/full/` | Raw upstream model catalog with capabilities |
| `GET /usage` | Copilot plan and quota usage |
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

## Notable behaviors

- **GitHub Models routing** — when enabled (default), requests whose translated
  model id uses the `publisher/model` form (e.g. `openai/gpt-4o`) are routed to
  the [GitHub Models](https://models.github.ai) inference API instead of Copilot,
  authenticated with the raw GitHub token (which must have the `models` scope).
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
- **Content filtering** — system-prompt add/remove and tool-result suffix
  removal are applied per your configuration.
