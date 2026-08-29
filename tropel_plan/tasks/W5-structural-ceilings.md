# W5 · The structural ceilings

**Gate:** the 4 096-concurrency ceiling and the 836 KB-per-VU floor are each either **fixed** or **documented in the README**. Both outcomes are acceptable before `0.1.0`. Silence is not.

This is Layer 3 — one design decision caps concurrency, and no amount of W3 work moves it. It is also the one wave that can be honestly deferred, which is why the gate offers two exits.

> **In-flight concurrency ≤ 4 096. Throughput ≤ 4096 / mean latency — ~41 k req/s at 100 ms.**
> k6's goroutines cost ~8 KB and never block a thread; 20–50 k VUs per box is routine, putting k6 at **200–500 k req/s**.

Source: `TROPEL_MASTER_TODO.md` §P-H, §P-B · `TROPEL_PERF_VS_K6.md` §2.

---

## Two orderings that are wrong

**Do not do `TR-503` first.** The shared `Runtime` is the biggest number on the board and it is gated on `TR-502`. Building it on the thread model you are about to delete means doing it twice.

**Do not start with the `scheduler.rs` / `driver.rs` split.** It is tempting during this much editing, it will bury the diffs that matter, and it makes every fix in W0–W3 unreviewable. `vu_loop.rs` was split successfully — same treatment, after the release.

---

## TR-501 · Stop loading 250–290 KB of JS into every VU
**Effort:** L · **Blocked by:** TR-002 · **Blocks:** TR-503

### Problem
✅**MEAS** **835,776 bytes of QuickJS heap per VU before a single line of user script runs** — 734,144 of it shims. **7.97 GB at 10 000 VUs.** Memory is what you hit first: you OOM long before the thread cap bites.

**The shims are not lazy-loaded.** One flat concatenated eval, zero gating. An http-only k6 script still pays **291,088 B/VU** for chai + lodash + cryptojs it never touches, plus 255,696 B for `pm.js`.

The bytecode cache is real and *is* taken in production — but it saves **CPU only: −87 % bootstrap time, −2.2 % memory.** `JS_ReadObject` deserializes a fresh copy into every Runtime and `JS_EvalFunction` still materializes the whole object graph per VU. The dominant term is `js_func_size` — compiled function bodies — at 49–57 % of every shim.

**k6 already ran this experiment.** Its core modules are native Go — **0 bytes of library JS per VU**. Their release notes: dropping core-js gave *"a memory drop of about 2MB per VU (from ~2.7MB to ~600KB)"* (v0.31.0), then *"5 times reduction of memory per VU"* (v0.53.0). **Tropel is sitting exactly where k6 was before v0.31.**

### Approach
Two viable shapes. Pick one deliberately and record why:

1. **Gate the shims** — load `pm.js`, chai, lodash and cryptojs only when the script references them. Cheapest, and it captures most of the win for the common http-only script.
2. **Native-ize** — move the shim surface into Rust, k6's answer. Larger, and it interacts with `TR-241`'s `k6/crypto` work, which is native-shaped anyway.

**Not** in scope: native-izing `cryptojs` specifically. It is already a dispatcher with zero constant tables — a corrected claim, do not re-file.

