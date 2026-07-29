"""Inventory the statistics upstream puts in terminal stream events.

Every surface ends a turn with a frame carrying more than the token counts the
proxy already records — Copilot's own billing units, latency checkpoints,
reasoning-token splits. This walks the captured debug bodies and reports which
of those fields actually appear, per endpoint, so what gets surfaced is chosen
from evidence rather than from guesses about the schema.

Run against a proxy that has served traffic with body capture on.
"""

from __future__ import annotations

import argparse
import json
import urllib.request
from collections import defaultdict


def frames(raw: str) -> list[dict]:
    """Every JSON frame in a body, whichever transport carried it."""
    out = []
    if raw.lstrip().startswith(("event:", "data:")):
        for block in raw.split("\n\n"):
            for line in block.splitlines():
                if line.startswith("data:"):
                    payload = line[5:].strip()
                    if payload and payload != "[DONE]":
                        try:
                            out.append(json.loads(payload))
                        except ValueError:
                            pass
        return out
    lines = [l for l in raw.splitlines() if l.strip()]
    if len(lines) > 1:
        for line in lines:
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
        return out
    try:
        out.append(json.loads(raw))
    except ValueError:
        pass
    return out


def leaves(obj, prefix="") -> list[tuple[str, object]]:
    """Flattens to dotted paths, collapsing list indices so paths aggregate."""
    found = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            found += leaves(v, f"{prefix}.{k}" if prefix else k)
    elif isinstance(obj, list):
        for v in obj[:1]:
            found += leaves(v, f"{prefix}[]")
    else:
        found.append((prefix, obj))
    return found


# Paths the proxy already records. Everything else is a candidate.
KNOWN = {
    "usage.input_tokens", "usage.output_tokens",
    "usage.cache_read_input_tokens", "usage.cache_creation_input_tokens",
    "usage.prompt_tokens", "usage.completion_tokens", "usage.total_tokens",
    "response.usage.input_tokens", "response.usage.output_tokens",
}

# Fragments that mark a value as a statistic worth considering.
INTERESTING = ("usage", "token", "latency", "ms", "duration", "nano", "aiu",
               "cost", "batch", "reasoning", "cached", "tier", "fingerprint")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8399)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--per-page", type=int, default=100)
    args = ap.parse_args()

    url = f"http://{args.host}:{args.port}/api/requests?page=1&per_page={args.per_page}"
    with urllib.request.urlopen(url, timeout=30) as r:
        items = json.load(r).get("items") or []

    # path -> endpoint -> (count, sample)
    seen: dict[str, dict[str, tuple[int, object]]] = defaultdict(lambda: defaultdict(lambda: (0, None)))
    bodies = 0

    for rec in items:
        raw = rec.get("response_body")
        if not raw:
            continue
        bodies += 1
        ep = rec.get("endpoint", "?")
        for fr in frames(raw):
            for path, value in leaves(fr):
                low = path.lower()
                if not any(f in low for f in INTERESTING):
                    continue
                if path in KNOWN:
                    continue
                n, sample = seen[path][ep]
                seen[path][ep] = (n + 1, value if sample is None else sample)

    if not bodies:
        print("No captured bodies. Turn on body capture and send some traffic first.")
        return 1

    print(f"{bodies} captured bodies across {len({i.get('endpoint') for i in items})} endpoints\n")
    width = max((len(p) for p in seen), default=10)
    for path in sorted(seen):
        eps = seen[path]
        total = sum(n for n, _ in eps.values())
        sample = next((s for _, s in eps.values() if s is not None), None)
        where = ",".join(sorted(eps))
        print(f"{path.ljust(width)}  x{total:<5} {str(sample)[:40]:<42} {where}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
