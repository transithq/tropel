#!/usr/bin/env bash
# Build @tropel/input-wasm: compile tropel-input-wasm for wasm32-unknown-unknown
# (size-tuned release-wasm profile), run wasm-bindgen, optimize with a modern
# binaryen when available, copy the artifacts into pkg/, and dry-run the pack.
#
# Mirrors packages/core-wasm/scripts/build.sh (same wasm-pack-less pipeline).
set -euo pipefail
cd "$(dirname "$0")/.."
REPO_ROOT="$(cd ../.. && pwd)"

# ── 1. Compile the crate (workspace + release-wasm profile in scope) ───────
(cd "$REPO_ROOT" && cargo build -p tropel-input-wasm --target wasm32-unknown-unknown --profile release-wasm)
WASM="$REPO_ROOT/target/wasm32-unknown-unknown/release-wasm/tropel_input_wasm.wasm"
test -f "$WASM" || { echo "error: $WASM not produced" >&2; exit 1; }

# ── 2. wasm-bindgen (web target) ───────────────────────────────────────────
WASM_BINDGEN_BIN="${WASM_BINDGEN:-wasm-bindgen}"
command -v "$WASM_BINDGEN_BIN" >/dev/null 2>&1 || {
  echo "error: wasm-bindgen not on PATH (cargo install wasm-bindgen-cli)" >&2
  exit 1
}
mkdir -p pkg
"$WASM_BINDGEN_BIN" --target web --out-dir pkg "$WASM"

# ── 3. binaryen -Oz (optional; skip with a warning when unavailable) ───────
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --strip-debug \
    --enable-bulk-memory --enable-sign-ext \
    --enable-nontrapping-float-to-int --enable-reference-types \
    -o pkg/tropel_input_wasm_bg.wasm pkg/tropel_input_wasm_bg.wasm
else
  echo "warning: wasm-opt not found — shipping unoptimized (npm exec -y -p binaryen -- wasm-opt …)" >&2
fi

# ── 4. Sanity: smoke test + size report ────────────────────────────────────
node smoke.mjs
SIZE=$(wc -c < pkg/tropel_input_wasm_bg.wasm)
echo "  wasm size: $((SIZE / 1024)) KiB"

# Lazy slice budget — generous on purpose: this wasm is fetched ONLY when the
# import UI opens (cold path), unlike the eager core tier's hard 700 KB gate.
# The gate catches runaway growth (e.g. an accidentally-linked scripting tier).
BUDGET=1500000
if [ "$SIZE" -ge "$BUDGET" ]; then
  echo "FAIL: input wasm over budget ($((BUDGET / 1024)) KiB)" >&2
  exit 1
fi

# ── 5. Dry-run the publish artifact ────────────────────────────────────────
npm pack --dry-run