#!/usr/bin/env bash
# Build @tropel/core-wasm: compile tropel-core-wasm for wasm32-unknown-unknown
# (size-tuned release-wasm profile), run wasm-bindgen, optimize with a modern
# binaryen, copy the artifacts into pkg/, and dry-run the pack so packaging
# problems surface pre-publish.
#
# wasm-pack itself is NOT used: its pinned binaryen rejects rustc's
# reference-types output, and its manifest parser refuses inherited workspace
# table fields. The steps below reproduce its pipeline minus those bugs.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO_ROOT="$(cd ../.. && pwd)"

# ── 1. Compile the crate (cargo handles it from the repo root so the
#       workspace + release-wasm profile are in scope) ────────────────────
(cd "$REPO_ROOT" && cargo build -p tropel-core-wasm --target wasm32-unknown-unknown --profile release-wasm)
WASM="$REPO_ROOT/target/wasm32-unknown-unknown/release-wasm/tropel_core_wasm.wasm"
test -f "$WASM" || { echo "error: $WASM not produced" >&2; exit 1; }

# ── 2. wasm-bindgen (web target; the CLI that wasm-pack would install) ────
WASM_BINDGEN_BIN="${WASM_BINDGEN:-wasm-bindgen}"
command -v "$WASM_BINDGEN_BIN" >/dev/null 2>&1 || {
  echo "error: wasm-bindgen not on PATH (cargo install wasm-bindgen-cli)" >&2
  exit 1
}
mkdir -p pkg
"$WASM_BINDGEN_BIN" --target web --out-dir pkg "$WASM"

# ── 3. binaryen -Oz (modern wasm-opt: --enable-* flags required for the
#       bulk-memory rustc emits) ──────────────────────────────────────────
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --strip-debug \
    --enable-bulk-memory --enable-sign-ext \
    --enable-nontrapping-float-to-int --enable-reference-types \
    -o pkg/tropel_core_wasm_bg.wasm pkg/tropel_core_wasm_bg.wasm
else
  echo "warning: wasm-opt not found — shipping unoptimized (npm exec -y -p binaryen -- wasm-opt …)" >&2
fi

# ── 4. Extract catalog metadata from the COMPILED wasm (single source of
#       truth: Rust PREDEFINED_VARIABLE_META) for init-free autocomplete.
#       Written as meta.js (plain ESM default export) so consumers need no
#       import-assertion support ───────────────────────────────────────────
node -e "
const fs = require('fs');
(async () => {
  const g = await import('./pkg/tropel_core_wasm.js');
  await g.default({ module_or_path: fs.readFileSync('./pkg/tropel_core_wasm_bg.wasm') });
  const meta = JSON.parse(g.predefinedVariablesMeta());
  fs.writeFileSync('./pkg/meta.js', 'export default ' + JSON.stringify(meta, null, 2) + ';\n');
})().catch((e) => { console.error(e); process.exit(1); });
"

# ── 5. Sanity: smoke test + size report ───────────────────────────────────
node smoke.mjs
SIZE=$(wc -c < pkg/tropel_core_wasm_bg.wasm)
echo "  wasm size: $((SIZE / 1024)) KiB"

# P5b-style budget gate (API_CLIENT_WEB_PAYLOAD.md §2.3): the variables-only
# core tier measured 457,202 B (wasm-opt -Oz; regex std+unicode-perl, no
# Unicode property tables, no chrono serde). Budget keeps headroom for the
# auth-signing and import-parses slices that join this tier. Re-measure with
# `twiggy top` after any payload change.
BUDGET=700000
if [ "$SIZE" -ge "$BUDGET" ]; then
  echo "FAIL: core wasm over budget ($((BUDGET / 1024)) KiB)" >&2
  exit 1
fi

# ── 5. Dry-run the publish artifact ───────────────────────────────────────
npm pack --dry-run
