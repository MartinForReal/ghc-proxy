"""Checks that every Claude alias resolves to a model the upstream will serve.

A mapping table is only correct if the name it produces is one Copilot accepts,
so this asks the live upstream rather than comparing strings.

Note this exercises the *running proxy's* `config.yaml`, not the built-in
defaults. A hand-tuned file may be missing a prefix the defaults carry, in which
case the alias falls through unmapped and Copilot rejects it -- that is a
finding about that file. The defaults themselves are covered by the unit tests
in `src/translate.rs`.
"""

import json
import sys
import urllib.error
import urllib.request

BASE = f"http://127.0.0.1:{sys.argv[1] if len(sys.argv) > 1 else '8399'}"

ALIASES = [
    "opus",
    "sonnet",
    "haiku",
    "opus5",
    "5[1m]",
    "opus4-8",
    "claude-opus-4.6",
    "claude-opus-4.8",
    "claude-opus-5",
    "claude-sonnet-4-5",
    "claude-sonnet-4.6",
    "claude-sonnet-5",
    "claude-sonnet-4-20250101",
    "claude-haiku-4.5",
]

failures = 0
for alias in ALIASES:
    req = urllib.request.Request(
        BASE + "/v1/messages",
        data=json.dumps(
            {
                "model": alias,
                "max_tokens": 8,
                "messages": [{"role": "user", "content": "Reply with the word ok."}],
            }
        ).encode(),
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            served = json.loads(r.read()).get("model", "?")
        print(f"  {alias:<26} -> {served}")
    except urllib.error.HTTPError as e:
        failures += 1
        print(f"  {alias:<26} -> HTTP {e.code} {e.read()[:120].decode(errors='replace')}")

print()
print("all aliases served" if failures == 0 else f"{failures} alias(es) failed")
sys.exit(1 if failures else 0)
