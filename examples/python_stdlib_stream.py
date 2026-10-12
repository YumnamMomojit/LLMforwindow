"""Dependency-free streaming client. Env: HUB_BASE_URL, HUB_API_KEY, HUB_MODEL."""
import json, os, urllib.error, urllib.request

base, key, model = os.environ["HUB_BASE_URL"].rstrip("/"), os.environ["HUB_API_KEY"], os.environ["HUB_MODEL"]
req = urllib.request.Request(
    base + "/chat/completions",
    data=json.dumps({"model": model, "stream": True, "max_tokens": 64,
                     "messages": [{"role": "user", "content": "Say hello."}]}).encode(),
    headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
)
try:
    with urllib.request.urlopen(req, timeout=600) as resp:   # explicit timeout
        for raw in resp:
            line = raw.decode().strip()
            if not line or line.startswith(":"):      # blank line / ": keepalive" comment
                continue
            if line == "data: [DONE]":
                break
            ev = json.loads(line[6:])
            if "error" in ev:
                raise SystemExit(f"server error: {ev['error']}")
            for c in ev.get("choices", []):
                print(c["delta"].get("content") or "", end="", flush=True)
    print()
except urllib.error.HTTPError as e:
    # 429/503 carry Retry-After: retry with backoff only for these, never blindly for 4xx.
    raise SystemExit(f"HTTP {e.code}: {e.read().decode()[:300]} retry-after={e.headers.get('Retry-After')}")
