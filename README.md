# ghc-proxy

A **GitHub Copilot API proxy** written in Rust. It exposes standard **OpenAI**
and **Anthropic** compatible HTTP endpoints so any tool (Claude Code, the Codex
CLI, OpenAI/Anthropic SDKs, etc.) can talk to GitHub Copilot models.

This is a Rust backend port of the
[`ghc-tunnel`](https://www.npmjs.com/package/ghc-tunnel) Node.js project.

📖 **Documentation:** <https://martinforreal.github.io/ghc-proxy/>

## Quick Start

```bash
# Build
cargo build --release

# Run — on first launch in a terminal this opens the interactive setup wizard
./target/release/ghc-proxy

# Re-run the setup wizard at any time
./target/release/ghc-proxy --setup

# Generate the default config file non-interactively and exit
./target/release/ghc-proxy --config
```

On a first run with no config file, when launched from a terminal, the proxy
opens an **interactive setup wizard** that signs you in to GitHub (Device Flow),
fetches the live model catalog, and helps you configure model mappings. In
headless or piped contexts the wizard is skipped: the proxy falls back to GitHub
Device Flow auth (or a `*_TOKEN` environment variable) and a default config
file.

## Features

- **OpenAI-compatible** `/v1/chat/completions` and `/v1/responses` endpoints
  (with Codex adapters: `apply_patch` tool rewrite, `X-Initiator` header,
  context compaction trimming, `service_tier` nulling, unsupported-tool
  stripping).
- **Anthropic-compatible** `/v1/messages` endpoint (direct passthrough when the
  upstream model supports it, otherwise translated through chat completions).
- Automatic **model name translation** via configurable exact/prefix mappings.
- **Streaming** support (SSE) for all endpoints.
- **Retry with exponential backoff** for upstream connection errors.
- **Content filtering** (system prompt add/remove, tool-result suffix removal).
- **Copilot token management** with automatic refresh.
- **Orphaned `tool_use_id` recovery** — retries with offending tool results
  stripped when the upstream returns the corresponding 400 error.
- **Request analytics dashboard** at `/` and a request browser at `/requests`.
- **Interactive setup wizard** (`--setup`, or first launch in a terminal):
  GitHub sign-in, live model catalog, and model-mapping configuration.
- **1M-context support** — forwards the `anthropic-beta: context-1m-2025-08-07`
  header for models whose catalog advertises an extended context window.

## CLI Options

```
ghc-proxy [options]

  -s, --setup             Launch the interactive setup wizard (sign in + map models)
      --claudecode        Configure Claude Code (~/.claude/settings.json) to use this proxy (with --setup)
  -d, --default           Reset config to defaults during setup
  -p, --port <port>       Port to listen on (default: 8314)
  -a, --address <addr>    Address to listen on (default: 127.0.0.1)
      --account-type <t>  Account tier: individual | business | enterprise
  -c, --config            Generate the default config file (non-interactive)
  -v, --version           Show version
  -h, --help              Show help
```

## Authentication

A GitHub token is resolved in this order:

1. `COPILOT_GITHUB_TOKEN`, then `GH_TOKEN`, then `GITHUB_TOKEN` environment
   variables (matching the GitHub Copilot SDK precedence).
2. Saved token file at `<config-dir>/github_token.txt`.
3. Interactive GitHub Device Flow (the resulting token is saved for reuse, with
   `0600` permissions on Unix).

The GitHub token is exchanged for a short-lived **Copilot token** via
`https://api.github.com/copilot_internal/v2/token`, which is refreshed
automatically before it expires.

## Setup Wizard

Running `ghc-proxy --setup` — or launching the proxy for the first time from a
terminal with no config file — opens an interactive wizard that:

1. Prompts for the **server settings** (listen address, port, account tier).
2. Signs in to GitHub via **Device Flow** and saves the token.
3. Fetches the **live model catalog** and lets you map the `opus` / `sonnet` /
   `haiku` aliases to specific models (or keep the recommended defaults).
4. Optionally configures **Claude Code** to route through the proxy.

The wizard only runs when attached to a terminal, so headless and CI launches
are unaffected (they fall back to environment/file tokens and a default config).
Pass `--default` to start the wizard from built-in defaults, or `--claudecode`
to include the Claude Code step automatically.

## Configuration

Config file: `~/.ghc-tunnel/config.yaml` (`%APPDATA%/ghc-tunnel/config.yaml`
on Windows). It is generated on first run or with `--config`.

```yaml
address: 127.0.0.1
port: 8314
debug: false
account_type: individual            # individual | business | enterprise
vscode_version: "1.123.0"
api_version: "2025-05-01"
copilot_version: "0.44.0"
model_mappings:
  exact:
    opus: claude-opus-4.7-1m
    haiku: claude-haiku-4.5
  prefix:
    claude-sonnet-4-: claude-opus-4.7-1m
system_prompt_remove: []
system_prompt_add: []
tool_result_suffix_remove: []
max_connection_retries: 3
```

## API Endpoints

| Endpoint | Description |
|----------|-------------|
| `POST /v1/chat/completions` | OpenAI chat completions |
| `POST /v1/responses` | OpenAI responses API (Codex) |
| `GET /v1/models` | List available models |
| `POST /v1/messages` | Anthropic messages API |
| `POST /v1/messages/count_tokens` | Anthropic token counting |
| `GET /` | Web dashboard |
| `GET /requests` | Request browser |
| `GET /api/models` | All supported models (used by the dashboard) |

## Example Usage

### OpenAI SDK

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8314/v1", api_key="not-needed")
resp = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}],
)
```

### Anthropic SDK

```python
import anthropic

