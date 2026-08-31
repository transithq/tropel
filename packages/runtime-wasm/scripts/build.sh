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
# Resolve tsc from the workspace, NEVER from the registry.
#
# This was a bare `npx tsc -p tsconfig.json`. `typescript` IS declared as a
# devDependency here, but with no `node_modules` present (CI runs `npm install`
# at the root first; a local release run did not) npx falls through to the
# registry and installs the package literally named `tsc` — an unrelated,
# deprecated package that is NOT the TypeScript compiler. It then prints
#
#   This is not the tsc command you are looking for
#
# which reads as a toolchain mystery rather than "your deps aren't installed".
#
# The real problem is worse than the confusing message: a bare `npx <name>` in
# a release build will silently download and EXECUTE an arbitrary registry
# package under a name the project never vendored. Resolve the local binary
# explicitly and fail loudly if it is absent.
find_tsc() {
  local candidate
  for candidate in ./node_modules/.bin/tsc ../../node_modules/.bin/tsc; do
    if [[ -x "$candidate" ]]; then printf '%s' "$candidate"; return 0; fi
  done
  return 1
}

# SELF-SUFFICIENT, deliberately. Requiring every caller to remember a prior
# `npm install` has now cost two failed releases: ci.yml runs it, release.sh
# was fixed to run it, and release.yml — the third caller — was missed, so the
# v0.2.0 tag failed here with the error below AFTER the wasm artifact had been
# built and copied. Installing the workspace's own declared devDependency is a
# precondition of this script, so this script owns it rather than each caller
# re-implementing it.
#
# This does NOT weaken the registry protection: it installs the pinned
# workspace devDependency and re-resolves the LOCAL binary. If that still
# fails it errors out; it never falls back to `npx`, which would fetch and
# execute a package this project never vendored.
TSC=$(find_tsc || true)
if [[ -z "$TSC" ]]; then
  echo "  typescript not resolved locally — running 'npm install' at the repo root" >&2
  ( cd ../.. && npm install --no-audit --no-fund ) >/dev/null 2>&1 || true
  TSC=$(find_tsc || true)
fi
if [[ -z "$TSC" ]]; then
  echo "error: typescript still not installed after 'npm install' at the repo root" >&2
  echo "       (typescript is a devDependency of this workspace package)." >&2
  echo "       Refusing to 'npx tsc', which would fetch an unrelated registry package." >&2
  exit 1
fi
"$TSC" -p tsconfig.json
echo "  compiled dist/"

# ── 3. Dry-run the publish artifact ───────────────────────────────────────
npm pack --dry-run
