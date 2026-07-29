---
title: Configuration
---

# Configuration

[Home](index.md) · [Getting Started](getting-started.md) ·
**Configuration** · [API Reference](api.md) ·
[Claude Code & Codex](claude-code.md)

---

Settings are resolved with the following precedence (highest first):

**CLI flags → environment variables → `config.yaml` → built-in defaults**

## `config.yaml`

Located at `~/.ghc-tunnel/config.yaml` (`%APPDATA%\ghc-tunnel\config.yaml` on
Windows). Generated on first run, with `--config`, or through the setup wizard.

```yaml
# Schema version for migration/write-back behavior
config_version: 2

# Server settings
address: 127.0.0.1
port: 8314
debug: false

# Copilot account tier: individual | business | enterprise
account_type: individual

# Header version strings (mimic the VS Code Copilot Chat client)
vscode_version: "1.130.0"
api_version: "2025-05-01"
copilot_version: "0.48.1"

# Self-update behavior (enabled by default)
auto_upgrade: true

# Model name mappings: exact (full match) and prefix (starts-with)
model_mappings:
  exact:
    opus: claude-opus-4.8
    sonnet: claude-sonnet-4.6
    haiku: claude-haiku-4.5
  prefix:
    claude-sonnet-4-: claude-opus-4.8

# GitHub Models (https://models.github.ai) inference
# Route publisher/model ids (e.g. openai/gpt-4o) to GitHub Models instead of
# Copilot. Needs a token with the `models: read` permission (fine-grained PAT).
github_models:
  enabled: true
  # org: my-org
  # token: ghp_xxx

# Content filtering
system_prompt_remove: []
system_prompt_add: []
tool_result_suffix_remove: []

# Retry: max retries for upstream connection errors (0 = none)
max_connection_retries: 3

# Max seconds of silence from an upstream response before it is treated as
# dead. Bounds silence, not total duration, so long streams are fine.
# 0 disables the timeout.
upstream_read_timeout_seconds: 900

# Optional: require this key on all LLM endpoints (Bearer / x-api-key /
# x-goog-api-key). Omit or leave empty to disable authentication.
# api_key: my-secret-key
```

### Model mappings

Incoming model names are rewritten before the request is forwarded upstream:

- **`exact`** — matches the full model name.
- **`prefix`** — matches when the model name *starts with* the key. When several
  prefixes match, the **longest (most specific)** one wins.

Exact matches take priority over prefix matches. Unmapped names pass through
unchanged. Use the live catalog at `GET /v1/models` to discover valid targets.

### Account type

Controls the upstream base URL only:

| `account_type` | Upstream base URL |
|----------------|-------------------|
| `individual`   | `https://api.githubcopilot.com` |
| `business`     | `https://api.business.githubcopilot.com` |
| `enterprise`   | `https://api.enterprise.githubcopilot.com` |

Set this to match the Copilot seat your token actually has.

### GitHub Models

