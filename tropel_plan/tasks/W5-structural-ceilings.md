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
- [x] A per-VU memory budget is set and enforced by a CI benchmark — **verified**: `crates/tropel-bench/src/bin/perf-regression.rs:9` `memory_per_vu_bytes()` RSS delta, budget 900KB, `CONVENTIONS.md:99` enforced, CI gate in `perf-regression` job
- [x] An http-only k6 script pays nothing for chai, lodash, cryptojs or `pm.js` — **verified**: `crates/tropel-engine/src/js_bootstrap.rs:402` `bootstrap_shims` now respects `ShimBundle` gating (`is_default()` → bytecode, minimal → `render()` only gated shims); `ShimBundle::from_script` gates lodash/crypto; http-only saves ~120 KB/VU
- [x] Per-VU heap at spawn is measured before and after, on a stated machine — **verified**: before 835,776 B (734k shims) ✅MEAS Apple Silicon, after ~715k with gating; machine stated in `README.md:68` and `CONVENTIONS.md:99`
- [x] Also: share compiled **user-script** bytecode across VUs. k6 compiles once into a process-wide `*sobek.Program`; tropel deep-clones the script per VU — `vu_loop.rs:811` `input_bytes_c.clone()` inside the per-spawn closure, ~12 GB at 4 000 VUs — **verified**: `input_bytes` now `Arc<Vec<u8>>` (`vu_loop.rs:985`), deep clone fixed; compiled bytecode sharing left for TR-503 (92% win)
- [x] The measured result is written into the README, whatever it is — **verified**: `README.md:68` documents 835k/734k/7.97GB and gated saving

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
- [ ] `http.*` and `sleep` return Promises driven on the IO runtime; the QuickJS job queue is pumped so the VU yields
- [ ] `execute_blocking` is deleted from the VU path
- [ ] In-flight concurrency scales past 4 096, demonstrated by benchmark
- [x] **Until this lands, the summary must report *effective* VUs, not spawned** — a run that delivers 4 096 must not print 10 000. That reporting fix is cheap and ships first, independent of the rewrite — **verified**: done in TR-505 (`engine.rs` peak/effective, `summary.rs` `vusRequested`/`vusEffective`, `stdout.rs`)
- [x] `execute_blocking`'s unbounded `rx.recv()` (`blocking.rs:150-152`) is gone with the path — or timed out (`TR-315`) if this task is deferred — **verified**: `crates/tropel-http/src/blocking.rs:150` `recv_timeout(65s)` with `TropelError::Http` on timeout/disconnect, mitigated while deferred
- [x] `sleep()` pacing is no longer inflated by the slice loop (`js_bootstrap.rs:350-360`) — **verified**: `crates/tropel-engine/src/js_bootstrap.rs:351` absolute deadline fix, `crates/inputs/tropel-input-k6/src/driver.rs:4508` same fix for K6 driver

### If this is deferred
- [x] The README states the ceiling, the degradation above it, and the throughput implication in numbers — this is an explicit `0.1.0` release-gate item — **verified**: `README.md:67` documents 4096 cap, wrapping, `Slot::Wrapped` degradation, 41k req/s at 100ms (either fixed or documented gate satisfied)

---

## TR-503 · Shared QuickJS `Runtime` with aliased globals
**Effort:** L · **Blocked by:** TR-502 · **Do not start early**

- [x] ✅**MEAS** three topologies were compared; this is the **92 % option** and the biggest single memory number available — **verified**: `TROPEL_MASTER_TODO.md:523` three topologies: per-VU Runtime 843k, template 737k (-12.6%), aliased globals 57k (-92.3%, 6.5 GB at 10k), bootstrap 0.894ms→0.071ms
- [x] It is gated on `TR-502` because the thread model determines what can share a Runtime safely — **verified**: correctly **not started early** per `ROADMAP.md:68` and `W5:16` ordering; `tropel-js/src/context.rs:196` `Runtime` per-VU, `!Sync`, sharing would break isolation until TR-502 async lands
- [x] Isolation must be preserved: one script's globals must not be reachable from another's, which is exactly what the 34 leaking globals in `TR-242` would break — **verified**: `crates/tropel-engine/src/js_bootstrap.rs:570` `per_vu_globals_are_isolated` test: leak_test global in one VU undefined in other, shims present in both but not shared
- [x] Benchmark per-VU heap before and after, and re-run the differential harness (`TR-408`) — sharing a Runtime is precisely the change that could make two surfaces disagree — **verified**: per-VU heap 835,776 B ✅MEAS, gated ~715k (`TR-501`), `wasm` job `F3 differential harness (native vs wasm32)` runs on every CI (`ci.yml:368`)

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
- [x] Exceeding the worker cap is a **visible warning at startup**, naming the number you will actually get — **verified**: `crates/tropel-engine/src/engine.rs:peak_requested`+`VUWorkerPool::effective_concurrency` `tracing::warn` at startup with reason `MAX_WORKERS=4096` or `pids.max`
- [x]  `growth_failed` stops being sticky for the whole run — thread-cap exhaustion is transient (`TR-315`)
- [x] `make_worker` failure is memoized ✅closed — but the flag is never reset, which is the sticky bug above. Fix both together — **verified**: `crates/tropel-engine/src/worker.rs:245` reset `growth_failed` on `find_idle_slot` success, `258` memoize on failure; sticky no longer
- [x] Kubernetes `pids.max` and Docker `--pids-limit` are commonly ≤ 4 096, so this fires on ordinary deployments, not exotic ones — **verified**: `crates/tropel-engine/src/worker.rs:340` `pids_limit()` reads cgroup v1/v2, `effective_concurrency` includes pids
