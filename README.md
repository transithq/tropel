# Tropel 🔥

> *Spanish: "a rushing throng; in droves"*

[![CI](https://github.com/transithq/tropel/actions/workflows/ci.yml/badge.svg)](https://github.com/transithq/tropel/actions/workflows/ci.yml)

**Tropel** is a high-performance, open-source load-testing framework built in
Rust. It runs **Postman collections**, **HAR files**, **OpenAPI specs**, and
**k6 scripts** as load tests — with a native Rust hot path and an embedded
QuickJS engine for script execution.

## Quick Start

```bash
# Build Tropel
cargo build --release

# Run a Postman collection as a load test
./target/release/tropel run examples/collections/simple-api.json \
  --vus 10 \
  --duration 30s

# Preview what a run will execute
./target/release/tropel inspect collection.json

# List the input formats this binary ships
./target/release/tropel extensions
```

Full docs: **[docs/](docs/README.md)** — CLI reference, executors, scripting,
metrics, extensions, distributed execution.

## What Tropel does

- **Seven executors** — constant-vus, ramping-vus, shared/per-vu iterations,
  constant & ramping arrival rate, and externally-controlled (live control
  API), with graceful stop/ramp-down, think time, and pacing.
- **Postman `pm.*` scripting** — `pm.test`, `pm.expect`, `pm.response`,
  `pm.variables`/`pm.environment`, `pm.iterationData`,
  `pm.execution.setNextRequest`, `pm.sendRequest`, custom metrics.
- **k6-style scripting** — JS/TS scripts with `http.*`, `check()`, `group()`,
  `sleep()`, `Counter/Gauge/Rate/Trend`; exported `options` are honored.
- **HDR-histogram metrics** — p50/p90/p95/p99, sub-timings (TTFB etc.),
  tag-scoped aggregation, thresholds with k6-compatible abort semantics.
- **Streaming outputs** — NDJSON, StatsD, InfluxDB, Prometheus, OTLP, plus
  stdout/JSON/CSV reporters.
- **Thread-per-core VUs** — each VU owns its QuickJS context and HTTP client
  on its own OS thread; lock-free metrics hot path.
- **Extensible** — `tropel-sdk` + `tropel build --with <crate>` for native
  extensions, and a sandboxed **WASM plugin** tier (`--plugins-dir`) for
  third-party input formats without recompiling.
- **Distributed** — k6-style execution segments for multi-node runs.

## Status

Tropel is **actively developed**. Most load-testing fundamentals are shipped
and tested; a few areas remain partial. **See
[docs/roadmap.md](docs/roadmap.md) for the honest per-area capability matrix**
— this README intentionally links there instead of repeating claims.

Notable limitations today:

- **WASM drivers cover a focused surface** — plugins can run iterations and
  use host-imported http/sleep/metrics, but the API is a subset of the
  in-process k6 driver's (no full scripting runtime inside the module).
- **JMeter and Locust adapters are not started** (planned §11.6).
- **10,000 VU concurrency, one OS thread per VU** — `MAX_WORKERS = 10_000`
  (`worker.rs:339`), raised from 4 096. `sleep` yields rather than parking its
  thread (`__tropel_native_sleep` is a Promise driven by `tokio::time::sleep`
  with job-queue pumping, `js_bootstrap.rs:348`), so co-located VUs no longer
  freeze each other. **`http.*` still parks the calling thread** via
  `execute_blocking`, so in-flight concurrency is still bounded by threads, not
  by tasks — the fiber model that removes that bound is TR-502's remaining
  half. Kubernetes `pids.max` and Docker `--pids-limit` cap this further, and
  the summary reports the effective number.
- **~486 KB of QuickJS heap per VU before a line of user script runs**
  ✅MEAS — `malloc_size` from `JS_ComputeMemoryUsage`, release build, Apple
  Silicon (M-series), rquickjs 0.12.2. Reproduce with
  `cargo test -p tropel-engine --release measure_per_vu_quickjs_heap -- --nocapture --ignored`.
  Breakdown: a bare context is 104,768 B; the full shim bundle takes it to
  497,584 B. At 10 000 VUs that is **~4.6 GB of QuickJS heap alone**.
  User-script bytes are shared across VUs via `Arc` (that one is fixed — it was
  a per-VU deep clone). Sharing a `Runtime` per worker thread and aliasing
  template globals — the change that would make this number small — is
  **designed but not implemented**; see TR-503. Do not quote a per-VU figure
  that this command does not print.

## Architecture

```
                    ┌─────────────────────┐
                    │   tropel (CLI)       │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │   tropel-engine      │
                    │   Orchestration      │
                    └──┬───────┬──────┬───┘
                       │       │      │
              ┌────────▼──┐ ┌──▼───┐ ┌▼─────────┐
              │  Input    │ │Exec. │ │  Output  │
              │  Adapters │ │Sched.│ │  stdout/  │
              │  +Drivers │ └──┬───┘ │  json/csv │
              └─────┬─────┘   │     │   +streams│
                    │         │     └───────────┘
        ┌───────────▼───┐ ┌───▼───────────┐
        │ Postman/HAR/  │ │  Protocol     │
        │ OpenAPI/k6/   │ │  HTTP + gRPC +│
        │ WASM/subproc  │ │  WebSocket    │
        └───────────────┘ └──────┬───────┘
                                 │
                    ┌────────────▼───────────┐
                    │  tropel-js (QuickJS)    │
                    │  per-VU, thread-local   │
                    └────────────┬───────────┘
                                 │
                    ┌────────────▼───────────┐
                    │  tropel-native bridge   │
                    │  crypto/hash/encode/    │
                    │  assert/json/http/sleep │
                    └─────────────────────────┘
```

## Crates

```
crates/
├── tropel/               CLI binary
├── tropel-engine/        Orchestration facade (run/inspect/archive/build)
├── tropel-core/          Shared domain types, config, execution segments
├── tropel-collection/    Postman collection model + parser
├── tropel-variables/     {{var}} / {{$dynamic}} resolution
├── tropel-executor/      VU scheduler + runner (thread-per-core)
├── tropel-http/          HTTP client, auth (Bearer/Basic/ApiKey/OAuth1/OAuth2/
│                         SigV4/Hawk/Digest), sub-timings
├── tropel-js/            QuickJS host, compile-once scripts
├── tropel-native/        Native bridge (crypto, hash, encoding, assert, JSON)
├── tropel-pm/            pm.* bridge + k6 shim glue
├── tropel-metrics/       HDR histograms, first-class metric types, thresholds
├── tropel-report/        Reporters + streaming outputs
├── tropel-es/            ESM/TypeScript transpiler
├── tropel-sdk/           Stable public adapter/driver contract
├── tropel-ext/           Registry, traits (InputAdapter, Driver, Protocol, Output)
├── tropel-wasm/          WASM plugin runtime (wasmtime, fuel, AOT, pooling)
├── tropel-build/         Custom binary builder
├── tropel-distributed/   Controller/agent, execution segments
├── tropel-bench/         Criterion benchmark suite
├── inputs/               tropel-input-{postman,har,openapi,k6,subprocess}
└── extensions/           tropel-x-{grpc,websocket,prometheus}
js/                       Vendored shims (pm-api, chai, lodash, cryptojs-shim, k6-shim)
```

## Development

```bash
cargo fmt
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release

# Benchmarks (see crates/tropel-bench)
cargo bench -p tropel-bench --bench perf          # release profile
cargo bench -p tropel-bench --bench perf --profile dev  # fast, disk-light
```

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)).
