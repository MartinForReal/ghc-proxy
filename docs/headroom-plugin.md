---
title: Headroom single-port integration
permalink: /headroom-plugin.html
---

# Headroom single-port integration

[Home](index.md) · [Getting Started](getting-started.md) ·
[Configuration](configuration.md) · [API Reference](api.md) ·
[Claude Code & Codex](claude-code.md) · **Headroom Plugin**

---

Headroom 0.36 already contains GitHub Copilot token exchange and transport
support. The optional `plugins/headroom-ghc` package is therefore a
**compatibility extension**, not a second Copilot implementation. It
consolidates routing into the Headroom process while preserving the aliases and
billing surfaces existing ghc-proxy clients use. Headroom becomes the only HTTP
server; the standalone Rust executable is not loaded and port 8314 is closed.

This is an integration replacement, not an FFI wrapper around the Axum server.
Headroom 0.36.x already implements Copilot token exchange, OpenAI
Chat/Responses, Responses WebSocket, Anthropic Messages, model metadata,
embeddings, inline completions, retries, compression, and memory. The extension
adds the compatibility pieces needed by an existing ghc-proxy installation:

- exact and longest-prefix aliases from the existing `config.yaml`;
- saved GitHub OAuth bridging for the Headroom quota tracker;
- observation of GitHub's authoritative `copilot_usage.total_nano_aiu` value;
- loopback-only `/usage`, `/api/usage`, `/api/cache`, and
  `/api/ghc/health` endpoints.

## Choose the right mode

| Requirement | Recommended setup |
|---|---|
| Only run Copilot CLI with a subscription | Headroom's native `wrap copilot --subscription`; no extension |
| Serve arbitrary OpenAI and Anthropic clients on one port | Headroom plus the `ghc` extension |
| Preserve ghc-proxy aliases and authoritative GitHub AI Credits | Headroom plus the `ghc` extension |
| Translate Gemini requests into Copilot requests | Keep standalone ghc-proxy |
| Keep the full Rust request browser and audit UI | Keep standalone ghc-proxy |

The single-port request path is:

```text
Claude Code / Codex / SDK
           │
           ▼
 Headroom + ghc extension :8787
           │
           ▼
 https://api.githubcopilot.com
```

## Requirements

- Python 3.10 or newer.
- Headroom 0.36.2 (the extension pins the supported line to 0.36.x).
- A reusable GitHub Copilot OAuth credential saved by
  `headroom copilot-auth login`.
- Both `OPENAI_TARGET_API_URL` and `ANTHROPIC_TARGET_API_URL` set to
  `https://api.githubcopilot.com` before Headroom starts.
- `GITHUB_COPILOT_USE_TOKEN_EXCHANGE=true`.
- `HEADROOM_PROXY_EXTENSIONS=ghc`.

The extension fails closed during app construction if either upstream still
points at the old sidecar or no reusable credential is available. This avoids a
partial installation that appears healthy but still depends on port 8314.

## Install

From a checkout of this repository:

```bash
python -m pip install "headroom-ai[proxy,code,ml,memory,relevance]>=0.36.2,<0.37"
python -m pip install ./plugins/headroom-ghc
python -m headroom.cli copilot-auth login
```

The final command stores a refreshable GitHub OAuth credential in Headroom's
private state directory. Do not put the token itself in environment files or
commit it to the repository.

## Configure

Set these values **before** Headroom constructs its FastAPI application:

| Variable | Required | Value / purpose |
|---|---:|---|
| `HEADROOM_PROXY_EXTENSIONS` | Yes | `ghc` |
| `GITHUB_COPILOT_API_URL` | Yes | `https://api.githubcopilot.com` |
| `GITHUB_COPILOT_USE_TOKEN_EXCHANGE` | Yes | `true` |
| `OPENAI_TARGET_API_URL` | Yes | `https://api.githubcopilot.com` |
| `ANTHROPIC_TARGET_API_URL` | Yes | `https://api.githubcopilot.com` |
| `HEADROOM_PROVIDER_NAME` | No | `GitHub Copilot`, used for display |
| `HEADROOM_NO_SUBSCRIPTION_TRACKING` | No | `true` disables the unrelated Claude subscription poller |
| `GHC_PROXY_CONFIG` | No | Path to an old `config.yaml`; defaults to the normal ghc-proxy config location |
| `GEMINI_TARGET_API_URL` | No | Native Google Gemini target; the extension does not translate Gemini to Copilot |
| `CLOUDCODE_TARGET_API_URL` | No | Native Google Cloud Code target |

PowerShell example:

