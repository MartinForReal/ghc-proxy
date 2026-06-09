---
title: ghc-proxy
---

# ghc-proxy

A **GitHub Copilot API proxy** written in Rust. It exposes standard **OpenAI**
and **Anthropic** compatible HTTP endpoints so any tool — Claude Code, the Codex
CLI, the OpenAI/Anthropic SDKs, and more — can talk to GitHub Copilot models.

> **Documentation:** [Getting Started](getting-started.md) ·
> [Configuration](configuration.md) · [API Reference](api.md) ·
> [Claude Code & Codex](claude-code.md)

---

## What it does

```text
   Your tool                ghc-proxy                  GitHub Copilot
┌──────────────┐  OpenAI/  ┌──────────────┐  disguised ┌──────────────┐
│ Claude Code  │ Anthropic │  translate   │  as the    │ api.github   │
│ Codex CLI    ├──────────►│  + filter    ├───────────►│ copilot.com  │
│ OpenAI SDK   │   HTTP    │  + retry     │  VS Code   │              │
└──────────────┘           └──────────────┘  client    └──────────────┘
```

The proxy authenticates to GitHub Copilot by faithfully impersonating the
official **VS Code Copilot Chat** client, then re-shapes requests and responses
so OpenAI- and Anthropic-native clients work unmodified.

## Highlights

- **OpenAI-compatible** `/v1/chat/completions` and `/v1/responses` (Codex) endpoints.
- **Anthropic-compatible** `/v1/messages` endpoint with native passthrough or translation.
- **Embeddings**, **model listing**, **token counting**, and a **usage** endpoint.
- **Model-name translation** via configurable exact and longest-prefix mappings.
- **Streaming (SSE)**, **retry with backoff**, and **content filtering**.
- **1M-context** support via the `anthropic-beta` header for capable models.
- **Interactive setup wizard** — GitHub sign-in, live model catalog, model mapping.
- **Claude Code integration** — one flag wires `~/.claude/settings.json` to the proxy.
- **Analytics dashboard** at `/` and a request browser at `/requests`.

## Quick start

```bash
# Build
cargo build --release

# Run — first launch in a terminal opens the interactive setup wizard
./target/release/ghc-proxy
```

Point any OpenAI client at `http://127.0.0.1:8314/v1`:

```bash
curl http://127.0.0.1:8314/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hello!"}]}'
```

Continue with the [Getting Started](getting-started.md) guide.

---

<sub>ghc-proxy is a community project and is not affiliated with or endorsed by
GitHub or Microsoft. Use it in accordance with the GitHub Copilot terms of
service.</sub>
