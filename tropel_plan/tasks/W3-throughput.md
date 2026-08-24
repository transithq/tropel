# W3 · Throughput — the egress ceiling

**Gate:** 100 k samples/s sustained with the drop counter at **zero**.

This is Layer 2: the tool caps out an order of magnitude below target, and every drop is invisible. **`TR-001` merges before anything in this wave** — without it you cannot distinguish a fix from a silent drop, and every measurement here is uninterpretable.

Every task in this wave needs a ✅MEAS number from `TR-002`'s harness. A perf PR without one cannot be reviewed.

Source: `TROPEL_MASTER_TODO.md` §P-D, §P-E, §P-F, §P-A, §P-J, §P-C.

---

# Track A — The back-pressure path into the VU hot loop

This is the one that turns a slow output into a slow *load test*, which is the worst possible coupling.

## TR-301 · A slow output starves the aggregator, which blocks every VU
**Effort:** L · **Blocked by:** TR-001, TR-002 · **Blocks:** the wave

### Problem
The outer runtime defaults to **2 workers**, shared by the aggregator and every output. Flushes don't yield. So a slow output stops the aggregator draining, its bounded channel fills in ~1 s, and `record_batch().await` blocks **every VU**. A network hiccup at your metrics backend becomes a throughput collapse in the run, reported as latency.

There is also **zero `spawn_blocking` in the entire workspace** — the only textual match is a comment.

### Acceptance criteria
- [ ] Outputs cannot back-pressure the VU path. Either a dedicated runtime, or a bounded drop-with-count path (`TR-001` makes the drop visible)
- [ ] The aggregator gets guaranteed scheduling independent of output flushes
- [ ] Flushes yield
- [ ] A benchmark with a deliberately slow output asserts VU throughput is unchanged and the drop count is reported
- [ ] The 2-worker default is justified with a measurement or raised

## TR-302 · `build_results` throttles load generation to ~55 % duty cycle
**Effort:** M · **Blocked by:** TR-002

- [ ] `histogram.rs:230-241` calls **four separate** percentile computations where one pass would do, on every 2 s tick
- [ ] At 100 k series it allocates ~1.2 M times per tick
- [ ] Target: aggregator duty cycle under 20 %, measured
- [ ] `retain_histograms` cloning every Trend histogram per tick is the sibling — fix together (`TR-122`)

## TR-303 · Connection lanes
**Effort:** L · **Blocked by:** TR-014, TR-002

**The highest-value single feature in the plan.** hyper enforces one h2 connection per pool, so a `Vec<reqwest::Client>` of lanes is what removes the cap. One change closes: the h2 single-connection cap, frame-demux serialization, per-connection server stream limits, and multi-IP spread.

- [ ] Lane count configurable, with a measured default
- [ ] A benchmark against an h2 server with a low `MAX_CONCURRENT_STREAMS` shows throughput scaling with lanes
- [ ] `max_idle_connections` defaults to 4 for an entire scenario ✅closed — keep the regression test
- [ ] Deliberately **not** in scope: dropping below reqwest to `hyper::client::conn::http2`. That is the moat (it unlocks stream-acquisition timing), and it is post-`0.1.0`

---

# Track B — Outputs

## TR-304 · OTLP is O(n²) and permanently oversubscribed
**Effort:** L · **Blocked by:** TR-001

- [ ] `otlp.rs:212-233` does a linear scan comparing full `Vec<(String,String)>` tag sets — quadratic in series count
- [ ] It ships **JSON, not protobuf, with no gzip**: 140–750 ms of CPU per 100 ms window, **1.4–7.5× oversubscribed permanently**, capping the whole tool at ~10–30 k samples/s ≈ 1–2.5 k req/s
- [ ] Hash the tag set; emit protobuf; enable gzip
- [ ] Delta Sums must carry `startTimeUnixNano` — `aggregationTemporality: 1` without it is not readable by a conformant collector
- [ ] A benchmark asserts the per-window CPU is under budget at 100 k samples/s