```powershell
$env:HEADROOM_PROXY_EXTENSIONS = 'ghc'
$env:HEADROOM_PROVIDER_NAME = 'GitHub Copilot'
$env:HEADROOM_NO_SUBSCRIPTION_TRACKING = 'true'
$env:GITHUB_COPILOT_API_URL = 'https://api.githubcopilot.com'
$env:GITHUB_COPILOT_USE_TOKEN_EXCHANGE = 'true'
$env:OPENAI_TARGET_API_URL = 'https://api.githubcopilot.com'
$env:ANTHROPIC_TARGET_API_URL = 'https://api.githubcopilot.com'

python -m headroom.cli proxy --host 127.0.0.1 --port 8787 --mode cache --backend anthropic --no-telemetry --memory --memory-storage project --learn --code-aware
```

`--backend anthropic` selects Headroom's request-processing dialect; it does
not mean Anthropic bills the request. The target URLs and Copilot token exchange
select GitHub Copilot as the inference provider.

### Client endpoints

| Client | Base URL |
|---|---|
| Claude Code / Anthropic SDK | `http://127.0.0.1:8787` |
| OpenAI SDK / Codex | `http://127.0.0.1:8787/v1` |
| Health and dashboards | `http://127.0.0.1:8787` |

Some clients require a syntactically non-empty API key even though Headroom
uses its saved Copilot OAuth credential upstream. A harmless local placeholder
is sufficient when the Headroom listener remains loopback-only.

### Model aliases

The extension imports `model_mappings.exact` and `model_mappings.prefix` from
the existing ghc-proxy `config.yaml`. Exact matches win, followed by the
longest matching prefix. Set `GHC_PROXY_CONFIG` when the file is in a custom
location. No token-bearing config fields are imported.

## Verify

These control-plane checks do not invoke a model:

```powershell
Invoke-RestMethod http://127.0.0.1:8787/health
Invoke-RestMethod http://127.0.0.1:8787/api/ghc/health
Invoke-RestMethod http://127.0.0.1:8787/api/usage
Invoke-RestMethod http://127.0.0.1:8787/v1/models
```

Expected results:

- `/health` reports `ready: true` and an upstream URL on
  `githubcopilot.com`;
- `/api/ghc/health` reports `transport: headroom-native-copilot` and
  `standalone_port: null`;
- `/api/usage` reports the Copilot plan, token-based billing flag, quotas, and
  AI Credits observed during this Headroom process;
- `/v1/models` returns the live Copilot catalog.

The compatibility endpoints are loopback-only and never return OAuth tokens.
Use Headroom `/stats` for compression and provider-cache metrics. Use
`/api/usage` for GitHub's authoritative quota and `copilot_usage` values.

## Windows service migration

The repository includes migration helpers for the existing supervised Windows
service layout. Build the extension wheel first, place the verified Headroom
wheel under `target/headroom-upgrade-0.36.2`, then run the migration from an
elevated PowerShell:

```powershell
python -m pip wheel --no-deps --wheel-dir target/headroom-upgrade-0.36.2 plugins/headroom-ghc
& .\scripts\migrate-headroom-ghc-plugin.ps1
```

The script installs the verified Headroom and plugin wheels, snapshots the
currently installed dual-service host, provisions one `headroom-default`
service, and checks health, quota, model discovery, service removal, and the
closed port.

If any step fails, it invokes `scripts/rollback-headroom-ghc-plugin.ps1`
automatically. The rollback script recreates both original Windows services
from the saved service-host publish directory and validates ports 8314 and 8787.

## Protocol coverage

| Surface | In-process mode |
|---|---|
| OpenAI Chat Completions | Headroom native Copilot transport |
| OpenAI Responses HTTP/SSE | Headroom native Copilot transport |
| OpenAI Responses WebSocket | Headroom native Copilot transport |
| Anthropic Messages | Extension pre-auth + Headroom native transport |
| Model metadata | Headroom native Copilot transport |
| Embeddings and inline completions | Headroom native Copilot transport |
| Gemini / Cloud Code | Uses configured Google upstream, not Copilot translation |

The standalone Rust proxy's Gemini-to-OpenAI translation is intentionally not
replicated. Point Gemini and Cloud Code clients at their normal Google targets,
or keep the standalone proxy if serving Gemini models through a Copilot
subscription is required.

## Observability

Use Headroom `/stats` and `/dashboard` for compression, cache, memory, and
estimated cost. Use `/api/usage` for the authoritative Copilot account quota and
session AI Credits observed from `copilot_usage`. The plugin never logs or
returns OAuth/API tokens.

Headroom's estimated dollar cost is based on its pricing table. GitHub's
`copilot_usage.total_nano_aiu`, exposed by the compatibility endpoint, remains
the billing source of truth.

## Rollback

Run `scripts/rollback-headroom-ghc-plugin.ps1` from an elevated PowerShell. The
migration keeps the Rust release binary, its versioned rollback copy, and the
exact pre-migration service-host publish directory. The installed plugin can
remain present because the restored service host does not enable it.
