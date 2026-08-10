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
#   - tropel-sdk must be live at the workspace version (currently 0.2.0) — the
#     leaf everything else builds on; publish it before running --execute (the
#     API-presence gate verifies the exact published artifact).
#   - tropel-http is NOT in the set (F1, review fix): it was published by
#     accident because sandbox's `send-request` used `dep:tropel-http`; the
#     sandbox now routes pm.sendRequest through the SDK `DriverHttpClient`
#     trait and `publish = false` blocks future publishes. The already-live
#     0.1.0 stays on the registry until yanked (cargo yank --version 0.1.0
#     tropel-http), but no new publish step references it.
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
#   crates.io API whether each of the six names exists. On --execute a taken
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
#   bash scripts/publish-runtime.sh --resume    # continue a rate-limited
#                                      # release: probe static.crates.io and
#                                      # skip any crate whose version is
#                                      # already live (implies --execute);
#                                      # 429s are waited out automatically
#   bash scripts/publish-runtime.sh --no-wait    # don't auto-wait on 429 rate
#                                      # limits (CI / unattended runs); fail
#                                      # fast instead of sleeping a window
#   bash scripts/publish-runtime.sh --skip-name-check  # skip the crates.io
#                                      # name pre-flight (air-gapped / rate-
#                                      # limited release machines)
#   bash scripts/publish-runtime.sh --skip-api-check  # skip the published-
#                                      # SDK API-presence gate
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

CRATES=(tropel-variables tropel-js tropel-native tropel-auth tropel-sandbox tropel-runtime)

EXECUTE=0
SKIP_NAME_CHECK=0
SKIP_API_CHECK=0
RESUME=0
NO_WAIT=0
for arg in "$@"; do
  case "$arg" in
    --execute) EXECUTE=1 ;;
    --resume) RESUME=1; EXECUTE=1 ;;
    --no-wait) NO_WAIT=1 ;;
    --skip-name-check) SKIP_NAME_CHECK=1 ;;
    --skip-api-check) SKIP_API_CHECK=1 ;;
    *) echo "unknown argument: $arg (expected --execute, --resume, --no-wait, --skip-name-check, --skip-api-check)" >&2; exit 2 ;;
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
  for dep in tropel-sdk tropel-js tropel-native tropel-variables tropel-auth tropel-sandbox tropel-runtime; do
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

# ── Resume support (--resume) ──────────────────────────────────────────────
# crates.io limits NEW crate creation (5/window), so a first-time 6-crate
# release inherently spans several rate-limit windows. --resume makes the
# re-run a one-command retry: before each publish it probes static.crates.io
# for the exact version THIS manifest declares and skips any crate already
# live (the ones that succeeded in an earlier window). HTTP 200 = live
# (skip); anything else = not live yet, publish it — failing safe toward
# attempting the publish, since cargo itself rejects a duplicate version.
crate_is_live() {
  local name="$1" version status
  version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "crates/$name/Cargo.toml" | head -1)
  if [[ -z "$version" ]]; then
    echo "    ? could not read version from crates/$name/Cargo.toml; assuming not published"
    return 1
  fi
  status=$(curl -s -o /dev/null -w '%{http_code}' -H 'User-Agent: tropel-release-gate' \
    "https://static.crates.io/crates/$name/$name-$version.crate" 2>/dev/null || echo 000)
  if [[ "$status" == "200" ]]; then
    echo "    ✓ $name $version is already live on crates.io — skipping"
    return 0
  fi
  echo "    · $name $version not on the registry (HTTP $status) — will publish"
  return 1
}

# ── Rate-limit auto-wait (429 handling) ────────────────────────────────────
# crates.io limits new crate creation (5/window). On a 429 cargo prints
# "try again after <RFC2822 date>". Parse that timestamp, sleep until the
# window opens, and retry the SAME crate — so a multi-window release
# completes in one invocation instead of requiring the operator to notice
# and re-run. --no-wait disables the sleep (CI / unattended runs fail fast
# and the operator re-runs --resume later).
parse_retry_after() {
  # stdin: cargo stderr text; stdout: the RFC2822 retry timestamp or empty.
  grep -oE '[A-Z][a-z]{2}, [0-9]{1,2} [A-Z][a-z]{2} [0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} GMT' | head -1
}

retry_epoch() {
  # $1: RFC2822 timestamp like "Mon, 10 Aug 2026 07:11:32 GMT".
  # GNU date (Linux / Git Bash).
  if date -d "$1" +%s 2>/dev/null; then
    return 0
  fi
  # BSD date (macOS).
  date -jf '%a, %d %b %Y %H:%M:%S GMT' "$1" +%s 2>/dev/null || return 1
}

