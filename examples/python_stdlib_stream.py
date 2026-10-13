"""Dependency-free streaming client. Env: SEMDOK_BASE_URL, SEMDOK_API_KEY, SEMDOK_MODEL."""
import json, os, sys, urllib.error, urllib.request

base, key, model = os.environ["SEMDOK_BASE_URL"].rstrip("/"), os.environ["SEMDOK_API_KEY"], os.environ["SEMDOK_MODEL"]
req = urllib.request.Request(
    base + "/chat/completions",
    data=json.dumps({"model": model, "stream": True, "max_tokens": 64,
                     "messages": [{"role": "user", "content": "Say hello."}]}).encode(),
    headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
)
try:
    done = False
    with urllib.request.urlopen(req, timeout=600) as resp:   # explicit timeout
        for raw in resp:
            line = raw.decode().strip()
            if not line or line.startswith(":"):      # blank line / ": keepalive" comment
                continue
            if line == "data: [DONE]":
                done = True
                break
            ev = json.loads(line[6:])
            if "error" in ev:
                raise SystemExit(f"server error: {ev['error']}")
            for c in ev.get("choices", []):
                d = c.get("delta", {})
                if d.get("reasoning_content"):   # thinking models (upstream extension): to stderr
                    print(d["reasoning_content"], end="", file=sys.stderr, flush=True)
                print(d.get("content") or "", end="", flush=True)
    print()
    if not done:   # a stream cut short is not a complete answer
        raise SystemExit("stream ended without [DONE]")
except urllib.error.HTTPError as e:
    # 429/503 carry Retry-After: retry with backoff only for these, never blindly for 4xx.
    raise SystemExit(f"HTTP {e.code}: {e.read().decode()[:300]} retry-after={e.headers.get('Retry-After')}")