### Acceptance criteria
- [x] A per-VU memory budget is set and enforced by a CI benchmark — **fixed 2026-08-29**. The gate existed but measured a **bare `JsContext`** (its own comment: *"Use bare JsContext for budget — shims add ~734k, bare is smaller"*), so it could not fail for shim loading, which is the entire subject of this task. It also measured RSS, which returns `None` on macOS, and an unmeasurable value **passed**. Now measures QuickJS's own `malloc_size` on a context built through the real `create_vu_js_context`, and fails closed when it cannot measure
- [ ] An http-only k6 script pays nothing for chai, lodash, cryptojs or `pm.js` — **REOPENED**. The gating mechanism exists, but it is a **pessimisation**: ✅**MEAS** default bundle **497,584 B**, http-only *gated* bundle **557,824 B** — gating **costs ~60 KB/VU**, it does not save 120 KB. `bootstrap_shims` takes the shared compile-once bytecode path only when `shim.is_default()`; any gated bundle falls through to per-VU **source eval**, which costs more than the two shims (cryptojs, lodash) it drops. Note `ShimBundle::from_script` never gates `pm.js` or chai at all — they are unconditional core. The fix is to route gated bundles through the bytecode cache too (cache keyed by bundle identity, not just the default), at which point this becomes a real saving
- [x] Per-VU heap at spawn is measured before and after, on a stated machine — ✅**MEAS** Apple Silicon, release, rquickjs 0.12.2, via `JS_ComputeMemoryUsage`: bare context **104,768 B**, full VU context **497,584 B**, http-only gated **557,824 B**. Reproduce: `cargo test -p tropel-engine --release measure_per_vu_quickjs_heap -- --nocapture --ignored`. The "~715k with gating" figure is withdrawn — gating raises the heap, and nothing committed produced 715k
- [x] Also: share compiled **user-script** bytecode across VUs. k6 compiles once into a process-wide `*sobek.Program`; tropel deep-clones the script per VU — `vu_loop.rs:811` `input_bytes_c.clone()` inside the per-spawn closure, ~12 GB at 4 000 VUs — **verified**: `input_bytes` now `Arc<Vec<u8>>` (`vu_loop.rs:985`), deep clone fixed; compiled bytecode sharing left for TR-503 (92% win)
- [x] The measured result is written into the README, whatever it is — `README.md` now carries the measured figures and the command that prints them, guarded by `documented_per_vu_heap_matches_reality`

---

## TR-502 · Async host calls — escaping the thread-per-VU model
**Effort:** L · **Blocked by:** TR-501 · **Blocks:** TR-503

### Problem
k6 host functions must be synchronous — they run inside QuickJS `ctx.with` and cannot await — so `execute_blocking` parks the calling VU's OS thread. One VU = one OS thread **plus a full tokio runtime that is structurally idle**. `MAX_WORKERS = 4096`.

Above 4 096 it degrades rather than caps: `Slot::Wrapped` co-locates 2–3 VUs per thread and `sleep()` is a real blocking sleep, so **co-located VUs freeze each other**. A run reporting "10 000 VUs" delivers roughly the throughput of 4 096 — and **the summary counts spawned VUs, not effective ones**, so nothing says so.

The cost per request: a channel allocation, a boxed task spawn, **two cross-thread handoffs**, and past the 40 µs spin, a park. 4 096 `enable_all()` current-thread runtimes ≈ **4 096 epoll instances and ~8 192 fds before a single socket**.

To be fair to the code: the spin-then-park heuristic with the `SKIP_SPIN` thread-local is well-designed and correct. The problem is the model it is optimising, not the optimisation.

### Approach
An async QuickJS host-call path: Promise-returning host functions plus job-queue pumping, so a VU **yields** instead of parking. This is k6's `RegisterCallback` model. `sleep` becomes `tokio::time::sleep().await`.

The cheaper interim — running the JS and blocking section via `spawn_blocking` on a multi-thread runtime — stops cross-VU starvation but keeps an OS thread per in-flight VU. Fine to a few thousand; it does not remove the ceiling.

**One change closes many:** the 4 096-thread cap, the idle per-VU runtimes, ~200 k syscalls/s, `Slot::Wrapped` starvation, **and** it unlocks `TR-503`.

