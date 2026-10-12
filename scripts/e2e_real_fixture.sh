#!/usr/bin/env bash
# Real-engine protocol test on Linux/macOS/Git-Bash: pinned upstream Colibri engine + its
# random-weight MiMo fixture, driven through hubd and the restricted gateway by real clients.
# The fixture proves the plumbing end to end; its output is NOT meaningful text.
# Asserted: every client exits 0 and shows its protocol markers (models list, streamed chunks,
# `data: [DONE]`, usage, non-empty text); verify_remote passes; hubd exits 0 on SIGINT and, in a
# second run, on SIGTERM, and no engine process survives either. The temp dir is removed on
# every exit path.
#   COLIBRI_DIR=/path/to/colibri-checkout [HUB_E2E_PORT=18787] scripts/e2e_real_fixture.sh
set -euo pipefail
: "${COLIBRI_DIR:?set COLIBRI_DIR to the pinned colibri checkout (see docs/UPSTREAM.md)}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${HUB_E2E_PORT:-18787}"
PY="${PYTHON:-python3}"
WORK="$(mktemp -d)"
HUBD_PID=""

# Running, i.e. neither gone nor a zombie waiting to be reaped.
alive() { local s; s=$(ps -p "$1" -o stat= 2>/dev/null) || return 1; case "$s" in Z*) return 1;; esac; }

# stop_hubd SIGNAL: signal hubd, wait up to 30 s, SIGKILL it if it hangs; returns hubd's exit status.
stop_hubd() {
  local pid=$HUBD_PID rc=0
  HUBD_PID=""
  kill -"$1" "$pid" 2>/dev/null || true
  for _ in $(seq 30); do alive "$pid" || break; sleep 1; done
  if alive "$pid"; then echo "hubd ignored SIG$1 for 30 s; killing it"; kill -KILL "$pid" 2>/dev/null || true; fi
  wait "$pid" || rc=$?
  return "$rc"
}

cleanup() {
  if [ -n "$HUBD_PID" ]; then stop_hubd INT || true; fi
  pkill -KILL -f "$WORK/model" 2>/dev/null || true   # last resort only; orphans already failed the run
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM

fail() {
  echo "FAIL: $*"
  if [ -f "$WORK/hubd.log" ]; then echo "--- hubd log (tail)"; tail -n 40 "$WORK/hubd.log"; fi
  exit 1
}

start_hubd() {
  "$H" serve --model fixture --colibri-dir "$COLIBRI_DIR" --port "$PORT" >"$WORK/hubd.log" 2>&1 &
  HUBD_PID=$!
  for _ in $(seq 120); do
    grep -q "^local API ready" "$WORK/hubd.log" && return 0
    alive "$HUBD_PID" || fail "hubd exited before it was ready"
    sleep 1
  done
  fail "hubd not ready after 120 s"
}

# shutdown_check SIGNAL: hubd must exit 0 and leave no engine process behind.
shutdown_check() {
  local rc=0
  echo "== SIG$1 shutdown leaves no orphans"
  stop_hubd "$1" || rc=$?
  [ "$rc" -eq 0 ] || fail "hubd exited with status $rc after SIG$1"
  sleep 1
  if pgrep -f "$WORK/model" >/dev/null; then pgrep -fa "$WORK/model" || true; fail "orphan engine processes after SIG$1"; fi
  echo "PASS: SIG$1, no orphans"
}

# client NAME CMD...: the client must exit 0; its output is kept in $WORK/NAME.out and shown.
client() {
  local name=$1; shift
  echo "== $name"
  if ! "$@" >"$WORK/$name.out" 2>&1; then cat "$WORK/$name.out"; fail "$name exited nonzero"; fi
  head -c 300 "$WORK/$name.out"; echo
}

# expect NAME ERE...: the client's output must match every extended regex.
expect() {
  local name=$1 re; shift
  for re in "$@"; do grep -Eq -- "$re" "$WORK/$name.out" || fail "$name output does not match /$re/"; done
}

( cd "$COLIBRI_DIR/c" && make mimo >/dev/null && make mimo-tiny-generate >/dev/null )
"$PY" -I - "$COLIBRI_DIR" "$WORK/model" <<'PYEOF'
import sys, shutil
sys.path.insert(0, sys.argv[1] + "/c/tests")
import mimo_serve_fixture as f
shutil.copytree(f.served_fixture(), sys.argv[2])
PYEOF

( cd "$HERE" && cargo build -q -p hubd )
H="$HERE/target/debug/hubd"
export HUB_DATA_DIR="$WORK/data" RUST_BACKTRACE=0
"$H" init >/dev/null
"$H" model add "$WORK/model" --id fixture --colibri-dir "$COLIBRI_DIR" >/dev/null
KEY=$("$H" key create e2e | grep '^lah_') || fail "key create printed no key"
export HUB_BASE_URL="http://127.0.0.1:$PORT/v1" HUB_API_KEY="$KEY" HUB_MODEL=fixture

start_hubd
client curl sh "$HERE/examples/curl.sh"
expect curl '"id": ?"fixture"' '"(content|reasoning_content)": ?"[^"]' '^data: \[DONE\]'
if grep -q '"error"' "$WORK/curl.out"; then fail "curl got an error response"; fi
# The fixture only "thinks": its text arrives as reasoning_content, which these two print to stderr.
client python_stdlib "$PY" "$HERE/examples/python_stdlib_stream.py"   # exits nonzero without [DONE]
expect python_stdlib '[^[:space:]]'
client openai_sdk "$PY" "$HERE/examples/python_openai_sdk.py"
expect openai_sdk "\\['fixture'\\]" 'completion_tokens=[1-9]' '^stream usage: .*completion_tokens=[1-9]'
client node_fetch node "$HERE/examples/node_fetch_stream.mjs"                # exits nonzero without [DONE]
expect node_fetch '[^[:space:]]'
client verify_remote "$PY" "$HERE/scripts/verify_remote.py" --base-url "$HUB_BASE_URL" --api-key "$KEY" --model fixture --allow-http
expect verify_remote '^RESULT: checks pass'
shutdown_check INT

start_hubd
shutdown_check TERM
echo "PASS: all clients, SIGINT and SIGTERM shutdown"
