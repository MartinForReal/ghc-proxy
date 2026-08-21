# Headroom GHC plugin

Runs GitHub Copilot subscription inference through Headroom's native transport,
without a standalone `ghc-proxy` listener.

The extension is intentionally small. Headroom 0.36.x owns HTTP/SSE/WebSocket
forwarding and Copilot token refresh. This package adds compatibility behavior:

- model aliases loaded from `%APPDATA%/ghc-tunnel/config.yaml`;
- saved Headroom Copilot OAuth bridging for quota tracking;
- authoritative `copilot_usage.total_nano_aiu` observation;
- loopback-only `/usage`, `/api/usage`, `/api/cache`, and `/api/ghc/health`.

Before Headroom starts, both `OPENAI_TARGET_API_URL` and
`ANTHROPIC_TARGET_API_URL` must point to GitHub Copilot CAPI, and
`HEADROOM_PROXY_EXTENSIONS` must include `ghc`. Authenticate once with
`headroom copilot-auth login` (or migrate an existing reusable GitHub OAuth
token into Headroom's auth store).

Native Gemini/Cloud Code requests are not translated by this extension. They
continue to use Headroom's configured Google targets. OpenAI Chat/Responses,
Responses WebSocket, Anthropic Messages, embeddings, model metadata, and inline
Copilot completions use Headroom's built-in Copilot transport.

For the supported Windows service migration, run
`scripts/migrate-headroom-ghc-plugin.ps1` from an elevated PowerShell. It saves
the currently installed dual-service host before removing the `ghc-proxy`
service. `scripts/rollback-headroom-ghc-plugin.ps1` restores that exact host if
the migration fails or a later rollback is required.
