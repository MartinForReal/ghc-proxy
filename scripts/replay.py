#!/usr/bin/env python3
"""Replay a request captured by the ghc-proxy dashboard and time the upstream SSE stream.

Unlike Claude Code, this has no stall watchdog: it waits for the upstream to
finish no matter how long it goes silent, so it can answer two questions the
client can never answer for itself -- how long the silence actually lasts, and
what the upstream eventually sends.

  python scripts/replay.py --list
  python scripts/replay.py --id <record-id> --target upstream
  python scripts/replay.py --id <record-id> --target proxy --max-tokens 4000
"""
import argparse
import json
import os
import sys
import time
import uuid

import requests

DASHBOARD = "http://127.0.0.1:8314"
CFG_DIR = os.path.join(os.environ.get("APPDATA", ""), "ghc-tunnel")
GITHUB_API = "https://api.github.com"


def load_config():
    """Reads the few config.yaml values that appear in Copilot request headers.

    Parsed by hand rather than with PyYAML so the script has no dependency the
    proxy itself doesn't already imply.
    """
    cfg = {}
    path = os.path.join(CFG_DIR, "config.yaml")
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#") or ":" not in line:
                continue
            k, _, v = line.partition(":")
            cfg[k.strip()] = v.strip().strip('"').strip("'")
    return cfg


def base_url(cfg):
    acct = cfg.get("account_type", "individual")
    if acct == "individual":
        return "https://api.githubcopilot.com"
    return f"https://api.{acct}.githubcopilot.com"


def copilot_token(cfg):
    """Exchanges the stored GitHub token for a short-lived Copilot token.

    Mirrors src/auth.rs::fetch_copilot_token. The proxy holds its own copy in
    memory and never exposes it, so the exchange is repeated here.
    """
    with open(os.path.join(CFG_DIR, "github_token.txt"), encoding="utf-8") as fh:
        gh = fh.read().strip()
    r = requests.get(
        f"{GITHUB_API}/copilot_internal/v2/token",
        headers={
            "Authorization": f"token {gh}",
            "Editor-Version": f"vscode/{cfg.get('vscode_version', '1.123.0')}",
            "User-Agent": "GithubCopilot/1.155.0",
        },
        timeout=30,
    )
    r.raise_for_status()
    return r.json()["token"]


def upstream_headers(cfg, tok, machine_id, beta):
    """Replicates SharedState::copilot_headers so the replay looks identical."""
    rid = str(uuid.uuid4())
    h = {
        "Authorization": f"Bearer {tok}",
        "Content-Type": "application/json",
        "Copilot-Integration-Id": "vscode-chat",
        "Editor-Version": f"vscode/{cfg.get('vscode_version', '1.123.0')}",
        "Editor-Plugin-Version": f"copilot-chat/{cfg.get('copilot_version', '0.48.1')}",
        "User-Agent": f"GitHubCopilotChat/{cfg.get('copilot_version', '0.48.1')}",
        "OpenAI-Intent": "conversation-panel",
        "openai-organization": "github-copilot",
        "vscode-machineid": machine_id,
        "vscode-sessionid": str(uuid.uuid4()),
        "X-GitHub-Api-Version": cfg.get("api_version", "2025-05-01"),
        "X-Interaction-Type": "conversation-panel",
        "X-Request-Id": rid,
        "X-Agent-Task-Id": rid,
        "X-VSCode-User-Agent-Library-Version": "electron-fetch",
        "anthropic-version": "2023-06-01",
    }
    if beta:
        h["anthropic-beta"] = beta
    return h


def derive_beta(payload):
    """Builds the `anthropic-beta` header the proxy would have sent.

    Mirrors server.rs::apply_anthropic_beta. `context_management` in the body
    is rejected with a misleading `Extra inputs are not permitted` 400 unless
    its beta is requested, so it has to be derived rather than hardcoded.
    """
    betas = ["claude-code-20250219", "context-1m-2025-08-07"]
    if "context_management" in payload:
        betas.append("context-management-2025-06-27")
    return ",".join(betas)


def fetch_records(limit=60):
    r = requests.get(f"{DASHBOARD}/api/requests?per_page={limit}", timeout=30)
    r.raise_for_status()
    return r.json()["items"]


def parse_sse(raw_events):
    """Splits a list of (t, bytes) chunks into (t, event_name, data) triples.

    A `ping` is two lines -- `event: ping` then its own `data:` line. Counting
    that data line as content is what made the first run of this experiment
    report a 15s maximum gap when the real gap was 455s.
    """
    buf = b""
    out = []
    for t, chunk in raw_events:
        buf += chunk
        while True:
            idx = buf.find(b"\n\n")
            idx_crlf = buf.find(b"\r\n\r\n")
            if idx < 0 and idx_crlf < 0:
                break
            if idx < 0 or (0 <= idx_crlf < idx):
                idx, width = idx_crlf, 4
            else:
                width = 2
            block, buf = buf[:idx], buf[idx + width:]
            name, data = None, None
            for line in block.split(b"\n"):
                line = line.rstrip(b"\r")
                if line.startswith(b"event:"):
                    name = line[6:].strip().decode("utf-8", "replace")
                elif line.startswith(b"data:"):
                    data = line[5:].strip().decode("utf-8", "replace")
            if name or data:
                out.append((t, name, data))
    return out


