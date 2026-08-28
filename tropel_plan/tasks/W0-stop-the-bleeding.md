# W0 · Stop the bleeding — the last P0s and the measurement floor

**Gate:** every P0 closed · no sample is dropped invisibly · a committed benchmark exists for every number this plan claims.

Three of the four original P0s are closed, and **all three were previously-fixed defects that reappeared on a path the first fix didn't cover.** That is the defining shape of this wave. Before you close anything here, grep for the twin and say in the PR which siblings you checked.

Source: `TROPEL_MASTER_TODO.md` §W0, §W-R4, §P-D.1, §P-I.1, §P-I.7.

---

# Track A — The measurement floor

Nothing downstream is verifiable without these three. They are the cheapest items in the plan and they gate the most expensive ones.

## TR-001 · Finish the dropped-sample counter
**Effort:** S · **Blocked by:** none · **Blocks:** all of W3

### Problem
> **◐ SUBSTANTIALLY CLOSED — verified at `2099cbe`.** `OUTPUT_SAMPLES_DROPPED` (`tropel-report/src/lib.rs:29`) is incremented in **all seven** `Lagged` handlers, carried on `MetricsResult`, printed by `stdout.rs:86-101` as *Samples lost* / *Samples dropped* / *Series dropped*, and emitted as `seriesDropped` in the JSON summary (`summary.rs:131`). The original finding — *logged at `trace!`, no counter, nothing in the summary* — is dead.

What remains is narrow, and it is the part that makes the counter trustworthy rather than merely present.

### Acceptance criteria
- [x] Per-output dropped-sample count is counted and reaches the summary
- [x] It is printed **always, including when zero** — today `stdout.rs:86` and `:98` are both gated on `> 0`, so a clean run cannot distinguish "no drops" from "counter not wired"
- [x] The count is available to `handleSummary` and to every reporter (JSON, JUnit, CSV) — JSON carries `seriesDropped`; confirm the others
- [x] A non-zero drop count marks the run **unverified** in the summary, in plain language
- [x] `dropped_iterations` is emitted **continuously**, not as one end-of-run sample — k6 emits it per tick, and "when did drops happen" is exactly the question the arrival-token starvation raises
- [x] Documented: which subsystems can drop, and what a user does about each

### Tests required
- A test that forces a slow output, asserts a non-zero summary drop count, and asserts the run is marked unverified
- A zero-drop run asserts the line is still present with `0`

## TR-002 · Benchmarks, or none of the perf plan is verifiable
**Effort:** M · **Blocked by:** none · **Blocks:** every task in W3 and W5

### Problem
> **◐ PARTIAL — verified at `2099cbe`.** `crates/tropel-bench/benches/perf.rs` exists and covers five suites: `context_bootstrap`, `script_iteration`, `native_vs_js`, `pool_dispatch`, and `memory_per_vu` (measured *inside* the timed body, deliberately, so it is a real per-context number). The VU and JS side is genuinely covered.

**What is missing is precisely the W3 set.** `perf.rs` is unusually honest — its own comments flag that `pool_dispatch` is a dispatch microbench and *not* end-to-end VUs/sec. Nothing measures egress, and egress is where the ceiling lives.

### Acceptance criteria
- [x] Per-iteration CPU, per-VU memory at spawn
- [ ] Benchmarks for: **samples/s egress per output**, **aggregator duty cycle**, **ramp wall-clock**, **request-path allocations** — **PARTIALLY REOPENED 2026-08-29.** Egress, duty cycle and request-path allocations are real and drive production code. **Ramp wall-clock is not measured at all** — the bench with that name added integers in a loop, and there is no public step-table API to benchmark (see TR-220). Five benches in this suite measured something other than their name, each with a MEAS number quoted from it in W3/W5; they are deleted or rewritten and the claims withdrawn. See the `perf.rs` module header for the list
- [x] Each emits a machine-readable number CI can compare against a committed baseline
- [x] Release profile only — a debug measurement is not a measurement (see `TROPEL_PERF_VS_K6.md` §1: debug runs the serde/alloc/QuickJS path 3–10× slower, and the original 22 % gap was measured debug-vs-release)
- [x] The harness records the machine, so ✅MEAS claims are reproducible
- [x] A regression over threshold fails the build

