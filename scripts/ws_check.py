"""Conformance sweep for the proxy's `ws:/responses` surface, across every
model that advertises it.

The catalogue is the source of truth for which models support this transport,
so the model list is discovered rather than hard-coded — a model added or
withdrawn upstream changes what gets tested without editing this file.

Each model is checked for the properties that make the transport usable:
the event vocabulary matches the SSE Responses API, sequence numbers are
monotonic, the turn terminates, text actually arrives, and usage is reported.
Models that do *not* advertise the transport are checked for a clean refusal —
an unsupported model must be told so, not left waiting on a socket.

Requires the proxy running with `websockets` installed:

    python scripts/ws_check.py --port 8399
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import urllib.request

import websockets

# Events the upstream emits for a plain text turn. Anything here that never
# arrives means the transport is not carrying a full Responses stream.
REQUIRED_EVENTS = ("response.created", "response.output_text.delta", "response.completed")


def catalogue(base: str) -> list[dict]:
    with urllib.request.urlopen(base + "/v1/models/full/", timeout=30) as r:
        return json.load(r).get("data") or []


def ws_models(base: str) -> tuple[list[str], list[str]]:
    """(models advertising ws:/responses, models that do not)."""
    yes, no = [], []
    for m in catalogue(base):
        eps = m.get("supported_endpoints") or []
        (yes if "ws:/responses" in eps else no).append(m["id"])
    return yes, no


class Check:
    def __init__(self) -> None:
        self.rows: list[tuple[str, bool, str]] = []

    def add(self, name: str, ok: bool, detail: str = "") -> None:
        self.rows.append((name, ok, detail))

    @property
    def failed(self) -> list[tuple[str, bool, str]]:
        return [r for r in self.rows if not r[1]]


async def collect(url: str, payload: dict, limit: int, timeout: float) -> list[dict]:
    """Sends one frame and returns every JSON event received until the turn ends."""
    events: list[dict] = []
    async with websockets.connect(url, max_size=None) as ws:
        await ws.send(json.dumps(payload))
        while len(events) < limit:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=timeout)
            except asyncio.TimeoutError:
                break
            except websockets.ConnectionClosed:
                break
            text = raw if isinstance(raw, str) else raw.decode("utf-8", "replace")
            try:
                ev = json.loads(text)
            except ValueError:
                events.append({"type": "__non_json__", "raw": text[:200]})
                continue
            events.append(ev)
            if ev.get("type") in ("response.completed", "response.failed",
                                  "response.incomplete", "error"):
                break
    return events


async def check_model(url: str, model: str, chk: Check, timeout: float) -> None:
    p = model
    payload = {
        "type": "response.create",
        "model": model,
        "input": "Reply with exactly: ok",
        "stream": True,
    }
    try:
        events = await collect(url, payload, limit=400, timeout=timeout)
    except Exception as e:  # noqa: BLE001
        chk.add(f"{p}: connects", False, f"{type(e).__name__}: {e}")
        return

    chk.add(f"{p}: connects", True)
    types = [e.get("type") for e in events]

    err = next((e for e in events if e.get("type") == "error"), None)
    if err:
        chk.add(f"{p}: no error frame", False, json.dumps(err.get("error"))[:160])
        return
    chk.add(f"{p}: no error frame", True)

    for want in REQUIRED_EVENTS:
        chk.add(f"{p}: emits {want}", want in types, ",".join(t for t in types[:8] if t))

    chk.add(f"{p}: terminates", bool(types) and types[-1] in
            ("response.completed", "response.incomplete"), str(types[-1] if types else None))

    seqs = [e["sequence_number"] for e in events if isinstance(e.get("sequence_number"), int)]
    chk.add(f"{p}: sequence numbers monotonic",
            seqs == sorted(seqs) and len(set(seqs)) == len(seqs),
            f"{seqs[:12]}")

    said = "".join(e.get("delta") or "" for e in events
                   if e.get("type") == "response.output_text.delta")
    chk.add(f"{p}: produced text", bool(said.strip()), repr(said[:60]))

    final = next((e for e in reversed(events)
                  if e.get("type") in ("response.completed", "response.incomplete")), None)
    usage = ((final or {}).get("response") or {}).get("usage") or {}
    chk.add(f"{p}: reports usage",
            isinstance(usage.get("input_tokens"), int) and isinstance(usage.get("output_tokens"), int),
            json.dumps(usage)[:120])
    # The model that answered should be the one asked for, or the mapping target.
    served = ((final or {}).get("response") or {}).get("model")
    chk.add(f"{p}: echoes a model", bool(served), str(served))


async def check_refusal(url: str, model: str, chk: Check, timeout: float) -> None:
    """A model without the transport must be refused, promptly and in the
    upstream's own error shape — not left hanging."""
    p = f"{model} (unsupported)"
    payload = {"type": "response.create", "model": model, "input": "hi", "stream": True}
    try:
        events = await collect(url, payload, limit=10, timeout=timeout)
    except Exception as e:  # noqa: BLE001
        chk.add(f"{p}: refused cleanly", False, f"{type(e).__name__}: {e}")
        return
    err = next((e for e in events if e.get("type") == "error"), None)
    chk.add(f"{p}: refused with an error frame", err is not None,
            ",".join(str(e.get("type")) for e in events[:4]))
    if err:
        msg = (err.get("error") or {}).get("message", "")
        chk.add(f"{p}: refusal names the alternative", "/v1/responses" in msg, msg[:160])


