# Native vs wasm differential divergences (TR-408)

This file enumerates the divergences between the native and wasm32 runtime that
are GENUINELY unavoidable — the `native_vs_wasm` differential harness must
either not exercise them, or the divergence must be listed here with a reason
before the CI merge-block is waived.

Current state: **no known unavoidable divergences.** The harness (`crates/
tropel-web/tests/native_vs_wasm.rs`) runs the identical `ScenarioRunner` code
compiled two ways (host vs wasm32-wasip1) with a shared deterministic fixture,
and asserts the normalized outcomes are byte-identical across:

- methods (GET/POST), JSON/query/form bodies
- auth schemes (bearer, basic, api-key, digest) — the wire signing is shared
- error statuses (404, 500), redirects (301), text/empty bodies
- a throwing test script (both legs report the same `script_failures`)

If a future divergence appears, add it here with the reason it is unavoidable
(e.g. a host-only feature like `std::time` precision, or a platform-specific
timestamp) AND mark the corresponding corpus item `#[ignore]` with a link to
this file. A divergence that is NOT listed here is a bug in the one-engine
claim and must be fixed, not waived.
