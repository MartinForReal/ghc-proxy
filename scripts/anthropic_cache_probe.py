"""Why does the Anthropic surface never populate the prompt cache?

The proxy forwards `cache_control` and Copilot echoes the full cache-accounting
shape back -- all zeros. This tries the remaining client-side levers one at a
time so the failing one can be named rather than guessed at.
"""

import json
import sys
import urllib.error
import urllib.request

BASE = f"http://127.0.0.1:{sys.argv[1] if len(sys.argv) > 1 else '8399'}"
MODEL = sys.argv[2] if len(sys.argv) > 2 else "claude-haiku-4.5"


def attempt(label, system, beta=None, ttl=None):
    payload = {
        "model": MODEL,
        "max_tokens": 8,
        "system": system,
        "messages": [{"role": "user", "content": "Reply with the word ok."}],
    }
    headers = {"content-type": "application/json"}
    if beta:
        headers["anthropic-beta"] = beta
    req = urllib.request.Request(
        BASE + "/v1/messages", data=json.dumps(payload).encode(), headers=headers
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            u = json.loads(r.read())["usage"]
    except urllib.error.HTTPError as e:
        print(f"{label:<42}{e.code} {e.read()[:90].decode(errors='replace')}")
        return
    print(
        f"{label:<42}in={u.get('input_tokens'):>6} "
        f"write={u.get('cache_creation_input_tokens'):>6} "
        f"read={u.get('cache_read_input_tokens'):>6}"
    )


def block(chars, ttl=None):
    cc = {"type": "ephemeral"}
    if ttl:
        cc["ttl"] = ttl
    return [{"type": "text", "text": ("cache me. " * 30000)[:chars], "cache_control": cc}]


print(f"model: {MODEL}\n")
# The text the other scripts use. Same character count as the block above but
# far fewer tokens, which is the whole question.
DEMO = ("The proxy records what the upstream reports. " * 520)[:22880]
# Each pair is sent twice: a write can only be observed as a read on the retry.
for label, kwargs in [
    ("4k chars, default beta", dict(system=block(4000))),
    ("23k chars, default beta", dict(system=block(23000))),
    ("92k chars, default beta", dict(system=block(92000))),
    (
        "23k chars, prompt-caching beta",
        dict(system=block(23000), beta="prompt-caching-2024-07-31"),
    ),
    ("23k chars, 1h ttl", dict(system=block(23000, ttl="1h"))),
    ("23k chars, plain string system", dict(system=("cache me. " * 30000)[:23000])),
    (
        "22.9k chars of demo filler",
        dict(
            system=[
                {
                    "type": "text",
                    "text": DEMO,
                    "cache_control": {"type": "ephemeral"},
                }
            ]
        ),
    ),
]:
    attempt(f"{label} (1st)", **kwargs)
    attempt(f"{label} (2nd)", **kwargs)
