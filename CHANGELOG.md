# Changelog

## [0.5.5] - 2026-09-10

Three tagged versions shipped without a changelog entry — 0.3.0 and 0.5.4 were
tagged, and the file stopped at 0.2.0. This section therefore covers
everything since 0.2.0 rather than 0.5.4 alone, because a reader upgrading
from the last documented release needs the whole span. `scripts/release-notes.sh`
extracts it, so it is also what the release page shows.

**The API is unstable pre-1.0.** `tropel-sdk` is at 0.4.0 and moves between
minor versions; k6 parity is still expanding.

### Added — the browser and desktop tiers can now execute what they could only configure

The theme of this release. KnockPort (and any embedder) could *create* a
digest, Hawk, SigV4, OAuth1, WSSE or EdgeGrid auth config and every send
**refused**, because the signing lived behind `tropel-auth`'s `reqwest`
feature and `wasm32-unknown-unknown` cannot compile reqwest. The rules moved
out from behind that gate and got a JS facade:

- **Pure auth builders** (`tropel-auth::builders`, ungated): digest
  challenge-response, Hawk, AWS SigV4 — including the service-derivation and
  S3 double-encoding rules — and OAuth1's RFC 5849 §3.4.1.2 base-string URI
  and form parsing. `signers.rs` became a thin adapter that reads fields off a
  `reqwest::Request` and calls in, so there is one implementation rather than
  two.
- **`@tropel/core-wasm` exports them**: `digestSign`, `hawkSign`,
  `awsSigV4Sign`, `oauth1Sign`, `edgegridSign`, plus
  `oauth1SignatureMethods()` so a picker offers exactly what the signer
  accepts instead of keeping its own list. `edgegridSign` takes the whole
  request, because the signature covers it — method, url, the *named*
  headers' values and the body hash — and refuses a missing nonce or
  timestamp rather than fabricating one: this tier has no clock and no
  CSPRNG worth using, and `crypto.getRandomValues` is a better source anyway.
- **`tropel agent`'s `/auth/sign` answers for EdgeGrid and WSSE too.** It had
  the signers (via `build_auth_signer`, which `/execute` already calls) and
  refused them as unknown at the endpoint the desktop tier actually calls —
  one surface signing what the other called unsupported. Its refusal message
  was worse than the gap: a hardcoded "supported: digest, hawk, awsSigV4,
  oauth1" that went stale the moment a scheme was added, so a caller reading
  it would conclude the agent could not sign something it could. Now derived
  from one declaration, with a test asserting every entry appears in the
  refusal.
- **WSSE UsernameToken** and **Akamai EdgeGrid (EG1-HMAC-SHA256)** through the
  signer builder. EdgeGrid uses `yyyyMMddTHH:mm:ss+0000`, *not* RFC 3339 — an
  ISO stamp goes into the signing data verbatim and produces a 401 that
  mentions neither the clock nor the format. Bodies over Akamai's 128 KiB
  default are **not hashed** rather than truncated, because a truncated
  content hash is a signature the server cannot reproduce.
- **OAuth1 PLAINTEXT**, over TLS only. RSA-* still refuses by name rather than
  silently downgrading to HMAC-SHA1.

### Added — declarative assertions in Rust

- `assert_evaluate(response, assertions)` with **28 operators**, their arity,
  and target resolution (`status`, `header.*`, `body.*`, JSON paths). Pinned
  by a test that asserts the set *and its order*, because the editor renders
  from it.
- `@tropel/core-wasm` exports `assertEvaluate` and the operator table. The
  regex matcher is the **host's** `RegExp`, passed in: linking a Rust regex
  would put back the 152 KB TR-434 removed, and it would be unfaithful —
  JavaScript has backreferences and lookaround that Rust's `regex` does not.
- Failure messages name the **target**, not the assertion, so a reader learns
  which field was wrong rather than which rule fired.

### Added — the loopback agent grew a rules surface

`tropel agent` went from an execute-only endpoint to the full seam an embedder
needs: `/resolve` and `/resolve/batch`, `/assert`, `/operators`, `/auth/sign`,
`/auth/oauth2`, `/variables/dynamic` (and its batched twin), `/constants`, and
`/script` — which carries all four variable scopes both ways, seeds the
response a test asserts against, returns what the script did to the request,
and has a **bidirectional host-callback channel** so `bru.runRequest` works.
`--exit-with-parent` means a dead supervisor cannot leak a live agent.

### Added — inputs and protocols

- **Bruno `.bru` text format** parsed directly, keeping disabled entries,
  vars, settings and assertions — a disabled row is deliberate local state,
  and dropping it makes the checkbox meaningless.
