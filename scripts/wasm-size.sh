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
# API_CLIENT_WEB_PAYLOAD.md §2.4 size discipline: the dedicated
# release-wasm profile (opt-level z, fat LTO, panic abort, strip) in the
# workspace Cargo.toml. Native release stays untouched (fast for load gen).
cargo build -p tropel-web --target wasm32-wasip1 --profile release-wasm

# N1 (TROPEL_MODULARIZATION_REVIEW_R2.md): the shims moved out of the wasm
# (host import), so the artifact is measured WITHOUT the ~289 KB of embedded
# JS. Find it under the default target dir (CI) or the machine-local
# override (C:/tropel-native-target — .cargo/config.toml) like build.sh and
# the F3 harness do. The custom profile lands in `release-wasm/`.
WASM=""
for c in \
  "target/wasm32-wasip1/release-wasm/tropel_web.wasm" \
  "C:/tropel-native-target/wasm32-wasip1/release-wasm/tropel_web.wasm"; do
  if [ -f "$c" ]; then WASM="$c"; break; fi
done
if [ -z "$WASM" ]; then
  echo "FAIL: tropel_web.wasm not produced (looked in target/ and C:/tropel-native-target release-wasm)"
  exit 1
fi

# Post-opt with wasm-opt when available (binaryen); measure the optimized
# artifact. QuickJS alone is ~1–1.5 MB; the shim bundle no longer rides along.
# --strip-debug also drops the ~372 KB "function names" subsection twiggy
# measured (API_CLIENT_WEB_PAYLOAD.md §2.4).
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --strip-debug -o /tmp/tropel_web_opt.wasm "$WASM"
  WASM=/tmp/tropel_web_opt.wasm
fi

# Portable size read: stat's format flags differ between BSD (macOS, -f%z)
# and GNU (Linux/MSYS, where %z is CHANGE-TIME, not size) — wc -c is identical
# on every platform.
SIZE=$(wc -c < "$WASM")
echo "wasm size: $((SIZE/1024)) KiB"

# N3 (TROPEL_MODULARIZATION_REVIEW_R2.md) + API_CLIENT_WEB_PAYLOAD.md §2.4:
# real measurement + ~15% headroom. Re-tightened 2026-08-10 after the
# release-wasm profile (opt z / fat LTO / panic abort / strip) landed:
# 3,977,788 B (default profile, post-N1) → 2,518,659 B raw. Budget is
# anchored to the RAW artifact — CI's wasm-opt -Oz --strip-debug pass is
# always smaller, so it holds everywhere. Re-measure with `twiggy top`
# after any payload change and re-tighten.
BUDGET=2900000
if [ "$SIZE" -ge "$BUDGET" ]; then
  echo "FAIL: wasm over budget ($((BUDGET/1024)) KiB)"
  exit 1
fi
echo "PASS: under $((BUDGET/1024)) KiB budget ($((SIZE/1024)) KiB)"
