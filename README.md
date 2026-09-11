<div align="center">

# Tropel 🔥

**Load-test with the collections you already have.**

<sub>*Spanish: "a rushing throng; in droves"*</sub>

[![CI](https://github.com/transithq/tropel/actions/workflows/ci.yml/badge.svg)](https://github.com/transithq/tropel/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tropel.svg?logo=rust)](https://crates.io/crates/tropel)
[![docs.rs](https://img.shields.io/docsrs/tropel-sdk?logo=docsdotrs&label=sdk%20docs)](https://docs.rs/tropel-sdk)
[![rust](https://img.shields.io/badge/rust-1.94%2B-orange.svg?logo=rust)](https://www.rust-lang.org)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)

</div>

---

A high-performance, open-source load-testing framework in Rust. Point it at a
Postman collection, a HAR capture, an OpenAPI spec or a k6 script and it runs
as a load test — native Rust on the hot path, embedded QuickJS for scripting.

```bash
tropel run collection.json --vus 500 --duration 2m
```

> [!NOTE]
> **Pre-1.0: the API is unstable.** k6 parity is actively expanding and the
> `tropel-sdk` surface still changes between minor versions. Pin an exact
> version if you depend on it.

## Install

Linux builds are **static musl** — they run unmodified in Alpine, distroless
and scratch containers. Every archive carries `tropel`, `tropel-controller`
and `tropel-agent`, with `SHA256SUMS` alongside.

```bash
# Linux x86_64  (swap the target for aarch64-unknown-linux-musl,
#                x86_64-apple-darwin or aarch64-apple-darwin)
curl -fsSL https://github.com/transithq/tropel/releases/download/v0.6.0/tropel-v0.6.0-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo install -m755 tropel-v0.6.0-x86_64-unknown-linux-musl/tropel /usr/local/bin/
tropel --version
```

<details>
<summary><b>Windows, and building from source</b></summary>

```powershell
Invoke-WebRequest -Uri "https://github.com/transithq/tropel/releases/download/v0.6.0/tropel-v0.6.0-x86_64-pc-windows-msvc.zip" -OutFile tropel.zip
Expand-Archive tropel.zip -DestinationPath .
```

```bash
cargo build --release   # Rust 1.94+, binary at ./target/release/tropel
```

</details>

## Quick start

```bash
tropel run examples/collections/simple-api.json --vus 10 --duration 30s
tropel inspect collection.json      # preview what a run will execute
tropel extensions                   # list the formats this binary ships
```

## Input formats

The point of tropel: the artifact your team already maintains *is* the load
test. No rewrite into a bespoke DSL.

| Format | Reads |
| :--- | :--- |
| **Postman** | v2.0 / v2.1 collections, with `pm.*` scripting |
| **k6** | JS/TS scripts — `http.*`, `check()`, `group()`, exported `options` |
| **HAR** | HTTP Archive captures |
| **OpenAPI** | 3.x specifications |
| **Bruno** | `.bru` collection exports |
| **Insomnia** | v4 collection exports |
| **`.http` / `.rest`** | VS Code REST Client and JetBrains files |
| **KnockPort** | collection directories (`knockport.yaml` + `requests/**`) |
| **Subprocess** | any external command that emits scenario JSON |
| **WASM plugins** | third-party formats, sandboxed, no recompile |

## What it does

- **Seven executors** — constant-vus, ramping-vus, shared / per-vu iterations,
  constant & ramping arrival rate, and externally-controlled via a live control
  API, with graceful stop/ramp-down, think time and pacing.
- **Postman `pm.*` scripting** — `pm.test`, `pm.expect`, `pm.response`,
  `pm.variables` / `pm.environment`, `pm.iterationData`,
  `pm.execution.setNextRequest`, `pm.sendRequest`, custom metrics.
- **HDR-histogram metrics** — p50/p90/p95/p99, sub-timings, tag-scoped
  aggregation, thresholds with k6-compatible abort semantics.
- **Streaming outputs** — NDJSON, StatsD, InfluxDB, Prometheus, OTLP, plus
  stdout / JSON / CSV reporters.
- **Thread-per-core VUs** — each VU owns its QuickJS context and HTTP client on
  its own OS thread; lock-free metrics hot path.
- **Auth built in** — Bearer, Basic, ApiKey, OAuth1, OAuth2, SigV4, Hawk,
  Digest, EdgeGrid.
- **Extensible** — `tropel-sdk` + `tropel build --with <crate>` for native
  extensions; gRPC and WebSocket ship as protocol extensions.
- **Distributed** — k6-style execution segments across multiple nodes.

## Documentation

| | | |
| :--- | :--- | :--- |
| [Getting started](docs/getting-started.md) | [CLI reference](docs/cli.md) | [Executors](docs/executors.md) |
| [Scripting](docs/scripting.md) | [Metrics](docs/metrics.md) | [Outputs](docs/outputs.md) |
| [Input formats](docs/inputs.md) | [Extensions](docs/extensions.md) | [Distributed](docs/distributed.md) |
| [`trp` API](docs/trp-api.md) | [Roadmap](docs/roadmap.md) | |

## Status

Actively developed. Most load-testing fundamentals are shipped and tested; a
few areas remain partial. **[docs/roadmap.md](docs/roadmap.md) carries the
honest per-area capability matrix** — this README links there rather than
repeating claims.

Limitations worth knowing before you pick it:

- **10,000 VUs, one OS thread per VU.** `sleep` and `http.*` both park the
  calling thread, so in-flight concurrency is bounded by threads, not tasks.
  Kubernetes `pids.max` and Docker `--pids-limit` cap it further; the run
  summary reports the effective number.
- **203–385 KB of QuickJS heap per VU** before a line of user script runs,
  depending on input format — 2.0–3.9 GB at 10,000 VUs.
- **WASM drivers cover a focused surface** — plugins run iterations and use
  host-imported http/sleep/metrics, but the API is a subset of the in-process
  k6 driver's.
- **JMeter and Locust adapters are not started.**

<details>
<summary><b>Per-VU heap, measured — and two withdrawn claims</b></summary>

`malloc_size` from `JS_ComputeMemoryUsage`, release build, Apple M2 / macOS
26.6, rquickjs 0.12.2, amortised over 25 real VU contexts sharing one
worker-thread `Runtime`. Reproduce with:

```bash
cargo test -p tropel-engine --release per_vu_heap_by_format -- --nocapture --ignored
```

| input format | B/VU | at 10,000 VUs |
| :--- | ---: | ---: |
| `har` / `openapi` / `http` / `insomnia` | 203,400 | 2.03 GB |
| content-gated http-only (no format) | 261,878 | 2.62 GB |
| `postman` (script uses lodash + CryptoJS) | 369,424 | 3.69 GB |
| `k6` / full bundle (unknown format) | 385,324 | 3.85 GB |

Expect ~0.2 % run-to-run drift; quote the figure the command prints, not one
from this table.

Shim *gating* used to make this worse, not better: the compiled-bytecode cache
was keyed on nothing, so only the full default bundle could use it and every
narrowed bundle paid a per-VU source parse+compile. The cache is now keyed by
bundle identity. User-script bytes are shared across VUs via `Arc`, and
TR-503's shared per-worker-thread `Runtime` is implemented — it is what the
figures above are measured under.

**Withdrawn.** This README once claimed `sleep` was a Promise driven by
`tokio::time::sleep` and so "no longer freezes co-located VUs". That was wrong:
the async host function was registered on a runtime with no spawner, so `sleep`
**panicked** on every declarative format. It is a blocking sleep with an
absolute deadline.

**Withdrawn.** Two earlier versions of the heap table. The `497,584`-class
numbers came from a harness that summed a *shared* runtime's heap once per
context and divided by N — every read returns the same runtime-scoped value, so
it reported the whole 25-context total as a per-VU figure. The earlier
`57 KB/VU, −92.3 %` claim was never measured at all. Neither should be cited.

</details>

## Architecture

```
        ┌──────────────┐
        │ tropel (CLI) │
        └──────┬───────┘
               │
       ┌───────▼────────┐
       │  tropel-engine │  orchestration
       └─┬──────┬─────┬─┘
         │      │     │
 ┌───────▼──┐ ┌─▼───┐ ┌▼──────────┐
 │  inputs  │ │sched│ │  outputs  │
 │ +drivers │ │uler │ │ stdout /  │
 └────┬─────┘ └──┬──┘ │ json/csv  │
      │          │    │ +streams  │
      │          │    └───────────┘
 ┌────▼──────────▼─┐
 │    protocols     │  HTTP + gRPC + WebSocket
 └────────┬─────────┘
          │
 ┌────────▼─────────┐
 │ tropel-runtime   │  QuickJS per VU, thread-local
 │  + native bridge │  crypto / hash / encode / assert / json
 └──────────────────┘
```

<details>
<summary><b>Crates</b></summary>

Published to crates.io at `0.6.0`. Depend on **`tropel-runtime`** for the
scripting stack — `tropel-js`, `tropel-native`, `tropel-sandbox` and
`tropel-variables` are internal to it and carry no stability guarantee.

| Crate | |
| :--- | :--- |
| `tropel` | the CLI binary |
| `tropel-engine` | scenario execution, VU scheduling, protocol registry |
| `tropel-core` | job configuration, scenario resolution, shared types |
| `tropel-collection` | the folder tree and per-layer model inputs share |
| `tropel-scheduler` | executor types — constant-vus, ramping-arrival-rate, … |
| `tropel-http` | reqwest client, connection pooling, sub-timings |
| `tropel-auth` | request signers (Bearer … SigV4, Hawk, Digest, EdgeGrid) |
| `tropel-metrics` | sharded aggregation, HDR histograms, thresholds |
| `tropel-report` | summary tables, threshold verdicts, streaming outputs |
| `tropel-runtime` | the scripting runtime — **the public scripting entry point** |
| `tropel-es` | TypeScript transpilation and ES module bundling |
| `tropel-sdk` | stable public contract for extension authors |
| `tropel-ext` | inventory-backed adapter/driver discovery |
| `tropel-wasm` | WASM plugin runtime (wasmtime, fuel, AOT, pooling) |
| `tropel-build` | custom binary builder, bundled asset embedding |
| `tropel-distributed` | controller/agent protocol, execution segments |
| `tropel-bench` | Criterion suite — bootstrap and per-iteration overhead |
| `tropel-input-*` | postman, k6, har, openapi, bru, insomnia, http, knockport, subprocess |
| `tropel-x-*` | grpc, websocket, prometheus |
| `tropel-web`, `tropel-core-wasm`, `tropel-input-wasm` | browser slices for embedders |

</details>

## Development

```bash
cargo fmt
cargo clippy --workspace -- -D warnings
cargo test --workspace
bash scripts/include-paths-in-crate.sh   # no include! may escape its crate

cargo bench -p tropel-bench --bench perf                 # release profile
cargo bench -p tropel-bench --bench perf --profile dev   # fast, disk-light
```

## License

[Apache License 2.0](LICENSE-APACHE).
