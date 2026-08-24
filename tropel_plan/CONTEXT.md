# CONTEXT — read before touching any code

## What tropel is

A Rust load-testing runtime with two consumers that pull in different directions:

- **A k6 alternative** — a CLI that runs k6 scripts and Postman collections at scale, and must produce the *same numbers k6 does* or every ported threshold is silently wrong.
- **The engine under knockport** — an API client that compiles the same runtime to wasm for the browser, links it natively on desktop, and stakes its pitch on *one engine from Send to 10 000 VU*.

Those two consumers share one non-negotiable: **a green run must never be wrong.** A load tester that lies is worse than no load tester, and an API client whose engine disagrees with itself has no product.

## The three layers, and why they need different treatment

This is the most useful framing in the corpus. Every task in this folder belongs to exactly one layer.

### Layer 1 — a green run can still be wrong

The layer that kills adoption. The failures are asymmetric and fire on the most common scripts in existence: `pm.test` renames a check on failure so a CI gate reads a series that is **100 % pass by construction**; `pm.response` is never reset after a transport error, so tests assert against the **previous** request's 200 — worst exactly at saturation; `.value`/`.last` has three implementations that disagree, one passing vacuously.

Several of these are now closed. The category is not.

### Layer 2 — the tool caps out an order of magnitude below target, silently

OTLP is O(n²) *and* ships JSON-not-protobuf with no gzip — 140–750 ms of CPU per 100 ms window, **1.4–7.5× oversubscribed permanently**, capping everything at ~10–30 k samples/s ≈ 1–2.5 k req/s. gRPC hashes the proto *source* (up to 1 MiB) inside a process-global mutex per request, capping at ~1–2 k RPC/s. And there is a live path from a slow output back into the VU hot path: outputs share a **2-thread** runtime with the aggregator, flushes don't yield, the aggregator stops draining, its bounded channel fills in ~1 s, and `record_batch().await` blocks every VU.

> **A user can lose 90 % of their data and see a clean run.** That is why the drop counter comes before every other throughput fix.

### Layer 3 — one design decision caps concurrency, and no amount of Layer 2 work moves it

k6 host functions must be synchronous — they run inside QuickJS `ctx.with` and cannot await — so `execute_blocking` parks the calling VU's OS thread. One VU = one OS thread **plus a full tokio runtime that is structurally idle**. Capped at `MAX_WORKERS = 4096`.

> **In-flight concurrency ≤ 4 096. Throughput ≤ 4096 / mean latency — ~41 k req/s at 100 ms.**
> k6's goroutines cost ~8 KB and never block a thread; 20–50 k VUs per box is routine, putting k6 at **200–500 k req/s**.

Above 4 096 it degrades rather than caps: `Slot::Wrapped` co-locates 2–3 VUs per thread and `sleep()` is a real blocking sleep, so co-located VUs freeze each other. **A run reporting "10 000 VUs" delivers roughly the throughput of 4 096**, and the summary counts spawned VUs, not effective ones. Memory hits first anyway — ✅**MEAS** 835,776 B of QuickJS heap per VU before a line of user script runs, **7.97 GB at 10 000 VUs**.

## Where tropel is genuinely ahead — protect these

A fix that trades one of these away is a regression even if it closes its ticket.

- **HDR histograms.** k6's `TrendSink` retains every raw observation forever and sorts under a global lock every 2 s — 10 k RPS for one hour ≈ **2.0 GB** in k6's engine alone. Tropel is O(1) per series.
- **A real cardinality cap.** `MAX_SERIES = 100_000` with drops counted. k6 has none — it warns, **doubles the threshold**, and its own comment says the process is expected to OOM first.
- **Shared connection pool** — one client per scenario vs k6's per-VU `Transport` + `tls.Config` + `cookiejar` + `Dialer`. It also means **TLS session resumption is shared across VUs**.
- **Sub-timings at ~1 alloc/request** vs k6's ~14.
- **`SharedArray`** — native copy with no per-element `JSON.parse`; k6's re-parses *and* recursively freezes on every element access.
- **`discardResponseBodies`** implemented correctly — it *drains* the body so the pooled connection survives.
- **Executors and outputs are a k6 v2 superset** — `externally-controlled` and StatsD were both deleted in k6 v2 and tropel kept them.

## The settled decisions

### D1 — Publish what strangers consume · git-dep what you consume · process-boundary everything else

Three artifacts, three audiences:

