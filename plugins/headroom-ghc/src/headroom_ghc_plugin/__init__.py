"""Headroom extension for direct GitHub Copilot subscription routing.

The extension does not launch or listen on behalf of ``ghc-proxy``. Headroom's
native Copilot transport owns inference in-process; this package supplies the
compatibility layer that used to live around the standalone Rust proxy:

* reuse the existing ghc-proxy model aliases;
* bridge Headroom's saved GitHub OAuth credential into its quota tracker;
* observe GitHub's authoritative ``copilot_usage`` payload without buffering or
  changing responses; and
* expose small, loopback-only usage/cache/health compatibility endpoints.

The service must set ``OPENAI_TARGET_API_URL`` and
``ANTHROPIC_TARGET_API_URL`` to a ``*.githubcopilot.com`` CAPI host before
Headroom creates its application. Extensions are installed after the proxy
transport is constructed, so failing closed here prevents a misleading partial
installation that would still send traffic to the old sidecar.
"""

from __future__ import annotations

import asyncio
import ipaddress
import json
import os
import re
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Awaitable, Callable, Iterable, Mapping
from urllib.parse import urlparse

from starlette.requests import Request
from starlette.responses import JSONResponse

PLUGIN_VERSION = "0.1.1"
_NANO_AIU_PER_CREDIT = 1_000_000_000
_CAPTURE_LIMIT = 8 * 1024 * 1024
_PROJECT_PREFIX = re.compile(r"^/p/[^/]+(?P<path>/.*)$")
_MODEL_BODY_PATHS = frozenset(
    {
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/responses",
        "/responses",
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/embeddings",
        "/embeddings",
    }
)
_SKIP_OBSERVATION_PREFIXES = ("/api/ghc", "/api/usage", "/api/cache", "/usage")
_TOKEN_ENV_VARS = (
    "GITHUB_COPILOT_GITHUB_TOKEN",
    "GITHUB_COPILOT_TOKEN",
    "COPILOT_GITHUB_TOKEN",
    "GITHUB_TOKEN",
)


@dataclass(frozen=True)
class ModelMappings:
    """Exact and longest-prefix model aliases compatible with ghc-proxy."""

    exact: Mapping[str, str] = field(default_factory=dict)
    prefix: Mapping[str, str] = field(default_factory=dict)

    def translate(self, model: str) -> str:
        exact = self.exact.get(model)
        if exact:
            return exact
        winner: tuple[int, str] | None = None
        for source, target in self.prefix.items():
            if model.startswith(source) and (winner is None or len(source) > winner[0]):
                winner = (len(source), target)
        return winner[1] if winner else model


def _logical_path(path: str) -> str:
    match = _PROJECT_PREFIX.match(path)
    return match.group("path") if match else path


def _default_mapping_path() -> Path:
    configured = os.environ.get("GHC_PROXY_CONFIG", "").strip()
    if configured:
        return Path(configured).expanduser()
    appdata = os.environ.get("APPDATA", "").strip()
    if appdata:
        return Path(appdata) / "ghc-tunnel" / "config.yaml"
    return Path.home() / ".ghc-tunnel" / "config.yaml"


def load_model_mappings(path: Path | None = None) -> ModelMappings:
    """Load only model mappings; token-bearing config fields are never exposed."""

    target = path or _default_mapping_path()
    try:
        import yaml

        payload = yaml.safe_load(target.read_text(encoding="utf-8")) or {}
        raw = payload.get("model_mappings") or {}
        exact = raw.get("exact") or {}
        prefix = raw.get("prefix") or {}
        if not isinstance(exact, dict) or not isinstance(prefix, dict):
            return ModelMappings()
        return ModelMappings(
            exact={str(k): str(v) for k, v in exact.items()},
            prefix={str(k): str(v) for k, v in prefix.items()},
        )
    except (FileNotFoundError, OSError, ValueError, TypeError):
        return ModelMappings()


def _rewrite_model_object(payload: Any, mappings: ModelMappings) -> tuple[str | None, str | None]:
    if not isinstance(payload, dict):
        return None, None

    holder = payload
    if not isinstance(holder.get("model"), str) and isinstance(holder.get("response"), dict):
        holder = holder["response"]

    original = holder.get("model")
    if not isinstance(original, str) or not original:
        return None, None
    translated = mappings.translate(original)
    if translated != original:
        holder["model"] = translated
    return original, translated


