---
title: Claude Code & Codex
---

# Claude Code & Codex

[Home](index.md) · [Getting Started](getting-started.md) ·
[Configuration](configuration.md) · [API Reference](api.md) ·
**Claude Code & Codex**

---

## Claude Code

### Automatic setup

Run the setup wizard with the Claude Code step enabled:

```bash
./target/release/ghc-proxy --setup --claudecode
```

This patches `~/.claude/settings.json`, merging `env.ANTHROPIC_BASE_URL` and
`env.ANTHROPIC_API_KEY` so Claude Code routes its Anthropic API calls through the
proxy. Existing settings are preserved — the base URL is updated, keys are added
only when missing, and the file is left untouched if it is not valid JSON.

### Manual setup

Set both `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` in
`~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8314",
    "ANTHROPIC_API_KEY": "ghc-proxy"
  }
}
```

Or export it in your shell before launching Claude Code:

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8314"
export ANTHROPIC_API_KEY="ghc-proxy"
```

### Mapping Claude Code's models

Claude Code sends specific model names (for example `claude-opus-4-7[1m]`). Use
[model mappings](configuration.md#model-mappings) to route those to whichever
Copilot model you want. For example, to always use Claude Opus 5 with its
native 1M context:

```yaml
model_mappings:
  exact:
    opus: claude-opus-5
    "5[1m]": claude-opus-5
  prefix:
    claude-opus-4-7: claude-opus-5
    claude-opus-4-8: claude-opus-5
```

The built-in defaults already do this for every Claude spelling. They apply to
a *new* config file only — an existing one keeps the targets it has, so if you
wrote yours before Opus 5 shipped, either edit it or re-run `--setup`.

Restart the proxy after editing `config.yaml` — mappings are read at startup.

## Codex CLI

The Codex `/v1/responses` endpoint is supported with adapters that make the
upstream Copilot Responses API behave like the Codex client expects:

- `apply_patch` tool rewriting
- `X-Initiator` header (`user` vs `agent`)
- context-compaction trimming
- `service_tier` nulling — Copilot answers 400 `service_tier is not supported`
  for every value, including the tiers Codex sends for Fast mode, so the field
  is normalized to null rather than forwarded
- stripping of unsupported tools

### Automatic setup

```bash
./target/release/ghc-proxy --setup --codex
```

This patches `~/.codex/config.toml`, adding a `model_providers.ghc-proxy` block
(pointing at `http://127.0.0.1:8314/v1`) and selecting it. Existing settings are
preserved, and the file is left untouched if it is not valid TOML.

A `model` already in the file is treated as a deliberate choice and kept; the
recommended default is only written when the config has none.

`model_context_window` is written from the live Copilot catalog for whichever
model ends up selected. Codex otherwise budgets context from its own built-in
table, which is sized for OpenAI's public limits — Copilot serves the same
slugs with different windows (1,050,000 for `gpt-5.5`, 264,000 for
`gpt-5-mini`), so without this the client compacts at the wrong point. The key
is skipped when the catalog cannot be reached.

When the proxy is configured with an `api_key`, the provider block also gets
`http_headers = { Authorization = "Bearer <key>" }`. Codex otherwise only reads
credentials from an environment variable named by `env_key`, and it fails a
turn when that variable is unset — which is exactly what happens when the key
lives in `config.yaml` and was never exported. The value is written literally
because it only opens the local proxy; the credential that reaches GitHub is
the Copilot token the proxy holds. Nothing is written when no key is set.

### Manual setup

Point the Codex CLI at the proxy's base URL:

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8314/v1"
```

## Gemini CLI

Configure the Gemini CLI automatically:

```bash
./target/release/ghc-proxy --setup --gemini
```

This writes `~/.gemini/.env` with `GOOGLE_GEMINI_BASE_URL`
(`http://127.0.0.1:8314`), `GEMINI_MODEL`, and disables telemetry, and
selects api-key auth in `~/.gemini/settings.json` to skip the first-launch
prompt. Any user-set `GEMINI_API_KEY` is preserved. The Gemini surface is served
at `/v1beta/models/{model}:generateContent` (plus streaming and token counting).

The base URL carries no API version: the Gen AI SDK appends `v1beta` itself, so
writing it here would send `/v1beta/v1beta/models/...` and 404.

### Context window

Gemini CLI does not read a context window from the server. `tokenLimit()` is a
hardcoded table whose default branch returns 1,048,576 tokens, so every model
this proxy serves is assumed to have a 1M window regardless of what actually
backs it. With the default `model.compressionThreshold` of `0.5`, compaction
would not trigger until roughly 524K tokens — far past where the upstream
rejects the request.

Set the threshold to about 80% of the real window, expressed as a fraction of
1,048,576, in `~/.gemini/settings.json`:

```json
{
  "model": {
    "compressionThreshold": 0.15
  }
}
```

`0.15` suits a 200K model; use `0.1` for 128K.

## Tips

- Use `GET /usage` (or `ghc-proxy check-usage`) to monitor your Copilot quota.
- The dashboard has three pages: the overview at `http://127.0.0.1:8314/` leads
  with what Copilot billed for the session in AI units, alongside quota, traffic
  and prompt-cache statistics; `/requests` browses each exchange as a
  conversation; `/metrics/dashboard` shows the raw metric list.
- Prompt caching is where an agent session gets cheap or expensive. The cache
  panel breaks the hit rate down per model, because a single global number
  cannot tell you *which* conversation stopped matching its prefix. Copilot
  needs a minimum cacheable prefix — around 4k tokens — before any of it is
  eligible, so short prompts legitimately show nothing.
- To inspect what a tool is actually sending, turn on body capture from the
  overview or with `curl -X POST http://127.0.0.1:8314/api/config/debug -d
  '{"debug": true}'`. It takes effect on the next request and lapses on
  restart.
- If a tool reports a model is unavailable, check `GET /v1/models` for the exact
  model id and add a mapping.