async def check_bad_frames(url: str, chk: Check, timeout: float) -> None:
    """Malformed input must produce an error frame rather than a hung socket."""
    cases = [
        ("not JSON", "this is not json"),
        ("missing type", json.dumps({"model": "gpt-5.5", "input": "hi"})),
        ("wrong type", json.dumps({"type": "nonsense", "model": "gpt-5.5", "input": "hi"})),
    ]
    for label, frame in cases:
        try:
            async with websockets.connect(url, max_size=None) as ws:
                await ws.send(frame)
                raw = await asyncio.wait_for(ws.recv(), timeout=timeout)
                ev = json.loads(raw if isinstance(raw, str) else raw.decode())
            chk.add(f"bad frame ({label}) -> error", ev.get("type") == "error",
                    json.dumps(ev)[:160])
        except asyncio.TimeoutError:
            chk.add(f"bad frame ({label}) -> error", False, "socket hung, no reply")
        except Exception as e:  # noqa: BLE001
            chk.add(f"bad frame ({label}) -> error", False, f"{type(e).__name__}: {e}")


async def main_async(args: argparse.Namespace) -> int:
    base = f"http://{args.host}:{args.port}"
    url = f"ws://{args.host}:{args.port}{args.path}"

    supported, unsupported = ws_models(base)
    if args.models:
        supported = [m.strip() for m in args.models.split(",") if m.strip()]
    print(f"catalogue advertises ws:/responses for {len(supported)} model(s):")
    for m in supported:
        print(f"  {m}")
    print()

    chk = Check()
    await check_bad_frames(url, chk, args.timeout)
    for m in supported:
        await check_model(url, m, chk, args.timeout)
        await asyncio.sleep(args.delay)
    if unsupported and not args.skip_refusal:
        await check_refusal(url, unsupported[0], chk, args.timeout)

    width = max(len(r[0]) for r in chk.rows)
    for name, ok, detail in chk.rows:
        line = f"[{'PASS' if ok else 'FAIL'}] {name.ljust(width)}"
        if not ok and detail:
            line += f"  <- {detail}"
        print(line)
    print(f"\n{len(chk.rows) - len(chk.failed)}/{len(chk.rows)} passed")
    return 1 if chk.failed else 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8399)
    ap.add_argument("--path", default="/v1/responses")
    ap.add_argument("--models", default="", help="override the discovered model list")
    ap.add_argument("--timeout", type=float, default=90.0)
    ap.add_argument("--delay", type=float, default=1.0,
                    help="pause between models; bursts of upstream traffic get rate limited")
    ap.add_argument("--skip-refusal", action="store_true")
    args = ap.parse_args()
    try:
        return asyncio.run(main_async(args))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