def rewrite_json_body(
    path: str, body: bytes, mappings: ModelMappings
) -> tuple[bytes, str | None, str | None]:
    """Rewrite a model field while leaving invalid/non-JSON bodies untouched."""

    if _logical_path(path) not in _MODEL_BODY_PATHS or not body:
        return body, None, None
    try:
        payload = json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return body, None, None
    original, translated = _rewrite_model_object(payload, mappings)
    if not original or original == translated:
        return body, original, translated
    return (
        json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8"),
        original,
        translated,
    )


def _iter_copilot_usage(value: Any) -> Iterable[dict[str, Any]]:
    if isinstance(value, dict):
        usage = value.get("copilot_usage")
        if isinstance(usage, dict):
            yield usage
        for child in value.values():
            yield from _iter_copilot_usage(child)
    elif isinstance(value, list):
        for child in value:
            yield from _iter_copilot_usage(child)


def _safe_int(value: Any) -> int:
    try:
        return max(0, int(value))
    except (TypeError, ValueError, OverflowError):
        return 0


class CopilotUsageAccumulator:
    """Extract the final cumulative Copilot billing record from JSON or SSE."""

    def __init__(self) -> None:
        self.total_nano_aiu: int | None = None
        self.token_details: list[dict[str, Any]] = []
        self._body = bytearray()
        self._line_buffer = bytearray()
        self._is_sse = False

    def set_content_type(self, content_type: str) -> None:
        self._is_sse = "text/event-stream" in content_type.lower()

    def _accept(self, value: Any) -> None:
        for usage in _iter_copilot_usage(value):
            total = usage.get("total_nano_aiu")
            if total is not None:
                parsed = _safe_int(total)
                if self.total_nano_aiu is None or parsed >= self.total_nano_aiu:
                    self.total_nano_aiu = parsed
                    details = usage.get("token_details")
                    if isinstance(details, list):
                        self.token_details = [d for d in details if isinstance(d, dict)]

    def _accept_json_bytes(self, raw: bytes) -> None:
        raw = raw.strip()
        if not raw or raw == b"[DONE]":
            return
        try:
            self._accept(json.loads(raw))
        except (json.JSONDecodeError, UnicodeDecodeError):
            return

    def feed(self, chunk: bytes) -> None:
        if not chunk:
            return
        if self._is_sse or chunk.lstrip().startswith((b"data:", b"event:")):
            self._is_sse = True
            self._line_buffer.extend(chunk)
            while b"\n" in self._line_buffer:
                line, _, remainder = self._line_buffer.partition(b"\n")
                self._line_buffer = bytearray(remainder)
                line = line.rstrip(b"\r")
                if line.startswith(b"data:"):
                    self._accept_json_bytes(bytes(line[5:].lstrip()))
            return
        if len(self._body) < _CAPTURE_LIMIT:
            room = _CAPTURE_LIMIT - len(self._body)
            self._body.extend(chunk[:room])

    def feed_websocket(self, payload: str | bytes) -> None:
        raw = payload.encode("utf-8") if isinstance(payload, str) else payload
        self._accept_json_bytes(raw)

    def finish(self) -> None:
        if self._line_buffer:
            line = bytes(self._line_buffer).rstrip(b"\r\n")
            if line.startswith(b"data:"):
                self._accept_json_bytes(line[5:].lstrip())
            self._line_buffer.clear()
        if self._body:
            self._accept_json_bytes(bytes(self._body))
            self._body.clear()


