#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Ordered multi-crate publishing for the runtime publish set (P3c).
#
# Cargo resolves every dependency of a published crate from crates.io —
# nothing is bundled or vendored — so the publish order must be dependency
# order. This script encodes that order so a release isn't seven hand-typed
# publishes in the wrong sequence.
#
# Order (each crate's deps must already be on the registry):
#   variables → js → native → auth → http → sandbox → runtime
#
# Notes:
#   - tropel-sdk must be live at the workspace version (currently 0.2.0) — the
#     leaf everything else builds on; publish it before running --execute (the
#     API-presence gate verifies the exact published artifact).
#   - tropel-http IS in the set, between auth and sandbox: tropel-sandbox's
#     DEFAULT feature is `send-request = ["dep:tropel-http"]`, so the
#     published sandbox pulls tropel-http from the registry at build time —
#     it cannot be skipped. (tropel-runtime additionally dev-depends on it.)
#     A consumer that only wants pm.response/pm.environment can set
#     default-features = false, but the default publish must carry it.
#   - Real publishes happen only after the BACKLOG_V2 Phase 0–2 release gate;
#     this script defaults to --dry-run so the sequence is exercised, not
#     executed, until you pass --execute.
#
#   Why --dry-run carries [patch] overrides: `cargo publish` (even --dry-run,
#   even `cargo package --no-verify`) normalizes the packaged manifest — every
#   `path` dep becomes a bare `version` requirement — and must resolve those
#   versions against a registry to embed the Cargo.lock. Until the earlier
#   crates are actually published, siblings are unfindable and packaging
#   aborts with `no matching package named <sibling> found`. The patches below
#   resolve the unpublished siblings from their local paths, so the dry-run
#   packages AND verify-builds the real .crate artifact (catching missing
#   versions, missing package files, broken packaged builds) while still
#   dry-run-aborting the upload. crates.io gets zero UPLOAD traffic — the only
#   contact is the read-only name-availability API check below (skip with
#   --skip-name-check). tropel-sdk is
#   patched to the local submodule checkout too — the dry-run verifies the
#   LOCAL SDK, not the published copy (desirable pre-release, but be aware of
#   the divergence when reading results).
#
#   Why both paths use --allow-dirty: the flip above edits a TRACKED file
#   (crates/$c/Cargo.toml) in place, so the working tree is dirty by the time
#   `cargo publish` runs — without --allow-dirty cargo refuses to package
#   ("uncommitted changes"). The flip is the point on --execute (release
#   intent, committed with the release); --allow-dirty only waives the dirty-
#   tree check, never any registry or manifest validation.
#
#   Name availability pre-flight: a dry-run can never catch a taken crates.io
#   name (nothing is uploaded), so before publishing, the script asks the
#   crates.io API whether each of the seven names exists. On --execute a taken
#   name aborts BEFORE any upload; on a dry-run it is informational only.
#
#   Published-SDK API presence pre-flight (--execute only): the dry-run's
#   [patch] overrides resolve tropel-sdk from the LOCAL submodule, masking a
#   stale PUBLISHED SDK. The real publish resolves tropel-sdk from crates.io,
#   so a missing API would fail verification of a LATER crate mid-sequence,
#   after earlier crates already went live. Before flipping anything, the
#   script downloads the exact published artifact the set will resolve and
#   greps it for the APIs the set needs (ExpectedStatus/status_is_expected).
#   Fail-closed: any download or grep failure aborts before a single publish
#   flag is flipped. Escape hatch: --skip-api-check.
#
# Usage:
#   bash scripts/publish-runtime.sh             # dry-run the whole sequence
#   bash scripts/publish-runtime.sh --execute   # actually publish, in order
#   bash scripts/publish-runtime.sh --skip-name-check  # skip the crates.io
#                                      # name pre-flight (air-gapped / rate-
#                                      # limited release machines)
#   bash scripts/publish-runtime.sh --skip-api-check  # skip the published-
#                                      # SDK API-presence gate
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

CRATES=(tropel-variables tropel-js tropel-native tropel-auth tropel-http tropel-sandbox tropel-runtime)

EXECUTE=0
SKIP_NAME_CHECK=0
SKIP_API_CHECK=0
for arg in "$@"; do
  case "$arg" in
    --execute) EXECUTE=1 ;;
    --skip-name-check) SKIP_NAME_CHECK=1 ;;
    --skip-api-check) SKIP_API_CHECK=1 ;;
    *) echo "unknown argument: $arg (expected --execute, --skip-name-check, --skip-api-check)" >&2; exit 2 ;;
  esac
done

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

# Dry-run only: resolve unpublished workspace siblings from local paths (see
# the header note). Patching extras is harmless — cargo applies a patch only
# when the name matches a dependency of the crate being packaged.
PATCH_ARGS=()
if [[ $EXECUTE -ne 1 ]]; then
  for dep in tropel-sdk tropel-http tropel-js tropel-native tropel-variables tropel-auth tropel-sandbox tropel-runtime; do
    PATCH_ARGS+=(--config "patch.crates-io.$dep.path=\"crates/$dep\"")
  done
fi

# ── Pre-flight ──────────────────────────────────────────────────────────────
# crates.io name availability. 404 = free; 200 = taken (by us or anyone);
# 000/unreachable = network/API problem, warn and continue (publish itself
# will surface a real registry error). Hard gate on --execute only.
check_name_available() {
  local name="$1" status
  status=$(curl -s -o /dev/null -w '%{http_code}' -H 'User-Agent: tropel-release-gate' \
    "https://crates.io/api/v1/crates/$name" 2>/dev/null || echo 000)
  if [[ "$status" == "404" ]]; then
    echo "    ✓ $name is free on crates.io"
    return 0
  elif [[ "$status" == "200" ]]; then
    echo "    ✗ $name is ALREADY TAKEN on crates.io"
    return 1
  else
    echo "    ? $name — crates.io API unreachable (HTTP $status); proceeding"
    return 0
  fi
}

