#!/usr/bin/env bash
# Real-engine protocol test on Linux/macOS/Git-Bash: pinned upstream Colibri engine + its
# random-weight MiMo fixture, driven through semdok-edge and the restricted gateway by real clients.
# The fixture proves the plumbing end to end; its output is NOT meaningful text.
# Asserted: every client exits 0 and shows its protocol markers (models list, streamed chunks,
# `data: [DONE]`, usage, non-empty text); verify_remote passes; semdok-edge exits 0 on SIGINT and, in a
# second run, on SIGTERM, and no engine process survives either. The temp dir is removed on
# every exit path.
#   COLIBRI_DIR=/path/to/colibri-checkout [SEMDOK_E2E_PORT=18787] scripts/e2e_real_fixture.sh
set -euo pipefail
: "${COLIBRI_DIR:?set COLIBRI_DIR to the pinned colibri checkout (see docs/UPSTREAM.md)}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${SEMDOK_E2E_PORT:-18787}"
PY="${PYTHON:-python3}"
WORK="$(mktemp -d)"
EDGE_PID=""
# The engine tree of the running semdok-edge (launcher, C engine, ...) and their process groups,
# recorded while it serves: the C engine gets the model path through its environment, so
# its command line (`<colibri>/mimo <cap>`) never names $WORK and pgrep -f cannot find it.
ENGINE_PIDS=""; ENGINE_PGIDS=""
MY_PGID=$(ps -o pgid= -p $$ | tr -d ' ')

# Running, i.e. neither gone nor a zombie waiting to be reaped.
alive() { local s; s=$(ps -p "$1" -o stat= 2>/dev/null) || return 1; case "$s" in Z*) return 1;; esac; }

# descendants PID: every process below PID.
descendants() { local c; for c in $(pgrep -P "$1" 2>/dev/null); do echo "$c"; descendants "$c"; done; }

# engine_tree: remember the running semdok-edge's engine tree and its process groups.
engine_tree() {
  local p
  ENGINE_PIDS=$(descendants "$EDGE_PID")
  ENGINE_PGIDS=$(for p in $ENGINE_PIDS; do ps -o pgid= -p "$p" || true; done | tr -d ' ' | { grep -vx "$MY_PGID" || true; } | sort -u)
}

# record_engine: engine_tree, which must exist and must not share this script's process group.
record_engine() {
  local p
  engine_tree
  [ -n "$ENGINE_PIDS" ] || fail "semdok-edge has no engine processes while serving"
  for p in $ENGINE_PIDS; do
    [ "$(ps -o pgid= -p "$p" | tr -d ' ')" != "$MY_PGID" ] || fail "an engine process is in the test script's process group"
  done
}

# survivors: live processes of the recorded engine tree, of its process groups, or naming $WORK.
survivors() {
  local p
  { for p in $ENGINE_PIDS; do echo "$p"; done
    for p in $ENGINE_PGIDS; do pgrep -g "$p" 2>/dev/null || true; done
    pgrep -f "$WORK/" 2>/dev/null || true
  } | sort -u | while read -r p; do if [ -n "$p" ] && alive "$p"; then echo "$p"; fi; done
}

# stop_edge SIGNAL: signal semdok-edge, wait up to 30 s, SIGKILL it if it hangs; returns semdok-edge's exit status.
stop_edge() {
  local pid=$EDGE_PID rc=0
  EDGE_PID=""
  kill -"$1" "$pid" 2>/dev/null || true
  for _ in $(seq 30); do alive "$pid" || break; sleep 1; done
  if alive "$pid"; then echo "semdok-edge ignored SIG$1 for 30 s; killing it"; kill -KILL "$pid" 2>/dev/null || true; fi
  wait "$pid" || rc=$?
  return "$rc"
}

cleanup() {
  local p
  if [ -n "$EDGE_PID" ]; then engine_tree; stop_edge INT || true; fi
  # Last resort only (orphans already failed the run): what is left of the engine tree.
  for p in $(survivors); do kill -KILL "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM

fail() {
  echo "FAIL: $*"
  if [ -f "$WORK/semdok-edge.log" ]; then echo "--- semdok-edge log (tail)"; tail -n 40 "$WORK/semdok-edge.log"; fi
  exit 1
}

# start_edge [COLIBRI]: defaults to the source checkout; the release-layout run passes a flat folder.
start_edge() {
  "$H" serve --model fixture --colibri-dir "${1:-$COLIBRI_DIR}" --port "$PORT" >"$WORK/semdok-edge.log" 2>&1 &
  EDGE_PID=$!
  for _ in $(seq 120); do
    grep -q "^local API ready" "$WORK/semdok-edge.log" && return 0
    alive "$EDGE_PID" || fail "semdok-edge exited before it was ready"
    sleep 1
  done
  fail "semdok-edge not ready after 120 s"
}

# shutdown_check SIGNAL: semdok-edge must exit 0 and leave no engine process behind (launcher and
# C engine alike).
shutdown_check() {
  local rc=0 left
  echo "== SIG$1 shutdown leaves no orphans"
  record_engine
  echo "engine tree: $(for p in $ENGINE_PIDS; do ps -o pid=,args= -p "$p"; done | tr -s ' ' | paste -sd ';' -)"
  stop_edge "$1" || rc=$?
  [ "$rc" -eq 0 ] || fail "semdok-edge exited with status $rc after SIG$1"
  sleep 1
  left=$(survivors)
  if [ -n "$left" ]; then ps -o pid=,pgid=,args= -p "$(echo $left | tr ' ' ,)" || true; fail "orphan engine processes after SIG$1"; fi
  ENGINE_PIDS=""; ENGINE_PGIDS=""   # all gone: their numbers may be reused from now on
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

( cd "$HERE" && cargo build -q -p semdok-edge )
H="$HERE/target/debug/semdok-edge"
export SEMDOK_DATA_DIR="$WORK/data" RUST_BACKTRACE=0
"$H" init >/dev/null
"$H" model add "$WORK/model" --id fixture --colibri-dir "$COLIBRI_DIR" >/dev/null
KEY=$("$H" key create e2e | grep '^sem_') || fail "key create printed no key"
export SEMDOK_BASE_URL="http://127.0.0.1:$PORT/v1" SEMDOK_API_KEY="$KEY" SEMDOK_MODEL=fixture

start_edge
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
client verify_remote "$PY" "$HERE/scripts/verify_remote.py" --base-url "$SEMDOK_BASE_URL" --api-key "$KEY" --model fixture --allow-http
expect verify_remote '^RESULT: checks pass'
shutdown_check INT

start_edge
shutdown_check TERM

# The official release archive is flat (openai_server.py, its imports and the engines at the
# root, no c/): build the same layout with upstream's own packer and serve from it.
echo "== release-archive layout"
mkdir -p "$WORK/release"
"$PY" -I "$COLIBRI_DIR/c/tools/pack_python.py" "$COLIBRI_DIR/c" "$WORK/release" >/dev/null
cp "$COLIBRI_DIR/c/mimo" "$WORK/release/"
[ ! -d "$WORK/release/c" ] || fail "packed release unexpectedly has a c/ folder"
"$H" model validate "$WORK/model" --colibri-dir "$WORK/release" >"$WORK/release_validate.out" 2>&1 \
  || { cat "$WORK/release_validate.out"; fail "model validate failed on the release layout"; }
start_edge "$WORK/release"
client release_curl sh "$HERE/examples/curl.sh"
expect release_curl '"id": ?"fixture"' '^data: \[DONE\]'
shutdown_check INT
echo "PASS: all clients, SIGINT and SIGTERM shutdown, release-archive layout"
