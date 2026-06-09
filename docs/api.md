---
title: API Reference
---

# API Reference

[Home](index.md) · [Getting Started](getting-started.md) ·
[Configuration](configuration.md) · **API Reference** ·
[Claude Code & Codex](claude-code.md)

---

All endpoints listen on `http://<address>:<port>` (default
`http://127.0.0.1:8314`). No API key is required by the proxy itself —
authentication to GitHub Copilot is handled internally.

## Endpoints

| Method & path | Description |
|---------------|-------------|
| `POST /v1/chat/completions` | OpenAI chat completions (also `/chat/completions`) |
| `POST /v1/responses` | OpenAI Responses API for Codex (also `/responses`) |
| `POST /v1/messages` | Anthropic Messages API |
| `POST /v1/messages/count_tokens` | Anthropic token counting (real BPE) |
| `POST /v1/embeddings` | Embeddings (also `/embeddings`) |
| `GET /v1/models` | List available models (also `/models`, `/api/models`) |
| `GET /v1/models/full/` | Raw upstream model catalog with capabilities |
| `GET /usage` | Copilot plan and quota usage |
| `GET /` | Web analytics dashboard |
| `GET /requests` | Request browser |
| `GET /api/stats` | Dashboard statistics (JSON) |
| `GET /api/requests` | Recent requests (JSON) |

Streaming (SSE) is supported on the chat, responses, and messages endpoints by
setting `"stream": true` in the request body.

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

- **Model translation** — model names are rewritten per your
  [mappings](configuration.md#model-mappings) before being forwarded.
- **1M context** — for Anthropic-native requests, the proxy forwards the
  `anthropic-beta: context-1m-2025-08-07` header for models whose catalog
  advertises an extended context window.
- **Retry with backoff** — upstream connection errors are retried with
  exponential backoff (`max_connection_retries`).
- **Orphaned tool-result recovery** — when the upstream rejects a request for an
  orphaned `tool_use_id`, the proxy retries with the offending tool results
  stripped.
- **Content filtering** — system-prompt add/remove and tool-result suffix
  removal are applied per your configuration.
