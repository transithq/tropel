# VERIFICATION — this plan audited against the code

**Audited:** `transithq/tropel` @ **`2099cbe`** (`master`, 2026-08-23), submodule `crates/tropel-sdk` @ **`5433412`**.
**Method:** clone and read. No commit messages consulted — the register's own rule, and this repo has a documented history of commit subjects over-claiming.
**Against:** `TROPEL_MASTER_TODO.md`, whose findings were taken at `111d2ea`.

> **The register is materially stale on structure, and partly stale on facts.** Seven items this plan listed as open are closed or substantially closed. Thirteen were re-confirmed verbatim, line and all.

---

## 1 · The tree was reorganized — nearly every path in the register is wrong

This matters more than any single item: a task whose file path no longer resolves reads as "already fixed" to the next agent.

| Register says | Actually |
|---|---|
| `crates/tropel-sdk/...` in-tree | **A git submodule**, pinned at `5433412` — a shallow clone gets an empty directory and every SDK grep returns nothing |
| `crates/tropel-input-*` | Moved under **`crates/inputs/`** |
| gRPC / WebSocket / Prometheus extensions | Moved under **`crates/extensions/`** (`tropel-x-*`) |
| `tropel-pm`, `tropel-exec` | **Gone.** New: `tropel-runtime`, `tropel-scheduler`, `tropel-metrics`, `tropel-sandbox`, `tropel-es`, `tropel-core-wasm`, `tropel-input-wasm` |
| `driver.rs:NNNN` | Split across `tropel-engine`, `tropel-runtime`, `tropel-sandbox` |
| `packages/exec-wasm` | Renamed **`runtime-wasm`** — and `.gitignore` still negates the old path |

**Consequence for every task below:** treat register line numbers as *evidence of what the defect was*, not as a location. Re-locate before editing.

---

## 2 · Closed — verified in the code

| Task | Evidence at `2099cbe` |
|---|---|
| **TR-003** MSRV | `Cargo.toml:45` `rust-version = "1.94"`, and `ci.yml` has an `msrv` job whose comment reads *"enforces the declared rust-version = 1.94; nothing did before"*. The "declares 1.85, gate cannot pass" finding is dead |
| **TR-004** `ExpectedStatus` | `tropel-sdk/src/config.rs:715` `pub fn parse(...) -> Result<ExpectedStatus, String>` — a strict fallible parse whose doc names the exact five strings from the register, plus a `checked_mul`/`checked_add` guard on the wildcard base. The only surviving `unwrap_or(0)` mentions are comments describing the old bug. **The last of the four original P0s is closed** |
| **TR-310** `merge_snapshots` | `collector.rs:1357` *"NOTE: rebuild_merged() is NOT called here — it is hoisted out of"* the absorb loop, called once at `:1493`. The 2-line hoist landed |
| **TR-605** SIGINT/SIGTERM | `vu_loop.rs:345-392` — a full handler: first signal → `request_stop()`, second → force-stop, `SIGTERM` on Unix, Ctrl-C on Windows. The register's *"zero occurrences in any crate or binary"* is wrong at this SHA |

## 3 · Substantially closed — the residue is smaller than the task claims

| Task | What actually exists | What is genuinely left |
|---|---|---|
| **TR-001** drop counter | `OUTPUT_SAMPLES_DROPPED` global (`tropel-report/src/lib.rs:29`), incremented in **all 7** `Lagged` handlers, carried on `MetricsResult`, printed by `stdout.rs:86-101` as *Samples lost* / *Samples dropped* / *Series dropped*, and in the JSON summary as `seriesDropped` (`summary.rs:131`) | Shown **only when non-zero**, so a clean run doesn't prove the counter is live · no *unverified-run* marking · continuous `dropped_iterations` unconfirmed |
| **TR-002** benchmarks | `crates/tropel-bench/benches/perf.rs` — five suites: `context_bootstrap`, `script_iteration`, `native_vs_js`, `pool_dispatch`, `memory_per_vu` (measured inside the timed body, deliberately) | **None of the W3 numbers**: samples/s egress per output, aggregator duty cycle, ramp wall-clock, request-path allocations. The VU/JS side is covered; the throughput side is not |
| **TR-201** unknown options | `deny_unknown_fields` on the scenario config (`tropel-sdk/src/config.rs:66`); `:1105` *"a typo'd camelCase option is a hard error"* | The parity finding is about the **~20 k6 root options** (`minIterationDuration`, `userAgent`, `batch`, `tags`, `systemTags`, …), a different struct. Re-scope the task to that |

## 4 · Re-confirmed open — verbatim, at the line