NAMES_TAKEN=()
if [[ $SKIP_NAME_CHECK -eq 1 ]]; then
  echo "── Pre-flight: crates.io name availability (skipped via --skip-name-check) ──"
else
  echo "── Pre-flight: crates.io name availability ──"
  for c in "${CRATES[@]}"; do
    if ! check_name_available "$c"; then
      NAMES_TAKEN+=("$c")
    fi
  done
fi
if [[ ${#NAMES_TAKEN[@]} -gt 0 ]]; then
  echo "⚠️  taken: ${NAMES_TAKEN[*]}"
  if [[ $EXECUTE -eq 1 ]]; then
    echo "❌ aborting: crates.io already has a crate named ${NAMES_TAKEN[*]}; cargo publish would fail at upload."
    exit 1
  fi
  echo "   (dry-run: informational only — nothing is uploaded, but a real publish would fail for these.)"
fi

# ── Pre-flight: published tropel-sdk API presence (--execute only) ─────────
# The dry-run's [patch] overrides resolve tropel-sdk from the LOCAL submodule,
# masking a stale PUBLISHED SDK. The real publish resolves tropel-sdk from
# crates.io, so a missing API would fail the verification build of a LATER
# crate (tropel-http) mid-sequence, after earlier crates already went live.
# Download the exact artifact the set will resolve and confirm it carries the
# APIs the set needs. Fail-closed: any download/extract/grep failure aborts
# BEFORE a single publish flag is flipped.
# Fail-closed by design: this gate exists to stop a partial release, so an
# inability to inspect the published artifact (download/extract/manifest) is
# itself a reason to abort — never to silently proceed. --skip-api-check is
# the explicit escape hatch for genuinely broken environments.
check_published_sdk_api() {
  local sdk_version tmpdir crate_file src_dir
  sdk_version=$(sed -n 's/^tropel-sdk = { path = "crates\/tropel-sdk", version = "\([0-9][^"]*\)".*/\1/p' Cargo.toml | head -1)
  if [[ -z "$sdk_version" ]]; then
    echo "❌ could not read tropel-sdk version from root Cargo.toml; aborting before any publish."
    echo "   Fix the tropel-sdk entry in [workspace.dependencies], or pass --skip-api-check to bypass."
    return 1
  fi
  echo "── Pre-flight: published tropel-sdk $sdk_version API presence ──"
  tmpdir=$(mktemp -d) || { echo "❌ mktemp failed; aborting before any publish."; return 1; }
  crate_file="$tmpdir/tropel-sdk-$sdk_version.crate"
  if ! curl -fsSL -H 'User-Agent: tropel-release-gate' -o "$crate_file" \
      "https://static.crates.io/crates/tropel-sdk/tropel-sdk-$sdk_version.crate" 2>/dev/null; then
    echo "❌ could not download published tropel-sdk $sdk_version from static.crates.io; aborting before any publish."
    echo "   Retry when network is available, or pass --skip-api-check to bypass."
    rm -rf "$tmpdir"
    return 1
  fi
  if ! tar xzf "$crate_file" -C "$tmpdir" 2>/dev/null; then
    echo "❌ could not extract published tropel-sdk $sdk_version; aborting before any publish."
    rm -rf "$tmpdir"
    return 1
  fi
  src_dir="$tmpdir/tropel-sdk-$sdk_version/src"
  # Match the exact public surface the set crates call, across ALL sdk modules
  # (not just config.rs), so a future module move can't false-negative.
  if grep -rq 'pub enum ExpectedStatus' "$src_dir" 2>/dev/null && grep -rq 'pub fn status_is_expected' "$src_dir" 2>/dev/null; then
    echo "    ✓ published tropel-sdk $sdk_version carries ExpectedStatus + status_is_expected"
    rm -rf "$tmpdir"
    return 0
  fi
  echo "❌ published tropel-sdk $sdk_version is MISSING ExpectedStatus/status_is_expected."
  echo "   The set would fail verification at tropel-http mid-sequence, after a partial release."
  echo "   Fix: bump + publish tropel-sdk with the new API, update the workspace dep + submodule pointer, then re-run."
  rm -rf "$tmpdir"
  return 1
}

if [[ $EXECUTE -eq 1 && $SKIP_API_CHECK -ne 1 ]]; then
  if ! check_published_sdk_api; then
    exit 1
  fi
fi

if [[ $EXECUTE -eq 1 ]]; then
  echo "⚠️  Verifying against the LOCAL tropel-sdk submodule checkout — consumers get the registry copy. Make sure crates/tropel-sdk is exactly the state you intend to ship."
fi

# On --execute, a mid-sequence failure would leave publish = true in the
# flipped manifests (no restore — that is the release intent). Report them so
# a confused second run can't happen.
if [[ $EXECUTE -eq 1 ]]; then
  trap 'echo; echo "⚠️  publish interrupted — publish = true left in:"; for m in "${FLIPPED[@]}"; do echo "    $m"; done; echo "Commit them with the release, or restore to publish = false before retrying."' ERR
fi

echo "── Publish order: ${CRATES[*]} ──"
for c in "${CRATES[@]}"; do
  echo
  echo "─── $c ───"
  flip_publish_flag "$c"
  if [[ $EXECUTE -eq 1 ]]; then
    cargo publish -p "$c" --allow-dirty
  else
    cargo publish -p "$c" --dry-run --allow-dirty "${PATCH_ARGS[@]}"
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
