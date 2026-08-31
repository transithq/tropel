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
# BOTH wasm targets. `wasm32-unknown-unknown` builds the core-wasm/input-wasm
# npm packages; `wasm32-wasip1` builds tropel-web, whose artifact
# @tropel/runtime-wasm copies in. Only the first used to be checked, so a
# missing wasip1 target surfaced four steps later as a runtime-wasm build
# failure instead of here.
INSTALLED_TARGETS=$(rustup target list --installed 2>/dev/null)
for t in wasm32-unknown-unknown wasm32-wasip1; do
  if grep -qx "$t" <<<"$INSTALLED_TARGETS"; then
    grn "  $t"
  else
    red "  $t target — MISSING"
    echo "      rustup target add $t"
    missing=1
  fi
done
(( missing == 0 )) || die "install the tools above, then re-run"

# Disk headroom. A full --locked workspace build plus clippy --all-targets adds
# several GB to target/, and when the disk fills mid-build cargo dies with
# ENOSPC — which this script's next step reports as "tests failed". That has
# already cost one misdiagnosis (a linker failure read as a code defect), so
# check it up front where the message can name the real cause.
AVAIL_KB=$(df -k . | awk 'NR==2 {print $4}')
AVAIL_GB=$(( AVAIL_KB / 1024 / 1024 ))
if (( AVAIL_GB < 5 )); then
  red "  free disk: ${AVAIL_GB} GB"
  echo "      A release build needs headroom. Reclaim it with:"
  echo "        cargo clean            # this repo's target/ (rebuilds afterwards)"
  echo "      Source edits survive cargo clean; only target/ is removed."
  die "not enough free disk (${AVAIL_GB} GB) — see above"
elif (( AVAIL_GB < 15 )); then
  ylw "  free disk: ${AVAIL_GB} GB — tight; a mid-build ENOSPC will look like a test failure"
else
  grn "  free disk: ${AVAIL_GB} GB"
fi

# File descriptors. THIS is what failed the release, twice.
#
# tropel is one-OS-thread-per-VU and the behavior_parity suite stands up real
# local HTTP servers, so `cargo test --workspace` opens a lot of descriptors —
# and libtest runs those tests in parallel. Under a low `ulimit -n` the tokio
# runtimes fail to build with
#
#   Os { code: 24, kind: TooManyOpenFiles, message: "Too many open files" }
#
# and every downstream assertion fails for a reason that looks like a product
# bug: "connection refused -> http_req_failed == 0", "all 12 batch requests
# must be served: 0", "all checks passed, got 11285 failed of 11285". None of
# those are real. macOS ships a 256-descriptor default in some shells, while
# others inherit a much larger value — which is exactly why this reproduced
# for the operator and not for a shell with a high limit.
#
# The soft limit is raised here rather than merely reported: it costs nothing,
# needs no privileges while below the hard limit, and applies to the cargo
# child processes that need it.
FD_WANT=16384
FD_HAVE=$(ulimit -n)
if [[ "$FD_HAVE" != "unlimited" ]] && (( FD_HAVE < FD_WANT )); then
  # macOS clamps a process to kern.maxfilesperproc; don't ask for more.
  FD_CAP=$(sysctl -n kern.maxfilesperproc 2>/dev/null || echo "$FD_WANT")
  FD_TARGET=$(( FD_WANT < FD_CAP ? FD_WANT : FD_CAP ))
  # `-S`, not bare `-n`: bare `ulimit -n N` sets the soft AND hard limits, so
  # it permanently lowers the ceiling if N is below the current hard limit —
  # after which raising back is EPERM for an unprivileged process. Only the
  # soft limit needs to move, and raising it is always allowed up to the hard
  # limit.
  if ulimit -S -n "$FD_TARGET" 2>/dev/null; then
    grn "  file descriptors: raised $FD_HAVE -> $(ulimit -n)"
  else
    red "  file descriptors: $FD_HAVE (could not raise; hard limit $(ulimit -Hn))"
    echo "      The test suite needs ~$FD_WANT. Raise it and re-run:"
    echo "        ulimit -S -n $FD_TARGET && bash scripts/release.sh $MODE"
    die "ulimit -n too low ($FD_HAVE) — behavior_parity will fail with EMFILE"
  fi
else
  grn "  file descriptors: $FD_HAVE"
fi

