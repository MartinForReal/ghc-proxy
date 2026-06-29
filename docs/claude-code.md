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
proxy. It also sets `CLAUDE_CODE_AUTO_COMPACT_WINDOW` and
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` (default 85%) so Claude Code compacts context
earlier — Copilot’s tokenizer differs from Anthropic’s, so local token estimates
can run lower than real usage. Existing settings are preserved — the base URL is
updated, keys and overrides are added only when missing, and the file is left
untouched if it is not valid JSON.

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
Copilot model you want. For example, to always use Claude Opus 4.8 with its
native 1M context:

```yaml
model_mappings:
  exact:
    claude-opus-4-8: claude-opus-4.8
  prefix:
    claude-opus-4-7: claude-opus-4.8
```

Restart the proxy after editing `config.yaml` — mappings are read at startup.

## Codex CLI

The Codex `/v1/responses` endpoint is supported with adapters that make the
upstream Copilot Responses API behave like the Codex client expects:

- `apply_patch` tool rewriting
- `X-Initiator` header (`user` vs `agent`)
- context-compaction trimming
- `service_tier` nulling
- stripping of unsupported tools

### Automatic setup

```bash
./target/release/ghc-proxy --setup --codex
```

This patches `~/.codex/config.toml`, adding a `model_providers.ghc-proxy` block
(pointing at `http://127.0.0.1:8314/v1`) and selecting it. Existing settings are
preserved, and the file is left untouched if it is not valid TOML.

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
(`http://127.0.0.1:8314/v1beta`), `GEMINI_MODEL`, and disables telemetry, and
selects api-key auth in `~/.gemini/settings.json` to skip the first-launch
prompt. Any user-set `GEMINI_API_KEY` is preserved. The Gemini surface is served
at `/v1beta/models/{model}:generateContent` (plus streaming and token counting).

## Tips

- Use `GET /usage` (or `ghc-proxy check-usage`) to monitor your Copilot quota.
- The dashboard at `http://127.0.0.1:8314/` shows live request statistics and
  the full list of supported models.
- If a tool reports a model is unavailable, check `GET /v1/models` for the exact
  model id and add a mapping.