def run(payload, url, headers, label, dump):
    print(f"\n=== {label} ===")
    print(f"POST {url}")
    body = json.dumps(payload).encode("utf-8")
    print(f"payload      : {len(body)} bytes")
    print(f"model        : {payload.get('model')}")
    print(f"max_tokens   : {payload.get('max_tokens')}")
    print(f"messages     : {len(payload.get('messages') or [])}")
    print(f"tools        : {len(payload.get('tools') or [])}")
    print("waiting (no client-side stall watchdog -- will wait indefinitely)...\n", flush=True)

    t0 = time.monotonic()
    raw = []
    # `timeout` is the connect/read-inactivity timeout. Deliberately generous:
    # the whole point is to outlast a silence that kills the real client.
    resp = requests.post(url, headers=headers, data=body, stream=True, timeout=(30, 1800))
    t_headers = time.monotonic() - t0
    print(f"HTTP {resp.status_code}   (headers after {t_headers:.1f}s)")
    print(f"content-type : {resp.headers.get('content-type')}")
    if resp.status_code >= 300:
        print("--- error body ---")
        print(resp.text[:4000])
        return

    for chunk in resp.iter_content(chunk_size=None):
        if chunk:
            raw.append((time.monotonic() - t0, chunk))
    total = time.monotonic() - t0

    events = parse_sse(raw)
    content_ts = [t for t, name, _ in events if name and name != "ping"]
    all_ts = [t for t, _ in raw]

    def max_gap(ts):
        if not ts:
            return total, 0.0
        pts = [0.0] + ts
        gaps = [(pts[i + 1] - pts[i], pts[i]) for i in range(len(pts) - 1)]
        gaps.append((total - ts[-1], ts[-1]))
        return max(gaps)

    gap_content, gap_at = max_gap(content_ts)
    gap_bytes, _ = max_gap(all_ts)

    counts, tool_json, stop_reason, usage = {}, 0, None, None
    for _, name, data in events:
        if name:
            counts[name] = counts.get(name, 0) + 1
        if not data:
            continue
        try:
            j = json.loads(data)
        except ValueError:
            continue
        d = j.get("delta") or {}
        if d.get("type") == "input_json_delta":
            tool_json += len(d.get("partial_json") or "")
        if d.get("stop_reason"):
            stop_reason = d["stop_reason"]
        if j.get("usage"):
            usage = j["usage"]
        if (j.get("message") or {}).get("usage"):
            usage = j["message"]["usage"]

    print("\n--- 汇总 ---")
    print(f"总耗时              {total:.1f}s")
    print(f"TTFB (首字节)       {all_ts[0]:.1f}s" if all_ts else "TTFB              n/a")
    print(f"最长静默(内容事件)  {gap_content:.1f}s   起于 t={gap_at:.1f}s")
    print(f"最长静默(任意字节)  {gap_bytes:.1f}s")
    print(f"ping 事件           {counts.get('ping', 0)}")
    print(f"事件计数            {counts}")
    print(f"tool 参数长度       {tool_json} 字符")
    print(f"stop_reason         {stop_reason}")
    print(f"usage               {usage}")
    verdict = "会被 abort" if gap_content > 300 else "不会被 abort"
    print(f"\n>>> Claude Code 300s 阈值: {verdict} (最长内容静默 {gap_content:.1f}s)")

    if dump:
        with open(dump, "w", encoding="utf-8") as fh:
            for t, name, data in events:
                fh.write(f"{t:9.3f}  {name or '-':24s} {(data or '')[:400]}\n")
        print(f"\n逐事件时序已写入 {dump}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="list recent records")
    ap.add_argument("--id", help="record id to replay")
    ap.add_argument("--target", choices=["upstream", "proxy"], default="upstream")
    ap.add_argument("--max-tokens", type=int, help="override max_tokens")
    ap.add_argument("--dump", help="write per-event timeline to this file")
    args = ap.parse_args()

    records = fetch_records()
    if args.list or not args.id:
        print(f"{'id':38s} {'time':9s} {'in':>8s} {'out':>7s} {'idle':>7s} {'status':>6s} kind")
        for it in records:
            print("%-38s %s %8d %7d %6.1fs %6s %s" % (
                it.get("id", "?"), it["timestamp"][11:19], it["input_tokens"],
                it["output_tokens"], (it.get("upstream_idle_max_ms") or 0) / 1000,
                it.get("status_code"), it.get("failure_kind") or ""))
        return

    rec = next((r for r in records if str(r.get("id")) == args.id), None)
    if rec is None:
        sys.exit(f"record {args.id} not found in the last {len(records)} records")
    if not rec.get("request_body"):
        sys.exit("record has no request_body -- is debug enabled in config.yaml?")

    payload = json.loads(rec["request_body"])
    if args.max_tokens:
        payload["max_tokens"] = args.max_tokens
    payload["stream"] = True

    cfg = load_config()
    label = f"replay {args.id} -> {args.target}"
    if args.target == "proxy":
        url = f"{DASHBOARD}/v1/messages"
        headers = {"Content-Type": "application/json", "anthropic-version": "2023-06-01"}
    else:
        with open(os.path.join(CFG_DIR, "machine_id.txt"), encoding="utf-8") as fh:
            machine_id = fh.read().strip()
        beta = derive_beta(payload)
        print(f"anthropic-beta: {beta}")
        headers = upstream_headers(cfg, copilot_token(cfg), machine_id, beta)
        url = f"{base_url(cfg)}/v1/messages"
    run(payload, url, headers, label, args.dump)


if __name__ == "__main__":
    main()
