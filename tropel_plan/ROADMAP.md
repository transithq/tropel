# ROADMAP — order, dependencies, parallelism

## Waves

| Wave | Theme | Gate to the next wave |
|---|---|---|
| **W0** | Stop the bleeding — the last P0s and the measurement floor | Every P0 closed · no sample is dropped invisibly · a benchmark exists for every number in this plan |
| **W1** | Honest numbers | No path where a broken thing reports success, or a working thing reports failure |
| **W2** | k6 parity — semantics, then surface | A real k6 script runs unmodified and produces **k6's** numbers |
| **W3** | Throughput — the egress ceiling | 100 k samples/s sustained with the drop counter at zero |
| **W4** | The knockport interface — one engine, proven | knockport builds from a clean clone · version handshake and differential harness green in CI |
| **W5** | The structural ceilings | 4 096 concurrency and 836 KB/VU each either fixed or **documented in the README** |
| **W6** | Release mechanics — `0.1.0` | Release gate green · crates.io, npm and the binary published in lockstep |

## The layer each wave attacks

`CONTEXT.md` defines three layers of problem. Mixing them in one PR is how a review stalls.

```
Layer 1  a green run can be wrong    →  W0 · W1 · W2
Layer 2  silent 10× throughput cap   →  W0 (visibility only) · W3
Layer 3  structural concurrency cap  →  W5
neither  packaging and shipping      →  W4 · W6
```

## The dependency graph

```
W0 ─┬─ TR-001 drop counter ─────────────────┐   30 lines; before ALL throughput work
    ├─ TR-002 benchmarks ───────────────────┼──> every W3 and W5 task is unverifiable without these
    ├─ TR-003 false MSRV ───────────────────┘
    ├─ TR-004 ExpectedStatus ──────────────────> TR-101 (thresholds read it)
    ├─ TR-005 bru ─┬─> TR-007 adapters compiled out of the CLI
    ├─ TR-006 insomnia ┘
    ├─ TR-008 input-wasm manifest ─┬────────────> knockport installs · TR-134 · W4
    ├─ TR-009 core-wasm static import ┘
    ├─ TR-010 ci.yml paths ────────────────────> TR-606
    ├─ TR-011 restore zero sub-timings ────────> TR-111 · TR-202
    ├─ TR-012 pause-gate wakeup · TR-013 deep-equal ──> TR-132
    └─ TR-014 h2 correctness ──────────────────> TR-303

W1 ─┬─ TR-101..105  always-green ─┐
    ├─ TR-110..114  always-red ───┼──> the 0.1.0 release gate
    ├─ TR-120..122  aggregation ──┘
    └─ TR-130..135  collapse duplicates ──> W2 (one path to fix, not two)
         TR-133 group paths ──> TR-207        TR-112 aggregate_series ──> TR-222

W2 ─┬─ TR-201 unknown-option warning        do first — ~20 silent drops become visible
    ├─ TR-202 http_req_duration ──> TR-203 emit-on-failure ──> TR-204 error_code ──> TR-212 systemTags
    ├─ TR-205 url←name ──> TR-206 http.url                     the cardinality mechanism
    ├─ TR-220 ramping step table ──> TR-221 striped offsets · TR-222 thresholds
    ├─ TR-230..233 the k6/http surface        TR-240..246 the rest of the JS surface
    ├─ TR-250..251 CLI and REST
    └─ TR-260..263 Postman ────────────────────> also serves knockport

W3 ─┬─ TR-301 output starvation ──> TR-302 aggregator duty cycle ──> TR-504
    ├─ TR-303 connection lanes                 highest-value single feature in the plan
    ├─ TR-304 OTLP · TR-305 output waste · TR-306 gRPC · TR-307 detect() · TR-308 $ref fanout
    ├─ TR-309 distributed snapshot · TR-310 merge_snapshots (2 lines, 25 s CPU)
    └─ TR-311..315 the per-request floor

W4 ─┬─ TR-401 package naming ──> TR-402 wasm ergonomics · TR-404 eager-tier weight
    ├─ TR-403 resolveDynamicVariables cap      knockport's send path, unwrapped
    ├─ TR-405 tropel agent ──> TR-406 version handshake ──> TR-411 load handoff
    ├─ TR-407 SDK inversion ──> TR-408 differential harness ──> TR-601
    └─ TR-409 signing contract · TR-410 conversion report

W5 ─┬─ TR-501 shim memory ──> TR-502 async host calls ──> TR-503 shared Runtime
    │      (do not reorder — 503 built on the model 502 deletes is 503 done twice)
    ├─ TR-504 aggregator sharding
    └─ TR-505 report effective VUs            independent, cheapest honesty win

W6 ─┬─ TR-603 auth correctness ──> TR-602 tropel-auth decision ──┐
    ├─ TR-604 SDK compile gates ────────────────────────────────┼──> TR-601 publish
    ├─ TR-605 SIGINT + control-API robustness ──────────────────┘
    └─ TR-606 branch protection · TR-607 documentation debt
```

