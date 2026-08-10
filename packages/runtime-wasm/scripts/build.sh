#!/usr/bin/env bash
# Build @tropel/runtime-wasm: compile TS, copy the tropel_web.wasm artifact into
# the package, and dry-run the pack so packaging problems surface pre-publish.
set -euo pipefail
cd "$(dirname "$0")/.."

# ── 1. The wasm artifact ──────────────────────────────────────────────────
# Built by the wasm job / scripts/wasm-size.sh. Accept an explicit path, then
# check the standard candidates (CI default target dir + the machine-local
# target-dir override from .cargo/config.toml).
ARTIFACT=""
if [[ -n "${TROPEL_WASM_PATH:-}" && -f "${TROPEL_WASM_PATH}" ]]; then
  ARTIFACT="${TROPEL_WASM_PATH}"
else
  for c in \
    "$PWD/../../target/wasm32-wasip1/release-wasm/tropel_web.wasm" \
    "C:/tropel-native-target/wasm32-wasip1/release-wasm/tropel_web.wasm" \
    "$PWD/../../target/wasm32-wasip1/release/tropel_web.wasm" \
    "C:/tropel-native-target/wasm32-wasip1/release/tropel_web.wasm"; do
    if [[ -f "$c" ]]; then ARTIFACT="$c"; break; fi
  done
  # A stale pre-profile release/ artifact (3.98 MB, embedded-shims era) must
  # never ship silently — the release-wasm profile is the size-tuned source.
  if [[ "$ARTIFACT" == *"/release/tropel_web.wasm" ]]; then
    echo "warning: using pre-profile release/ artifact ($ARTIFACT) — run the wasm job / scripts/wasm-size.sh to build release-wasm" >&2
  fi
fi
if [[ -z "$ARTIFACT" ]]; then
  echo "error: tropel_web.wasm not found — run the wasm job / scripts/wasm-size.sh first, or set TROPEL_WASM_PATH" >&2
  exit 1
fi
mkdir -p wasm
cp "$ARTIFACT" wasm/tropel_web.wasm
echo "  copied wasm: $(du -h wasm/tropel_web.wasm | cut -f1)"

# ── 2. Compile TS ─────────────────────────────────────────────────────────
npx tsc -p tsconfig.json
echo "  compiled dist/"

# ── 3. Dry-run the publish artifact ───────────────────────────────────────
npm pack --dry-run
