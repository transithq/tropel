#!/usr/bin/env bash
# TR-407: two guards that prevent the SDK inversion from rotting again.
# Verified: the inversion is currently held in place by nothing but care.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "=== Guard 1: every in-tree adapter depends only on tropel-sdk ==="
# postman is the documented exception (it needs tropel-collection's parser).
# All other adapters must depend on tropel-sdk only — no tropel-core.
# Fail CLOSED when `cargo tree` itself fails. The old form —
# `cargo tree -p X 2>/dev/null | grep -q` — discarded stderr, so a failed
# invocation (crate renamed, broken workspace) produced empty output, grep
# found nothing, and the guard said "ok" about a dependency graph it never
# saw. "Could not inspect" and "no forbidden dependency" must not look alike.
for adapter in tropel-input-har tropel-input-openapi tropel-input-bru tropel-input-insomnia; do
  if ! tree_out=$(cargo tree -p "$adapter" 2>&1); then
    echo "FAIL: cargo tree -p $adapter failed — the guard cannot verify its dependency graph, so it fails closed:" >&2
    echo "$tree_out" | sed 's/^/    /' >&2
    exit 1
  fi
  if grep -q 'tropel-core' <<<"$tree_out"; then
    echo "FAIL: $adapter depends on tropel-core (should depend on tropel-sdk only)" >&2
    exit 1
  fi
  echo "ok: $adapter (no tropel-core)"
done
# postman: must NOT depend on tropel-core either (it may pull tropel-collection).
if ! tree_out=$(cargo tree -p tropel-input-postman 2>&1); then
  echo "FAIL: cargo tree -p tropel-input-postman failed — fails closed:" >&2
  echo "$tree_out" | sed 's/^/    /' >&2
  exit 1
fi
if grep -q 'tropel-core' <<<"$tree_out"; then
  echo "FAIL: tropel-input-postman depends on tropel-core (exception: tropel-collection is allowed)" >&2
  exit 1
fi
echo "ok: tropel-input-postman (no tropel-core; tropel-collection the documented exception)"

echo "=== Guard 2: sample extension builds from outside the workspace ==="
# Package the SDK and compile a minimal extension against it in a temp dir —
# the actual proof of "no full checkout required".
SDK_DIR="crates/tropel-sdk"
if [[ ! -f "$SDK_DIR/Cargo.toml" ]]; then
  # Fail closed. "Submodule absent" and "guard passed" were indistinguishable,
  # and every cargo job in ci.yml checks out with `submodules: recursive`, so
  # absence means something is wrong. Same shape as the version-lockstep pin
  # check, which skipped silently for its entire life (TR-407).
  echo "FAIL: tropel-sdk submodule not checked out — the guard cannot run, so it fails closed. Run 'git submodule update --init' (TR-407)." >&2
  exit 1
fi

# Build against the PACKAGED crate, not the working tree.
#
# TR-407's criterion is explicit: "CI runs `cargo package -p tropel-sdk`, then
# compiles an example extension in a temp dir against that packaged crate
# only. This is the actual proof of 'no full checkout required'."
#
# A `path` dependency on `crates/tropel-sdk` reads the working tree and proves
# something weaker — it cannot catch a file missing from the published
# artifact, which is the failure a stranger running `cargo add tropel-sdk`
# actually hits. `cargo package` applies include/exclude and .gitignore, so
# building against its output is the real test.
echo "packaging tropel-sdk…"
cargo package -p tropel-sdk --allow-dirty --no-verify >/dev/null
SDK_VERSION=$(grep -m1 '^version' "$SDK_DIR/Cargo.toml" | sed -E 's/version *= *"([^"]+)"/\1/')
CRATE_FILE="target/package/tropel-sdk-${SDK_VERSION}.crate"
if [[ ! -f "$CRATE_FILE" ]]; then
  echo "FAIL: cargo package produced no $CRATE_FILE — cannot verify the published artifact" >&2
  exit 1
fi
# Unpack the tarball rather than reusing cargo's verify-extract directory:
# this is byte-for-byte what crates.io would serve, so a file dropped by
# include/exclude is missing here exactly as it would be for a stranger.
UNPACK=$(mktemp -d)
trap "rm -rf '$UNPACK'" EXIT
tar -xzf "$CRATE_FILE" -C "$UNPACK"
PACKAGED="$UNPACK/tropel-sdk-${SDK_VERSION}"
if [[ ! -f "$PACKAGED/Cargo.toml" ]]; then
  echo "FAIL: $CRATE_FILE does not contain tropel-sdk-${SDK_VERSION}/Cargo.toml" >&2
  exit 1
fi
echo "ok: packaged tropel-sdk $SDK_VERSION ($(tar -tzf "$CRATE_FILE" | wc -l | tr -d ' ') files)"

TMPDIR=$(mktemp -d)
trap "rm -rf '$TMPDIR' '$UNPACK'" EXIT
mkdir -p "$TMPDIR/src"
cat > "$TMPDIR/Cargo.toml" <<EOF
[package]
name = "test-extension"
version = "0.0.0"
edition = "2021"
[dependencies]
tropel-sdk = { path = "$(realpath "$PACKAGED")" }
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
  echo "ok: sample extension builds from outside the workspace, against the PACKAGED crate"
else
  echo "FAIL: sample extension failed to build" >&2
  exit 1
fi