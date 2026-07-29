"""Protocol conformance sweep against a running ghc-proxy.

The proxy is a translating gateway: a client speaks Anthropic, OpenAI chat
completions, the OpenAI Responses API or Gemini, and the upstream only ever
speaks chat completions or Responses. The property that matters is therefore
not "did it return 200" but "did it return the shape the *client* asked for" —
a Gemini client must get `candidates`, never `choices`, no matter what the
upstream sent back.

Run with the proxy already listening:

    python scripts/protocol_check.py --port 8399

Every check is an assertion about the response the client actually receives.
Nothing here inspects the dashboard; this is about the wire.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass, field

TIMEOUT = 180


@dataclass
class Result:
    name: str
    ok: bool
    detail: str = ""


@dataclass
class Report:
    results: list[Result] = field(default_factory=list)

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        self.results.append(Result(name, ok, detail))
        return ok

    @property
    def failed(self) -> list[Result]:
        return [r for r in self.results if not r.ok]


def post(base: str, path: str, body: dict) -> tuple[int, str, str]:
    """Returns (status, content_type, text). Never raises on HTTP errors."""
    req = urllib.request.Request(
        base + path,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type", ""), e.read().decode("utf-8", "replace")


def get(base: str, path: str) -> tuple[int, str]:
    try:
        with urllib.request.urlopen(base + path, timeout=TIMEOUT) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def sse_events(text: str) -> list[tuple[str | None, str]]:
    """Splits an SSE body into (event name, data payload) pairs."""
    out = []
    for frame in text.split("\n\n"):
        name, data = None, []
        for line in frame.splitlines():
            if line.startswith("event:"):
                name = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].strip())
        if data or name:
            out.append((name, "\n".join(data)))
    return out


def sse_types(text: str) -> list[str]:
    """The `type` of every JSON frame, which is how all three streaming
    protocols label what a frame is."""
    types = []
    for name, data in sse_events(text):
        if data == "[DONE]":
            types.append("[DONE]")
            continue
        try:
            j = json.loads(data)
        except ValueError:
            continue
        types.append(j.get("type") or name or ("chat.chunk" if "choices" in j else "?"))
    return types


# --------------------------------------------------------------------------
# Anthropic  /v1/messages
# --------------------------------------------------------------------------

def check_anthropic(base: str, rep: Report, model: str) -> None:
    p = "anthropic"

    status, _, text = post(base, "/v1/messages", {
        "model": model, "max_tokens": 60,
        "messages": [{"role": "user", "content": "Reply with exactly: ok"}],
    })
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: sync 200", status == 200, text[:200])
    rep.check(f"{p}: sync type=message", j.get("type") == "message", str(j.get("type")))
    rep.check(f"{p}: sync role=assistant", j.get("role") == "assistant", str(j.get("role")))
    rep.check(f"{p}: sync content is block array",
              isinstance(j.get("content"), list) and bool(j["content"]),
              json.dumps(j.get("content"))[:160])
    rep.check(f"{p}: sync has stop_reason", j.get("stop_reason") is not None, str(j.get("stop_reason")))
    u = j.get("usage") or {}
    rep.check(f"{p}: sync usage has input/output tokens",
              "input_tokens" in u and "output_tokens" in u, json.dumps(u)[:160])
    rep.check(f"{p}: sync does NOT leak choices", "choices" not in j, "leaked OpenAI shape")

    status, ctype, text = post(base, "/v1/messages", {
        "model": model, "max_tokens": 60, "stream": True,
        "messages": [{"role": "user", "content": "Count 1 to 3."}],
    })
    types = sse_types(text)
    rep.check(f"{p}: stream 200", status == 200, text[:200])
    rep.check(f"{p}: stream content-type", "text/event-stream" in ctype, ctype)
    for want in ("message_start", "content_block_start", "content_block_delta",
                 "content_block_stop", "message_delta", "message_stop"):
        rep.check(f"{p}: stream has {want}", want in types, ",".join(types[:12]))
    rep.check(f"{p}: stream ends with message_stop",
              "message_stop" in types and types.index("message_stop") >= len(types) - 2,
              ",".join(types[-3:]))
    rep.check(f"{p}: stream frames carry event: names",
              all(n for n, d in sse_events(text) if d and d != "[DONE]"),
              "a frame was missing its event: line")

    status, _, text = post(base, "/v1/messages", {
        "model": model, "max_tokens": 200,
        "tools": [{
            "name": "get_weather",
            "description": "Look up the weather",
            "input_schema": {"type": "object", "properties": {"city": {"type": "string"}},
                             "required": ["city"]},
        }],
        "messages": [{"role": "user", "content": "Weather in Paris? Call the tool."}],
    })
    j = json.loads(text) if status == 200 else {}
    blocks = j.get("content") or []
    tool_use = [b for b in blocks if b.get("type") == "tool_use"]
    rep.check(f"{p}: tools produce tool_use block", bool(tool_use), json.dumps(blocks)[:200])
    if tool_use:
        rep.check(f"{p}: tool_use has name+input+id",
                  all(k in tool_use[0] for k in ("name", "input", "id")),
                  json.dumps(tool_use[0])[:200])
        rep.check(f"{p}: stop_reason=tool_use", j.get("stop_reason") == "tool_use",
                  str(j.get("stop_reason")))

    # Multi-turn with a tool_result the client sends back.
    if tool_use:
        tu = tool_use[0]
        status, _, text = post(base, "/v1/messages", {
            "model": model, "max_tokens": 120,
            "tools": [{"name": "get_weather", "description": "Look up the weather",
                       "input_schema": {"type": "object",
                                        "properties": {"city": {"type": "string"}},
                                        "required": ["city"]}}],
            "messages": [
                {"role": "user", "content": "Weather in Paris? Call the tool."},
                {"role": "assistant", "content": blocks},
                {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": tu["id"],
                    "content": [{"type": "text", "text": "18C and clear"}],
                }]},
            ],
        })
        rep.check(f"{p}: tool_result round-trip accepted", status == 200, text[:200])

    status, _, text = post(base, "/v1/messages/count_tokens", {
        "model": model,
        "messages": [{"role": "user", "content": "hello world"}],
    })
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: count_tokens 200", status == 200, text[:200])
    rep.check(f"{p}: count_tokens returns input_tokens",
              isinstance(j.get("input_tokens"), int) and j["input_tokens"] > 0,
              json.dumps(j)[:160])


# --------------------------------------------------------------------------
# OpenAI chat completions  /v1/chat/completions
# --------------------------------------------------------------------------

def check_chat(base: str, rep: Report, model: str) -> None:
    p = "chat"

    status, _, text = post(base, "/v1/chat/completions", {
        "model": model, "max_tokens": 60,
        "messages": [{"role": "user", "content": "Reply with exactly: ok"}],
    })
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: sync 200", status == 200, text[:200])
    rep.check(f"{p}: sync has choices[]", isinstance(j.get("choices"), list) and bool(j["choices"]),
              json.dumps(j)[:160])
    if j.get("choices"):
        c = j["choices"][0]
        rep.check(f"{p}: choice has message.role=assistant",
                  (c.get("message") or {}).get("role") == "assistant", json.dumps(c)[:160])
        rep.check(f"{p}: choice has finish_reason", c.get("finish_reason") is not None,
                  str(c.get("finish_reason")))
    u = j.get("usage") or {}
    rep.check(f"{p}: usage has prompt/completion tokens",
              "prompt_tokens" in u and "completion_tokens" in u, json.dumps(u)[:160])
    rep.check(f"{p}: sync does NOT leak Anthropic shape", "content" not in j, "leaked content[]")

    status, ctype, text = post(base, "/v1/chat/completions", {
        "model": model, "max_tokens": 60, "stream": True,
        "stream_options": {"include_usage": True},
        "messages": [{"role": "user", "content": "Count 1 to 3."}],
    })
    frames = [d for _, d in sse_events(text) if d]
    rep.check(f"{p}: stream 200", status == 200, text[:200])
    rep.check(f"{p}: stream content-type", "text/event-stream" in ctype, ctype)
    rep.check(f"{p}: stream terminates with [DONE]", frames and frames[-1] == "[DONE]",
              frames[-1][:80] if frames else "no frames")
    parsed = [json.loads(d) for d in frames if d != "[DONE]"]
    rep.check(f"{p}: every frame has choices[]", all("choices" in d for d in parsed),
              "a frame was missing choices")
    text_seen = "".join(
        (c.get("delta") or {}).get("content") or ""
        for d in parsed for c in d.get("choices") or []
    )
    rep.check(f"{p}: stream produced text", bool(text_seen.strip()), repr(text_seen[:80]))
    rep.check(f"{p}: stream reports a finish_reason",
              any(c.get("finish_reason") for d in parsed for c in d.get("choices") or []),
              "none present")

    status, _, text = post(base, "/v1/chat/completions", {
        "model": model, "max_tokens": 200,
        "tools": [{"type": "function", "function": {
            "name": "get_weather", "description": "Look up the weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                           "required": ["city"]}}}],
        "messages": [{"role": "user", "content": "Weather in Paris? Call the tool."}],
    })
    j = json.loads(text) if status == 200 else {}
    calls = ((j.get("choices") or [{}])[0].get("message") or {}).get("tool_calls") or []
    rep.check(f"{p}: tools produce tool_calls", bool(calls), json.dumps(j)[:200])
    if calls:
        fn = calls[0].get("function") or {}
        rep.check(f"{p}: tool_call has name", bool(fn.get("name")), json.dumps(calls[0])[:160])
        ok_args = False
        try:
            ok_args = isinstance(json.loads(fn.get("arguments") or ""), dict)
        except ValueError:
            pass
        rep.check(f"{p}: tool_call arguments parse as JSON", ok_args, repr(fn.get("arguments"))[:160])


# --------------------------------------------------------------------------
# OpenAI Responses  /v1/responses
# --------------------------------------------------------------------------

def check_responses(base: str, rep: Report, model: str) -> None:
    p = "responses"

    status, _, text = post(base, "/v1/responses", {
        "model": model,
        "input": [{"role": "user", "content": "Reply with exactly: ok"}],
    })
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: sync 200", status == 200, text[:200])
    rep.check(f"{p}: sync has output[]", isinstance(j.get("output"), list), json.dumps(j)[:160])
    rep.check(f"{p}: sync has status", j.get("status") is not None, str(j.get("status")))
    u = j.get("usage") or {}
    rep.check(f"{p}: usage has input/output tokens",
              "input_tokens" in u and "output_tokens" in u, json.dumps(u)[:160])

    status, ctype, text = post(base, "/v1/responses", {
        "model": model, "stream": True,
        "input": [{"role": "user", "content": "Count 1 to 3."}],
    })
    types = sse_types(text)
    rep.check(f"{p}: stream 200", status == 200, text[:200])
    rep.check(f"{p}: stream content-type", "text/event-stream" in ctype, ctype)
    for want in ("response.created", "response.output_text.delta", "response.completed"):
        rep.check(f"{p}: stream has {want}", want in types, ",".join(types[:10]))
    rep.check(f"{p}: stream ends at response.completed",
              types and types[-1] in ("response.completed", "[DONE]"),
              ",".join(types[-3:]))
    deltas = "".join(
        json.loads(d).get("delta") or ""
        for n, d in sse_events(text)
        if d and d != "[DONE]" and json.loads(d).get("type") == "response.output_text.delta"
    )
    rep.check(f"{p}: stream produced text", bool(deltas.strip()), repr(deltas[:80]))


# --------------------------------------------------------------------------
# Gemini  /v1beta/models/{model}:generateContent
# --------------------------------------------------------------------------

def check_gemini(base: str, rep: Report, model: str) -> None:
    p = "gemini"
    # Enough budget that internal reasoning cannot consume the whole allowance
    # before any text is emitted — at 60 this turn intermittently finished on
    # MAX_TOKENS with empty parts, which is a real upstream outcome and not
    # something the shape checks should depend on.
    body = {
        "contents": [{"role": "user", "parts": [{"text": "Reply with exactly: ok"}]}],
        "generationConfig": {"maxOutputTokens": 512},
    }

    status, _, text = post(base, f"/v1beta/models/{model}:generateContent", body)
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: sync 200", status == 200, text[:200])
    # The whole point of the Gemini surface: the client must never see the
    # chat-completions shape the upstream actually returned.
    rep.check(f"{p}: sync has candidates[]",
              isinstance(j.get("candidates"), list) and bool(j["candidates"]),
              json.dumps(j)[:200])
    rep.check(f"{p}: sync does NOT leak choices[]", "choices" not in j, "leaked OpenAI shape")
    if j.get("candidates"):
        cand = j["candidates"][0]
        parts = (cand.get("content") or {}).get("parts")
        finish = cand.get("finishReason")
        rep.check(f"{p}: candidate has content.parts[]", isinstance(parts, list),
                  json.dumps(cand)[:200])
        # A turn cut off by the token limit owes no content; anything else does.
        rep.check(f"{p}: candidate carries content unless truncated",
                  bool(parts) or finish == "MAX_TOKENS",
                  json.dumps(cand)[:200])
        rep.check(f"{p}: candidate has finishReason", finish is not None, str(finish))
    um = j.get("usageMetadata") or {}
    rep.check(f"{p}: has usageMetadata with token counts",
              "promptTokenCount" in um and "candidatesTokenCount" in um, json.dumps(um)[:160])

    status, ctype, text = post(base, f"/v1beta/models/{model}:streamGenerateContent", body)
    frames = [d for _, d in sse_events(text) if d and d != "[DONE]"]
    rep.check(f"{p}: stream 200", status == 200, text[:200])
    rep.check(f"{p}: stream content-type", "text/event-stream" in ctype, ctype)
    parsed = []
    for d in frames:
        try:
            parsed.append(json.loads(d))
        except ValueError:
            pass
    rep.check(f"{p}: stream frames have candidates[]",
              bool(parsed) and all("candidates" in d for d in parsed),
              json.dumps(parsed[0])[:200] if parsed else "no frames")
    rep.check(f"{p}: stream frames do NOT leak choices[]",
              all("choices" not in d for d in parsed), "leaked OpenAI shape")
    said = "".join(
        p_.get("text") or ""
        for d in parsed
        for c in d.get("candidates") or []
        for p_ in ((c.get("content") or {}).get("parts") or [])
    )
    truncated = any(c.get("finishReason") == "MAX_TOKENS"
                    for d in parsed for c in d.get("candidates") or [])
    rep.check(f"{p}: stream produced text", bool(said.strip()) or truncated, repr(said[:80]))

    status, _, text = post(base, f"/v1beta/models/{model}:countTokens", body)
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: countTokens returns totalTokens",
              status == 200 and isinstance(j.get("totalTokens"), int) and j["totalTokens"] > 0,
              text[:160])


# --------------------------------------------------------------------------
# Cross-cutting
# --------------------------------------------------------------------------

def check_errors(base: str, rep: Report) -> None:
    p = "errors"

    # An unknown model must not come back as a 200 SSE stream that never
    # produces an event — the failure the client sees has to be the failure
    # that happened.
    status, ctype, text = post(base, "/v1/messages", {
        "model": "definitely-not-a-model", "max_tokens": 10, "stream": True,
        "messages": [{"role": "user", "content": "hi"}],
    })
    rep.check(f"{p}: unknown model on stream is not 200", status != 200, f"{status} {ctype}")
    rep.check(f"{p}: unknown model returns JSON error, not SSE",
              "text/event-stream" not in ctype, ctype)
    try:
        rep.check(f"{p}: error body has error.message",
                  bool((json.loads(text).get("error") or {}).get("message")), text[:160])
    except ValueError:
        rep.check(f"{p}: error body is JSON", False, text[:160])

    # A model that only lives on /responses must say so rather than 500.
    status, _, text = post(base, "/v1/chat/completions", {
        "model": "gpt-5.5", "max_tokens": 10,
        "messages": [{"role": "user", "content": "hi"}],
    })
    rep.check(f"{p}: responses-only model rejected with 4xx on chat", 400 <= status < 500, f"{status} {text[:120]}")
    rep.check(f"{p}: rejection names the right endpoint", "/v1/responses" in text, text[:200])


def check_discovery(base: str, rep: Report) -> None:
    p = "discovery"
    status, text = get(base, "/v1/models")
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: /v1/models 200", status == 200, text[:160])
    rep.check(f"{p}: models list is OpenAI-shaped",
              j.get("object") == "list" and isinstance(j.get("data"), list) and bool(j["data"]),
              json.dumps(j)[:160])
    rep.check(f"{p}: every model has id and object=model",
              all(m.get("id") and m.get("object") == "model" for m in j.get("data", [])),
              "a model entry was malformed")

    status, text = get(base, "/health")
    j = json.loads(text) if status == 200 else {}
    rep.check(f"{p}: /health 200", status == 200, text[:160])
    rep.check(f"{p}: health reports ready + version",
              j.get("ready") is not None and bool(j.get("version")), json.dumps(j)[:160])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8399)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--anthropic-model", default="claude-haiku-4.5")
    ap.add_argument("--chat-model", default="gpt-4o-mini")
    ap.add_argument("--responses-model", default="gpt-5.5")
    ap.add_argument("--gemini-model", default="gemini-3.5-flash")
    ap.add_argument("--only", default="", help="comma-separated subset of suites to run")
    args = ap.parse_args()

    base = f"http://{args.host}:{args.port}"
    rep = Report()

    suites = {
        "discovery": lambda: check_discovery(base, rep),
        "anthropic": lambda: check_anthropic(base, rep, args.anthropic_model),
        "chat": lambda: check_chat(base, rep, args.chat_model),
        "responses": lambda: check_responses(base, rep, args.responses_model),
        "gemini": lambda: check_gemini(base, rep, args.gemini_model),
        "errors": lambda: check_errors(base, rep),
    }
    wanted = [s.strip() for s in args.only.split(",") if s.strip()] or list(suites)

    for name in wanted:
        fn = suites.get(name)
        if not fn:
            print(f"unknown suite: {name}", file=sys.stderr)
            return 2
        try:
            fn()
        except Exception as e:  # a crashed suite is a failed suite, not a crashed run
            rep.check(f"{name}: suite raised", False, f"{type(e).__name__}: {e}")

    width = max(len(r.name) for r in rep.results)
    for r in rep.results:
        mark = "PASS" if r.ok else "FAIL"
        line = f"[{mark}] {r.name.ljust(width)}"
        if not r.ok and r.detail:
            line += f"  <- {r.detail}"
        print(line)

    print(f"\n{len(rep.results) - len(rep.failed)}/{len(rep.results)} passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
