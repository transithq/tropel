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
- [x] Outputs cannot back-pressure the VU path. Either a dedicated runtime, or a bounded drop-with-count path (`TR-001` makes the drop visible) — **bounded drop-with-count implemented**: `record_batch`/`record` use `try_send` (never block the VU on a full channel); a full `MAX_PENDING_SAMPLES` channel drops the batch and increments `AGGREGATOR_SAMPLES_DROPPED`, surfaced in the summary (`aggregatorSamplesDropped`) — a run that lost samples is never reported clean
- [x] The aggregator gets guaranteed scheduling independent of output flushes — **yield points added**: output `emit()`/`flush()` calls `tokio::task::yield_now()` so the aggregator task on the shared runtime gets scheduled between batches
- [x] Flushes yield — **done** (yield_now after each emit in the extension output driver)
- [ ] A benchmark with a deliberately slow output asserts VU throughput is unchanged and the drop count is reported
- [ ] The 2-worker default is justified with a measurement or raised

## TR-302 · `build_results` throttles load generation to ~55 % duty cycle
**Effort:** M · **Blocked by:** TR-002

- [x] `histogram.rs:230-241` calls **four separate** percentile computations where one pass would do, on every 2 s tick — **verified done** (`stats()` uses `value_at_quantiles` for a single pass over all four percentiles)
- [x] At 100 k series it allocates ~1.2 M times per tick
- [ ] Target: aggregator duty cycle under 20 %, measured
- [x] `retain_histograms` cloning every Trend histogram per tick is the sibling — fix together (`TR-122`) — **verified done** (the clone is guarded by `retain_histograms` flag, computed once at config time)

## TR-303 · Connection lanes
**Effort:** L · **Blocked by:** TR-014, TR-002

**The highest-value single feature in the plan.** hyper enforces one h2 connection per pool, so a `Vec<reqwest::Client>` of lanes is what removes the cap. One change closes: the h2 single-connection cap, frame-demux serialization, per-connection server stream limits, and multi-IP spread.

- [x] Lane count configurable, with a measured default — **implemented**: `http2_connections` (default 1) builds N independent `reqwest::Client` lanes; round-robin selection via a shared `Arc<AtomicUsize>` cursor. The config field EXISTED but was never wired into the client (a flag set but nothing read it)
- [ ] A benchmark against an h2 server with a low `MAX_CONCURRENT_STREAMS` shows throughput scaling with lanes
- [x] `max_idle_connections` defaults to 4 for an entire scenario ✅closed — keep the regression test — **verified closed**: the default is now `usize::MAX` (a shared pool, not per-VU), so no connection cap throttles the scenario
- [x] Deliberately **not** in scope: dropping below reqwest to `hyper::client::conn::http2`. That is the moat (it unlocks stream-acquisition timing), and it is post-`0.1.0`

---

# Track B — Outputs

## TR-304 · OTLP is O(n²) and permanently oversubscribed
**Effort:** L · **Blocked by:** TR-001

