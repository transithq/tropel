#!/usr/bin/env bash
set -euo pipefail
# P5b size budget gate (TROPEL_MODULARIZATION_TODO.md → P5b "Build + size
# budget"): build the browser slice (tropel-web) for wasm32-wasip1 and fail
# CI above the 8 MB ceiling. Mirrors TROPEL_WASM_BUILD.md Step 7.
#
# Requires the WASI SDK (see TROPEL_WASM_BUILD.md Step 1/2). The repo's
# committed .cargo/config.toml points [env] at the machine-local SDK; CI
# overrides via shell env (shell env wins over [env]).

cd "$(dirname "$0")/.."

echo "── P5b gate: tropel-web builds for wasm32-wasip1 ──"
cargo build -p tropel-web --target wasm32-wasip1 --release

# N1 (TROPEL_MODULARIZATION_REVIEW_R2.md): the shims moved out of the wasm
# (host import), so the artifact is measured WITHOUT the ~289 KB of embedded
# JS. Find it under the default target dir (CI) or the machine-local
# override (C:/tropel-native-target — .cargo/config.toml) like build.sh and
# the F3 harness do.
WASM=""
for c in \
  "target/wasm32-wasip1/release/tropel_web.wasm" \
  "C:/tropel-native-target/wasm32-wasip1/release/tropel_web.wasm"; do
  if [ -f "$c" ]; then WASM="$c"; break; fi
done
if [ -z "$WASM" ]; then
  echo "FAIL: tropel_web.wasm not produced (looked in target/ and C:/tropel-native-target)"
  exit 1
fi

# Post-opt with wasm-opt when available (binaryen); measure the optimized
# artifact. QuickJS alone is ~1–1.5 MB; the shim bundle no longer rides along.
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz -o /tmp/tropel_web_opt.wasm "$WASM"
  WASM=/tmp/tropel_web_opt.wasm
fi

# Portable size read: stat's format flags differ between BSD (macOS, -f%z)
# and GNU (Linux/MSYS, where %z is CHANGE-TIME, not size) — wc -c is identical
# on every platform.
SIZE=$(wc -c < "$WASM")
echo "wasm size: $((SIZE/1024)) KiB"

# N3 (TROPEL_MODULARIZATION_REVIEW_R2.md): real measurement + ~15% headroom.
# Measured 2026-08-10 after N1 (shims → host import): 3,977,788 B unoptimized
# (~3.8 MB). Gate = 4.6 MB — catches a shim re-embedding or a new dep, not
# just a catastrophe. Re-tighten after any payload change (see N1 note).
BUDGET=4600000
if [ "$SIZE" -ge "$BUDGET" ]; then
  echo "FAIL: wasm over budget ($((BUDGET/1024/1024)) MB)"
  exit 1
fi
echo "PASS: under $((BUDGET/1024/1024)) MB budget ($((SIZE/1024)) KiB)"