| Artifact | Audience | Channel |
|---|---|---|
| `tropel-sdk` | third-party extension authors — strangers | **crates.io** |
| the wasm npm packages | knockport's web app and extension | **npm** |
| the `tropel` binary, incl. `tropel agent` | knockport's native transport | **GitHub Releases** — a process boundary, not a dependency |

The consequence worth protecting: **knockport carries zero Rust.** It depends on npm packages and speaks to a spawned `tropel agent` over a socket.

`tropel-exec` does **not** go to crates.io. Publishing it would drag every transitive workspace member along, because published crates cannot have path dependencies. Git deps resolve sibling path deps in-checkout; use a tag-pinned git dep if a Rust consumer ever appears.

### D2 — One version number for the whole repo, plus a runtime handshake

> The one-engine claim dies quietly if the client loads wasm `0.4.1` while the connected agent ships `0.5.0`. Semantics have forked, silently — the exact failure the architecture exists to prevent.

Binary, SDK and npm packages are stamped by the same CI job and never released independently. On connect, the client compares the agent's version against the loaded wasm's; a mismatch is a visible warning and load results are marked **unverified-parity**.

### D3 — `tropel-sdk` is the contract at the *bottom* of the graph, not a shim on top

Today in-tree extensions import `tropel-ext`/`tropel-core` directly and bypass the SDK entirely — it was found **unused**. Publishing it as it stands ships the rot. Invert it first: the SDK holds `Driver`, `InputAdapter`, the registration macro, `Scenario`/`Request`/`Response`/`Sample`/`AuthConfig`, and **no tokio, no reqwest, no `std::fs`** so tier-1 native and tier-2 wasm guests use the same crate. `tropel-core` keeps `JobConfig`/`ExecutionConfig`/`HttpConfig`/`TlsConfig` and depends on the SDK.

### D4 — k6 parity is conformance on semantics, coverage on surface

Match the **algorithms and the metric contract** exactly; match the module surface by demand. `http_req_duration` is `sending + waiting + receiving` — it deliberately excludes `blocked`, `connecting` and `tls_handshaking`. Get that wrong and every duration threshold ported from a k6 script is wrong by one connection setup. That single line outranks any number of missing modules.

Where tropel is *better* than k6, keep it and document it — deterministic multipart ordering, escaped field names, StatsD, `externally-controlled`. Parity means "a k6 script gets k6's answer", not "copy k6's bugs".

## Invariants — breaking one fails review

1. **A green run is never wrong.** Any path where a broken thing reports success is a P0, no matter how narrow.
2. **No silent drop.** Every dropped sample, iteration, or request is counted and surfaced in the summary — not at `trace!`.
3. **Never declare a capability that isn't forwarded.** A declared-but-ignored option is worse than a missing one; it defeats the user's own checking.
4. **One implementation per behaviour.** Duplicate implementations are the most expensive recurring bug class in this tree — two HTTP bridges, two `Method` parsers, two shim lists, three deep-equal copies, four `bootstrap.rs` divergences.
5. **Comments describe shipped behaviour, not the intended fix.** Seven comments currently describe a fix that never landed; a reviewer trusting them signs off on live bugs.
6. **Every fix ships a test that fails before and passes after, asserting the user-visible number** — not the internal call.
7. **A test that pins a bug green is deleted or inverted in the same PR.** Four are known. This is the single biggest reason defects survived five review rounds.
8. **Version lockstep.** Binary, SDK and npm packages ship one version, stamped by one CI job.

## Repo shape

> **Corrected against `master` @ `2099cbe`.** The reviews describe a flat `crates/tropel-*` layout that no longer exists. See `VERIFICATION.md` §1 — **a register line number is evidence of what the defect was, not a location.** Re-locate before editing.

Verified layout:

```
crates/
  tropel-sdk         ← A GIT SUBMODULE, pinned at 5433412. A shallow clone leaves it
                       EMPTY and every grep silently returns nothing.
                       git submodule update --init --depth 1
  inputs/            tropel-input-{postman,k6,har,openapi,subprocess,http,bru,insomnia}
  extensions/        tropel-x-{grpc,websocket,prometheus}
  tropel-engine      builtins · cli · vu_loop · summary · outputs
  tropel-runtime     the declarative runner        tropel-scheduler   executors
  tropel-metrics     collector · aggregation       tropel-sandbox     the pm/JS bridge
  tropel-http        client · subtimings · rps     tropel-auth        signers · oauth
  tropel-js  tropel-es  tropel-native  tropel-collection  tropel-variables
  tropel-core  tropel-ext  tropel-wasm  tropel-web  tropel-build  tropel-report
  tropel-core-wasm   tropel-input-wasm   tropel-distributed   tropel-bench
packages/
  core-wasm   input-wasm   runtime-wasm (was exec-wasm)   shims
js/
  k6-shim/   shared/   (the shim bundle)
```

