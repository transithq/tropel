#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Ordered multi-crate publishing for the runtime publish set (P3c).
#
# Cargo resolves every dependency of a published crate from crates.io —
# nothing is bundled or vendored — so the publish order must be dependency
# order. This script encodes that order so a release isn't six hand-typed
# publishes in the wrong sequence.
#
# Order (each crate's deps must already be on the registry):
#   variables → js → native → auth → sandbox → runtime
#
# Notes:
#   - tropel-sdk is already live (0.1.0) — the leaf everything else builds on.
#   - tropel-http is deliberately NOT in the set: tropel-runtime declares it
#     only as a [dev-dependency] (test-only TestHttpClient seam) and
#     tropel-sandbox declares it optional behind the `send-request` feature,
#     which the publish defaults keep OFF (sandbox publishes without it).
#   - Real publishes happen only after the BACKLOG_V2 Phase 0–2 release gate;
#     this script defaults to --dry-run so the sequence is exercised, not
#     executed, until you pass --execute.
#
# Usage:
#   bash scripts/publish-runtime.sh            # dry-run the whole sequence
#   bash scripts/publish-runtime.sh --execute  # actually publish, in order
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

CRATES=(tropel-variables tropel-js tropel-native tropel-auth tropel-sandbox tropel-runtime)

EXECUTE=0
if [[ "${1:-}" == "--execute" ]]; then
  EXECUTE=1
fi

cd "$(dirname "$0")/.."   # repo root

# P0 marked every internal crate `publish = false` so a stray
# `cargo publish --workspace` can't leak them. Publishing the runtime set is
# the deliberate exception, so the guard is flipped for the duration of this
# script. On --execute the flip stays (the release intent); on a dry-run it is
# restored so the P0 guard isn't silently weakened and the working tree stays
# clean of unintended manifest edits.
FLIPPED=()
flip_publish_flag() {
  local manifest="crates/$1/Cargo.toml"
  if grep -q '^publish = false' "$manifest"; then
    sed -i 's/^publish = false/publish = true/' "$manifest"
    FLIPPED+=("$manifest")
    echo "    flipped publish = true in $manifest (temporarily)"
  fi
}

restore_publish_flags() {
  for manifest in "${FLIPPED[@]}"; do
    sed -i 's/^publish = true/publish = false/' "$manifest"
    echo "    restored publish = false in $manifest"
  done
}

# Failure-safe: under `set -euo pipefail`, a mid-sequence error would exit
# before the explicit restore below, leaving publish = true behind. Trap the
# same restore (dry-run only) so cleanup happens on ANY exit path.
if [[ $EXECUTE -ne 1 ]]; then
  trap restore_publish_flags EXIT
fi

echo "── Publish order: ${CRATES[*]} ──"
for c in "${CRATES[@]}"; do
  echo
  echo "─── $c ───"
  flip_publish_flag "$c"
  if [[ $EXECUTE -eq 1 ]]; then
    cargo publish -p "$c"
  else
    cargo publish -p "$c" --dry-run --allow-dirty
  fi
done

# A dry-run must not leave publish = true behind.
if [[ $EXECUTE -ne 1 && ${#FLIPPED[@]} -gt 0 ]]; then
  restore_publish_flags
fi

if [[ $EXECUTE -eq 1 ]]; then
  echo
  echo "✅ published: ${CRATES[*]}"
else
  echo
  echo "✅ dry-run OK — re-run with --execute to actually publish."
fi
