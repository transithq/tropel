#!/usr/bin/env bash
# Build @tropel/shims: refresh the shim copies from the monorepo's js/ dir
# (single source of truth), render the ESM bundle in the engine's
# ShimBundle::default() order, and dry-run the pack so packaging problems
# surface pre-publish.
set -euo pipefail
cd "$(dirname "$0")/.."

# ── 1. Refresh the shim sources from ../../js/ ────────────────────────────
mkdir -p shim
cp ../../js/scripting-api/pm.js shim/pm.js
cp ../../js/scripting-api/bru.js shim/bru.js
cp ../../js/chai/chai-shim.js shim/chai-shim.js
cp ../../js/lodash/lodash-shim.js shim/lodash-shim.js
cp ../../js/cryptojs-shim/cryptojs.js shim/cryptojs.js
cp ../../js/exec/exec.js shim/exec.js
cp ../../js/k6-shim/k6-shim.js shim/k6-shim.js
cp ../../js/k6-shim/jslib-shim.js shim/jslib-shim.js
cp ../../js/k6-shim/open-data-shim.js shim/open-data-shim.js
cp ../../js/k6-shim/sleep-shim.js shim/sleep-shim.js
echo "  copied 10 shim sources from ../../js/"

# ── 2. Render the ESM bundle + types ──────────────────────────────────────
node scripts/render-bundle.mjs

# ── 3. Dry-run the publish artifact ───────────────────────────────────────
npm pack --dry-run
