#!/usr/bin/env bash
#
# Tropel release driver.
#
#   bash scripts/release.sh --check     # verify only, publishes nothing
#   bash scripts/release.sh --publish   # the real thing
#
# Publishes every crate in dependency order, then the npm packages, then tags.
#
# WHY THE ORDER MATTERS: a `path` + `version` dependency only resolves at
# publish time if that EXACT version is already on the registry. Publish a
# dependent before its dependency and cargo rejects it. The order below is the
# dependency graph flattened — do not reorder it casually.
#
# WHY IT WAITS: crates.io's index is eventually consistent. A crate published
# a second ago is not yet visible to the next `cargo publish`, which then fails
# with "failed to select a version". Each step polls the API until the version
# it just pushed is actually visible.
#
# Everything here is idempotent: a crate already at the target version is
# skipped, so a re-run after a mid-way failure resumes rather than restarts.
set -uo pipefail
cd "$(dirname "$0")/.."

MODE="${1:---check}"
DRY=1
[[ "$MODE" == "--publish" ]] && DRY=0

red()  { printf '\033[31m%s\033[0m\n' "$*"; }
grn()  { printf '\033[32m%s\033[0m\n' "$*"; }
ylw()  { printf '\033[33m%s\033[0m\n' "$*"; }
step() { printf '\n\033[1m── %s ─────────────────────────────\033[0m\n' "$*"; }

die() { red "FAIL: $*"; exit 1; }

# Dependency order. Leaves first.
CRATES=(
  tropel-sdk        # leaf: the published contract
  tropel-variables
  tropel-js
  tropel-auth
  tropel-native
  tropel-http
  tropel-core
  tropel-metrics
  tropel-sandbox
  tropel-runtime
  tropel-scheduler
  tropel-report
  tropel-collection
  tropel-es
  tropel-ext
  tropel-build
  tropel            # the binary, last
)

crate_version() {
  local c="$1" f
  for f in "crates/$c/Cargo.toml" "crates/inputs/$c/Cargo.toml" "crates/extensions/$c/Cargo.toml"; do
    [[ -f "$f" ]] && { grep -m1 '^version' "$f" | sed -E 's/.*"(.*)".*/\1/'; return; }
  done
}

published_versions() {
  curl -s -H "User-Agent: tropel-release" "https://crates.io/api/v1/crates/$1" \
    | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    print(' '.join(v['num'] for v in d['versions'] if not v['yanked']))
except Exception:
    print('')
" 2>/dev/null
}

wait_for_index() {
  local crate="$1" want="$2" i
  for i in $(seq 1 60); do
    [[ " $(published_versions "$crate") " == *" $want "* ]] && { grn "    visible on crates.io"; return 0; }
    sleep 5
  done
  die "$crate@$want never appeared in the index after 5 minutes"
}

# ── 0 · Preflight ────────────────────────────────────────────────────────────
step "Preflight"

[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty — commit or stash first"

BRANCH=$(git rev-parse --abbrev-ref HEAD)
[[ "$BRANCH" == "master" ]] || ylw "WARNING: on '$BRANCH', not master"

git fetch -q origin master
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/master)" ]] \
  || die "HEAD is not origin/master — pull or push first"

VERSION=$(crate_version tropel)
grn "  release version: $VERSION"

# Toolchain check FIRST — before the test suite (minutes) and long before any
# irreversible publish. The npm step shells out to four build.sh scripts that
# need wasm-bindgen/wasm-opt/node; discovering a missing one at step 2 used to
# cost a full build and reported it as "size gate?", which is misleading.
# Every message below carries the exact command to fix it.
step "Toolchain"
WB_VERSION=$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/[",]/,""); print $3; exit}' Cargo.lock)
missing=0
need() { # need <cmd> <install hint>
  if command -v "$1" >/dev/null 2>&1; then
    grn "  $1"
  else
    red "  $1 — MISSING"
    echo "      $2"
    missing=1
  fi
}
need node    "install Node 20+ (https://nodejs.org)"
need npm     "ships with Node"
need gh      "brew install gh   # needed for the GitHub release"
need wasm-opt "brew install binaryen   # or: npm i -g wasm-opt"
# wasm-bindgen-cli MUST match the wasm-bindgen crate in Cargo.lock exactly —
# a mismatch fails with a confusing schema error, not a version error.
need wasm-bindgen "cargo install wasm-bindgen-cli --version $WB_VERSION --locked"
if command -v wasm-bindgen >/dev/null 2>&1; then
  have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}')
  if [[ -n "$WB_VERSION" && "$have" != "$WB_VERSION" ]]; then
    red "  wasm-bindgen is $have but Cargo.lock pins $WB_VERSION"
    echo "      cargo install wasm-bindgen-cli --version $WB_VERSION --locked --force"
    missing=1
  fi
