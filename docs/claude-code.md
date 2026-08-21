---
title: Claude Code & Codex
---

# Claude Code & Codex

[Home](index.md) · [Getting Started](getting-started.md) ·
[Configuration](configuration.md) · [API Reference](api.md) ·
**Claude Code & Codex** · [Headroom Plugin](headroom-plugin.md)

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

The built-in defaults already do this for every Opus and Sonnet spelling. They
apply to a *new* config file only — an existing one keeps the targets it has,
so if you wrote yours before Opus 5 shipped, either edit it or re-run
`--setup`.

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

The provider uses command-backed authentication through `ghc-proxy codex-auth-token`.
Besides supplying the current local `api_key` (or a harmless
placeholder when authentication is disabled), this tells Codex to refresh the
provider's model catalog. No OpenAI account or exported environment variable is
required. When an `api_key` is active during setup, its resolved value is also
written as `x-api-key`; this keeps environment-only proxy launches working when
ChatGPT Desktop does not inherit the server's shell environment.

Codex requests `GET /v1/models?client_version=...`; the proxy recognizes that
query and returns Codex-native model metadata with `context_window` and
`max_context_window` populated from the live Copilot catalog. Plain
`GET /v1/models` remains the standard OpenAI list. This matters because Copilot
serves familiar slugs with different limits — currently 1,050,000 for
`gpt-5.6-sol` and 264,000 for `gpt-5-mini`. The window now follows the selected
model automatically, so setup removes any stale top-level
`model_context_window` override.

### Manual setup

Define a command-authenticated provider so Codex also discovers model limits:

```toml
model_provider = "ghc-proxy"

[model_providers.ghc-proxy]
name = "GHC Proxy"
base_url = "http://127.0.0.1:8314/v1"
wire_api = "responses"

[model_providers.ghc-proxy.auth]
command = "/usr/local/bin/ghc-proxy"
args = ["codex-auth-token"]
timeout_ms = 5000
refresh_interval_ms = 300000
```

Use the actual path of the `ghc-proxy` executable for `command`.

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