class UsageRecorder:
    """Thread-safe, process-local aggregate of authoritative Copilot AIU."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._requests = 0
        self._total_nano_aiu = 0
        self._by_model: dict[str, dict[str, int]] = {}
        self._tokens_by_type: dict[str, int] = {}
        self._last_updated: float | None = None

    def record(self, model: str | None, accumulator: CopilotUsageAccumulator) -> None:
        accumulator.finish()
        if accumulator.total_nano_aiu is None:
            return
        model_name = model or "unknown"
        with self._lock:
            self._requests += 1
            self._total_nano_aiu += accumulator.total_nano_aiu
            row = self._by_model.setdefault(model_name, {"requests": 0, "nano_aiu": 0})
            row["requests"] += 1
            row["nano_aiu"] += accumulator.total_nano_aiu
            for detail in accumulator.token_details:
                token_type = str(detail.get("token_type") or "unknown")
                self._tokens_by_type[token_type] = self._tokens_by_type.get(token_type, 0) + _safe_int(
                    detail.get("token_count")
                )
            self._last_updated = time.time()

    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            return {
                "source": "upstream copilot_usage",
                "requests": self._requests,
                "total_nano_aiu": self._total_nano_aiu,
                "total_ai_credits": round(self._total_nano_aiu / _NANO_AIU_PER_CREDIT, 9),
                "by_model": {
                    model: {
                        **row,
                        "ai_credits": round(row["nano_aiu"] / _NANO_AIU_PER_CREDIT, 9),
                    }
                    for model, row in sorted(self._by_model.items())
                },
                "tokens_by_type": dict(sorted(self._tokens_by_type.items())),
                "last_updated": self._last_updated,
            }


async def _collect_request(receive: Callable[[], Awaitable[dict[str, Any]]]) -> tuple[bytes, bool]:
    chunks: list[bytes] = []
    disconnected = False
    while True:
        message = await receive()
        kind = message.get("type")
        if kind == "http.disconnect":
            disconnected = True
            break
        if kind != "http.request":
            continue
        chunks.append(message.get("body", b""))
        if not message.get("more_body", False):
            break
    return b"".join(chunks), disconnected


def _replace_content_length(scope: dict[str, Any], length: int) -> dict[str, Any]:
    copied = dict(scope)
    headers = []
    replaced = False
    for name, value in scope.get("headers", []):
        if name.lower() == b"content-length":
            headers.append((name, str(length).encode("ascii")))
            replaced = True
        else:
            headers.append((name, value))
    if not replaced:
        headers.append((b"content-length", str(length).encode("ascii")))
    copied["headers"] = headers
    return copied


class GhcCompatibilityMiddleware:
    """Model aliasing plus non-invasive Copilot billing observation."""

    def __init__(
        self,
        app: Any,
        *,
        mappings: ModelMappings,
        recorder: UsageRecorder,
        copilot_target: str = "https://api.githubcopilot.com",
    ) -> None:
        self.app = app
        self.mappings = mappings
        self.recorder = recorder
        self.copilot_target = copilot_target.rstrip("/")

    async def _authenticated_scope(self, scope: dict[str, Any]) -> dict[str, Any]:
        path = _logical_path(str(scope.get("path") or ""))
        is_inline_completion = path.startswith("/v1/engines/") and path.endswith(
            "/completions"
        )
        if path not in _MODEL_BODY_PATHS and not is_inline_completion:
            return scope

        from headroom.copilot_auth import apply_copilot_api_auth

        headers = {
            name.decode("latin-1"): value.decode("latin-1")
            for name, value in scope.get("headers", [])
        }
        resolved = await apply_copilot_api_auth(
            headers,
            url=f"{self.copilot_target}{path}",
        )
        copied = dict(scope)
        copied["headers"] = [
            (str(name).encode("latin-1"), str(value).encode("latin-1"))
            for name, value in resolved.items()
        ]
        return copied

    async def __call__(self, scope: dict[str, Any], receive: Any, send: Any) -> None:
        scope = await self._authenticated_scope(scope)
        scope_type = scope.get("type")
        if scope_type == "http":
            await self._http(scope, receive, send)
            return
        if scope_type == "websocket":
            await self._websocket(scope, receive, send)
            return
        await self.app(scope, receive, send)

    async def _http(self, scope: dict[str, Any], receive: Any, send: Any) -> None:
        path = str(scope.get("path") or "")
        logical = _logical_path(path)
        original_model: str | None = None
        effective_model: str | None = None
        next_scope = scope
        next_receive = receive

        if scope.get("method") == "POST" and logical in _MODEL_BODY_PATHS:
            body, disconnected = await _collect_request(receive)
            if disconnected:
                return
            rewritten, original_model, effective_model = rewrite_json_body(path, body, self.mappings)
            next_scope = _replace_content_length(scope, len(rewritten))
            delivered = False

            async def replay() -> dict[str, Any]:
                nonlocal delivered
                if not delivered:
                    delivered = True
                    return {"type": "http.request", "body": rewritten, "more_body": False}
                return {"type": "http.disconnect"}

            next_receive = replay

        accumulator = CopilotUsageAccumulator()
        status_code = 0
        observe = not logical.startswith(_SKIP_OBSERVATION_PREFIXES)

        async def observe_send(message: dict[str, Any]) -> None:
            nonlocal status_code
            kind = message.get("type")
            if observe and kind == "http.response.start":
                status_code = _safe_int(message.get("status"))
                for name, value in message.get("headers", []):
                    if name.lower() == b"content-type":
                        accumulator.set_content_type(value.decode("latin-1", errors="replace"))
            elif observe and kind == "http.response.body":
                accumulator.feed(message.get("body", b""))
                if not message.get("more_body", False) and 200 <= status_code < 400:
                    self.recorder.record(effective_model or original_model, accumulator)
            await send(message)

        await self.app(next_scope, next_receive, observe_send)

    async def _websocket(self, scope: dict[str, Any], receive: Any, send: Any) -> None:
        accumulator = CopilotUsageAccumulator()
        model: str | None = None
        recorded = False

        async def rewrite_receive() -> dict[str, Any]:
            nonlocal model
            message = await receive()
            if message.get("type") != "websocket.receive":
                return message
            payload = message.get("text")
            is_text = payload is not None
            raw = payload.encode("utf-8") if is_text else message.get("bytes")
            if not raw:
                return message
            try:
                value = json.loads(raw)
            except (json.JSONDecodeError, UnicodeDecodeError):
                return message
            original, translated = _rewrite_model_object(value, self.mappings)
            model = translated or original or model
            if original and translated != original:
                encoded = json.dumps(value, separators=(",", ":"), ensure_ascii=False)
                return {"type": "websocket.receive", "text": encoded} if is_text else {
                    "type": "websocket.receive",
                    "bytes": encoded.encode("utf-8"),
                }
            return message

        async def observe_send(message: dict[str, Any]) -> None:
            nonlocal recorded
            if message.get("type") == "websocket.send":
                payload = message.get("text")
                if payload is None:
                    payload = message.get("bytes")
                if payload is not None:
                    accumulator.feed_websocket(payload)
            if message.get("type") == "websocket.close" and not recorded:
                self.recorder.record(model, accumulator)
                recorded = True
            await send(message)

        try:
            await self.app(scope, rewrite_receive, observe_send)
        finally:
            if not recorded:
                self.recorder.record(model, accumulator)


class QuotaBridge:
    """Short-TTL bridge to GitHub's Copilot account snapshot."""

    def __init__(self, ttl_seconds: float = 30.0) -> None:
        self._ttl = ttl_seconds
        self._lock = asyncio.Lock()
        self._raw: dict[str, Any] | None = None
        self._fetched_at = 0.0

    async def fetch(self) -> dict[str, Any]:
        now = time.monotonic()
        if self._raw is not None and now - self._fetched_at < self._ttl:
            return self._raw
        async with self._lock:
            now = time.monotonic()
            if self._raw is not None and now - self._fetched_at < self._ttl:
                return self._raw
            from headroom.copilot_auth import (
                _fetch_copilot_user_info,
                read_headroom_copilot_oauth_token,
            )

            token = next((os.environ.get(name, "").strip() for name in _TOKEN_ENV_VARS if os.environ.get(name, "").strip()), "")
            token = token or (read_headroom_copilot_oauth_token() or "")
            if not token:
                raise RuntimeError("No reusable GitHub Copilot OAuth token is available")
            raw = await asyncio.to_thread(_fetch_copilot_user_info, token)
            if not isinstance(raw, dict):
                raise RuntimeError("GitHub Copilot usage endpoint returned no account snapshot")
            self._raw = raw
            self._fetched_at = now
            return raw


