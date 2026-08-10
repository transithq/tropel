# Changelog

## [Unreleased]

### Changed
- **License: dual MIT OR Apache-2.0 → Apache-2.0 only.** `LICENSE-MIT` removed; `LICENSE-APACHE` now carries the full canonical text (previously a placeholder stub) and is copied into `crates/tropel-sdk/` so published artifacts include it. `deny.toml` keeps `MIT` on the *dependency* allowlist (third-party crates only).

## [runtime set 0.1.0] - 2026-08-10

First crates.io release of the runtime publish set — seven crates, each with zero internal coupling beyond the chain below, all depending on the published `tropel-sdk 0.2.0` leaf.

### Dependency chain (publication order)

`tropel-variables → tropel-js → tropel-native → tropel-auth → tropel-http → tropel-sandbox → tropel-runtime`

Every crate resolves its tropel-* dependencies from crates.io; nothing is vendored or patched at runtime. `tropel-sdk 0.2.0` is the shared leaf everything builds on (see the SDK note below).

### Added — tropel-variables 0.1.0
- `{{var}}` resolution with scope precedence and a dynamic-variable catalog (`catalog`, `resolver` modules).
- Zero tropel-* dependencies (pure leaf): serde, serde_json, thiserror, tracing, uuid, rand, regex, chrono.

### Added — tropel-js 0.1.0
- rquickjs wrapper: per-VU `AsyncContext`, execution timeouts, memory limits, interrupt handler, and the bootstrap sequence (`clock`, `context`, `error` modules).
- Zero tropel-* dependencies.

### Added — tropel-native 0.1.0
- Native Rust implementations of heavy primitives installed into the JS context at bootstrap: crypto, hashing, encoding, JSON, and assertions (`crypto`, `encoding`, `fn` modules — includes `generate_uuid`, `random_int`, `random_float`).
- Depends on `tropel-sdk` + `tropel-js`, plus the sha2/sha1/sha3/md-5/md4/ripemd/hmac/aes/aes-gcm family.

### Added — tropel-auth 0.1.0
- Request signers operating on a fully built `reqwest::Request`: `BearerAuth`, `BasicAuth`, `ApiKeyAuth`, `OAuth2Auth`, `AwsSigV4Auth`, OAuth1 (RFC 5849 HMAC-SHA1), Hawk, and HTTP Digest (RFC 7616, challenge-response).
- `AuthSigner` trait (Send + Sync). Depends on `tropel-sdk` + reqwest, base64, hmac, sha1, sha2, md-5, hex, chrono, percent-encoding, rand.

### Added — tropel-http 0.1.0
- HTTP Protocol implementation: reqwest client, connection pooling, redirects, per-VU cookie jar (`blocking`, `client`, `config`, `dns`, `rps`, `subtimings` modules).
- Auth signers intentionally live in `tropel-auth` so the executor and wasm slice can depend on them without pulling in the full HTTP stack.
- Depends on `tropel-sdk` + `tropel-auth`, plus reqwest, tower, serde_json, simd-json, tokio, serde_urlencoded.

### Added — tropel-sandbox 0.1.0
- The script sandbox: native host functions + JS glue providing `pm.environment`, `pm.variables`, `pm.test`, `pm.expect`, `pm.response`, `pm.sendRequest`, `pm.iterationData`.
- P4b layout: binding-agnostic state model (`state`) + `bindings` and `config` modules.
- Depends on `tropel-sdk`, `tropel-js`, `tropel-native`, `tropel-variables`, and `tropel-http` (optional; the default `send-request` feature enables it).

### Added — tropel-runtime 0.1.0
- `ScenarioRunner`: one pass through a scenario — resolve, script, sign, send, assert, jump.
- Split from the old `tropel-executor` (P5): the load-shaped half (VU count, rate, duration) lives in `tropel-scheduler`.
- Depends on `tropel-sdk`, `tropel-sandbox` (default-features = false), `tropel-js`, `tropel-variables`; dev-depends on `tropel-http`.

### tropel-sdk 0.2.0 (companion leaf)
- **Breaking (minor bump per pre-1.0 policy):** `Response` gains the pub field `request_body_size` (data-sent decoupling), so the exhaustively-constructible struct is no longer literal-constructible by downstream crates. Confirmed breaking by `cargo-semver-checks`.
- **Additive:** `config.rs` gains `ExpectedStatus` enum + `status_is_expected` helper.
- Published to crates.io as `0.2.0`; the runtime set declares `tropel-sdk = "0.2.0"` and requires it.

## [0.1.0] - 2026-07-29

### Added
- Initial project structure (Rust workspace with 14+ crates)
- Postman Collection v2.1/v2.0 parser (`tropel-collection`)
- `{{var}}` resolution with scope precedence (`tropel-variables`)
- QuickJS engine wrapper (`tropel-js`)
- Native Rust builtins for crypto, hashing, encoding, assertions (`tropel-native`)
- `pm.*` API bridge (`tropel-pm`)
- HTTP protocol executor with auth signers (`tropel-http`)
- VU scheduler with 4 execution modes (`tropel-executor`)
- HDR histogram metrics aggregation (`tropel-metrics`)
- Reporters: stdout, JSON, CSV (`tropel-report`)
- Extension SDK with registration system (`tropel-ext`)
- Engine orchestration facade (`tropel-engine`)
- CLI binary with `tropel run` command
- Vendored JS libraries: pm-api, chai, lodash, CryptoJS shim
- CI pipeline (format, lint, test, build)
- Extension crates: gRPC, WebSocket, Prometheus (placeholders)
- Custom binary builder (`tropel-build`)
