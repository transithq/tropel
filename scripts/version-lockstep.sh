#!/usr/bin/env bash
# P6 lockstep versioning (TROPEL_MODULARIZATION_TODO.md): one version stamped
# into the binary, the wasm runtime, and the npm packages by the SAME CI job.
#
# The version surfaces that must agree:
#   - the `tropel` binary crate  (crates/tropel/Cargo.toml)
#   - the wasm runtime crate     (crates/tropel-web/Cargo.toml) — compiled into
#     tropel_web.wasm and read back by @tropel/exec-wasm as `runtimeVersion`
#   - @tropel/exec-wasm           (packages/exec-wasm/package.json)
#   - @tropel/shims               (packages/shims/package.json)
#
# A drift here is the mixed-version deployment the version handshake exists
# to catch: the client compares the agent's version against the wasm's, so
# publishing the wasm at a different version than the packages breaks the
# handshake before a single load runs.
set -euo pipefail
cd "$(dirname "$0")/.."

bin_ver=$(grep -m1 '^version' crates/tropel/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')
web_ver=$(grep -m1 '^version' crates/tropel-web/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')
exec_ver=$(node -p "require('./packages/exec-wasm/package.json').version")
shims_ver=$(node -p "require('./packages/shims/package.json').version")

echo "lockstep versions:"
echo "  binary (tropel)        = $bin_ver"
echo "  wasm runtime (tropel-web) = $web_ver"
echo "  @tropel/exec-wasm      = $exec_ver"
echo "  @tropel/shims          = $shims_ver"

if [[ "$bin_ver" != "$web_ver" || "$web_ver" != "$exec_ver" || "$exec_ver" != "$shims_ver" ]]; then
  echo "FAIL: versions are out of lockstep — bump them together (the wasm must be rebuilt after)" >&2
  exit 1
fi
echo "ok: all four version surfaces agree at $bin_ver"
