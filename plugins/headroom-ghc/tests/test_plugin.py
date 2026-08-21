from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from headroom_ghc_plugin import (
    CopilotUsageAccumulator,
    GhcCompatibilityMiddleware,
    ModelMappings,
    UsageRecorder,
    load_model_mappings,
    rewrite_json_body,
    summarize_quota,
)


class ModelMappingTests(unittest.TestCase):
    def test_exact_and_longest_prefix(self) -> None:
        mappings = ModelMappings(
            exact={"sonnet": "claude-sonnet-5"},
            prefix={"claude-": "broad", "claude-sonnet-": "specific"},
        )
        self.assertEqual(mappings.translate("sonnet"), "claude-sonnet-5")
        self.assertEqual(mappings.translate("claude-sonnet-old"), "specific")
        self.assertEqual(mappings.translate("gpt-5"), "gpt-5")

    def test_yaml_loader_reads_only_mappings(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "config.yaml"
            path.write_text(
                "api_key: secret-that-must-not-surface\n"
                "model_mappings:\n"
                "  exact:\n"
                "    opus: claude-opus-5\n"
                "  prefix:\n"
                "    old-: new-model\n",
                encoding="utf-8",
            )
            mappings = load_model_mappings(path)
        self.assertEqual(mappings.translate("opus"), "claude-opus-5")
        self.assertEqual(mappings.translate("old-v1"), "new-model")

    def test_rewrite_json_body(self) -> None:
        mappings = ModelMappings(exact={"sonnet": "claude-sonnet-5"})
        body, original, translated = rewrite_json_body(
            "/p/repo/v1/messages",
            b'{"model":"sonnet","messages":[]}',
            mappings,
        )
        self.assertEqual(original, "sonnet")
        self.assertEqual(translated, "claude-sonnet-5")
        self.assertEqual(json.loads(body)["model"], "claude-sonnet-5")


class UsageTests(unittest.TestCase):
    def test_extracts_json_usage(self) -> None:
        acc = CopilotUsageAccumulator()
        acc.feed(
            json.dumps(
                {
                    "copilot_usage": {
                        "total_nano_aiu": 123,
                        "token_details": [
                            {"token_type": "input", "token_count": 10},
                            {"token_type": "cache_read", "token_count": 90},
                        ],
                    }
                }
            ).encode()
        )
        acc.finish()
        self.assertEqual(acc.total_nano_aiu, 123)
        self.assertEqual(len(acc.token_details), 2)

    def test_sse_uses_final_cumulative_usage_once(self) -> None:
        acc = CopilotUsageAccumulator()
        acc.set_content_type("text/event-stream")
        acc.feed(b'data: {"copilot_usage":{"total_nano_aiu":10}}\n')
        acc.feed(
            b'data: {"copilot_usage":{"total_nano_aiu":25,'
            b'"token_details":[{"token_type":"output","token_count":3}]}}\n\n'
        )
        recorder = UsageRecorder()
        recorder.record("gpt", acc)
        snapshot = recorder.snapshot()
        self.assertEqual(snapshot["requests"], 1)
        self.assertEqual(snapshot["total_nano_aiu"], 25)
        self.assertEqual(snapshot["tokens_by_type"]["output"], 3)

    def test_quota_summary_keeps_authoritative_credits_used(self) -> None:
        raw = {
            "login": "user",
            "copilot_plan": "enterprise",
            "token_based_billing": True,
            "quota_reset_date_utc": "2026-09-01T00:00:00Z",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 100,
                    "remaining": 80,
                    "credits_used": 21,
                    "percent_remaining": 80.0,
                    "overage_permitted": True,
                }
            },
        }
        result = summarize_quota(raw, UsageRecorder())
        quota = result["quotas"]["premium_interactions"]
        self.assertEqual(quota["credits_used"], 21)
        self.assertTrue(quota["overage_permitted"])


