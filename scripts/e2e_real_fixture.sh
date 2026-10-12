#!/usr/bin/env bash
# Real-engine protocol test on Linux/macOS/Git-Bash: pinned upstream Colibri engine + its
# random-weight MiMo fixture, driven through hubd and the restricted gateway by real clients.
# The fixture proves the plumbing end to end; its output is NOT meaningful text.
#   COLIBRI_DIR=/path/to/colibri-checkout scripts/e2e_real_fixture.sh
set -euo pipefail
: "${COLIBRI_DIR:?set COLIBRI_DIR to the pinned colibri checkout (see docs/UPSTREAM.md)}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"; trap 'kill $HUBD_PID 2>/dev/null || true; wait 2>/dev/null || true; rm -rf "$WORK"' EXIT
export HUB_DATA_DIR="$WORK/data" RUST_BACKTRACE=0
PY="${PYTHON:-python3}"

( cd "$COLIBRI_DIR/c" && make mimo >/dev/null && make mimo-tiny-generate >/dev/null )
"$PY" -I - "$COLIBRI_DIR" "$WORK/model" <<'PYEOF'
import sys, shutil
sys.path.insert(0, sys.argv[1] + "/c/tests")
import mimo_serve_fixture as f
shutil.copytree(f.served_fixture(), sys.argv[2])
PYEOF

( cd "$HERE" && cargo build -q -p hubd )
H="$HERE/target/debug/hubd"
"$H" init >/dev/null
"$H" model add "$WORK/model" --id fixture --colibri-dir "$COLIBRI_DIR" >/dev/null
KEY=$("$H" key create e2e | grep '^lah_')
"$H" serve --model fixture --colibri-dir "$COLIBRI_DIR" --port 18787 >"$WORK/hubd.log" 2>&1 & HUBD_PID=$!
for _ in $(seq 60); do grep -q "Ctrl-C" "$WORK/hubd.log" && break; kill -0 $HUBD_PID || { cat "$WORK/hubd.log"; exit 1; }; sleep 1; done
set +o pipefail
export HUB_BASE_URL=http://127.0.0.1:18787/v1 HUB_API_KEY="$KEY" HUB_MODEL=fixture

echo "== curl";            sh "$HERE/examples/curl.sh" | head -c 400; echo
echo "== python stdlib";   "$PY" "$HERE/examples/python_stdlib_stream.py" | head -c 100 | od -c | head -2
echo "== openai sdk";      "$PY" "$HERE/examples/python_openai_sdk.py" 2>&1 | head -8
echo "== node fetch";      node "$HERE/examples/node_fetch_stream.mjs" | head -c 100 | od -c | head -2
echo "== verify_remote (local dry run)"; "$PY" "$HERE/scripts/verify_remote.py" --base-url "$HUB_BASE_URL" --api-key "$KEY" --model fixture --allow-http
echo "== shutdown leaves no orphans"
kill -INT $HUBD_PID; wait $HUBD_PID || true; sleep 1
if pgrep -f "$WORK/model" >/dev/null; then echo "FAIL: orphan processes"; exit 1; else echo "PASS: no orphans"; fi