## What can run in parallel

- All of **W0** — the five tracks touch different subsystems. `TR-001` still merges first, because without it you cannot distinguish a throughput fix from a silent drop.
- **W1** and **W2** overlap on purpose: both are Layer 1, and several W2 items *are* a W1 fix restated in k6's terms. Where a task appears in both, W2 names the W1 id rather than restating it — `TR-203`/`TR-121` and `TR-207`/`TR-133` are the two.
- **W3** and **W4** are independent — different crates, no shared files.
- Within **W2**, the metric-fidelity, algorithm and module-surface tracks are three independent tracks.
- **W5** is independent of everything but its own internal order, and is the one wave that can be closed by documenting instead of fixing.
- **W6 Track A** blocks on `TR-603`; Tracks B and C do not.

## Suggested first ten PRs

Re-ordered after the `2099cbe` audit (`VERIFICATION.md`) — `TR-003` and `TR-004` are already closed and have left the list.

1. `TR-008` + `TR-009` the packaging P0s — knockport's `pnpm install` fails on them today, and they are two of the three cheapest items here
2. `TR-010` `ci.yml` paths — still missing `packages/**`, which is *why* 1 reached `master` untested. Ship it with 1 or it recurs
3. `TR-002` the egress half of the benchmarks — the VU/JS suites exist, nothing measures samples/s or aggregator duty cycle, so no W3 task can be reviewed
4. `TR-001` print the drop counter unconditionally — one gate change; today a clean run cannot distinguish "no drops" from "counter not wired"
5. `TR-202` `http_req_duration` formula — **confirmed still wall-clock at `runner.rs:631`**; the highest-impact single line in the parity doc
6. `TR-012` the pause-gate lost wakeup — confirmed at `vu_loop.rs:100-107`, and a parked VU is a silent throughput loss
7. `TR-005` + `TR-006` + `TR-007` the input adapters — both are confirmed broken *and* CLI-unreachable while being browser-reachable
8. `TR-201` re-scoped to the k6 **root** options — the scenario half already landed
9. `TR-014` h2 correctness — h2 is on by default and is currently a ~100× regression
10. `TR-301` a slow output starves the aggregator, which blocks the VUs

## One change closes many

Where the register documents a single change that closes a list, the list ships in one PR and the body enumerates it. The highest-leverage:

| Do this | Closes |
|---|---|
| `TR-001` per-output drop counter | Makes **every** throughput fix verifiable. Do it first |
| `TR-130` `stringify_tag_map_into` on both http paths | `check()` tags · custom-metric tags · the batch whole-map drop · the single-path filter |
| `TR-112` dispatch on `self.metric_type`, add the `value`/`last` arm | `avg > max` · `absorb_snapshot` · reserved-name collisions · Trend vacuous pass · `vus:['value>10']` |
| `TR-014` thread `Response::version()` through | the `protocol` tag · the `res.proto` lie · protocol on failed requests (beats k6) |
| `TR-303` `Vec<reqwest::Client>` connection lanes | h2 single-conn cap · frame-demux serialization · per-conn server limits · multi-IP spread |
| `TR-308` skip `responses` in `paths` resolution | 4.3× of the OpenAPI `$ref` fanout, free |
| `TR-310` hoist `rebuild_merged` out of the absorb loop | 25 s CPU + 30 GB churn at 50 agents, in 2 lines |
| `TR-314` two wasmtime dials | ~2–2.6× on all guest code |
| `TR-502` async host calls | the 4 096-thread cap · idle per-VU runtimes · ~200 k syscalls/s · `Slot::Wrapped` starvation · **and** it unlocks `TR-503` |

## Explicitly not before `0.1.0`

- **Parity breadth** — the ~100 remaining dynamic variables, the full lodash surface, `k6/experimental/*`. Coverage, not correctness.
- **The `scheduler.rs` / `driver.rs` split.** It will bury the diffs that matter and make every fix unreviewable. Same treatment `vu_loop.rs` got — after the release.
- **Dropping below reqwest to `hyper::client::conn::http2`.** That is the moat, and it is post-release.
- **Distributed features** beyond the `run_duration` P0 and `TR-309`.
- **`k6/browser`** — decide it explicitly (`TR-245`), then scope it out.
- **Native-izing `cryptojs`** — already a dispatcher with zero constant tables.
- **`panic = "abort"` on the native profile, and `target-cpu=native`.** Both are corrected claims; see `CONVENTIONS.md`.

## Effort key

**S** ≈ under a day · **M** ≈ 1–3 days · **L** ≈ a week or more. Estimates assume an agent with repo context, not a cold start.
