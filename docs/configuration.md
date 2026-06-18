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
vscode_version: "1.123.0"
api_version: "2025-05-01"
copilot_version: "0.48.1"

# Self-update behavior
auto_upgrade: false

# Model name mappings: exact (full match) and prefix (starts-with)
model_mappings:
  exact:
    opus: claude-opus-4.8
    sonnet: claude-sonnet-4.6
    haiku: claude-haiku-4.5
  prefix:
    claude-sonnet-4-: claude-opus-4.8

# Content filtering
system_prompt_remove: []
system_prompt_add: []
tool_result_suffix_remove: []

# Retry: max retries for upstream connection errors (0 = none)
max_connection_retries: 3
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

## Command-line options

```text
ghc-proxy [options]

  -s, --setup               Launch the interactive setup wizard
      --claudecode          Configure Claude Code to use this proxy (with --setup)
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
      --no-auto-upgrade     Disable app auto-upgrade
      --update-config       Persist migrated config/default additions back to config.yaml
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
| `GHC_PROXY_REDIRECT_ANTHROPIC` | Always translate Anthropic via chat completions |
| `GHC_PROXY_SHOW_TOKEN` | Log tokens on refresh (`true`/`1`) |
| `GHC_PROXY_DYNAMIC_VSCODE_VERSION` | Fetch latest VS Code version (`true`/`1`) |
| `GHC_PROXY_AUTO_UPGRADE` | Auto-upgrade app on startup (`true`/`1`) |
| `GHC_PROXY_RATE_LIMIT_SECONDS` | Minimum seconds between requests |
| `GHC_PROXY_RATE_LIMIT_WAIT` | Wait instead of rejecting when limited (`true`/`1`) |
| `GHC_PROXY_MANUAL_APPROVE` | Require manual approval per request (`true`/`1`) |

Token-related variables (`COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`) are
covered in [Getting Started](getting-started.md#authentication).

## Rate limiting & manual approval

To stay comfortably under GitHub Copilot abuse thresholds:

- `--rate-limit 5` enforces a minimum 5-second gap between forwarded requests.
  Combine with `--wait` to delay instead of returning HTTP 429.
- `--manual` pauses before each upstream call until you approve it on the
  console — useful when dialing in a new client.

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