def summarize_quota(raw: Mapping[str, Any], recorder: UsageRecorder) -> dict[str, Any]:
    quotas: dict[str, Any] = {}
    snapshots = raw.get("quota_snapshots")
    if isinstance(snapshots, dict):
        for name, value in snapshots.items():
            if not isinstance(value, dict):
                continue
            quotas[str(name)] = {
                "unlimited": bool(value.get("unlimited")),
                "entitlement": value.get("entitlement"),
                "remaining": value.get("remaining", value.get("quota_remaining")),
                "percent_remaining": value.get("percent_remaining"),
                "credits_used": value.get("credits_used"),
                "overage_count": value.get("overage_count", 0),
                "overage_permitted": bool(value.get("overage_permitted")),
                "timestamp_utc": value.get("timestamp_utc"),
            }
    return {
        "plan": raw.get("copilot_plan"),
        "login": raw.get("login"),
        "token_based_billing": raw.get("token_based_billing"),
        "quota_reset_date": raw.get("quota_reset_date_utc") or raw.get("quota_reset_date"),
        "quotas": quotas,
        "proxy_session": recorder.snapshot(),
    }


def _copilot_host(url: str) -> bool:
    try:
        host = (urlparse(url).hostname or "").lower()
    except ValueError:
        return False
    return host == "githubcopilot.com" or host.endswith(".githubcopilot.com") or (
        host.startswith("copilot-api.") and host.endswith(".ghe.com")
    )