class MiddlewareTests(unittest.TestCase):
    def test_http_injects_copilot_auth_when_client_omits_it(self) -> None:
        seen: dict = {}

        async def downstream(scope, receive, send) -> None:
            seen["headers"] = {
                name.decode("latin-1").lower(): value.decode("latin-1")
                for name, value in scope["headers"]
            }
            await receive()
            await send(
                {
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"application/json")],
                }
            )
            await send({"type": "http.response.body", "body": b"{}", "more_body": False})

        async def inject(headers, *, url):
            self.assertEqual(url, "https://api.githubcopilot.com/v1/messages")
            return {**headers, "Authorization": "Bearer tid_injected"}

        middleware = GhcCompatibilityMiddleware(
            downstream,
            mappings=ModelMappings(),
            recorder=UsageRecorder(),
        )
        messages = [
            {
                "type": "http.request",
                "body": b'{"model":"claude-haiku-4.5","messages":[]}',
                "more_body": False,
            }
        ]

        async def receive() -> dict:
            return messages.pop(0)

        async def send(_message: dict) -> None:
            return None

        scope = {
            "type": "http",
            "method": "POST",
            "path": "/v1/messages",
            "headers": [(b"content-type", b"application/json")],
        }
        with patch("headroom.copilot_auth.apply_copilot_api_auth", side_effect=inject):
            asyncio.run(middleware(scope, receive, send))

        self.assertEqual(seen["headers"]["authorization"], "Bearer tid_injected")

    def test_http_alias_and_usage_observation(self) -> None:
        seen: dict = {}
        recorder = UsageRecorder()

        async def downstream(scope, receive, send) -> None:
            seen["scope"] = scope
            seen["body"] = (await receive())["body"]
            payload = json.dumps(
                {"copilot_usage": {"total_nano_aiu": 77, "token_details": []}}
            ).encode()
            await send(
                {
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"application/json")],
                }
            )
            await send({"type": "http.response.body", "body": payload, "more_body": False})

        middleware = GhcCompatibilityMiddleware(
            downstream,
            mappings=ModelMappings(exact={"sonnet": "claude-sonnet-5"}),
            recorder=recorder,
        )
        request_messages = [
            {
                "type": "http.request",
                "body": b'{"model":"sonnet","messages":[]}',
                "more_body": False,
            }
        ]
        sent: list[dict] = []

        async def receive() -> dict:
            return request_messages.pop(0)

        async def send(message: dict) -> None:
            sent.append(message)

        scope = {
            "type": "http",
            "method": "POST",
            "path": "/v1/messages",
            "headers": [
                (b"content-type", b"application/json"),
                (b"authorization", b"Bearer tid_test-only"),
            ],
        }
        asyncio.run(middleware(scope, receive, send))

        self.assertEqual(json.loads(seen["body"])["model"], "claude-sonnet-5")
        forwarded_headers = dict(seen["scope"]["headers"])
        self.assertEqual(forwarded_headers[b"authorization"], b"Bearer tid_test-only")
        self.assertEqual(recorder.snapshot()["total_nano_aiu"], 77)
        self.assertEqual(sent[-1]["type"], "http.response.body")


class InstallTests(unittest.TestCase):
    def test_health_endpoint_accepts_request_object(self) -> None:
        from fastapi import FastAPI
        from fastapi.testclient import TestClient
        from headroom_ghc_plugin import install

        app = FastAPI()
        app.state.proxy = type(
            "Proxy",
            (),
            {
                "OPENAI_API_URL": "https://api.githubcopilot.com",
                "ANTHROPIC_API_URL": "https://api.githubcopilot.com",
            },
        )()
        with patch.dict(
            os.environ,
            {"GITHUB_COPILOT_GITHUB_TOKEN": "test-only-token"},
            clear=False,
        ):
            install(app, object())
            with TestClient(app, client=("127.0.0.1", 50000)) as client:
                response = client.get("/api/ghc/health")
        self.assertEqual(response.status_code, 200)
        self.assertTrue(response.json()["ready"])


if __name__ == "__main__":
    unittest.main()
