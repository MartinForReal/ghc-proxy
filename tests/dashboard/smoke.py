"""Exercise the shipped dashboard in Chromium without a Copilot account."""

import argparse
from pathlib import Path

from playwright.sync_api import Route, expect, sync_playwright

ROOT = Path(__file__).resolve().parents[2]
ORIGIN = "http://dashboard.test"
MODEL = {"id": "demo-chat", "display_name": "Demo Chat", "owned_by": "demo"}
RESPONSES = {
    "/api/stats": {
        "total_input_tokens": 48000,
        "total_cache_read_tokens": 36000,
        "total_output_tokens": 6400,
        "total_reasoning_tokens": 1600,
        "total_nano_aiu": 1250000000,
        "request_count": 24,
        "bytes_sent": 208896,
        "bytes_received": 81920,
    },
    "/health": {
        "ready": True,
        "version": "demo",
        "uptime_seconds": 1080,
        "models_loaded": 1,
        "auth_required": True,
        "copilot_token": {"present": True, "expires_in_seconds": 1020},
    },
    "/usage": {"quotas": {"premium_interactions": {"entitlement": 300, "remaining": 264}}},
    "/api/cache": {"totals": {"input_tokens": 48000, "cache_read_tokens": 36000}},
    "/api/models": {"data": [MODEL]},
}


def run(screenshot: bool) -> None:
    errors = []

    def serve(route: Route) -> None:
        path = route.request.url.removeprefix(ORIGIN)
        if path == "/":
            route.fulfill(path=ROOT / "public" / "dashboard.html", content_type="text/html")
        elif path == "/app.css":
            route.fulfill(path=ROOT / "public" / "app.css", content_type="text/css")
        elif path in RESPONSES:
            route.fulfill(json=RESPONSES[path])
        else:
            errors.append(f"Unexpected request: {route.request.method} {route.request.url}")
            route.abort()

    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        page = browser.new_page(viewport={"width": 1100, "height": 780})
        page.route("**/*", serve)
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.goto(ORIGIN)
        expect(page.locator("#navStatus")).to_contain_text("ready")
        expect(page.locator("#hero")).to_contain_text("75% served from cache")
        expect(page.locator("#quotaBody")).to_contain_text("264 of 300 left")

        if screenshot:
            box = page.locator(".cols-2").bounding_box()
            assert box is not None
            page.screenshot(
                path=ROOT / "docs" / "assets" / "dashboard.png",
                clip={"x": 0, "y": 0, "width": 1100, "height": round(box["y"] + box["height"] + 16)},
            )

        row = page.get_by_role("cell", name=MODEL["display_name"], exact=True)
        expect(row).to_be_hidden()
        page.get_by_text("Supported models (1)", exact=True).click()
        expect(row).to_be_visible()
        browser.close()
        assert not errors, "\n".join(errors)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--screenshot", action="store_true", help="refresh the README screenshot")
    run(parser.parse_args().screenshot)
    print("Dashboard smoke test passed.")