## TR-305 · Per-output waste
**Effort:** M · **Blocked by:** TR-002

- [ ] `TagPolicy::apply` deep-copies every tag when the policy is a no-op (`output.rs:40-65`) — **11 allocs/sample × 4 outputs = 44/sample**. Return the `Arc` unchanged when the policy is empty
- [ ] `sanitize_prometheus_name` is the one sanitizer that never got `Cow` — the sibling-miss shape
- [ ] Prometheus `cumulative` is **never evicted** — ~140 MB at max cardinality; tag limits ship disabled with no CLI flag to enable them
- [ ] InfluxDB int/float flap — `12.0 → 12i`, `12.5 → 12.5` on the same field; the first sample pins the type and the next one is rejected
- [ ] Newline unescaped in Influx tag values
- [ ] NDJSON writes ~**1.7 TB in 24 h** — `.append(true)` with no rotation, no size cap, no sampling

---

# Track C — Extensions and adapters

## TR-306 · gRPC caps the whole process at ~1–2 k RPC/s
**Effort:** M · **Blocked by:** TR-002

- [ ] `lib.rs:248-249` — the cache key is the proto **source** (up to 1 MiB), hashed inside a **process-global mutex, per request**
- [ ] Cold-start stampede: the double-checked cache **releases the lock before** compiling and connecting, so N VUs all compile the same proto
- [ ] Two `std::env::var` calls per request, each taking the global environment lock
- [ ] Key the cache on a cheap stable identity; hold the lock across compile, or use a per-key once-cell

## TR-307 · `detect()` fully parses the document, and every adapter is probed
**Effort:** M · **Blocked by:** none

- [ ] `har/lib.rs:150` and siblings — no short-circuit, so importing an 80 MB file parses it once per adapter
- [ ] Cheap discriminators first; parse once, at most
- [ ] Subprocess **double-parses** — a `Value` tree up to ~50–80 MB from 16 MiB of output
- [ ] `BASE_URL` is read via `std::env::var` inside `parse()` (`openapi/lib.rs:419`), making parsing non-deterministic and environment-dependent
- [ ] Browser import retains **10.5× the input, permanently, and parses the document 10 times** — this one sits on knockport's path, see `TR-403`

## TR-308 · The OpenAPI `$ref` fanout
**Effort:** M · **Blocked by:** none

- [ ] **Skip `responses` when resolving `paths`** — the same skip already applies to `components.schemas`. ✅**CALC** it is **4.3×** of the fanout, and the fix is free
- [ ] The `responses` skip is currently at the wrong nesting level (`lib.rs:996` tests `if mk == "responses"` one level too high)
- [ ] The memo caches work, not space — `in_progress` only cuts *cycles*, so an acyclic diamond still explodes
- [ ] Three deep copies per `$ref` target, and the cycle check runs **after** them
- [ ] `"type": ["string","null"]` fails the whole document — the canonical 3.1 idiom, while `detect()` claims 3.1 support
- [ ] Auth placeholders emit `__token__` where the syntax is `{{var}}`, so they are unsubstitutable

---

# Track D — Distributed

## TR-309 · Distributed mode has no periodic snapshot
**Effort:** M · **Blocked by:** TR-001

- [ ] `agent.rs:76-88` runs the **entire engine to completion** before reporting, so a distributed run is blind until it ends and a crashed agent loses everything
- [ ] Periodic snapshots on the same cadence as single-node

## TR-310 · `merge_snapshots` wastes 25 s CPU and 30 GB churn — the fix is 2 lines
**Effort:** S · **Blocked by:** none

> **✅ CLOSED — verified at `2099cbe`.** `collector.rs:1357` now reads *"NOTE: rebuild_merged() is NOT called here — it is hoisted out of"* the absorb loop, with the single call at `:1493`. The hoist landed.