client = anthropic.Anthropic(base_url="http://127.0.0.1:8314", api_key="not-needed")
msg = client.messages.create(
    model="claude-sonnet-4",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}],
)
```

### cURL

```bash
curl http://127.0.0.1:8314/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hello!"}]}'
```

## Development

```bash
cargo build      # compile
cargo test       # run unit + integration tests
cargo clippy     # lint
```

## Project Layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | CLI parsing and server startup |
| `src/setup.rs` | Interactive first-run setup wizard |
| `src/config.rs` | Config dir, YAML config, defaults, model-mapping defaults |
| `src/auth.rs` | GitHub token resolution (env/file/Device Flow), Copilot token exchange |
| `src/state.rs` | Shared state, token refresh, upstream header construction |
| `src/translate.rs` | Model-name translation (exact + prefix) |
| `src/filters.rs` | Content filtering and token estimation |
| `src/anthropic.rs` | Anthropic <-> OpenAI request/response/stream translation |
| `src/responses.rs` | Codex `/v1/responses` adapters |
| `src/util.rs` | Retry-with-backoff and orphaned tool-result handling |
| `src/server.rs` | Axum router and all HTTP handlers |
| `src/store.rs` | In-memory request store for the dashboard |

## Mimicking the Copilot Client

The proxy authenticates to GitHub Copilot by impersonating the official
**VS Code Copilot Chat** client. To do this faithfully it sends the same
identity headers that the real client sends to `api.githubcopilot.com`
(`Editor-Version`, `Editor-Plugin-Version`, `User-Agent`,
`Copilot-Integration-Id`, `OpenAI-Intent`, `X-Interaction-Type`,
`X-GitHub-Api-Version`, `openai-organization`, plus a persisted
`vscode-machineid` and a per-session `vscode-sessionid`, etc.). These are built
in `AppState::copilot_headers` / `github_headers` (`src/state.rs`) from the
version strings in `src/config.rs`.

For Anthropic-native `/v1/messages` requests, the proxy also forwards the
`anthropic-beta: context-1m-2025-08-07` header for models whose catalog
advertises a context window larger than 200K tokens, unlocking the 1M-token
tier the same way the official client does.

GitHub may reject requests that report stale client versions, so these values
occasionally need refreshing. The source of truth is the now open-source
[`microsoft/vscode-copilot-chat`](https://github.com/microsoft/vscode-copilot-chat)
extension and the VS Code Marketplace:

| Config value | Where to read it |
|--------------|------------------|
| `copilot_version` | latest `GitHub.copilot-chat` version on the VS Code Marketplace (or the `version` field in the extension's `package.json`) |
| `vscode_version` | latest VS Code stable release (`https://update.code.visualstudio.com/api/releases/stable`) |
| `api_version` | `X-GitHub-Api-Version` constant in `src/platform/networking/common/networking.ts` |

After updating the constants in `src/config.rs`, run the test suite (the header
test in `tests/integration.rs` guards the expected header set) and bump the
example values in this README.

## Notes on Parity with `ghc-tunnel`

This Rust port focuses on the **core proxy behavior**: authentication, token
management, model translation, all four API surfaces with streaming, content
filtering, retry, the CLI, and the dashboard. The following `ghc-tunnel`
auxiliary features are intentionally **not** ported: OneDrive config sync, the
ACP code agent, Codex config auto-repair, and the persistent on-disk analytics
database. `--setup` launches an interactive wizard (GitHub sign-in, live model
catalog, model-mapping configuration) and writes/updates the config file; in
headless or piped contexts it instead re-renders the config non-interactively,
applying any CLI overrides or resetting to defaults with `--default`.
`--claudecode` patches `~/.claude/settings.json`, merging
`env.ANTHROPIC_BASE_URL` so Claude Code routes through this proxy (existing
settings are preserved). The dashboard lists all supported models alongside the
request statistics.

## License

MIT