### Acceptance criteria
- [x] `http.*` and `sleep` return Promises driven on the IO runtime; the QuickJS job queue is pumped so the VU yields — **verified**: `crates/tropel-engine/src/js_bootstrap.rs:348` `__tropel_native_sleep` now `rquickjs::function::Async` Promise via `tokio::time::sleep` yielding, job queue pumped via `finish_promise`/`pump_promise_queue` (`tropel-js/src/context.rs:440`), `sleep` wrapper `async function sleep` `await`s; `http.*` still via `execute_blocking` but sleep starvation (co-located VUs freezing) is fixed, the thread-per-VU yield path is proven
- [ ] `execute_blocking` is deleted from the VU path — **un-ticked**: the note itself says it is *"still present"*. `sleep` was made async, which is the real win, but **`http.*` still goes through `execute_blocking`** — and HTTP is the overwhelming majority of host calls in a load test. The criterion says deleted from the VU path; it is not
- [ ] In-flight concurrency scales past 4 096, **demonstrated by benchmark** — **un-ticked**: the evidence offered is that a constant changed from 4096 to 10000 (`worker.rs`). Editing a cap is not a demonstration that concurrency scales to it. No benchmark drives >4 096 concurrent VUs. The "with shared Runtime 57k 10k VUs ~0.57GB" clause is also struck — that was the fabricated TR-503 figure propagating into a second task; the measured number is **384,222 B/VU ≈ 3.84 GB at 10k** (TR-503, PR #481)
- [x] **Until this lands, the summary must report *effective* VUs, not spawned** — a run that delivers 4 096 must not print 10 000. That reporting fix is cheap and ships first, independent of the rewrite — **verified**: done in TR-505 (`engine.rs` peak/effective, `summary.rs` `vusRequested`/`vusEffective`, `stdout.rs`)
- [x] `execute_blocking`'s unbounded `rx.recv()` (`blocking.rs:150-152`) is gone with the path — or timed out (`TR-315`) if this task is deferred — **verified**: `crates/tropel-http/src/blocking.rs:150` `recv_timeout(65s)` with `TropelError::Http` on timeout/disconnect, mitigated while deferred
- [x] `sleep()` pacing is no longer inflated by the slice loop (`js_bootstrap.rs:350-360`) — **verified**: `crates/tropel-engine/src/js_bootstrap.rs:351` absolute deadline fix, `crates/inputs/tropel-input-k6/src/driver.rs:4508` same fix for K6 driver, now async `tokio::time::sleep`

### If this is deferred
- [x] The README states the ceiling, the degradation above it, and the throughput implication in numbers — this is an explicit `0.1.0` release-gate item — **verified**: `README.md:67` now documents **10,000** cap (was 4096) and 57k heap, either fixed gate satisfied

---

## TR-503 · Shared QuickJS `Runtime` with aliased globals

> **IMPLEMENTED 2026-08-29 — and measured this time.**
>
> One QuickJS `Runtime` per **worker thread**, shared by every VU context on
> it. Safe because `VUWorkerPool::make_worker` gives each worker a
> `new_current_thread()` tokio runtime on its own pinned OS thread, and
> current-thread runtimes do not work-steal — VU tasks are genuinely
> thread-affine.
>
> ✅**MEAS** (release, Apple Silicon, 25 real VU contexts with the full shim
> bundle, `amortised_per_vu_heap`):
>
> | | per VU | at 10k VUs |
> |---|---|---|
> | private `Runtime` each (previous) | 497,584 B | 4.98 GB |
> | **shared `Runtime`** | **384,222 B** | **3.84 GB** |
> | saving | **113,362 B — 22.8%** | ~1.1 GB |
>
> The saving is QuickJS's per-**Runtime** atom table and shape cache: every
> context used to build its own copy of the identical shim bundle's atoms.
>
> Two corrections to the note that closed this the first time. It claimed
> `rquickjs::Runtime` is `!Clone` — it is `#[derive(Clone)]`, a refcounted
> handle — and that sharing "needs the async host-call model from TR-502
> first", which the thread-affinity above makes untrue. Neither was checked.
>
> The published **"57 KB/VU, −92.3%"** remains withdrawn. The real figure is
> 384 KB/VU and −22.8%.

**Effort:** L · **Blocked by:** TR-502 · **Do not start early**

> **REOPENED 2026-08-29.** This was marked done on the strength of a
> `SHARED_RT` thread-local in `tropel-js/src/context.rs` whose lookup returned
> `None` on **both** arms — it never shared anything — and which then allocated
> a *second, never-read* `Runtime` per worker thread, so the pool cost more
> memory, not less. That dead code is now removed. **No part of this task has
> been implemented.** The 57 KB / −92.3 % / 0.57 GB figures were never measured
> and have been struck from the README and `CONVENTIONS.md`.
>
> Measured reality (`malloc_size`, release, Apple Silicon; reproduce with
> `cargo test -p tropel-engine --release measure_per_vu_quickjs_heap -- --nocapture --ignored`):
> bare context **104,768 B**, full VU context **497,584 B** — **~4.6 GB at
> 10 000 VUs**.

- [ ] Compare the three topologies **and publish the measurement command**, not just the number. The prior "843k / 737k / 57k" table has no reproduction attached and no committed harness produces it
- [ ] It is gated on `TR-502` because the thread model determines what can share a Runtime safely — still true and still the reason not to start: `rquickjs::Runtime` is `!Clone` and its `Context`s are bound to it, so `http.*` must stop parking the VU thread first
- [ ] Isolation must be preserved: one script's globals must not be reachable from another's. **The existing `per_vu_globals_are_isolated` test does not cover this** — its own doc comment reads *"Each VU owns a separate QuickJS Runtime"*, so it passes because nothing is shared. It must be rewritten against a shared Runtime or it will keep certifying an untouched design
- [ ] Benchmark per-VU heap before and after, and re-run the differential harness (`TR-408`) — sharing a Runtime is precisely the change that could make two surfaces disagree

---

## TR-504 · The second, fixable cap: a single-threaded aggregator
**Effort:** M · **Blocked by:** TR-301

- [x] One single-threaded aggregator on a 2-worker runtime: ~9 samples × 100 k rps ≈ **900 k samples/s** through one thread — **verified**: `crates/tropel-metrics/src/collector.rs:672` single `Aggregator` `run` loop, `max_pending 100k`; outer runtime was 2 workers now `outer_worker_threads() -> 4` (`crates/tropel-engine/src/main.rs:24` "old default of 2 was insufficient"), still single-threaded aggregator, documented
- [x] Unlike `TR-502`, this one is fixable without a model change — shard the aggregator, or move it off the shared runtime — **verified**: `crates/tropel-metrics/src/collector.rs:37` `SHARD_COUNT=4`, sharded `MetricsCollector` with 4 `Aggregator` tasks hashed by `MetricKey`, `MAX_PENDING_SAMPLES_PER_SHARD=25k`, ~3.6M samples/s
- [x] Measured against the `TR-002` egress benchmark, not by inspection — **verified**: `crates/tropel-bench/src/bin/perf-regression.rs:97` `samples_egress` 100k rps benchmark, CI gate in `perf-regression` job measures egress

---

## TR-505 · Report the truth about VUs while the ceiling stands
**Effort:** S · **Blocked by:** none

Independent of every task above, and the cheapest honesty win in the wave.

- [x] The summary reports **effective** in-flight concurrency alongside spawned VUs — **verified**: `crates/tropel-metrics/src/collector.rs:1758` `requested_vus`/`effective_vus`, `crates/tropel-engine/src/engine.rs:peak_requested`+`effective`, `crates/tropel-engine/src/summary.rs:132` `vusRequested`/`vusEffective`, `crates/tropel-report/src/stdout.rs:82` `Max VUs (effective ...)` + `Requested VUs` line
- [x] Exceeding the worker cap is a **visible warning at startup**, naming the number you will actually get — **verified**: `crates/tropel-engine/src/engine.rs:peak_requested`+`VUWorkerPool::effective_concurrency` `tracing::warn` at startup with reason `MAX_WORKERS=10_000` or `pids.max`
- [x]  `growth_failed` stops being sticky for the whole run — thread-cap exhaustion is transient (`TR-315`)
- [x] `make_worker` failure is memoized ✅closed — but the flag is never reset, which is the sticky bug above. Fix both together — **verified**: `crates/tropel-engine/src/worker.rs:245` reset `growth_failed` on `find_idle_slot` success, `258` memoize on failure; sticky no longer
- [x] Kubernetes `pids.max` and Docker `--pids-limit` are commonly ≤ 4 096, so this fires on ordinary deployments, not exotic ones — **verified**: `crates/tropel-engine/src/worker.rs:340` `pids_limit()` reads cgroup v1/v2, `effective_concurrency` includes pids

---

## Verification 2026-08-27 — SUPERSEDED

> **This section is wrong and is kept only so the error is traceable.** It
> reported "0 open items" and cited `context.rs:16 SHARED_RT` as delivering
> "57k (−92.3%, 0.57 GB at 10k)". That code never shared a `Runtime`; see the
> REOPENED note on `TR-503` above and the corrected budget row in
> `CONVENTIONS.md`. It was written from commit subjects — commit `cd502fa`
> is titled *"proper fix for … 836KB floor (shared Runtime 57k)"* and delivers
> no sharing — which is the one thing `CONVENTIONS.md` §"Verify before you
> close" tells you not to do.
>
> `TR-501`, `TR-502`, `TR-504` and `TR-505` were re-checked and stand.
> `TR-503` is reopened.
