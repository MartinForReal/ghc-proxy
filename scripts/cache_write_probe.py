"""Does the Written column ever carry data outside the Anthropic surface?

Sends a prefix well past the minimum cacheable size twice per surface: the
first call should populate the cache, the second read it back. If a surface
never reports a write, the column is dead weight there and the dashboard should
say so rather than print a zero.
"""

import json
import sys
import urllib.error
import urllib.request

BASE = f"http://127.0.0.1:{sys.argv[1] if len(sys.argv) > 1 else '8399'}"

# ~28k tokens: past every observed minimum, so a surface that can write will.
BIG = ("cache me. " * 40000)[:92000]


def call(path, payload):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=180) as r:
            r.read()
        return None
    except urllib.error.HTTPError as e:
        return str(e.code)
    except Exception as e:  # noqa: BLE001
        return str(e)[:40]


def chat(model):
    return call(
        "/v1/chat/completions",
        {
            "model": model,
            "max_tokens": 8,
            "messages": [
                {"role": "system", "content": BIG},
                {"role": "user", "content": "Reply with the word ok."},
            ],
        },
    )


def messages(model):
    return call(
        "/v1/messages",
        {
            "model": model,
            "max_tokens": 8,
            "system": [
                {"type": "text", "text": BIG, "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{"role": "user", "content": "Reply with the word ok."}],
        },
    )


def responses(model):
    return call(
        "/v1/responses",
        {
            "model": model,
            "max_output_tokens": 16,
            "instructions": BIG,
            "input": "Reply with the word ok.",
        },
    )


PLAN = [
    ("claude-haiku-4.5", "/v1/messages", messages),
    ("claude-haiku-4.5", "/chat/completions", chat),
    ("gemini-3.5-flash", "/chat/completions", chat),
    ("gpt-5-mini", "/chat/completions", chat),
    ("gpt-5-mini", "/responses", responses),
    ("gpt-5.5", "/responses", responses),
]

before = {
    i["id"]
    for i in json.loads(
        urllib.request.urlopen(BASE + "/api/requests?per_page=500", timeout=30).read()
    )["items"]
}

for model, endpoint, fn in PLAN:
    for turn in (1, 2):
        err = fn(model)
        if err:
            print(f"  {model} {endpoint} turn {turn}: {err}")

items = json.loads(
    urllib.request.urlopen(BASE + "/api/requests?per_page=500", timeout=30).read()
)["items"]
fresh = [i for i in items if i["id"] not in before]
fresh.reverse()

print()
print(f"{'endpoint':<22}{'model':<20}{'in':>8}{'read':>8}{'write':>8}{'prices?':>9}")
for i in fresh:
    print(
        f"{i['endpoint']:<22}{i['model']:<20}{i['input_tokens']:>8}"
        f"{i['cache_read_input_tokens']:>8}{i['cache_creation_input_tokens']:>8}"
        f"{str(i.get('prices_cache_write')):>9}"
    )
