# Changelog

## [0.2.0] - 2026-08-31

First tagged release. Versioned 0.2.0 rather than 0.1.0 across all seven
surfaces (binary, `tropel-engine`, `tropel-web`, and the four `@tropel/*` npm
packages — `tropel-engine` is what `--version` prints) because
`@tropel/shims@0.1.0` and `@tropel/runtime-wasm@0.1.0` were published on
2026-08-10 and npm versions are immutable: those artifacts still call the
`__tropel_pm_*` bridges this tree renamed to `__tropel_trp_*`, so they cannot
be paired with this runtime. 0.2.0 is the first npm set that matches the
binary.

**The API is unstable pre-1.0.** k6 parity is actively expanding and the
`tropel-sdk` surface still moves between minor versions.

### Fixed — correctness

- **`sleep()` was non-functional on every declarative format** (Postman, HAR,
  OpenAPI, `.http`, Insomnia, Bruno). `__tropel_native_sleep` was registered as
  an async host function on an rquickjs `Runtime` with no spawner, so the first
  call panicked with *"tried to use async function in non async runtime"*;
  rquickjs stashed that panic and re-raised it on whichever VU next threw — a
  different VU on a shared runtime. Now a blocking sleep with an absolute
  deadline. This also fixes `bru.sleep(ms)`.
- **Headline `http_req_duration` / `iteration_duration` were computed from one
  metrics shard.** `shard_for_key` hashed the metric name *and* its tags,
  against its own documented contract, so `{url:/a}` and `{url:/b}` landed on
  different shards and the merge kept only the largest-count partial. Count,
  avg, min, max and p95 were derived from a fraction of the population.
- **`http_req_failed` was the maximum of per-shard rates** — 10 failures across
  400 requests reported `0.10` instead of `0.025`. Now merges numerator and
  denominator.
- **`output_samples_dropped` / `aggregator_samples_dropped` were multiplied by
  `SHARD_COUNT`** — process-global atomics read once per shard and summed, so
  one output dropping 1,000 samples reported 4,000.
- **`absorb_snapshot` discarded over-cap series without counting them**, so a
  controller merge that lost series still reported `"unverified": false`.
- **The QuickJS promise-rejection tracker was overwritten by every new VU.**
  It is a property of the runtime, not the context, so on a shared runtime one
  VU's unhandled rejection was recorded into another's map — the first VU
  passed silently, the second failed with an error it never raised.
- **`bru.setVar` / `setEnvVar` / `setCollectionVar` were not inverses of their
  getters** — they wrote with `String(value)` while the getters used
  `JSON.parse`, so `setVar('id','1234')` read back as the number `1234` and an
  object read back as `"[object Object]"`.
- **`@tropel/shims` shipped without `k6-core.js`**, leaving every
  `@tropel/runtime-wasm` embedder without `check`/`group`/`Counter`/`Gauge`/
  `Rate`/`Trend`. The npm bundle list is now derived from `Shim::ALL` in the
  Rust source, so the two cannot diverge silently.
- **`ws_*` samples carried `group:"ws"`** where k6's root group is `""`.

### Changed
- **Per-VU JS shims are now selected by input format, and the bytecode cache is keyed by bundle (TR-501).** The compiled-shim-bytecode cache was a single `OnceLock` keyed on nothing, so only the full default bundle could use it and every narrowed bundle paid a per-VU source parse+compile — which made shim gating a *pessimisation*: an http-only script measured **557,824 B/VU** against the full bundle's **497,584 B/VU**. The cache is now keyed by bundle identity, and `ShimBundle::for_format` picks the shim set from the resolving `InputAdapter::id()`. ✅MEAS (Apple M2, release, N=25, contexts sharing one worker-thread `Runtime`): har/openapi/http/insomnia **203,400 B/VU**, content-gated http-only **261,878**, postman **369,424**, k6/unknown format **385,324**. (An earlier revision of this entry quoted 280,480 / 336,848 / 479,952 / 497,584 — those came from a harness that summed a *shared* runtime's heap once per context and divided by N, reporting the whole 25-context total as a per-VU figure. Withdrawn; do not cite them.) Behaviour change: a context created by `create_vu_js_context` no longer necessarily has all of `pm`/`chai`/`_`/`CryptoJS`/`bru` — `bru` is dropped for every format except `bru`, and the assertion/utility libraries are dropped for the four formats whose adapters cannot emit a script. `pm.js` is still loaded for every format. The k6 *Driver*'s own shim bundle (`tropel-input-k6`) is unchanged.
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
