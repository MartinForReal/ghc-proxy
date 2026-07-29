"""Probe whether the Copilot API accepts a WebSocket upgrade on /responses.

The model catalogue advertises `ws:/responses` for several models, but GitHub
publishes no documentation for the inference API at all, and the token
response's `endpoints` object carries no websocket URL. So the only way to
learn whether this surface exists — and what it speaks — is to ask it.

This performs a raw RFC 6455 handshake and reports what came back. It sends no
payload and holds no connection open; it is a question, not a client.

Nothing here prints the Copilot token.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import pathlib
import socket
import ssl
import sys
import urllib.request

UA = "GithubCopilot/1.155.0"
EDITOR = "vscode/1.104.0"


def github_token() -> str:
    env = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if env:
        return env.strip()
    p = pathlib.Path.home() / "AppData/Roaming/ghc-tunnel/github_token.txt"
    if not p.exists():
        p = pathlib.Path.home() / ".config/ghc-tunnel/github_token.txt"
    return p.read_text().strip()


def copilot_token() -> tuple[str, dict]:
    req = urllib.request.Request(
        "https://api.github.com/copilot_internal/v2/token",
        headers={
            "Authorization": "token " + github_token(),
            "Accept": "application/json",
            "Editor-Version": EDITOR,
            "User-Agent": UA,
        },
    )
    d = json.load(urllib.request.urlopen(req, timeout=30))
    return d["token"], d.get("endpoints") or {}


def handshake(host: str, path: str, token: str, extra: dict[str, str]) -> tuple[str, dict[str, str], bytes]:
    """Raw RFC 6455 upgrade. Returns (status line, response headers, trailing body)."""
    key = base64.b64encode(os.urandom(16)).decode()
    headers = {
        "Host": host,
        "Upgrade": "websocket",
        "Connection": "Upgrade",
        "Sec-WebSocket-Key": key,
        "Sec-WebSocket-Version": "13",
        "Authorization": f"Bearer {token}",
        "User-Agent": UA,
        "Editor-Version": EDITOR,
        "Editor-Plugin-Version": "copilot-chat/0.26.7",
        "Copilot-Integration-Id": "vscode-chat",
        "Origin": f"https://{host}",
        **extra,
    }
    raw = f"GET {path} HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers.items()) + "\r\n"

    ctx = ssl.create_default_context()
    with socket.create_connection((host, 443), timeout=20) as sock:
        with ctx.wrap_socket(sock, server_hostname=host) as tls:
            tls.sendall(raw.encode())
            buf = b""
            while b"\r\n\r\n" not in buf and len(buf) < 65536:
                chunk = tls.recv(4096)
                if not chunk:
                    break
                buf += chunk

    head, _, rest = buf.partition(b"\r\n\r\n")
    lines = head.decode("utf-8", "replace").split("\r\n")
    status = lines[0] if lines else "(no response)"
    resp_headers = {}
    for line in lines[1:]:
        k, _, v = line.partition(":")
        if k:
            resp_headers[k.strip().lower()] = v.strip()
    return status, resp_headers, rest


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--paths", default="/responses,/v1/responses,/ws/responses,/realtime",
                    help="comma-separated paths to probe")
    args = ap.parse_args()

    token, endpoints = copilot_token()
    api = endpoints.get("api") or "https://api.githubcopilot.com"
    host = api.split("://", 1)[-1].rstrip("/")
    print(f"api host: {host}\n")

    interesting = ("upgrade", "connection", "sec-websocket-accept", "sec-websocket-protocol",
                   "content-type", "x-request-id", "allow")

    for path in [p.strip() for p in args.paths.split(",") if p.strip()]:
        try:
            status, hdrs, body = handshake(host, path, token, {})
        except Exception as e:  # noqa: BLE001 - a probe reports failures, it does not raise
            print(f"{path:<20} ERROR {type(e).__name__}: {e}")
            continue
        shown = {k: v for k, v in hdrs.items() if k in interesting}
        print(f"{path:<20} {status}")
        for k, v in shown.items():
            print(f"{'':<20}   {k}: {v}")
        if body[:400].strip():
            print(f"{'':<20}   body: {body[:300].decode('utf-8', 'replace')!r}")
        print()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