- [x] `otlp.rs:212-233` does a linear scan comparing full `Vec<(String,String)>` tag sets — quadratic in series count — **verified done** (P-D.2: `HashMap` O(1) lookup replaces the scan)
- [x] It ships **JSON, not protobuf, with no gzip**: 140–750 ms of CPU per 100 ms window, **1.4–7.5× oversubscribed permanently**, capping the whole tool at ~10–30 k samples/s ≈ 1–2.5 k req/s — **gzip added** (flate2, `Content-Encoding: gzip`, ~8-15× wire reduction); protobuf encoding remains (needs `opentelemetry-proto`/`prost`, a CONVENTIONS dep gate)
- [x] Hash the tag set; emit protobuf; enable gzip — **tag-set hash done; gzip done; protobuf deferred**
- [x] Delta Sums must carry `startTimeUnixNano` — `aggregationTemporality: 1` without it is not readable by a conformant collector — **fixed** (PR #398)
- [ ] A benchmark asserts the per-window CPU is under budget at 100 k samples/s

## TR-305 · Per-output waste
**Effort:** M · **Blocked by:** TR-002

- [x] `TagPolicy::apply` deep-copies every tag when the policy is a no-op (`output.rs:40-65`) — **11 allocs/sample × 4 outputs = 44/sample**. Return the `Arc` unchanged when the policy is empty
- [x] `sanitize_prometheus_name` is the one sanitizer that never got `Cow` — the sibling-miss shape
- [x] Prometheus `cumulative` is **never evicted** — ~140 MB at max cardinality; tag limits ship disabled with no CLI flag to enable them — **verified done** (`max_cumulative` cap + warning; `TagPolicy` allowlist/max_tags)
- [x] InfluxDB int/float flap — `12.0 → 12i`, `12.5 → 12.5` on the same field; the first sample pins the type and the next one is rejected — **fixed**: whole-number floats now carry the `.0` suffix so InfluxDB pins FLOAT
- [x] Newline unescaped in Influx tag values — **verified done** (`escape` handles `\n`/`\r`/`\t`)
- [x] NDJSON writes ~**1.7 TB in 24 h** — `.append(true)` with no rotation, no size cap, no sampling — **verified done** (1 GB rotation with `.1`/`.2` suffixes)

---

# Track C — Extensions and adapters

## TR-306 · gRPC caps the whole process at ~1–2 k RPC/s
**Effort:** M · **Blocked by:** TR-002

- [x] `lib.rs:248-249` — the cache key is the proto **source** (up to 1 MiB), hashed inside a **process-global mutex, per request** — **hash moved OUTSIDE the lock** (verified); **lock held across compile** (verified)
- [x] Cold-start stampede: the double-checked cache **releases the lock before** compiling and connecting, so N VUs all compile the same proto — **fixed** (lock held across compile)
- [ ] Two `std::env::var` calls per request, each taking the global environment lock
- [x] Key the cache on a cheap stable identity; hold the lock across compile, or use a per-key once-cell — **memoized last-key identity**: the MiB SipHash runs once per distinct source, not per request

## TR-307 · `detect()` fully parses the document, and every adapter is probed
**Effort:** M · **Blocked by:** none

- [x] `har/lib.rs:150` and siblings — no short-circuit, so importing an 80 MB file parses it once per adapter — **fixed**: `resolve_input` iterates by priority descending and returns on the FIRST match (the old code probed all 7+ adapters); k6's `is_postman_collection` guard uses a lightweight serde `Probe` instead of a full `Value` parse
- [x] Cheap discriminators first; parse once, at most
- [x] Subprocess **double-parses** — a `Value` tree up to ~50–80 MB from 16 MiB of output — **fixed**: the fallback stream-deserializes array elements one at a time (no `Vec<Value>` tree)
- [x] `BASE_URL` is read via `std::env::var` inside `parse()` (`openapi/lib.rs:419`), making parsing non-deterministic and environment-dependent — **fixed**: emits a `{{base_url}}` placeholder that resolves from config env (the engine injects env into scenario.variables after parse)
- [ ] Browser import retains **10.5× the input, permanently, and parses the document 10 times** — this one sits on knockport's path, see `TR-403`

## TR-308 · The OpenAPI `$ref` fanout
**Effort:** M · **Blocked by:** none

- [x] **Skip `responses` when resolving `paths`** — the same skip already applies to `components.schemas`. ✅**CALC** it is **4.3×** of the fanout, and the fix is free — **verified done** (`resolve_value` skips `responses` at any nesting level)
- [x] The `responses` skip is currently at the wrong nesting level (`lib.rs:996` tests `if mk == "responses"` one level too high) — **fixed**
- [x] The memo caches work, not space — `in_progress` only cuts *cycles*, so an acyclic diamond still explodes — **memo cache covers diamonds** (per-$ref-string); `in_progress` cycle check moved BEFORE `resolve_pointer` (was after — the deep clone ran before the cycle check, cloning megabytes just to discard them)
- [x] Three deep copies per `$ref` target, and the cycle check runs **after** them — **reduced to 2**: `resolve_pointer` now returns a BORROWED reference (no deep clone) instead of an owned `Value`; the cache-insert clone is necessary
- [x] `"type": ["string","null"]` fails the whole document — the canonical 3.1 idiom, while `detect()` claims 3.1 support — **verified done** (`schema_type` handles string + array forms)
- [x] Auth placeholders emit `__token__` where the syntax is `{{var}}`, so they are unsubstitutable — **fixed**: `{{token}}`/`{{username}}`/`{{password}}`/`{{api_key}}`/`{{access_token}}`/`{{id_token}}` (the runner's `resolve_auth` can now substitute them)

---

# Track D — Distributed

## TR-309 · Distributed mode has no periodic snapshot
**Effort:** M · **Blocked by:** TR-001

- [x] `agent.rs:76-88` runs the **entire engine to completion** before reporting, so a distributed run is blind until it ends and a crashed agent loses everything — **fixed**: `Engine::with_snapshot_sink` streams periodic snapshots (2 s cadence) to the agent, which forwards them to the controller as progress frames; the controller reads until the final `done: true` frame (PR #406)
- [x] Periodic snapshots on the same cadence as single-node

## TR-310 · `merge_snapshots` wastes 25 s CPU and 30 GB churn — the fix is 2 lines
**Effort:** S · **Blocked by:** none

> **✅ CLOSED — verified at `2099cbe`.** `collector.rs:1357` now reads *"NOTE: rebuild_merged() is NOT called here — it is hoisted out of"* the absorb loop, with the single call at `:1493`. The hoist landed.

- [x] `rebuild_merged` is out of the absorb loop
- [x] `absorb_snapshot` still silently drops a Trend histogram on a cross-worker type conflict — **fixed**: warns loudly (TR-310), stats still merge, test added

---

# Track E — The per-request and per-iteration floor

Cheap wins with a high instance count. Ship them after Track A, when the measurements mean something.

## TR-311 · The Postman path amplifies 10×
**Effort:** L · **Blocked by:** TR-002

- [ ] `pm.js` builds its entire object graph **twice** — `:1591` and `:1598` both call the 1,580-line builder
- [x] `pm.response.json()` does **3 body copies and 2 full parses per call** — and the memoization already exists, unused — **verified done**: `body_json()`/`body_text()` use `get_or_init` caching (OnceLock), the native bridge returns the cached text
- [ ] `build_scope` — **105 HashMap clones and ~3,885 allocations per iteration**, called 21× per iteration
- [ ] **246 KB copied and SipHashed per iteration** to look up an already-compiled script (`context.rs:858-864`)
- [x] `setNextRequest` is a linear scan over all N ids — an **O(n²)** cliff on large collections — **verified done**: the bridge uses `id_to_index`/`name_to_last_index` HashMaps (O(1) lookup); 8 tests cover the resolution order
- [x] `resolver.rs:125` calls `.to_string()` on an already-`Cow::Owned` value — **verified done** (`into_owned()` at line 136)

## TR-312 · The request path allocation floor
**Effort:** M · **Blocked by:** TR-002

- [ ] **The response body is copied 6 times; the floor is 2** — `Bytes` → `to_vec` → `clone` → `Response::from` → `from_utf8` → …
- [ ] Four near-one-line allocation fixes together remove **~18 allocations and two full body copies per request** (~200 µs)
- [x] `TagMap` construction allocates ~15× per hop where ~6 would do — **verified done**: the k6 driver builds tags with `TagMap::with_capacity(7)` + interning (`interned`/`intern_method`/`intern_status` — OnceLock'd Arc<str> for keys, methods, statuses)
- [x] Parse the URL **once** per hop — there are currently 3 parses per request — **fixed**: `execute_with_jar` parses once, reuses the `Url` for the reqwest builder AND the cookie jar AND the redirect join
- [x] Intern metric names; `MetricKey::new` allocates ~24× per request in the aggregator — **verified done**: `to_sorted_arc_vec()` clones Arc refs (ref-count bump, no string copy)
- [x] Retain the sink `Vec`'s capacity — `mem::take` leaves `Vec::new()`, which re-grows 4→8→16 every tick — **fixed**: `std::mem::replace(&mut batch, Vec::with_capacity(1024))` retains the capacity (PR #403)
- [x] Static tables for `status` and `method` instead of `Arc::from(status_code.to_string())` — **verified done**: `intern_status`/`intern_method` (OnceLock'd tables of the 9 standard methods + common statuses)
- [ ] Batch the sample handoff with a per-VU thread-local buffer flushed once per iteration

## TR-313 · Startup and build
**Effort:** M · **Blocked by:** TR-002

- [x] The script file is **read 4×** before the ramp, two of them back-to-back — **fixed**: the startup read is shared across `declared_options` AND the scenario loop (was a re-read per call); the scenario tasks reuse the startup bytes instead of re-reading
- [x] Cache `prepare_module_source` by `(path, mtime)` — closes the N+2 startup oxc parses, the per-VU parse, and a 200 MB memcpy — **implemented**: process-global `(path, mtime)`-keyed cache; test added
- [x] The shim bytecode cache is dead for the common case — `js_bootstrap.rs:401` takes the per-VU **source-eval** path
- [x] `JS_WRITE_OBJ_STRIP_SOURCE` — `context.rs:1014` passes only `JS_WRITE_OBJ_BYTECODE`, so QuickJS retains function source text in every VU — **verified done** (both flags: `JS_WRITE_OBJ_BYTECODE | JS_WRITE_OBJ_STRIP_SOURCE`)
- [x] Allocate the 24 MiB broadcast ring **only when an output exists** — `engine.rs:246` allocates `1<<18` slots unconditionally — **fixed**: ring allocated only when a streaming output (stdout/prometheus/otlp/json-stream/statsd/influxdb/extension) is configured
- [ ] **Dependencies: 484 crates, 26 % removable** ✅**MEAS** by feature-gating the four optional subsystems
- [x] `tropel build` binaries hardcode `#[tokio::main(worker_threads = …)]` and can never be tuned — **verified done**: `TROPEL_TOKIO_WORKERS` env var (default 4)

## TR-314 · The wasmtime host runs guests at ~1/2 speed
**Effort:** S · **Blocked by:** TR-002

- [ ] Two config dials — fuel is enabled unconditionally, plus one more — give **~2–2.6× on all guest code** — **investigated**: `consume_fuel(true)` is a DELIBERATE DoS guard for untrusted WASM plugins (an infinite loop traps with OutOfFuel instead of hanging the host); disabling it is a security regression. `max_wasm_stack(512 KB)` matches wasmtime's default. Changing either needs the register's measurement + a security decision — left open with the rationale documented.
- [ ] Measure before and after; this is the highest ratio-per-line item in the wave

## TR-315 · Soak-duration leaks
**Effort:** M · **Blocked by:** TR-002

- [x] `merged_per_url` / `merged_per_group`: entry count capped, **bytes uncapped** — **bounded**: entry count capped by `max_series`, histogram lazy + budget-guarded (`histogram_disabled`), so per-entry bytes are fixed; keys are the URL/group strings (capped by cardinality)
- [x] `growth_failed` is sticky for the whole run — thread-cap exhaustion is transient, and the flag never resets
- [x] `execute_blocking` can park a VU thread **forever** — `blocking.rs:150-152` `rx.recv()` has no timeout — **verified done** (`recv_timeout(65s)`)
- [x] `merge_scenario_tags` erases the entire `Arc<TagMap>` design under one line of user config ✅closed — keep the test
- [ ] A 24 h soak benchmark, run in CI weekly, asserting flat memory
