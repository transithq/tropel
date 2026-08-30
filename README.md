# Tropel 🔥

> *Spanish: "a rushing throng; in droves"*

[![CI](https://github.com/transithq/tropel/actions/workflows/ci.yml/badge.svg)](https://github.com/transithq/tropel/actions/workflows/ci.yml)

**Tropel** is a high-performance, open-source load-testing framework built in
Rust. It runs **Postman collections**, **HAR files**, **OpenAPI specs**, and
**k6 scripts** as load tests — with a native Rust hot path and an embedded
QuickJS engine for script execution.

## Install

Prebuilt binaries for Linux (x86_64 / aarch64, static musl — runs in Alpine,
distroless and scratch), macOS (Intel / Apple Silicon) and Windows are attached
to every [release](https://github.com/transithq/tropel/releases), with
`SHA256SUMS`. Each archive carries `tropel`, `tropel-controller` and
`tropel-agent`.

```bash
# Linux x86_64 — adjust the tag and target as needed
curl -fsSL https://github.com/transithq/tropel/releases/latest/download/tropel-v0.1.0-x86_64-unknown-linux-musl.tar.gz \
  | tar xz
sudo mv tropel-v0.1.0-x86_64-unknown-linux-musl/tropel /usr/local/bin/
tropel --version
```

Or build from source (Rust 1.94+):

```bash
cargo build --release   # binary at ./target/release/tropel
```

## Quick Start

```bash
# Run a Postman collection as a load test
tropel run examples/collections/simple-api.json --vus 10 --duration 30s

# Preview what a run will execute
tropel inspect collection.json

# List the input formats this binary ships
tropel extensions
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
  (`worker.rs:339`), raised from 4 096. **`sleep` and `http.*` both park the
  calling thread**, so in-flight concurrency is bounded by threads, not tasks —
  the fiber model that removes that bound is TR-502's remaining half.
  Kubernetes `pids.max` and Docker `--pids-limit` cap this further, and the
  summary reports the effective number.

  This README previously claimed `sleep` was a Promise driven by
  `tokio::time::sleep` and so "no longer freezes co-located VUs". That was
  wrong in a way worth stating plainly: the async host function was registered
  on a runtime with no spawner, so calling `sleep` **panicked** on every
  declarative format. It is a blocking sleep with an absolute deadline, and the
  claim has been withdrawn.
- **203–385 KB of QuickJS heap per VU before a line of user script runs,
  depending on the input format** ✅MEAS — `malloc_size` from
  `JS_ComputeMemoryUsage`, release build, Apple M2 / macOS 26.6, rquickjs
  0.12.2, amortised over 25 real VU contexts sharing one worker-thread
  `Runtime`. Reproduce with
  `cargo test -p tropel-engine --release per_vu_heap_by_format -- --nocapture --ignored`.
  TR-501 picks the shim bundle from the resolving adapter's format:

  | input format | B/VU | at 10 000 VUs |
  |---|---|---|
  | `har` / `openapi` / `http` / `insomnia` | 203,400 | 2.03 GB |
  | content-gated http-only (no format) | 261,878 | 2.62 GB |
  | `postman` (script uses lodash + CryptoJS) | 369,424 | 3.69 GB |
  | `k6` / full bundle (unknown format) | 385,324 | 3.85 GB |

  Expect ~0.2 % run-to-run drift; quote the figure the command prints, not one
  from this table.

  Shim *gating* used to make this WORSE, not better: the compiled-bytecode
  cache was keyed on nothing, so only the full default bundle could use it and
  every narrowed bundle paid a per-VU source parse+compile. The cache is now
  keyed by bundle identity, so narrowing is finally a saving. User-script bytes
  are shared across VUs via `Arc` (that one is fixed — it was a per-VU deep
  clone), and TR-503's shared per-worker-thread `Runtime` **is now
  implemented** — it is what the figures above are measured under.

  Two prior versions of this table were wrong and are withdrawn. The
  `497,584`-class numbers were produced by a harness that summed a **shared**
  runtime's heap once per context and divided by N — every read returns the
  same runtime-scoped value, so it reported the whole 25-context total as a
  per-VU figure. The earlier `57 KB/VU, −92.3 %` claim was never measured at
  all. Neither should be cited.

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
