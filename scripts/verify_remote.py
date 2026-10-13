#!/usr/bin/env python3
"""Externally runnable verification of a Semdok Edge endpoint (stdlib only).

  python verify_remote.py --base-url https://api.example.com/v1 --api-key sem_... --model ID \
      [--from-separate-network]

Run it from a machine on a DIFFERENT network (e.g. a phone hotspot or a cloud VM). A request
that works from the host itself proves nothing about internet reachability, so the result is
labelled UNVERIFIED unless you assert --from-separate-network (an honest-operator flag; this
script cannot prove where it runs). Exit code 0 only if every check passes.
"""
import argparse, json, ssl, sys, time, urllib.error, urllib.request

def req(url, key=None, body=None, timeout=60, method=None):
    h = {"Content-Type": "application/json"}
    if key:
        h["Authorization"] = f"Bearer {key}"
    r = urllib.request.Request(url, data=json.dumps(body).encode() if body is not None else None, headers=h, method=method)
    try:
        return urllib.request.urlopen(r, timeout=timeout)
    except urllib.error.HTTPError as e:
        return e

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", required=True)
    ap.add_argument("--api-key", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--from-separate-network", action="store_true")
    ap.add_argument("--allow-http", action="store_true", help="local dry run only; skips the https check")
    a = ap.parse_args()
    base = a.base_url.rstrip("/")
    results = []
    def check(name, ok, detail=""):
        results.append(ok)
        print(f"[{'PASS' if ok else 'FAIL'}] {name} {detail}")

    check("uses https (valid certificate is enforced by this client)", base.startswith("https://") or a.allow_http, "(skipped: --allow-http)" if a.allow_http else "")
    root = base[: -len("/v1")] if base.endswith("/v1") else base
    try:
        r = req(base + "/models", a.api_key, timeout=30)
        data = json.load(r) if r.status == 200 else {}
        ids = [m["id"] for m in data.get("data", [])]
        check("GET /models with key", r.status == 200 and a.model in ids, f"status={r.status} ids={ids}")
    except ssl.SSLError as e:
        check("TLS certificate", False, str(e)); return finish(results, a)
    except Exception as e:
        check("reachable", False, repr(e)); return finish(results, a)
    check("no key -> 401", req(base + "/models").status == 401)
    check("bad key -> 401", req(base + "/models", "sem_00000000_bad").status == 401)
    for p in ["/admin", "/v1/completions", "/v1/embeddings", "/api/models", "/healthz", "/v1/systemone"]:
        s = req(root + p, a.api_key, timeout=30).status
        check(f"blocked route {p}", s in (404, 405), f"status={s}")
    r = req(base + "/chat/completions", a.api_key, {"model": a.model, "max_tokens": 8, "messages": [{"role": "user", "content": "Hi"}]}, timeout=600)
    body = json.load(r) if r.status == 200 else {}
    check("POST chat (JSON)", r.status == 200 and "usage" in body, f"status={r.status}")
    t0 = time.time(); arrivals = []; done = False
    r = req(base + "/chat/completions", a.api_key, {"model": a.model, "stream": True, "max_tokens": 64, "messages": [{"role": "user", "content": "Count from 1 to 20."}]}, timeout=600)
    ctype = r.headers.get("Content-Type", "") if r.status == 200 else ""
    if r.status == 200:
        for raw in r:
            line = raw.decode().strip()
            if line.startswith("data: "):
                arrivals.append(time.time() - t0)
                if line == "data: [DONE]":
                    done = True
    spread = (arrivals[-1] - arrivals[0]) if len(arrivals) > 1 else 0
    check("SSE content-type + [DONE]", ctype.startswith("text/event-stream") and done, f"events={len(arrivals)}")
    # A body that arrives as one buffered blob is read out in well under a millisecond; a fast
    # engine (the e2e fixture) can stream 60 events in 40 ms, so 10 ms separates the two.
    check("SSE incremental (events spread over time, not one buffered blob)", len(arrivals) > 3 and spread > 0.01, f"spread={spread:.3f}s")
    finish(results, a)

def finish(results, a):
    ok = all(results)
    print()
    if ok and a.from_separate_network:
        print("RESULT: PASS - operator asserts this ran from a separate network.")
    elif ok:
        print("RESULT: checks pass, but public reachability is UNVERIFIED (not asserted to be off-network).")
    else:
        print("RESULT: FAIL")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