| Task | Evidence at `2099cbe` |
|---|---|
| **TR-005** bru | `crates/inputs/tropel-input-bru/src/lib.rs:302` — `Some("http-request") =>` |
| **TR-006** insomnia | `crates/inputs/tropel-input-insomnia/src/lib.rs:71` — `meta_sort_key: Option<i64>`, sorted at `:240` |
| **TR-007** adapters unreachable | `tropel-engine/src/builtins.rs:23-29` links postman, har, openapi, k6, http. **No bru, no insomnia.** They *are* in the wasm table (`tropel-input-wasm/src/lib.rs:98`) — so both adapters are browser-reachable and **CLI-unreachable**, an inversion the register didn't state |
| **TR-008** input-wasm manifest | `packages/input-wasm/` exists; `git ls-files` shows **no** `packages/input-wasm/package.json`. `.gitignore:41-42` still negates `!packages/exec-wasm/package.json` — a path that no longer exists |
| **TR-009** core-wasm import | `packages/core-wasm/src/index.js:21` — `import CATALOG_META from "../pkg/meta.js";`, still static and top-level, and no `pkg/` file is tracked |
| **TR-010** CI paths | `ci.yml` `paths:` = `crates/** js/** examples/** .github/** Cargo.toml Cargo.lock rust-toolchain.toml deny.toml`. **`packages/**` and the root `package.json` are still absent** — which is exactly why TR-008 and TR-009 reached `master` |
| **TR-011** zero sub-timings | `tropel-runtime/src/runner.rs:692` — *"Backlog line 459: omit tls_handshaking and sending"*. Still omitted |
| **TR-012** pause gate | `tropel-engine/src/vu_loop.rs:100-107` — `notified()` is constructed at `:101`, **after** the `is_paused()` read at `:100`, and the `select!` has **no timeout arm**. The `while` re-check narrows the window; it does not close it |
| **TR-013** deep-equal | `js/shared/deep-equal.js:126` — `var keysA = Object.keys(a).sort();` |
| **TR-014** protocol | `js/k6-shim/k6-shim.js:515` — `this.proto = 'HTTP/1.1';`. No `protocol` metric tag exists anywhere |
| **TR-202** `http_req_duration` | `tropel-runtime/src/runner.rs:631` emits `resp.response_time`, which is `hop_total` / `total_duration` (`client.rs:1025`, `:1202`) — **wall-clock**, not `sending + waiting + receiving`. The highest-impact parity item stands |
| **TR-407** broken WIT | `crates/tropel-sdk/wit/adapter.wit` still ships. Improvement: only **one** `.wit` remains — `world.wit` and `tropel-types.wit` are gone |
| **TR-603** SigV4 slashes | `crates/tropel-auth/src/signers.rs:516` — `let normalized = path.replace("//", "/");`, still a single pass |

## 5 · Not re-checked

Everything else. This pass covered W0 in full plus the highest-profile items from W2, W3, W4 and W6 — roughly 20 of 103 tasks. The pattern it found (**structure very stale, facts mostly accurate**) is the useful result; do not read an unlisted task as either confirmed or closed.

Two that specifically warrant a targeted check before anyone works them:

- **The four duration-based executors and the stop flag.** The handler now exists and `vu_loop`'s pause gate reads `is_stop_requested()`. Whether the duration executors poll it is the actual R4 claim, and it needs the scheduler read.
- **`TR-604`'s SDK items.** `OnceCell`, `OnceLock` and `__tropel_body` return **zero** hits across the submodule's `src/`. Either fixed or relocated — but the submodule is pinned at the exact SHA the register reviewed, so "fixed" would be surprising. Re-derive before filing.

---

## 6 · Second pass — the SDK (`tropel-sdk@1563667`, 2026-08-22)

Checked against the SDK's **own master**, not the submodule pin.

**The inversion is done.** `Cargo.toml` states it as a contract — *"This crate is a LEAF: it must not depend on any tropel-\* crate."* Verified: zero `tropel-*` deps, and zero `tokio` / `reqwest` / `std::fs` in `src/`, so a tier-1 native extension and a tier-2 wasm guest really do share one crate. Dogfooding is real — har, openapi, bru and insomnia each depend on `tropel-sdk` and nothing else.

**Four things the inversion did not bring with it:**

| | Status |
|---|---|
| **Guard 1** — CI `cargo tree` assertion | **Absent.** No match in `.github/` or `scripts/` |
| **Guard 2** — out-of-workspace build against the packaged crate | **Absent.** `scripts/` is only `publish-runtime.sh`, `version-lockstep.sh`, `wasm-size.sh` |
| **`wit/adapter.wit`** | **Still ships**, and its only consumer is still a `wit-parser` dev-dep asserting it *parses*. `world.wit` and `tropel-types.wit` are gone — 1 of 3 left |
| **`tropel-input-postman`** | Pulls `tropel-collection` as well as the SDK — the one adapter that isn't clean |

**Two new findings, neither in the register:**

1. **The submodule pin is behind its own master.** `transithq/tropel` pins `5433412`; the SDK repo's master is `1563667`. The engine builds against an older contract than the one under review — the mixed-version hazard `TR-406` exists to prevent, on the one surface it does not cover.
2. **`scripts/version-lockstep.sh` exists** and covers the binary, `tropel-web`, `@tropel/runtime-wasm` and `@tropel/shims` — but **not** `core-wasm`, `input-wasm` or the SDK. knockport consumes the first two directly, so the surfaces most likely to drift are exactly the ones left out.

**Unverified:** publication state. crates.io rejected the API lookup under its data-access policy, so the register's *"0.1.0 and 0.2.0 only"* is neither confirmed nor refuted. Check by hand before working `TR-601`.