MAX_RATE_RETRIES=8   # a window is ~10 min; 8 covers any first-release spread

publish_one() {
  local c="$1" out retry_ts now target wait_sec attempt=0
  while :; do
    attempt=$((attempt + 1))
    if out=$(cargo publish -p "$c" --allow-dirty 2>&1); then
      echo "$out" | grep -E 'Uploaded|Published' | head -2 || true
      return 0
    fi
    # Surface the failure, but treat a rate limit as resumable.
    if ! grep -qi '429 Too Many Requests' <<<"$out"; then
      echo "$out" | grep -E '^error|Caused by' | head -4 || true
      return 1
    fi
    if [[ $NO_WAIT -eq 1 || $attempt -gt $MAX_RATE_RETRIES ]]; then
      echo "$out" | grep -E '^error|Caused by' | head -4 || true
      echo "    (429 rate limit — re-run with --resume after the window, or drop --no-wait to auto-wait)"
      return 1
    fi
    retry_ts=$(parse_retry_after <<<"$out" || true)
    if [[ -z "$retry_ts" ]] || ! now=$(date +%s) || ! target=$(retry_epoch "$retry_ts"); then
      echo "$out" | grep -E '^error|Caused by' | head -4 || true
      echo "    (could not parse the 429 retry time — re-run with --resume after the window)"
      return 1
    fi
    wait_sec=$((target - now))
    [[ $wait_sec -lt 1 ]] && wait_sec=1
    echo "    ⏳ rate limited (attempt $attempt); retrying $c in ${wait_sec}s (window opens at $retry_ts)"
    sleep "$wait_sec"
  done
}

NAMES_TAKEN=()
if [[ $SKIP_NAME_CHECK -eq 1 || $RESUME -eq 1 ]]; then
  if [[ $RESUME -eq 1 ]]; then
    echo "── Pre-flight: crates.io name availability (skipped — --resume expects the names to exist) ──"
  else
    echo "── Pre-flight: crates.io name availability (skipped via --skip-name-check) ──"
  fi
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
# crate mid-sequence, after earlier crates already went live.
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
  echo "   The set would fail verification of a later crate mid-sequence, after a partial release."
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
  trap 'echo; echo "⚠️  publish interrupted — publish = true left in:"; for m in "${FLIPPED[@]}"; do echo "    $m"; done; echo "Commit them with the release, or restore to publish = false before retrying. Re-run with --resume to continue past crates already live on crates.io."' ERR
fi

if [[ $RESUME -eq 1 ]]; then
  echo "── Resume mode: skipping crates already live on crates.io (implies --execute) ──"
  if [[ $NO_WAIT -eq 1 ]]; then
    echo "⚠️  --no-wait: rate limits are NOT waited out — re-run --resume manually after each window."
  else
    echo "⚠️  REAL PUBLISH — this run uploads to crates.io and waits out 429 rate limits automatically."
  fi
elif [[ $EXECUTE -eq 1 ]]; then
  if [[ $NO_WAIT -eq 1 ]]; then
    echo "⚠️  REAL PUBLISH — this run uploads to crates.io; 429 rate limits fail fast (--no-wait)."
  else
    echo "⚠️  REAL PUBLISH — this run uploads to crates.io and waits out 429 rate limits automatically."
  fi
fi

echo "── Publish order: ${CRATES[*]} ──"
PUBLISHED_THIS_RUN=()
SKIPPED_THIS_RUN=()
for c in "${CRATES[@]}"; do
  echo
  echo "─── $c ───"
  if [[ $RESUME -eq 1 ]] && crate_is_live "$c"; then
    SKIPPED_THIS_RUN+=("$c")
    continue
  fi
  flip_publish_flag "$c"
  if [[ $EXECUTE -eq 1 ]]; then
    publish_one "$c"
    PUBLISHED_THIS_RUN+=("$c")
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
  if [[ ${#SKIPPED_THIS_RUN[@]} -gt 0 ]]; then
    echo "✅ done — already live (skipped): ${SKIPPED_THIS_RUN[*]}"
    echo "   published this run: ${PUBLISHED_THIS_RUN[*]:-none}"
  else
    echo "✅ published: ${CRATES[*]}"
  fi
else
  echo
  echo "✅ dry-run OK — re-run with --execute to actually publish."
fi