## TR-003 · The declared MSRV is false and the CI gate cannot pass
**Effort:** S · **Blocked by:** none

> **✅ CLOSED — verified at `2099cbe`.** `Cargo.toml:45` now declares `rust-version = "1.94"`, and `ci.yml` carries an `msrv` job commented *"enforces the declared rust-version = 1.94; nothing did before."* Kept for the record; do not re-file.

- [x] The declared MSRV is true
- [x] CI builds on exactly the declared MSRV
- [x] Confirm the `msrv` job is in the **required** set once branch protection lands (`TR-606`) — confirmed at `1.94` (PR #349): branch protection requires `CI OK`, and `ci-ok` lists `msrv` in its `needs` and fails the aggregate when any dependency fails

---

# Track B — The last of the four P0s

## TR-004 · `ExpectedStatus` — one typo produces a perfect green run
**Effort:** S · **Blocked by:** none · **Blocks:** W1 thresholds

> **✅ CLOSED — verified at `2099cbe`** (submodule `5433412`). `crates/tropel-sdk/src/config.rs:715` is now `pub fn parse(&str) -> Result<ExpectedStatus, String>`: a strict fallible parse that rejects the exact five strings named below, plus a `checked_mul`/`checked_add` guard on the wildcard base so `"656xx"` can no longer wrap. The surviving `unwrap_or(0)` text is comments describing the old bug. **This was the last of the four original P0s.**

### Problem (historical — kept because the shape recurs)
`tropel-sdk/config.rs:697-698` parses the expected-status range with `unwrap_or(0)` / `unwrap_or(u16::MAX)`. ✅**EXEC**: `"200-"`, `"-"`, `"abc-def"`, `"2xx-3xx"`, `"20-30-40"` all match 100/200/250/404/500/599. So `["200-"]` yields **a perfect green run against a server returning nothing but 500s**, and `http_req_failed` is zero.

This is invariant #1 in one line of code.

### Acceptance criteria
- [x] The parse is fallible; a malformed range is a **startup error naming the offending string**, never a default
- [x] Overflow, `"200-"`, `"2XX"`, and reversed ranges each produce a distinct, actionable message
- [x] Every sibling call site of the same function is fixed in the same PR — name them in the PR body
- [x] k6's default range is 200–399 **inclusive**, and `null` suppresses `http_req_failed` entirely — match both

### Tests required
- Each of the five ✅EXEC strings asserts a startup error, not a match
- A regression test asserting `http_req_failed == 1.0` against an all-500 server with a valid range

---

# Track C — P0 · The input adapters and the packaging

These are one story: the newest adapters do not work, and the npm side does not install. **Track C is what currently blocks knockport from building at all**, so it is also the entry point to W4.

## TR-005 · `tropel-input-bru` rejects every real Bruno export
**Effort:** M · **Blocked by:** none

- [x] `lib.rs:302` matches `Some("http-request")`; Bruno's exporter rewrites the type, so no real export matches
- [x] Test fixtures are **exports produced by Bruno**, not hand-written JSON — this is exactly the gap that let it ship
- [x] Bruno path params are preserved (`:345` currently keeps only `param_type == "query"`)
- [x] Duplicate query keys survive — `merge_pairs` joins with `", "`, collapsing `[{ids,1},{ids,2}]` into one
- [x] A request that fails to convert is **reported**, not skipped: `if let Ok(child)` currently drops bad requests silently, contradicting the adapter's own docs

## TR-006 · `tropel-input-insomnia` fails on any drag-reordered workspace
**Effort:** S · **Blocked by:** none

- [x] `lib.rs:71` types `metaSortKey` as `Option<i64>`; Insomnia assigns sort keys by **midpoint averaging**, so they go fractional the first time a user drags anything. Type it as a float
- [x] Fixture: a workspace exported *after* reordering
- [x] Same silent-drop and duplicate-query-key fixes as `TR-005`

## TR-007 · Bruno and Insomnia are compiled out of the CLI entirely
**Effort:** S · **Blocked by:** TR-005, TR-006

- [x] `tropel-engine/Cargo.toml:32-37` depends on postman, k6, har, openapi, subprocess and http — **not** insomnia, not bru. The adapters exist and are unreachable
- [x] Add both to the inventory and the dispatch table
- [x] Resolve the priority collision: `http` and `bru` both register at priority 25, so the tie-break is **link-order-dependent**
- [x] A test asserts every adapter in the workspace is reachable from the CLI — otherwise the next one ships unwired too

## TR-008 · `packages/input-wasm/package.json` was never committed
**Effort:** S · **Blocked by:** none · **Blocks:** knockport install

### Problem
Root `.gitignore:33` is a blanket `*.json`. This is its **third** victim — the file's own comments record the prior two. Four reproduced consequences: the directory is not a workspace member (npm silently skips manifest-less dirs); its own `scripts/build.sh:50` fails `npm pack --dry-run` with `ENOENT` *after* producing `pkg/`; the README's `npm run build` reports `Missing script`; and **knockport's `file:` dep cannot resolve**, so `pnpm install` fails there.

### Acceptance criteria
- [x] **Scope the ignore rule** — do not add a fifth exception
- [x] Delete the two dead `!packages/exec-wasm/*` lines left from the rename to `runtime-wasm`
- [x] A CI check asserts every `packages/*` directory has a committed manifest and is a workspace member
- [x] A clean clone runs `npm install` at the root with no missing-manifest error

## TR-009 · `@tropel/core-wasm` cannot be imported without a prior Rust build
**Effort:** S · **Blocked by:** none · **Blocks:** knockport boot

- [x] `packages/core-wasm/src/index.js:21` is a **static top-level** `import CATALOG_META from "../pkg/meta.js"`, and `pkg/` is gitignored and build-generated → **module load** fails, not the call
- [x] knockport depends on the source directory via `file:`, so `packages/core/src/tropel.ts:10` and `oauth2.ts:8` break the whole app
- [x] The glue is already lazily `await import`ed — make `meta.js` lazy the same way, or commit it
- [x] A CI job imports the package from a clean clone with **no Rust toolchain present** and asserts it loads

## TR-010 · `ci.yml` `paths:` omits `packages/**` and the root manifest
**Effort:** S · **Blocked by:** none

- [x] A PR touching only `packages/core-wasm/src/index.js`, either smoke test, either build script, or the workspace manifest runs **no CI at all** — which is how `TR-008` and `TR-009` reached `master`
- [x] Add `packages/**` and the root `package.json`
- [x] Assert the negative: a PR touching only a package triggers the JS jobs

---

# Track D — Regressions introduced by earlier recommendations

Three fixes made things worse. They are called out separately because reverting them correctly needs the reasoning, not just the diff.

## TR-011 · Restore the always-zero sub-timings
**Effort:** S · **Blocked by:** none

### Problem
The advice was *"emit fewer samples — `tls_handshaking`/`sending` are always 0."* Done at `driver.rs:1144-1154` / `runner.rs:692-701`, and it flipped **two stock k6 thresholds to permanent FAIL**: neither has a first-class arm, so `aggregate_series` returns `None` → `unwrap_or((false, 0.0, 0.0))` → `✗ 0.00 (FAIL)` and a non-zero exit code. `options.rs:1057` still asserts the translator emits `http_req_tls_handshaking.avg < 100`; a `handleSummary` reading `data.metrics['http_req_sending']` now throws; and `res.timings.tls_handshaking` is **still populated**, so the script and the metrics disagree.

Emitting the two zero samples was correct and cheap.

### Acceptance criteria
- [x] Both samples emitted again, **or** `aggregate_series` grows arms that return 0 for a metric with no observations — pick one and say why
- [x] A missing series never renders as `FAIL`; "no data" and "failed" are distinct verdicts (see `TR-111`)
- [x] `res.timings` and the emitted metrics agree — a test asserts it
- [x] k6 also declares `looking_up` and never assigns it; emit it as 0 for byte-compat

## TR-012 · The pause-gate lost wakeup
**Effort:** S · **Blocked by:** none

- [x] `vu_loop.rs:100-107` creates `Notified` **after** reading `is_paused()`, registering only on first poll, while `set_paused` uses edge-triggered `notify_waiters()` (`scheduler.rs:426-427`)
- [x] A resume PATCH landing in that window is dropped, and **the `select!` has no timeout** → the VU parks for the rest of the run while the control API returns 200 and reports `paused: false`
- [x] Fix: construct and pin `notified()` *before* the check, **or** keep a `sleep(100ms)` backstop
- [x] `worker.rs:413-419` documents why edge-triggered `Notify` is wrong, 40 lines away. Fix the **sibling** too: the arrival-token bucket has the same shape in all three legs
- [x] A test pauses and resumes inside the race window and asserts the VU makes progress

## TR-013 · The shared deep-equal is structurally blind
**Effort:** S · **Blocked by:** none

- [x] Consolidating the three copies was right; the implementation is not. `js/shared/deep-equal.js:126-139` compares `Object.keys(a).sort()` — so it cannot see a key present in `b` and absent in `a` when counts match, among other holes
- [x] The asymmetric `Date` compare throws: `:31-35` guards `b instanceof Date` then calls `a.getTime()`
- [x] Property test over generated object pairs, asserting agreement with a reference implementation

---

# Track E — h2 is on by default, and is currently a ~100× regression

## TR-014 · HTTP/2 correctness
**Effort:** L · **Blocked by:** TR-002 · **Blocks:** TR-303

### Problem
`http2` is explicitly enabled, `h2 v0.4.15` is in the lock, `config.http2` defaults `true`, and ALPN advertises `h2` first — **every HTTPS request negotiates h2.** hyper-util then collapses all traffic for an origin onto exactly **one TCP connection**; a second h2 connection to a known origin is dropped rather than pooled. A server advertising 100 `MAX_CONCURRENT_STREAMS` at 50 ms latency caps you at **2 000 req/s regardless of VU count**, where h1.1 would reach ~200 000.

It is undetectable from inside the tool. On a multiplexed stream the connector never runs, so `blocked`/`dns`/`connecting` are all 0 and the entire stream-queueing wait lands in `http_req_waiting`, indistinguishable from server latency. (k6 has the mirror bug — it folds h2 queueing into `http_req_sending`.)

### Acceptance criteria
- [x] A `protocol` tag on every sample, from `Response::version()` — this also closes the `res.proto` lie and gives protocol on *failed* requests, which beats k6
- [x] The k6 shim stops hardcoding `'HTTP/1.1'` (the Rust path is fixed; the shim string remains)
- [x] h2 stream-queueing time is **not** reported as server latency — either its own timing or an explicit documented divergence
- [x] Typed h2 error classification, replacing the substring match on error text at `driver.rs:1022`
- [x] h2 keep-alive tuning: interval 10 s ✅landed, plus `timeout=5s` and `while_idle=true`
- [x] A benchmark against an h2 server with a low `MAX_CONCURRENT_STREAMS` proves the cap is gone or is documented

### Do not re-file
h2 is **on by default** — that is the problem, not the gap. hyper already overrides the h2 windows to 5 MiB / 2 MiB; at 10 k streams you want the stream window *smaller*, not larger.
