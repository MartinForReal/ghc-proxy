"""Learn what the Copilot `ws:/responses` surface speaks.

The catalogue advertises `ws:/responses` for several models and the endpoint
answers `101 Switching Protocols`, but there is no documentation for it. This
opens one connection, sends one Responses-API request, and prints every frame
that comes back, so the framing can be read off the wire instead of guessed.

Nothing here prints the Copilot token.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import pathlib
import urllib.request

import websockets

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


def copilot_token() -> tuple[str, str]:
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
    api = (d.get("endpoints") or {}).get("api") or "https://api.githubcopilot.com"
    return d["token"], api


def summarize(frame: str, full: bool) -> str:
    try:
        j = json.loads(frame)
    except ValueError:
        return f"(non-JSON, {len(frame)}B) {frame[:200]!r}"
    if full:
        return json.dumps(j, indent=2)[:2000]
    t = j.get("type") or "?"
    bits = [f"type={t}"]
    for k in ("delta", "status", "sequence_number", "output_index", "content_index", "error", "code", "message"):
        if k in j and not isinstance(j[k], (dict, list)):
            v = str(j[k])
            bits.append(f"{k}={v[:80]!r}" if k == "delta" else f"{k}={v[:80]}")
    if "response" in j and isinstance(j["response"], dict):
        r = j["response"]
        bits.append(f"response.status={r.get('status')}")
        if r.get("usage"):
            bits.append(f"usage={json.dumps(r['usage'])[:120]}")
    if not isinstance(j.get("type"), str):
        bits.append(f"keys={sorted(j.keys())[:10]}")
    return " ".join(bits)


async def run(model: str, envelope: str, full: bool, path: str, limit: int,
              raw: str | None, query: str, init: str | None) -> None:
    token, api = copilot_token()
    host = api.split("://", 1)[-1].rstrip("/")
    url = f"wss://{host}{path}" + (f"?{query}" if query else "")

    headers = {
        "Authorization": f"Bearer {token}",
        "User-Agent": UA,
        "Editor-Version": EDITOR,
        "Editor-Plugin-Version": "copilot-chat/0.26.7",
        "Copilot-Integration-Id": "vscode-chat",
        "Openai-Intent": "conversation-edits",
    }

    body = {
        "model": model,
        "stream": True,
        "input": [{"role": "user", "content": "Reply with exactly: ok"}],
    }
    payloads = {
        "bare": body,
        "typed": {"type": "response.create", "response": body},
        "wrapped": {"type": "request", "payload": body},
        "topmodel": {"type": "response.create", "model": model, "response": body},
    }
    payload = json.loads(raw) if raw else payloads[envelope]

    print(f"connect {url}")
    if init:
        print(f"init    {init[:200]}")
    print(f"send    {json.dumps(payload)[:300]}\n")

    async with websockets.connect(url, additional_headers=headers, max_size=None) as ws:
        if init:
            await ws.send(init)
        await ws.send(json.dumps(payload))
        n = 0
        try:
            while n < limit:
                frame = await asyncio.wait_for(ws.recv(), timeout=60)
                n += 1
                text = frame if isinstance(frame, str) else frame.decode("utf-8", "replace")
                print(f"[{n:>3}] {summarize(text, full)}")
                try:
                    if json.loads(text).get("type") in ("response.completed", "response.failed",
                                                        "response.incomplete", "error"):
                        break
                except ValueError:
                    pass
        except asyncio.TimeoutError:
            print("(timed out waiting for the next frame)")
        except websockets.ConnectionClosed as e:
            print(f"(closed: code={e.code} reason={e.reason!r})")
    print(f"\n{n} frames")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="gpt-5.5")
    ap.add_argument("--envelope", default="bare",
                    choices=("bare", "typed", "wrapped", "topmodel"))
    ap.add_argument("--path", default="/responses")
    ap.add_argument("--query", default="", help="URL query string, e.g. model=gpt-5.5")
    ap.add_argument("--raw", default=None, help="send this exact JSON instead of a preset")
    ap.add_argument("--raw-file", default=None,
                    help="send the JSON in this file (avoids shell quoting)")
    ap.add_argument("--init", default=None, help="send this JSON first, before the payload")
    ap.add_argument("--full", action="store_true", help="print whole frames, not a summary")
    ap.add_argument("--limit", type=int, default=40)
    args = ap.parse_args()
    raw = args.raw
    if args.raw_file:
        raw = pathlib.Path(args.raw_file).read_text(encoding="utf-8")
    asyncio.run(run(args.model, args.envelope, args.full, args.path, args.limit,
                    raw, args.query, args.init))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
