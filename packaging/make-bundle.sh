#!/usr/bin/env bash
# Build the downloadable Local AI Hub bundles from a Linux host:
#   dist/LocalAIHub-<ver>-windows-x64.zip   (cross-compiled hubd.exe + hub.cmd)
#   dist/LocalAIHub-<ver>-linux-x64.tar.gz  (hubd)
# Needs: rustup target x86_64-pc-windows-gnu, x86_64-w64-mingw32-gcc, cargo-about (--features cli).
# The Colibri engine and Python are NOT bundled: the user adds the official release (README-FIRST.txt).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT="$ROOT/dist"
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "missing x86_64-w64-mingw32-gcc"; exit 1; }
cargo about --version >/dev/null 2>&1 || { echo "missing cargo-about (cargo install cargo-about --locked --features cli)"; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean; commit first so BUILDINFO names the real source"; exit 1; }

cargo build --release --locked -p hubd --target x86_64-pc-windows-gnu
cargo build --release --locked -p hubd
rm -rf "$OUT"; mkdir -p "$OUT"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
cargo about generate --locked -c packaging/about.toml -m crates/hubd/Cargo.toml packaging/about.hbs -o "$TMP/THIRD-PARTY-LICENSES.txt"

crlf() { sed 's/\r$//; s/$/\r/' "$1" > "$2"; }   # Notepad-friendly text in the Windows bundle
stage() { # stage <dir> <os>
  local d=$1 os=$2
  mkdir -p "$d/scripts" "$d/examples" "$d/docs" "$d/colibri"
  cp scripts/validate_model.py scripts/verify_remote.py "$d/scripts/"
  cp examples/* "$d/examples/"
  cp docs/*.md "$d/docs/"
  cp LICENSE NOTICE "$d/"
  if [ "$os" = windows ]; then
    cp target/x86_64-pc-windows-gnu/release/hubd.exe "$d/"
    cp packaging/hub.cmd "$d/"
    crlf packaging/README-FIRST.txt "$d/README-FIRST.txt"
    crlf "$TMP/THIRD-PARTY-LICENSES.txt" "$d/THIRD-PARTY-LICENSES.txt"
    printf 'Unzip the official Colibri v2.0.0 Windows release here (see README-FIRST.txt, step 2).\r\n' > "$d/colibri/PUT-COLIBRI-HERE.txt"
  else
    cp target/release/hubd "$d/"
    cp "$TMP/THIRD-PARTY-LICENSES.txt" "$d/"
    printf 'Put a Colibri v2.0.0 checkout or release here, then: ./hubd ... --colibri-dir colibri (see docs/WINDOWS.md and README.md).\n' > "$d/colibri/PUT-COLIBRI-HERE.txt"
    cp README.md "$d/"
  fi
  {
    echo "Local AI Hub $VER ($os x64)"
    echo "source commit: $(git rev-parse HEAD)"
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ) with $(rustc --version)"
    echo "target: $([ "$os" = windows ] && echo x86_64-pc-windows-gnu || echo x86_64-unknown-linux-gnu)"
    echo "Colibri pin: v2.0.0 bf2442915d6e3dd4cdfd2eb9c2a3d2aa44a25850 (not included)"
    echo "code signing: none (unsigned)"
  } | { if [ "$os" = windows ]; then sed 's/$/\r/'; else cat; fi; } > "$d/BUILDINFO.txt"
  (cd "$d" && find . -type f ! -name SHA256SUMS.txt -print0 | sort -z | xargs -0 sha256sum | sed 's#  \./#  #') \
    | { if [ "$os" = windows ]; then sed 's/$/\r/'; else cat; fi; } > "$d/SHA256SUMS.txt"
}
W="LocalAIHub-$VER-windows-x64"; L="LocalAIHub-$VER-linux-x64"
stage "$TMP/$W" windows
stage "$TMP/$L" linux
# Deterministic order; the zip keeps one top-level folder.
(cd "$TMP" && python3 - "$W" "$OUT/$W.zip" <<'PY'
import os, sys, zipfile
root, out = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for d, dirs, files in sorted(os.walk(root)):
        dirs.sort()
        for f in sorted(files):
            p = os.path.join(d, f)
            z.write(p, p.replace(os.sep, "/"))
        if not files and not dirs:
            z.writestr(d.replace(os.sep, "/") + "/", "")
PY
)
tar -C "$TMP" --owner=0 --group=0 --sort=name -czf "$OUT/$L.tar.gz" "$L"
(cd "$OUT" && sha256sum "$W.zip" "$L.tar.gz" > SHA256SUMS.txt)
ls -l "$OUT"; cat "$OUT/SHA256SUMS.txt"
