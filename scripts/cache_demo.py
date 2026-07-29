"""Exercises the prompt cache on models that price it differently.

The point is not that caching works -- that is already covered -- but that the
proxy reports a saving only where the model itself published rates to compute
one from. Run against a proxy with body capture off; only the recorded
statistics matter.
"""

import json
import sys
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8399"

# Long enough to clear Anthropic's minimum cacheable prefix. The content is
# irrelevant; only its length and its being byte-identical across turns matter.
FILLER = ("The proxy records what the upstream reports. " * 520)[:22880]


def post(path, payload):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=180) as r:
        return json.loads(r.read())


def anthropic(turn):
    return post(
        "/v1/messages",
        {
            "model": "claude-haiku-4.5",
            "max_tokens": 32,
            "system": [
                {
                    "type": "text",
                    "text": FILLER,
                    "cache_control": {"type": "ephemeral"},
                }
            ],
            "messages": [{"role": "user", "content": f"Reply with the number {turn}."}],
        },
    )


def chat(model):
    return post(
        "/v1/chat/completions",
        {
            "model": model,
            "max_tokens": 32,
            "messages": [
                {"role": "system", "content": FILLER},
                {"role": "user", "content": "Reply with the word ok."},
            ],
        },
    )


def responses(model):
    return post(
        "/v1/responses",
        {
            "model": model,
            "max_output_tokens": 32,
            "instructions": FILLER,
            "input": "Reply with the word ok.",
        },
    )


print("anthropic turn 1 (populates the cache)")
anthropic(1)
print("anthropic turn 2 (should read it back)")
anthropic(2)
print("gemini-3.5-flash (prices cache writes at zero)")
chat("gemini-3.5-flash")
# Responses-only, and observed to publish no cache_write rate at all -- the
# case that a flat zero in the Written column would misrepresent.
print("gpt-5.5 (publishes no cache-write rate)")
try:
    responses("gpt-5.5")
except Exception as e:  # noqa: BLE001 - a refusal is still a data point
    print(f"  gpt-5.5: {e}")

cache = json.loads(urllib.request.urlopen(BASE + "/api/cache", timeout=30).read())
print()
print(f"{'model':<24}{'in':>9}{'read':>9}{'write':>9}{'writes?':>9}{'saved nAIU':>14}")
for m in cache["by_model"]:
    print(
        f"{m['model']:<24}{m['input_tokens']:>9}{m['cache_read_tokens']:>9}"
        f"{m['cache_creation_tokens']:>9}{str(m['prices_cache_write']):>9}"
        f"{m['saved_nano_aiu']:>14}"
    )
