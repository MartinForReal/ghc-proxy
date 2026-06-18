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
4. **Claude Code** — optionally configure `~/.claude/settings.json` to route
   through the proxy (`ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`; existing
   API key values are preserved).

Pass `--default` to start from built-in defaults, or `--claudecode` to include
the Claude Code step automatically. The wizard only runs when attached to a
terminal, so automated launches are unaffected.

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
