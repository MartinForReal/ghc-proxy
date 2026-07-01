---
title: Getting Started
---

# Getting Started

[Home](index.md) · **Getting Started** ·
[Configuration](configuration.md) · [API Reference](api.md) ·
[Claude Code & Codex](claude-code.md)

---

## Prerequisites

- A **GitHub account with an active Copilot subscription** (individual,
  business, or enterprise).
- **Rust** (stable) to build from source — install via [rustup](https://rustup.rs).

## Build

```bash
git clone https://github.com/MartinForReal/ghc-proxy.git
cd ghc-proxy
cargo build --release
```

The binary is written to `target/release/ghc-proxy` (`.exe` on Windows).

## Run

```bash
./target/release/ghc-proxy
```

On a first run with no config file, when launched from a terminal, the proxy
opens the **interactive setup wizard** (see below). In headless or piped
contexts the wizard is skipped and the proxy falls back to environment/file
tokens and a default configuration.

Once running, the proxy prints the endpoints it serves:

```text
Starting GitHub Copilot API Proxy on 127.0.0.1:8314
Dashboard:      http://127.0.0.1:8314/
Metrics UI:     http://127.0.0.1:8314/metrics/dashboard
OpenMetrics:    http://127.0.0.1:8314/metrics
Reload config:  POST http://127.0.0.1:8314/api/config/reload
OpenAI API:     http://127.0.0.1:8314/v1/chat/completions
Responses API:  http://127.0.0.1:8314/v1/responses
Anthropic API:  http://127.0.0.1:8314/v1/messages
Gemini API:     http://127.0.0.1:8314/v1beta/models/{model}:generateContent
OpenAPI spec:   http://127.0.0.1:8314/openapi.json
```

## Authentication

A GitHub token is resolved in this order:

1. The `COPILOT_GITHUB_TOKEN`, then `GH_TOKEN`, then `GITHUB_TOKEN` environment
   variables (matching the GitHub Copilot SDK precedence).
2. The saved token file at `<config-dir>/github_token.txt`.
3. Interactive **GitHub Device Flow** — the proxy prints a code and URL; once you
   authorize, the token is saved for reuse (with `0600` permissions on Unix).

The GitHub token is exchanged for a short-lived **Copilot token** via
`https://api.github.com/copilot_internal/v2/token`, which the proxy refreshes
automatically before it expires.

The Device Flow requests the `read:user copilot models` scopes; the `models`
scope authorizes the [GitHub Models](configuration.md#github-models) inference
API. If you bring your own token, give it the `models` scope (classic/OAuth) or
`models: read` permission (fine-grained PAT) to use GitHub Models.

To authenticate without starting the server (useful for CI/headless setups):

```bash
./target/release/ghc-proxy auth
```

## The setup wizard

Run the interactive wizard at any time:

```bash
./target/release/ghc-proxy --setup
```

It walks through:

1. **Server settings** — listen address, port, and account tier.
2. **GitHub sign-in** — Device Flow authentication, with the token saved.
3. **Model mappings** — fetches the live model catalog and lets you map the
   `opus` / `sonnet` / `haiku` aliases to specific models, or keep the
   recommended defaults.
4. **Client setup** — optionally configure Claude Code
   (`~/.claude/settings.json`), Codex (`~/.codex/config.toml`), and the Gemini
   CLI (`~/.gemini/.env`) to route through the proxy. Existing settings are
   preserved and any user-set API key is left untouched.

Pass `--default` to start from built-in defaults, or `--claudecode` / `--codex`
/ `--gemini` to include the matching client-setup step automatically. The wizard
only runs when attached to a terminal, so automated launches are unaffected.

## Config directory

| Platform | Location |
|----------|----------|
| Windows  | `%APPDATA%\ghc-tunnel\` |
| macOS / Linux | `~/.ghc-tunnel/` |

This directory holds `config.yaml`, the saved `github_token.txt`, and the
persisted `machine_id.txt` used for client disguise.

## Next steps

- Tune behavior in the [Configuration](configuration.md) reference.
- Explore the [API Reference](api.md) and client examples.
- Wire up [Claude Code & Codex](claude-code.md).