- [x] `rebuild_merged` is out of the absorb loop
- [ ] `absorb_snapshot` still silently drops a Trend histogram on a cross-worker type conflict — **not** covered by the hoist, still open

---

# Track E — The per-request and per-iteration floor

Cheap wins with a high instance count. Ship them after Track A, when the measurements mean something.

## TR-311 · The Postman path amplifies 10×
**Effort:** L · **Blocked by:** TR-002

- [ ] `pm.js` builds its entire object graph **twice** — `:1591` and `:1598` both call the 1,580-line builder
- [ ] `pm.response.json()` does **3 body copies and 2 full parses per call** — and the memoization already exists, unused
- [ ] `build_scope` — **105 HashMap clones and ~3,885 allocations per iteration**, called 21× per iteration
- [ ] **246 KB copied and SipHashed per iteration** to look up an already-compiled script (`context.rs:858-864`)
- [ ] `setNextRequest` is a linear scan over all N ids — an **O(n²)** cliff on large collections
- [ ] `resolver.rs:125` calls `.to_string()` on an already-`Cow::Owned` value

## TR-312 · The request path allocation floor
**Effort:** M · **Blocked by:** TR-002

- [ ] **The response body is copied 6 times; the floor is 2** — `Bytes` → `to_vec` → `clone` → `Response::from` → `from_utf8` → …
- [ ] Four near-one-line allocation fixes together remove **~18 allocations and two full body copies per request** (~200 µs)
- [ ] `TagMap` construction allocates ~15× per hop where ~6 would do
- [ ] Parse the URL **once** per hop — there are currently 3 parses per request
- [ ] Intern metric names; `MetricKey::new` allocates ~24× per request in the aggregator
- [ ] Retain the sink `Vec`'s capacity — `mem::take` leaves `Vec::new()`, which re-grows 4→8→16 every tick
- [ ] Static tables for `status` and `method` instead of `Arc::from(status_code.to_string())`
- [ ] Batch the sample handoff with a per-VU thread-local buffer flushed once per iteration

## TR-313 · Startup and build
**Effort:** M · **Blocked by:** TR-002

- [ ] The script file is **read 4×** before the ramp, two of them back-to-back
- [ ] Cache `prepare_module_source` by `(path, mtime)` — closes the N+2 startup oxc parses, the per-VU parse, and a 200 MB memcpy
- [ ] The shim bytecode cache is dead for the common case — `js_bootstrap.rs:401` takes the per-VU **source-eval** path
- [ ] `JS_WRITE_OBJ_STRIP_SOURCE` — `context.rs:1014` passes only `JS_WRITE_OBJ_BYTECODE`, so QuickJS retains function source text in every VU
- [ ] Allocate the 24 MiB broadcast ring **only when an output exists** — `engine.rs:246` allocates `1<<18` slots unconditionally
- [ ] **Dependencies: 484 crates, 26 % removable** ✅**MEAS** by feature-gating the four optional subsystems
- [ ] `tropel build` binaries hardcode `#[tokio::main(worker_threads = …)]` and can never be tuned

## TR-314 · The wasmtime host runs guests at ~1/2 speed
**Effort:** S · **Blocked by:** TR-002

- [ ] Two config dials — fuel is enabled unconditionally, plus one more — give **~2–2.6× on all guest code**
- [ ] Measure before and after; this is the highest ratio-per-line item in the wave

## TR-315 · Soak-duration leaks
**Effort:** M · **Blocked by:** TR-002

- [ ] `merged_per_url` / `merged_per_group`: entry count capped, **bytes uncapped**
- [ ] `growth_failed` is sticky for the whole run — thread-cap exhaustion is transient, and the flag never resets
- [ ] `execute_blocking` can park a VU thread **forever** — `blocking.rs:150-152` `rx.recv()` has no timeout
- [ ] `merge_scenario_tags` erases the entire `Arc<TagMap>` design under one line of user config ✅closed — keep the test
- [ ] A 24 h soak benchmark, run in CI weekly, asserting flat memory
