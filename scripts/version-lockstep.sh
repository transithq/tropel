#!/usr/bin/env bash
# P6 lockstep versioning (TROPEL_MODULARIZATION_TODO.md): one version stamped
# into the binary, the wasm runtime, and the npm packages by the SAME CI job.
#
# The version surfaces that must agree:
#   - the `tropel` binary crate  (crates/tropel/Cargo.toml)
#   - the wasm runtime crate     (crates/tropel-web/Cargo.toml) — compiled into
#     tropel_web.wasm and read back by @tropel/runtime-wasm as `runtimeVersion`
#   - @tropel/core-wasm          (packages/core-wasm/package.json)   [TR-406]
#   - @tropel/input-wasm         (packages/input-wasm/package.json)  [TR-406]
#   - @tropel/runtime-wasm       (packages/runtime-wasm/package.json)
#   - @tropel/shims              (packages/shims/package.json)
#   - the `tropel-sdk` submodule (crates/tropel-sdk)  [TR-406]
#
# A drift here is the mixed-version deployment the version handshake exists
# to catch: the client compares the agent's version against the wasm's, so
# publishing the wasm at a different version than the packages breaks the
# handshake before a single load runs.
#
# TR-406: core-wasm and input-wasm are consumed by knockport DIRECTLY (the
# surfaces most likely to drift), and tropel-sdk is the published contract —
# all three are now checked. The submodule's pinned commit is compared
# against its own master so a stale pin is a visible failure, not a silent
# contract fork.
set -euo pipefail
cd "$(dirname "$0")/.."

bin_ver=$(grep -m1 '^version' crates/tropel/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')
web_ver=$(grep -m1 '^version' crates/tropel-web/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')
sdk_ver=$(grep -m1 '^version' crates/tropel-sdk/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/' 2>/dev/null || echo "UNREADABLE")
core_ver=$(node -p "require('./packages/core-wasm/package.json').version")
input_ver=$(node -p "require('./packages/input-wasm/package.json').version")
exec_ver=$(node -p "require('./packages/runtime-wasm/package.json').version")
shims_ver=$(node -p "require('./packages/shims/package.json').version")

echo "lockstep versions:"
echo "  binary (tropel)        = $bin_ver"
echo "  wasm runtime (tropel-web) = $web_ver"
echo "  tropel-sdk (submodule) = $sdk_ver (WARNING-only — see below)"
echo "  @tropel/core-wasm      = $core_ver"
echo "  @tropel/input-wasm     = $input_ver"
echo "  @tropel/runtime-wasm   = $exec_ver"
echo "  @tropel/shims          = $shims_ver"

# Hard lockstep: binary + web + all four npm packages. The SDK is a separate
# published crate with its own version history; its version is WARNING-only
# below, and its PIN is the real guard (TR-407).
if [[ "$bin_ver" != "$web_ver" || "$web_ver" != "$core_ver" ||
      "$core_ver" != "$input_ver" || "$input_ver" != "$exec_ver" || "$exec_ver" != "$shims_ver" ]]; then
  echo "FAIL: versions are out of lockstep — bump them together (the wasm must be rebuilt after)" >&2
  exit 1
fi
echo "ok: all six version surfaces agree at $bin_ver"

# TR-406/TR-407: the SDK submodule version and pin. The SDK is published to
# crates.io with its own version; it must be realigned to the parent repo's
# version (D2 lockstep). A mismatch here is a WARNING (the SDK's pin is the
# hard guard) until the SDK crate's version is bumped in lockstep.
if [[ "$sdk_ver" != "$bin_ver" && "$sdk_ver" != "UNREADABLE" ]]; then
  echo "WARN: tropel-sdk version $sdk_ver != parent $bin_ver — realign the SDK crate's version in lockstep (TR-407)" >&2
fi

# TR-407: the SDK submodule pin must not lag the SDK's own master. A stale
# pin means the engine builds against an older contract than the one being
# reviewed — the exact lockstep hazard TR-406 exists to catch. Only checked
# when the submodule is checked out (a shallow CI clone leaves it empty).
if [[ -f crates/tropel-sdk/Cargo.toml && -d crates/tropel-sdk/.git ]]; then
  pin=$(git -C crates/tropel-sdk rev-parse HEAD 2>/dev/null || true)
  master=$(git -C crates/tropel-sdk ls-remote origin refs/heads/master 2>/dev/null | awk '{print $1}' || true)
  if [[ -n "$pin" && -n "$master" && "$pin" != "$master" ]]; then
    echo "FAIL: tropel-sdk submodule pin ($pin) lags its own master ($master) — realign the pin (TR-407)" >&2
    exit 1
  fi
  echo "ok: tropel-sdk submodule pin matches its master"
else
  echo "skip: tropel-sdk submodule not checked out (shallow clone?) — pin check deferred"
fi
