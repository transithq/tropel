#!/usr/bin/env bash
# TR-407: two guards that prevent the SDK inversion from rotting again.
# Verified: the inversion is currently held in place by nothing but care.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "=== Guard 1: every in-tree adapter depends only on tropel-sdk ==="
# postman is the documented exception (it needs tropel-collection's parser).
# All other adapters must depend on tropel-sdk only — no tropel-core.
for adapter in tropel-input-har tropel-input-openapi tropel-input-bru tropel-input-insomnia; do
  if cargo tree -p "$adapter" 2>/dev/null | grep -q 'tropel-core'; then
    echo "FAIL: $adapter depends on tropel-core (should depend on tropel-sdk only)" >&2
    exit 1
  fi
  echo "ok: $adapter (no tropel-core)"
done
# postman: must NOT depend on tropel-core either (it may pull tropel-collection).
if cargo tree -p tropel-input-postman 2>/dev/null | grep -q 'tropel-core'; then
  echo "FAIL: tropel-input-postman depends on tropel-core (exception: tropel-collection is allowed)" >&2
  exit 1
fi
echo "ok: tropel-input-postman (no tropel-core; tropel-collection the documented exception)"

echo "=== Guard 2: sample extension builds from outside the workspace ==="
# Package the SDK and compile a minimal extension against it in a temp dir —
# the actual proof of "no full checkout required".
SDK_DIR="crates/tropel-sdk"
if [[ ! -f "$SDK_DIR/Cargo.toml" ]]; then
  echo "skip: tropel-sdk submodule not checked out — cannot test"
  exit 0
fi
TMPDIR=$(mktemp -d)
trap "rm -rf '$TMPDIR'" EXIT
mkdir -p "$TMPDIR/src"
cat > "$TMPDIR/Cargo.toml" <<EOF
[package]
name = "test-extension"
version = "0.0.0"
edition = "2021"
[dependencies]
tropel-sdk = { path = "$(realpath "$SDK_DIR")" }
EOF
cat > "$TMPDIR/src/lib.rs" <<EOF
use tropel_sdk::traits::{Driver, DriverInstance, DriverDeclaredOptions, VuContext};
use tropel_sdk::types::{Request, Method, AuthConfig};
use tropel_sdk::Result;

pub struct MyDriver;
impl Driver for MyDriver {
    fn id(&self) -> &str { "test" }
    fn detect(&self, _bytes: &[u8]) -> bool { false }
    async fn init(
        &self,
        _bytes: &[u8],
        _path: Option<&std::path::Path>,
        _exec: Option<&str>,
    ) -> Result<Box<dyn DriverInstance>> {
        Ok(Box::new(MyInstance))
    }
    async fn declared_options(
        &self,
        _bytes: &[u8],
        _path: Option<&std::path::Path>,
        _env: &std::collections::HashMap<String, String>,
    ) -> Result<Option<DriverDeclaredOptions>> {
        Ok(None)
    }
}

pub struct MyInstance;
impl DriverInstance for MyInstance {
    async fn run_iteration(&mut self, _ctx: &mut VuContext) -> Result<()> {
        Ok(())
    }
}

// Touch a few contract types so the build proves they resolve from the SDK.
#[allow(dead_code)]
fn touch(req: &Request, method: &Method, auth: &AuthConfig) {
    let _ = (req, method, auth);
}
EOF
if cargo build --manifest-path "$TMPDIR/Cargo.toml" 2>&1; then
  echo "ok: sample extension builds from outside the workspace"
else
  echo "FAIL: sample extension failed to build" >&2
  exit 1
fi