fi
if ! rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  red "  wasm32-unknown-unknown target — MISSING"
  echo "      rustup target add wasm32-unknown-unknown"
  missing=1
else
  grn "  wasm32-unknown-unknown"
fi
(( missing == 0 )) || die "install the tools above, then re-run"

bash scripts/version-lockstep.sh || die "version surfaces disagree"
bash scripts/sdk-guard.sh        || die "sdk guard failed"

step "Test suite"
cargo test --workspace --locked -- --nocapture >/dev/null 2>&1 \
  || die "tests failed — do not release a red tree"
grn "  workspace tests pass"

cargo clippy --workspace --all-targets --locked -- -D warnings >/dev/null 2>&1 \
  || die "clippy failed"
grn "  clippy clean"

# ── 1 · Crates ───────────────────────────────────────────────────────────────
step "Crates (dependency order)"
for c in "${CRATES[@]}"; do
  v=$(crate_version "$c")
  [[ -z "$v" ]] && { ylw "  $c — no manifest, skipping"; continue; }

  if grep -qE '^publish\s*=\s*false' crates/"$c"/Cargo.toml 2>/dev/null; then
    ylw "  $c — publish = false, skipping"
    continue
  fi

  if [[ " $(published_versions "$c") " == *" $v "* ]]; then
    grn "  $c@$v — already published, skipping"
    continue
  fi

  if (( DRY )); then
    ylw "  $c@$v — WOULD PUBLISH"
    continue
  fi

  printf '  %s@%s — publishing…\n' "$c" "$v"
  cargo publish -p "$c" --locked || die "cargo publish -p $c failed"
  wait_for_index "$c" "$v"
done

# ── 2 · npm packages ─────────────────────────────────────────────────────────
step "npm packages"
for pkg in core-wasm input-wasm runtime-wasm shims; do
  d="packages/$pkg"
  [[ -d "$d" ]] || { ylw "  $pkg — missing, skipping"; continue; }

  # The size gates live in these build scripts and are load-bearing: they
  # regenerate the README Size line, and CI fails on a stale one.
  if [[ -f "$d/scripts/build.sh" ]]; then
    printf '  %s — building…\n' "$pkg"
    bash "$d/scripts/build.sh" >/dev/null || die "$pkg build failed (size gate?)"
  fi

  if (( DRY )); then
    ylw "  @tropel/$pkg — WOULD PUBLISH"
  else
    (cd "$d" && npm publish --access public) || die "npm publish $pkg failed"
    grn "  @tropel/$pkg published"
  fi
done

if [[ -n "$(git status --porcelain -- packages/)" ]]; then
  ylw "  NOTE: a build.sh regenerated a README Size line — commit it, CI fails on a stale one:"
  git status --porcelain -- packages/ | sed 's/^/      /'
fi

# ── 3 · Tag ──────────────────────────────────────────────────────────────────
step "Tag and GitHub release"
# Pushing the tag is the LAST thing this script does, and it is the trigger:
# .github/workflows/release.yml fires on `v*`, cross-compiles the binaries for
# five targets, and creates the GitHub release with them attached.
#
# This script deliberately does NOT create the release itself. It used to, with
# `gh release create --notes-file CHANGELOG.md`, which meant a release page
# with no downloadable binary on it — and it ran AFTER every crate had gone to
# crates.io, where versions are permanent. Anything that failed there failed
# with the irreversible half already done.
if (( DRY )); then
  ylw "  WOULD tag v$VERSION and push it, triggering release.yml"
  if [[ ! -f .github/workflows/release.yml ]]; then
    die "release.yml is missing — the tag would produce a release with no binaries"
  fi
  grn "  release.yml present"
else
  git tag -a "v$VERSION" -m "tropel v$VERSION"
  git push origin "v$VERSION"
  grn "  tagged and pushed v$VERSION"
  echo
  echo "  release.yml is now building binaries for 5 targets + wasm."
  echo "  Watch it:   gh run watch \$(gh run list --workflow=release.yml --limit=1 --json databaseId -q '.[0].databaseId')"
  echo "  Then:       gh release view v$VERSION"
fi

step "Done"
if (( DRY )); then
  ylw "This was a CHECK run. Nothing was published."
  echo "Re-run with --publish when the above looks right."
else
  grn "tropel v$VERSION is out."
fi
