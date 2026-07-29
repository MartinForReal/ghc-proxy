"""Sweeps models across every surface and reports which ones yielded cache facts.

Answers one question: when the dashboard shows a dash, is that because the
model reported nothing, or because the proxy failed to read what it reported?
Needs body capture on, so the raw upstream payload can be inspected.
"""

import json
import sys
import urllib.error
import urllib.request

BASE = f"http://127.0.0.1:{sys.argv[1] if len(sys.argv) > 1 else '8399'}"

# Long enough to be worth caching, and identical across calls so a second pass
# can read the first one back.
FILLER = ("The proxy records what the upstream reports. " * 520)[:22880]


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
        return f"{e.code}"
    except Exception as e:  # noqa: BLE001
        return str(e)[:40]


def chat(model, stream):
    return call(
        "/v1/chat/completions",
        {
            "model": model,
            "max_tokens": 24,
            "stream": stream,
            "messages": [
                {"role": "system", "content": FILLER},
                {"role": "user", "content": "Reply with the word ok."},
            ],
        },
    )


def messages(model, stream):
    return call(
        "/v1/messages",
        {
            "model": model,
            "max_tokens": 24,
            "stream": stream,
            "system": [
                {
                    "type": "text",
                    "text": FILLER,
                    "cache_control": {"type": "ephemeral"},
                }
            ],
            "messages": [{"role": "user", "content": "Reply with the word ok."}],
        },
    )


def responses(model, stream):
    return call(
        "/v1/responses",
        {
            "model": model,
            "max_output_tokens": 24,
            "stream": stream,
            "instructions": FILLER,
            "input": "Reply with the word ok.",
        },
    )


def gemini(model, stream):
    action = "streamGenerateContent" if stream else "generateContent"
    return call(
        f"/v1beta/models/{model}:{action}",
        {
            "systemInstruction": {"parts": [{"text": FILLER}]},
            "contents": [{"role": "user", "parts": [{"text": "Reply with the word ok."}]}],
            "generationConfig": {"maxOutputTokens": 24},
        },
    )


# `/api/models` is the plain OpenAI list; only the full catalogue carries
# `supported_endpoints`, which is what decides where each model can be called.
with urllib.request.urlopen(BASE + "/v1/models/full/", timeout=30) as r:
    catalog = json.loads(r.read())
supported = {
    m["id"]: set(m.get("supported_endpoints") or [])
    for m in (catalog.get("data") or catalog.get("models") or [])
}

PLAN = [
    ("chat", "/chat/completions", chat),
    ("messages", "/v1/messages", messages),
    ("responses", "/responses", responses),
]

# One representative per family, so the sweep stays inside the rate limit.
WANTED = [
    "claude-haiku-4.5",
    "claude-sonnet-4.5",
    "claude-opus-4.6",
    "gemini-3.5-flash",
    "gemini-3-pro",
    "gpt-5-mini",
    "gpt-5.4",
    "gpt-5.5",
    "gpt-5.3-codex",
    "grok-4.5",
]

for model in WANTED:
    caps = supported.get(model)
    if caps is None:
        print(f"  {model}: not in catalog")
        continue
    for label, endpoint, fn in PLAN:
        if endpoint not in caps:
            continue
        for stream in (False, True):
            err = fn(model, stream)
            tag = f"{model} {label}{' stream' if stream else ''}"
            print(f"  {tag}: {err or 'ok'}")
    if "/chat/completions" in caps and model.startswith("gemini"):
        for stream in (False, True):
            err = gemini(model, stream)
            print(f"  {model} gemini{' stream' if stream else ''}: {err or 'ok'}")

print()
audit = json.loads(urllib.request.urlopen(BASE + "/api/audit?per_page=200", timeout=30).read())
print(f"{'endpoint':<26}{'model':<20}{'body':>10}{'copilot_usage':>15}{'details':>9}")
seen = set()
for i in audit["items"]:
    body = i.get("response_body")
    # An uncaptured body is its own state; calling it a stream would hide the
    # fact that this row proves nothing either way.
    shape = "none" if not body else "json" if body.startswith("{") else "stream"
    key = (i["endpoint"], i["model"], shape)
    if key in seen:
        continue
    seen.add(key)
    print(
        f"{i['endpoint']:<26}{i['model']:<20}{shape:>10}"
        f"{str('copilot_usage' in (body or '')):>15}"
        f"{str('token_details' in (body or '')):>9}"
    )

print()
cache = json.loads(urllib.request.urlopen(BASE + "/api/cache", timeout=30).read())
print(f"{'model':<24}{'in':>8}{'read':>8}{'write':>8}{'writes?':>9}{'saved':>14}")
for m in cache["by_model"]:
    print(
        f"{m['model']:<24}{m['input_tokens']:>8}{m['cache_read_tokens']:>8}"
        f"{m['cache_creation_tokens']:>8}{str(m['prices_cache_write']):>9}"
        f"{str(m['saved_nano_aiu']):>14}"
    )