bash scripts/version-lockstep.sh || die "version surfaces disagree"
bash scripts/sdk-guard.sh        || die "sdk guard failed"

# Output goes to a log, not /dev/null. `2>&1 >/dev/null` on a release gate is
# actively hostile: it turns "one test failed" into "tests failed" with no name,
# no assertion and no panic, so the operator cannot tell a real regression from
# a flake without re-running the whole suite by hand.
step "Test suite"
LOG_DIR="${TMPDIR:-/tmp}/tropel-release-$$"
mkdir -p "$LOG_DIR"
if ! cargo test --workspace --locked > "$LOG_DIR/test.log" 2>&1; then
  red "  tests failed — do not release a red tree"
  echo
  grep -E '^test .*FAILED|^failures:|panicked at|^test result: FAILED' "$LOG_DIR/test.log" \
    | head -30 | sed 's/^/      /'
  echo
  echo "      full log: $LOG_DIR/test.log"
  die "tests failed"
fi
grn "  workspace tests pass"

if ! cargo clippy --workspace --all-targets --locked -- -D warnings \
     > "$LOG_DIR/clippy.log" 2>&1; then
  red "  clippy failed"
  grep -E '^error' "$LOG_DIR/clippy.log" | head -20 | sed 's/^/      /'
  echo "      full log: $LOG_DIR/clippy.log"
  die "clippy failed"
fi
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
# @tropel/runtime-wasm does not COMPILE tropel-web — it copies the prebuilt
# `target/wasm32-wasip1/release-wasm/tropel_web.wasm` into its package. That
# artifact is produced by scripts/wasm-size.sh (which also asserts the 2.9 MB
# budget), and CI happens to run it earlier in the same job. This script never
# did, so the npm loop reached runtime-wasm and died with
#
#   error: tropel_web.wasm not found — run the wasm job / scripts/wasm-size.sh
#          first, or set TROPEL_WASM_PATH
#   FAIL: runtime-wasm build failed (size gate?)
#
# after @tropel/core-wasm and @tropel/input-wasm had already gone public. Build
# it up front so the whole npm step either can proceed or fails before any
# publish, rather than half-way through.
# macOS rustup layout: `rust-lld` (the wasm linker) has an @rpath pointing at
# <toolchain>/lib/rustlib/<host>/lib/libLLVM.dylib, but some installs ship the
# dylib at <toolchain>/lib/libLLVM.dylib instead. Linking any wasm target then
# dies with
#
#   dyld: Library not loaded: @rpath/libLLVM.dylib
#   error: could not compile `tropel-web` (lib)
#
# The file is present, just not where the rpath looks. Point DYLD at the real
# location rather than modifying the toolchain — no filesystem surgery, and it
# is inert on installs that are already correct.
if [[ "$(uname -s)" == "Darwin" ]] && command -v rustc >/dev/null 2>&1; then
  RUST_SYSROOT=$(rustc --print sysroot 2>/dev/null || true)
  if [[ -n "$RUST_SYSROOT" && -f "$RUST_SYSROOT/lib/libLLVM.dylib" ]]; then
    export DYLD_FALLBACK_LIBRARY_PATH="$RUST_SYSROOT/lib:${DYLD_FALLBACK_LIBRARY_PATH:-}"
  fi
fi

step "tropel-web wasm artifact"
WEB_WASM="target/wasm32-wasip1/release-wasm/tropel_web.wasm"
if [[ -f "$WEB_WASM" ]]; then
  grn "  already built: $WEB_WASM"
else
  printf '  building via scripts/wasm-size.sh…\n'
  if ! bash scripts/wasm-size.sh > "$LOG_DIR/wasm-size.log" 2>&1; then
    red "  wasm-size.sh failed"
    tail -20 "$LOG_DIR/wasm-size.log" | sed 's/^/      /'
    echo "      full log: $LOG_DIR/wasm-size.log"
    die "could not build $WEB_WASM"
  fi
  grep -E '^(PASS|FAIL)' "$LOG_DIR/wasm-size.log" | sed 's/^/  /'
  [[ -f "$WEB_WASM" ]] || die "wasm-size.sh reported success but $WEB_WASM is absent"
  grn "  built: $WEB_WASM"
fi

