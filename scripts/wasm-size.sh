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

WASM="target/wasm32-wasip1/release/tropel_web.wasm"
if [ ! -f "$WASM" ]; then
  echo "FAIL: $WASM not produced"
  exit 1
fi

# Post-opt with wasm-opt when available (binaryen); measure the optimized
# artifact. The ceiling is deliberately loose: QuickJS alone is ~1–1.5 MB.
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz -o /tmp/tropel_web_opt.wasm "$WASM"
  WASM=/tmp/tropel_web_opt.wasm
fi

SIZE=$(stat -f%z "$WASM" 2>/dev/null || stat -c%z "$WASM")
echo "wasm size: $((SIZE/1024)) KiB"

if [ "$SIZE" -ge 8000000 ]; then
  echo "FAIL: wasm over budget (8 MB)"
  exit 1
fi
echo "PASS: under 8 MB budget"