def _loopback(client_host: str | None) -> bool:
    if not client_host:
        return False
    try:
        return ipaddress.ip_address(client_host).is_loopback
    except ValueError:
        return client_host.lower() == "localhost"


def _bridge_saved_oauth() -> str:
    for name in _TOKEN_ENV_VARS:
        if os.environ.get(name, "").strip():
            return f"env:{name}"
    try:
        from headroom.copilot_auth import read_headroom_copilot_oauth_token

        token = read_headroom_copilot_oauth_token()
    except Exception:
        token = None
    if token:
        # The inference transport can read the saved auth file itself. Exporting
        # it here additionally enables Headroom's background Copilot quota
        # tracker, which currently discovers environment credentials only.
        os.environ.setdefault("GITHUB_COPILOT_GITHUB_TOKEN", token)
        return "headroom-copilot-auth"
    return "missing"


def install(app: Any, config: Any) -> None:
    """Install the extension into a Headroom 0.36.x FastAPI application."""

    proxy = getattr(getattr(app, "state", None), "proxy", None)
    if proxy is None:
        raise RuntimeError("Headroom app.state.proxy is unavailable")

    openai_target = str(getattr(proxy, "OPENAI_API_URL", ""))
    anthropic_target = str(getattr(proxy, "ANTHROPIC_API_URL", ""))
    if not _copilot_host(openai_target) or not _copilot_host(anthropic_target):
        raise RuntimeError(
            "ghc extension requires OPENAI_TARGET_API_URL and "
            "ANTHROPIC_TARGET_API_URL to point at GitHub Copilot before app creation"
        )

    os.environ.setdefault("GITHUB_COPILOT_API_URL", openai_target)
    os.environ.setdefault("GITHUB_COPILOT_USE_TOKEN_EXCHANGE", "true")
    auth_source = _bridge_saved_oauth()
    if auth_source == "missing":
        raise RuntimeError(
            "no reusable Copilot OAuth token; run `headroom copilot-auth login` first"
        )

    mappings = load_model_mappings()
    recorder = UsageRecorder()
    quota = QuotaBridge()
    state = {
        "version": PLUGIN_VERSION,
        "transport": "headroom-native-copilot",
        "openai_target": openai_target,
        "anthropic_target": anthropic_target,
        "auth_source": auth_source,
        "model_aliases": {"exact": len(mappings.exact), "prefix": len(mappings.prefix)},
        "standalone_port": None,
    }
    app.state.ghc_plugin = state
    app.add_middleware(
        GhcCompatibilityMiddleware,
        mappings=mappings,
        recorder=recorder,
        copilot_target=openai_target,
    )

    def forbidden() -> JSONResponse:
        return JSONResponse(status_code=403, content={"error": "loopback access required"})

    async def usage_endpoint(request: Request) -> JSONResponse:
        if not _loopback(getattr(request.client, "host", None)):
            return forbidden()
        try:
            raw = await quota.fetch()
            return JSONResponse(content=summarize_quota(raw, recorder))
        except Exception as exc:
            return JSONResponse(status_code=503, content={"error": str(exc)})

    async def cache_endpoint(request: Request) -> JSONResponse:
        if not _loopback(getattr(request.client, "host", None)):
            return forbidden()
        snapshot = recorder.snapshot()
        return JSONResponse(
            content={
                "source": "GitHub copilot_usage observed after Headroom processing",
                "session": snapshot,
                "note": "Provider prompt-cache detail remains available in Headroom /stats.",
            }
        )

    async def health_endpoint(request: Request) -> JSONResponse:
        if not _loopback(getattr(request.client, "host", None)):
            return forbidden()
        return JSONResponse(content={**state, "ready": auth_source != "missing"})

    for path in ("/usage", "/api/usage"):
        app.add_api_route(path, usage_endpoint, methods=["GET"], include_in_schema=False)
    app.add_api_route("/api/cache", cache_endpoint, methods=["GET"], include_in_schema=False)
    app.add_api_route("/api/ghc/health", health_endpoint, methods=["GET"], include_in_schema=False)


__all__ = [
    "CopilotUsageAccumulator",
    "GhcCompatibilityMiddleware",
    "ModelMappings",
    "QuotaBridge",
    "UsageRecorder",
    "install",
    "load_model_mappings",
    "rewrite_json_body",
    "summarize_quota",
]