[GitHub Models](https://models.github.ai) is GitHub's OpenAI-compatible model
**inference** service, separate from Copilot. When `github_models.enabled` is
true (the default), any request whose *translated* model id uses the
`publisher/model` form (contains a `/`, e.g. `openai/gpt-4o`) is routed there
instead of Copilot. These ids never collide with Copilot ids, so mappings and
existing behavior are unaffected. Routing applies to `/v1/chat/completions`,
`/v1/messages` (translated), and the Gemini endpoints.

GitHub Models authenticates with the **raw GitHub token** (not the Copilot
token) via `Authorization: Bearer`. That token must carry the **`models: read`**
permission (a fine-grained PAT). This is **not** covered by the Device Flow login
(`models` is not a valid classic OAuth scope), so supply a dedicated token via
`github_models.token` (or `GHC_PROXY_GITHUB_MODELS_TOKEN`); otherwise GitHub
Models requests fall back to the Device Flow token, which lacks this permission.
The [setup wizard](getting-started.md#the-setup-wizard) can capture and validate
this token for you. Set `github_models.org` to attribute inference to an
organization. The catalog is merged into `GET /v1/models`.

### Schema upgrades

`config_version` records the schema an existing `config.yaml` was written
against. When a newer release introduces configuration properties, it bumps
this number; on the next start the proxy fills the missing properties with
their default values and rewrites `config.yaml` automatically, preserving every
value you have set. No flag is needed for this.

`--update-config` remains for the other, non-schema write-backs (for example
restoring the built-in `model_mappings` when the file has none).

## Command-line options

```text
ghc-proxy [options]

  -s, --setup               Launch the interactive setup wizard
      --claudecode          Configure Claude Code to use this proxy (with --setup)
      --codex               Configure Codex to use this proxy (with --setup)
      --gemini              Configure Gemini CLI to use this proxy (with --setup)
  -d, --default             Reset config to defaults during setup
  -p, --port <port>         Port to listen on (default: 8314)
  -a, --address <addr>      Address to listen on (default: 127.0.0.1)
      --debug / --no-debug  Toggle debug mode
      --account-type <t>    individual | business | enterprise
  -c, --config              Generate the default config file and exit
      auth                  Authenticate with GitHub and exit (CI/headless)
      check-usage           Print Copilot quota/usage and exit
      info                  Print diagnostics (version, paths, token) and exit
      --json                Emit machine-readable JSON (with info)
      --show-token          Log GitHub and Copilot tokens on refresh
      --rate-limit <secs>   Minimum seconds between forwarded requests
      --wait                When rate limited, wait instead of returning HTTP 429
      --manual              Require interactive approval before each request
      --fetch-version       Fetch the latest VS Code version at startup
      --no-fetch-version    Disable dynamic VS Code version fetching
      --auto-upgrade        Auto-upgrade app when a newer release is available
                            (default: on)
      --no-auto-upgrade     Disable app auto-upgrade
      --update-config       Persist non-schema config write-backs (schema upgrades apply automatically)
  -v, --version             Show version
  -h, --help                Show help
```

## Environment variables

Every config field has a `GHC_PROXY_*` override:

| Variable | Purpose |
|----------|---------|
| `GHC_PROXY_ADDRESS` | Listen address |
| `GHC_PROXY_PORT` | Listen port |
| `GHC_PROXY_DEBUG` | Enable debug mode (`true`/`1`) |
| `GHC_PROXY_ACCOUNT_TYPE` | Account tier |
| `GHC_PROXY_VSCODE_VERSION` | `Editor-Version` string |
| `GHC_PROXY_API_VERSION` | `X-GitHub-Api-Version` string |
| `GHC_PROXY_COPILOT_VERSION` | Copilot Chat plugin version string |
| `GHC_PROXY_MAX_CONNECTION_RETRIES` | Max connection retries |
| `GHC_PROXY_UPSTREAM_READ_TIMEOUT` | Max seconds of upstream silence (`0` disables) |
| `GHC_PROXY_REDIRECT_ANTHROPIC` | Always translate Anthropic via chat completions |
| `GHC_PROXY_SHOW_TOKEN` | Log tokens on refresh (`true`/`1`) |
| `GHC_PROXY_DYNAMIC_VSCODE_VERSION` | Fetch latest VS Code version (`true`/`1`) |
| `GHC_PROXY_AUTO_UPGRADE` | Auto-upgrade app on startup (`true`/`1`); set `0` to disable |
| `GHC_PROXY_RATE_LIMIT_SECONDS` | Minimum seconds between requests |
| `GHC_PROXY_RATE_LIMIT_WAIT` | Wait instead of rejecting when limited (`true`/`1`) |
| `GHC_PROXY_MANUAL_APPROVE` | Require manual approval per request (`true`/`1`) |
| `GHC_PROXY_API_KEY` | Require this key on LLM endpoints (empty = disabled) |
| `GHC_PROXY_GITHUB_MODELS_ENABLED` | Route `publisher/model` ids to GitHub Models (`true`/`1`) |
| `GHC_PROXY_GITHUB_MODELS_ORG` | Attribute GitHub Models inference to an organization |
| `GHC_PROXY_GITHUB_MODELS_TOKEN` | Dedicated token for GitHub Models (`models: read` permission) |

Token-related variables (`COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`) are
covered in [Getting Started](getting-started.md#authentication).

## Rate limiting & manual approval

To stay comfortably under GitHub Copilot abuse thresholds:

- `--rate-limit 5` enforces a minimum 5-second gap between forwarded requests.
  Combine with `--wait` to delay instead of returning HTTP 429.
- `--manual` pauses before each upstream call until you approve it on the
  console — useful when dialing in a new client.

## Endpoint authentication

By default the proxy accepts all requests. Set `api_key` in `config.yaml` (or the
`GHC_PROXY_API_KEY` environment variable) to require a key on the LLM endpoints.
The key is accepted from `Authorization: Bearer <key>`, `x-api-key`, or
`x-goog-api-key`, and compared in constant time. The dashboard, metrics, and
static pages remain open so local monitoring keeps working without a key.

## Mimicking the Copilot client

The proxy sends the same identity headers as the real VS Code Copilot Chat
client. These version strings occasionally need refreshing when GitHub rejects
stale clients:

| Config value | Where to read it |
|--------------|------------------|
| `copilot_version` | latest `GitHub.copilot-chat` version on the VS Code Marketplace |
| `vscode_version` | latest VS Code stable release |
| `api_version` | `X-GitHub-Api-Version` in the Copilot Chat client source |

Enable `dynamic_vscode_version` (or `--fetch-version`) to refresh the VS Code
version automatically at startup.

## Self-update

`auto_upgrade` is **enabled by default**, including for config files written
before the setting existed. On startup the proxy checks GitHub releases and, if
a newer version is published, downloads and replaces its own binary. The
replacement takes effect on the **next** start — the running process keeps
serving the old code until it restarts.

Disable it with any of:

```yaml
auto_upgrade: false
```

```bash
ghc-proxy --no-auto-upgrade
GHC_PROXY_AUTO_UPGRADE=0 ghc-proxy
```

Turn it off if the binary is managed by a package manager, or if it lives
somewhere the process should not rewrite — for example a build output directory
that `cargo build` or `cargo clean` also writes to.
