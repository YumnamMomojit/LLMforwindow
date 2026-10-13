#!/usr/bin/env bash
# Build the downloadable Semdok Edge bundles from a Linux host:
#   dist/SemdokEdge-<ver>-windows-x64.zip   (cross-compiled semdok-edge.exe + semdok.cmd)
#   dist/SemdokEdge-<ver>-linux-x64.tar.gz  (semdok-edge)
# Needs: rustup target x86_64-pc-windows-gnu, x86_64-w64-mingw32-gcc, cargo-about (--features cli).
# The Colibri engine and Python are NOT bundled: the user adds the official release (README-FIRST.txt).
# Reproducible: the same commit and toolchain (rustc, mingw-w64) give the same archives, whatever the checkout
# path, the target directory or the time of the build. Every date is the commit's (SOURCE_DATE_EPOCH); the
# checkout, target, cargo and rustup paths are remapped out of the binaries; the Windows linker writes no
# timestamp and the symbol table is stripped (it carries rustc temp-dir names); archive entries have fixed
# order, owners, modes and times.
# semdok-edge is always relinked here and copied from the path cargo reports for this very build, so a binary left
# over from an earlier build (another CARGO_TARGET_DIR or build.target-dir) is never shipped.
set -euo pipefail
export LC_ALL=C TZ=UTC
umask 022
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT="$ROOT/dist"
WIN=x86_64-pc-windows-gnu LINUX=x86_64-unknown-linux-gnu
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "missing x86_64-w64-mingw32-gcc"; exit 1; }
cargo about --version >/dev/null 2>&1 || { echo "missing cargo-about (cargo install cargo-about --locked --features cli)"; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean; commit first so BUILDINFO names the real source"; exit 1; }
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# Where cargo really writes (CARGO_TARGET_DIR, build.target-dir), not an assumed ./target.
TARGET_DIR=$(cargo metadata --format-version 1 --no-deps --locked | python3 -I -c \
  'import json, sys; print(json.load(sys.stdin)["target_directory"])')
# Rust flags for these builds only (this replaces any RUSTFLAGS of the environment); \x1f-separated so
# that paths with spaces survive. With several matching prefixes rustc uses the last one: the target
# directory, often inside the checkout, comes last.
flags() { local IFS=$'\x1f'; echo "$*"; }
REMAP=("--remap-path-prefix=$ROOT=/build/semdok-edge"
       "--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
       "--remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
       "--remap-path-prefix=$TARGET_DIR=/target")

# build <triple> <rustflags...>: relink semdok-edge for <triple> and set EXE to the executable cargo reports.
build() {
  local triple=$1; shift
  local rf; rf="$(flags "$@")"
  # Only semdok-edge's own artifacts: the dependencies stay cached, but semdok-edge is compiled and linked again.
  cargo clean --release -p semdok-edge --target "$triple" --quiet
  : > "$TMP/stamp"
  CARGO_ENCODED_RUSTFLAGS="$rf" cargo build --release --locked -p semdok-edge --target "$triple" \
    --message-format=json-render-diagnostics > "$TMP/cargo.json"
  EXE=$(python3 -I - "$TMP/cargo.json" <<'PY'
import json, sys
found = []
for line in open(sys.argv[1], encoding="utf-8"):
    try:
        m = json.loads(line)
    except ValueError:
        continue
    t = m.get("target") or {}
    if m.get("reason") == "compiler-artifact" and t.get("name") == "semdok-edge" and "bin" in t.get("kind", ()) \
            and m.get("executable"):
        found.append((m["executable"], m.get("fresh")))
if len(found) != 1 or found[0][1] is not False:
    sys.exit(f"cargo did not report one freshly built semdok-edge executable: {found}")
print(found[0][0])
PY
)
  [ -f "$EXE" ] && [ "$EXE" -nt "$TMP/stamp" ] || { echo "$EXE is missing or older than this build"; exit 1; }
}
build "$WIN" "${REMAP[@]}" -C link-arg=-Wl,--no-insert-timestamp -C strip=symbols
WIN_EXE="$TMP/semdok-edge.exe"; cp "$EXE" "$WIN_EXE"
build "$LINUX" "${REMAP[@]}"
LINUX_EXE="$TMP/semdok-edge"; cp "$EXE" "$LINUX_EXE"
rm -rf "$OUT"; mkdir -p "$OUT"
cargo about generate --locked -c packaging/about.toml -m crates/semdok-edge/Cargo.toml packaging/about.hbs -o "$TMP/THIRD-PARTY-LICENSES.txt"

crlf() { sed 's/\r$//; s/$/\r/' "$1" > "$2"; }   # Notepad-friendly text in the Windows bundle
stage() { # stage <dir> <os>
  local d=$1 os=$2
  mkdir -p "$d/scripts" "$d/examples" "$d/docs" "$d/colibri"
  cp scripts/validate_model.py scripts/verify_remote.py "$d/scripts/"
  cp examples/* "$d/examples/"
  cp docs/*.md "$d/docs/"
  cp LICENSE NOTICE "$d/"
  if [ "$os" = windows ]; then
    cp "$WIN_EXE" "$d/semdok-edge.exe"
    crlf packaging/semdok.cmd "$d/semdok.cmd"   # cmd.exe misreads labels in LF-only batch files
    crlf packaging/README-FIRST.txt "$d/README-FIRST.txt"
    crlf "$TMP/THIRD-PARTY-LICENSES.txt" "$d/THIRD-PARTY-LICENSES.txt"
    printf 'Unzip the official Colibri v2.0.0 Windows release here (see README-FIRST.txt, step 2).\r\n' > "$d/colibri/PUT-COLIBRI-HERE.txt"
  else
    cp "$LINUX_EXE" "$d/semdok-edge"
    cp "$TMP/THIRD-PARTY-LICENSES.txt" "$d/"
    printf 'Put a Colibri v2.0.0 checkout or release here, then: ./semdok-edge ... --colibri-dir colibri (see docs/WINDOWS.md and README.md).\n' > "$d/colibri/PUT-COLIBRI-HERE.txt"
    cp README.md "$d/"
  fi
  {
    echo "Semdok Edge $VER ($os x64)"
    echo "source commit: $(git rev-parse HEAD)"
    echo "source date: $(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ); built with $(rustc --version)"
    echo "target: $([ "$os" = windows ] && echo "$WIN" || echo "$LINUX")"
    echo "Colibri pin: v2.0.0 bf2442915d6e3dd4cdfd2eb9c2a3d2aa44a25850 (not included)"
    echo "code signing: none (unsigned)"
  } | { if [ "$os" = windows ]; then sed 's/$/\r/'; else cat; fi; } > "$d/BUILDINFO.txt"
  (cd "$d" && find . -type f ! -name SHA256SUMS.txt -print0 | sort -z | xargs -0 sha256sum | sed 's#  \./#  #') \
    | { if [ "$os" = windows ]; then sed 's/$/\r/'; else cat; fi; } > "$d/SHA256SUMS.txt"
}
W="SemdokEdge-$VER-windows-x64"; L="SemdokEdge-$VER-linux-x64"
stage "$TMP/$W" windows
stage "$TMP/$L" linux
# Deterministic order, times and modes; the zip keeps one top-level folder.
(cd "$TMP" && python3 -I - "$W" "$OUT/$W.zip" "$SOURCE_DATE_EPOCH" <<'PY'
import os, sys, time, zipfile
root, out, epoch = sys.argv[1], sys.argv[2], int(sys.argv[3])
stamp = time.gmtime(max(epoch, 315532800))[:6]  # zip dates start in 1980
def entry(name, mode):
    i = zipfile.ZipInfo(name, stamp)
    i.create_system, i.external_attr = 3, mode << 16
    i.compress_type = zipfile.ZIP_DEFLATED
    return i
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for d, dirs, files in sorted(os.walk(root)):
        for f in sorted(files):
            p = os.path.join(d, f)
            mode = 0o100755 if os.access(p, os.X_OK) else 0o100644
            with open(p, "rb") as fh:
                z.writestr(entry(p.replace(os.sep, "/"), mode), fh.read(), compresslevel=9)
        if not files and not dirs:
            z.writestr(entry(d.replace(os.sep, "/") + "/", 0o40755), b"")
PY
)
tar -C "$TMP" --format=gnu --owner=0 --group=0 --numeric-owner --sort=name --mtime="@$SOURCE_DATE_EPOCH" \
  --mode='go-w' -cf - "$L" | gzip -n -9 > "$OUT/$L.tar.gz"
(cd "$OUT" && sha256sum "$W.zip" "$L.tar.gz" > SHA256SUMS.txt)
ls -l "$OUT"; cat "$OUT/SHA256SUMS.txt"