# The package build scripts need the workspace's devDependencies (typescript
# for @tropel/runtime-wasm). CI does `npm install` at the root before building
# these; this script did not, so runtime-wasm's `npx tsc` fell through to the
# registry. There is no root package-lock.json, so `npm install` — matching CI
# exactly — not `npm ci`.
step "npm workspace deps"
if [[ -x node_modules/.bin/tsc ]]; then
  grn "  already installed"
else
  printf '  npm install (workspace root)…\n'
  npm install --no-audit --no-fund > "$LOG_DIR/npm-install.log" 2>&1 \
    || { tail -20 "$LOG_DIR/npm-install.log" | sed 's/^/      /'; die "npm install failed"; }
  [[ -x node_modules/.bin/tsc ]] || die "npm install completed but node_modules/.bin/tsc is absent"
  grn "  installed"
fi

step "npm packages"
for pkg in core-wasm input-wasm shims runtime-wasm; do
  d="packages/$pkg"
  [[ -d "$d" ]] || { ylw "  $pkg — missing, skipping"; continue; }

  # Name and version come from the manifest, never assumed from the directory
  # name — a mismatch would make the skip check silently test the wrong package
  # and republish.
  PKG_NAME=$(node -p "require('./$d/package.json').name" 2>/dev/null || echo "")
  PKG_VER=$(node -p "require('./$d/package.json').version" 2>/dev/null || echo "")
  [[ -n "$PKG_NAME" && -n "$PKG_VER" ]] \
    || die "$pkg: could not read name/version from $d/package.json"

  # IDEMPOTENCE, matching the crates step. An npm version is immutable, so
  # republishing one is a hard 403:
  #
  #   npm error 403 You cannot publish over the previously published versions
  #
  # The first release run got @tropel/core-wasm and @tropel/input-wasm out
  # before failing on runtime-wasm, so every subsequent run hit that 403 and
  # could never reach the two packages that still needed publishing. Skip what
  # is already live and the step resumes instead of restarting.
  #
  # Checked BEFORE the build: an already-published package needs no rebuild,
  # which is what makes a resume fast. Its size gate cannot matter
  # retroactively — that artifact is already on the registry.
  #
  # `npm view <name>@<version>` exits non-zero for both "no such version" and
  # "no such package", which is the answer we want in either case. A network
  # failure also reads as "not published" and falls through to the publish
  # attempt, where the 403 is caught below rather than silently passing.
  if npm view "$PKG_NAME@$PKG_VER" version >/dev/null 2>&1; then
    # Report WHEN it was published. Skipping is correct — the version is
    # immutable — but "already published" silently means "the registry keeps
    # whatever content was uploaded then, not what is in this tree". If that
    # date is old, the published artifact predates the fixes being released
    # and the version must be BUMPED for them to reach consumers. Republishing
    # is not an option, so the date is the only signal that anything is wrong.
    PKG_WHEN=$(npm view "$PKG_NAME" time --json 2>/dev/null \
      | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{console.log((JSON.parse(s)['$PKG_VER']||'?').slice(0,10))}catch{console.log('?')}})" 2>/dev/null)
    grn "  $PKG_NAME@$PKG_VER — already published ${PKG_WHEN:-?}, skipping"
    continue
  fi

  # ORDER IS LOAD-BEARING: shims BEFORE runtime-wasm. runtime-wasm imports
  # `@tropel/shims`, whose `types` points at the GENERATED dist/bundle.d.ts.
  # With shims last, tsc fails with "Cannot find module '@tropel/shims'".
  #
  # The size gates live in these build scripts and are load-bearing: they
  # regenerate the README Size line, and CI fails on a stale one.
  if [[ -f "$d/scripts/build.sh" ]]; then
    printf '  %s — building…\n' "$pkg"
    bash "$d/scripts/build.sh" >/dev/null || die "$pkg build failed (size gate?)"
  fi

  if (( DRY )); then
    ylw "  $PKG_NAME@$PKG_VER — WOULD PUBLISH"
  else
    if (cd "$d" && npm publish --access public); then
      grn "  $PKG_NAME@$PKG_VER published"
    else
      # Lost a race, or the skip check was wrong. Re-check before dying: a 403
      # because the version is already live is not a release failure.
      if npm view "$PKG_NAME@$PKG_VER" version >/dev/null 2>&1; then
        ylw "  $PKG_NAME@$PKG_VER — already on the registry, treating as done"
      else
        die "npm publish $pkg failed"
      fi
    fi
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