`tropel-pm` and `tropel-exec` are gone. The historical shape the reviews name:

```
crates/
  tropel-sdk         the published contract (D3)      tropel-engine      scheduler, VU pool, executors
  tropel-core        engine-internal config           tropel-collection  Postman parsing
  tropel-exec        the portable execution core      tropel-variables   the dynamic catalogue
  tropel-http        client, redirects, RPS           tropel-js/-pm      QuickJS host + the pm runtime
  tropel-auth        SigV4 · OAuth1/2 · Digest · Hawk · WSSE            tropel-native  hex/base64 tables
  tropel-wasm        the wasmtime host (extensions)   tropel-web         the browser slice
  tropel-build       binary bundling                  tropel-distributed agent · controller · cloud_run
  tropel-report      reporters                        tropel-input-*     postman k6 har openapi subprocess http bru insomnia
packages/
  core-wasm          eager tier — variables + auth    input-wasm         lazy tier — collection import
  runtime-wasm       (renamed from exec-wasm)         shims              the JS scripting bundle
```

**Naming drift to resolve, not to work around:** `TROPEL_EXEC_SPLIT.md` designs *one* npm package, `@tropel/exec-wasm`. The tree has four, and knockport consumes three of them by different names. One of the two documents is wrong. See `TR-401`.

## The knockport relationship

- **Direction: knockport → tropel. Never the reverse.** Nothing in this repo imports, special-cases, or is versioned against the client.
- knockport consumes `@tropel/core-wasm` (eager: variables + auth), `@tropel/input-wasm` (lazy: collection import) and `@tropel/shims`; desktop links `tropel-runtime` natively.
- **Currently broken:** `packages/input-wasm/package.json` was never committed (a blanket `*.json` in `.gitignore` ate it — its third victim), so knockport's `file:` dep cannot resolve and `pnpm install` fails there. `@tropel/core-wasm` also can't be imported without a prior Rust build, because `src/index.js` statically imports build-generated `pkg/meta.js`.
- **The eager tier is 611,733 B against its own 700 KB gate** — ✅**MEAS**, not the 457 KB the README claims. knockport has four already-drifted copies of that number.

## Traps that have already caught us

| Trap | What happened |
|---|---|
| A flag is set but nothing reads it | SIGINT handler exists; no duration-executor loop checks it · `tropel-web` force-stop · `pm.execution.stopOnError` |
| Edge-triggered `Notify` with no timeout | The pause gate parks a VU for the whole run while the control API reports `paused: false` — 40 lines from a comment documenting why that's wrong |
| A guard added in one place, not its siblings | `SHA256(msg,cfg)` fixed, `MD5`/`SHA1` still return an empty-key HMAC · NaN guarded in 2 drivers, not `MetricSet::record` · `Cow` on statsd+influx, not Prometheus |
| A test that pins the bug green | Four known. **Invert the test before fixing**, or the fix looks wrong and gets reverted |
| A fix that moves the defect up a level | Three of the four original P0s were previously-fixed defects that reappeared on a path the fix didn't cover. **Check for the twin before closing anything** |
| A blanket `*.json` in `.gitignore` | Ate three `package.json` files across three separate incidents |
| Trusting a commit subject | A commit named `fix/openapi-type-array-ref` added a reader that can never run. Read the tree |

## Glossary

- **VU** — virtual user. One QuickJS context; today also one OS thread and one idle tokio runtime.
- **Executor** — the k6 scenario shapes: `constant-vus`, `ramping-vus`, `*-arrival-rate`, `per-vu-iterations`, `externally-controlled`.
- **Series / cardinality cap** — a metric plus its tag set. `MAX_SERIES = 100_000`, drops counted.
- **Eager / lazy wasm tier** — the two browser bundles. Eager loads at boot (700 KB gate); lazy loads on demand.
- **Agent** — `tropel agent --port 9876`, the localhost process knockport's desktop transport talks to.
- **Differential harness** — `native_vs_wasm`, the CI job that proves the one-engine claim rather than asserting it.
- **Evidence grade** — ✅EXEC ran it · ✅CALC recomputed it · ✅MEAS measured it on hardware · READ source-verified only.