- **OpenAPI declarations reach the Scenario** (`ScenarioItem::contract`): the
  declared parameters with their types and `required` flags, the declared
  request body, and the declared **responses** keyed by the spec's own string
  so `4XX` and `default` survive. Plus `path_template`, because building an
  executable request substitutes `/users/{id}` into `/users/example` and the
  substituted segment is indistinguishable from a literal one afterwards.
- **Private CA bundles** on the TLS builder (`root_cert_paths`,
  `keep_system_roots`). Multi-cert bundles read every certificate —
  `Certificate::from_pem` reads only the FIRST, so a chain handed to it whole
  drops the intermediate and then fails verification in a very confusing way.
  A config that would trust no CA at all is refused by name.
- **A proxy surface**: `off` / `fixed` / `system` / `pac`, proxy auth, and
  bypass rules with exact hosts, `*.suffix` (subdomains only), bare `*`, IP
  and CIDR. A malformed bypass entry is an error, not a fuzzy match, because
  both ways of getting it wrong route traffic silently. PAC directive
  **failover** ordering with a bounded TTL is implemented; PAC *evaluation*
  (running `FindProxyForURL`) still needs a JS engine and is not.
- **Per-request proxy and TLS**: `Request::proxy` alongside `Request::certificate`,
  executed by keying one lazily-built client per profile — reqwest bakes both
  into the client at build time. An explicit `Off` is not the same as absent:
  one refuses the client's proxy, the other inherits it.

### Fixed — correctness

- **A public-suffix `Domain` could scope a cookie across sites.** A `Set-Cookie`
  for `Domain=.co.uk` was accepted, so one site's cookie was sent to every
  other.
- **A binary response body did not survive `/execute`.** It went through a
  JSON string; `bodyEncoding` now says whether the body is text or base64, and
  a consumer that ignores it would hand the viewer base64 as a document.
- **`/execute` dropped most of the request.** Client certificates, the `Host`
  override, cookies and the timeout were each hard-coded to their empty value,
  so a caller asking for one was ignored *without being told*. Headers are a
  pair ARRAY now, because a JSON object cannot hold two `Set-Cookie` rows.
- **The CORS header reached only the preflight**, not the actual response, so a
  browser saw the OPTIONS succeed and the real request fail.
- **Header names folded with ASCII case, not Unicode**, unlike JavaScript.
- **`pm.sendRequest` did not send** — `/script` had no HTTP client at all.
- **The loopback agent was unreachable**, and its realm lacked the embedder's
  namespace.
- **`resolveTemplateDetailed` was missing from the core-wasm facade** after a
  refactor, and **the signers were not surfaced** in it (0.4.1).
- Batch resolve reports **why** it stopped — a cycle and an unknown name are
  different failures — rather than one opaque string.

### Changed

- `tropel-sdk` **0.4.0**. Adding a public field to a struct with no
  `#[non_exhaustive]` breaks exhaustive literals, which is a breaking change
  and therefore a minor bump pre-1.0. `Scenario`, `ScenarioInfo` and
  `ScenarioItem` gained `Default`, so `ScenarioItem { name, ..Default::default() }`
  survives the next field addition — the option that was missing from the
  `#[non_exhaustive]`-versus-bump argument.
- All seven version surfaces realigned at **0.5.5**. `@tropel/shims` was
  published at 0.5.5 on its own to get a fixtures export to the registry,
  which left the version-lockstep gate failing — and `wasm` is in `ci-ok`'s
  needs, so that blocked every PR.
- The `tropel-core-wasm` artifact is **447,492 B** after `wasm-opt -Oz`
  (measured, not typed — the README line is generated), 252 KB under the
  700 KB gate.

### Fixed — CI gates that were passing while red, or red while ignored

Worth listing separately, because each one hid something:

- **`semver-checks` had been failing since `ScenarioItem::authoring` landed**,
  correctly: tropel-sdk 0.3.0 is published and two public fields were added to
  it in place.
- **`rustfmt` was failing on master**, so every open PR inherited a red check
  belonging to someone else's diff.
- **The version-lockstep failure was masking the four steps after it** in the
  same job — including the core-wasm build, which had been broken by a module
  inserted directly beneath `signers`' `#[cfg(feature = "reqwest")]`. An
  attribute applies to the item that follows it, so the insertion *moved* it
  and the browser tier stopped compiling.
- **`.gitignore`'s `*.pem` silently swallowed the CA-bundle test fixtures**,
  so those tests passed locally and failed in CI on all three platforms from
  the day they landed.
- **The out-of-workspace SDK guard broke on every field addition**, because its
  sample extension named every field. It uses `..Default::default()` now, so
  it tests the contract rather than the maintainer's diligence.

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
