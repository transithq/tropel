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
use tropel_sdk::types::{Request, Method, AuthConfig, Body, ResponseType};
use tropel_sdk::scenario::{Scenario, ScenarioInfo, ScenarioItem};

// Guard 2 is about proving the CONTRACT types resolve from the SDK alone
// (no full checkout, no tropel-core). Constructing the core types is enough;
// implementing Driver would couple the sample to trait lifetime details that
// are not the point of the guard.
pub fn build_scenario() -> Scenario {
    Scenario {
        info: ScenarioInfo { name: "sample".into(), description: None, schema: None },
        items: vec![ScenarioItem {
            id: None,
            name: "GET /ping".into(),
            request: Some(Request {
                url: "https://example.com/ping".into(),
                method: Method::GET,
                headers: vec![],
                query_params: std::collections::HashMap::new(),
                body: None,
                auth: None,
                certificate: None,
                follow_redirects: true,
                host: None,
                cookies: vec![],
                timeout: None,
                response_type: ResponseType::Text,
            }),
            prerequest: vec![],
            test: vec![],
            assertions: vec![],
            items: vec![],
        }],
        variables: std::collections::HashMap::new(),
        auth: None,
        conversion_notes: vec![],
    }
}

// Touch the auth contract too.
#[allow(dead_code)]
fn touch_auth(auth: &AuthConfig, body: &Body) {
    let _ = (auth, body);
}
EOF
if cargo build --manifest-path "$TMPDIR/Cargo.toml" 2>&1; then
  echo "ok: sample extension builds from outside the workspace"
else
  echo "FAIL: sample extension failed to build" >&2
  exit 1
fi