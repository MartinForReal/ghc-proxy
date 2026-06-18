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

This patches `~/.claude/settings.json`, merging both `env.ANTHROPIC_BASE_URL`
and `env.ANTHROPIC_API_KEY` so Claude Code routes its Anthropic API calls
through the proxy. Existing settings are preserved — the base URL is updated,
an API key is added only when missing, and the file is left untouched if it is
not valid JSON.

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

Point the Codex CLI at the proxy's base URL:

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8314/v1"
```

## Tips

- Use `GET /usage` (or `ghc-proxy check-usage`) to monitor your Copilot quota.
- The dashboard at `http://127.0.0.1:8314/` shows live request statistics and
  the full list of supported models.
- If a tool reports a model is unavailable, check `GET /v1/models` for the exact
  model id and add a mapping.
