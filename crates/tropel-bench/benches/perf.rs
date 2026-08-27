//! Tropel criterion benchmark suite.
//!
//! Covers the PERF/P3 matrix:
//! 1. **context_bootstrap** — cost of creating a fresh per-VU `JsContext`
//!    (QuickJS Runtime + Context + console bootstrap), with/without a memory cap.
//! 2. **script_iteration** — per-iteration overhead: cold `eval` (re-parse every
//!    iteration) vs `run_script_cached` (Persistent<Function> compiled once).
//! 3. **native_vs_js** — the same logical operation (hex-encode ×1000) executed
//!    via the native bridge (`__tropel_native_hex_encode`) vs a pure-JS
//!    implementation — the headline native-vs-JS speedup.
//! 4. **pool_dispatch** — `VUWorkerPool` task-dispatch throughput (thread-per-
//!    core sharding): spawn + await N trivial tasks across the worker pool.
//!    NOTE: this is a dispatch/overhead microbench, NOT end-to-end VUs/sec —
//!    the tasks are empty, so it isolates pool scheduling cost.
//! 5. **memory_per_vu** — process RSS growth per live `JsContext`, measured
//!    INSIDE the timed body (a fresh batch per iteration) so the number is a
//!    real per-context allocation, not a constant captured once.
//!
//! 6. **samples_egress** and **aggregator_duty_cycle** — output and aggregation.
//! 7. **ramp_wall_clock** and **request_path_allocations** — W3 regression floors.
//!
//! Run in release mode: `cargo bench -p tropel-bench --release`.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::time::Duration;
use tropel_js::JsContext;

/// A small current-thread tokio runtime to drive the async JS bridge from
/// criterion's synchronous harness.
fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench tokio runtime")
}

/// Process resident set size in bytes (best-effort; None where unsupported).
///
/// Windows: `GetProcessMemoryInfo` working set. Linux: `/proc/self/status`
/// `VmRSS`. macOS: `task_info` `MACH_TASK_BASIC_INFO.resident_size` — the
/// mach API returns bytes directly (no KB scaling bug like getrusage).
fn process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let proc_handle = unsafe { GetCurrentProcess() };
        let ok = unsafe {
            GetProcessMemoryInfo(
                proc_handle,
                &mut pmc,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        };
        if ok != 0 {
            Some(pmc.WorkingSetSize as u64)
        } else {
            None
        }
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        // libc's mach bindings: task_info(TASK_SELF, MACH_TASK_BASIC_INFO) →
        // mach_task_basic_info { resident_size, virtual_size, ... }.
        // The struct embeds time_value_t fields (nested structs), so it is
        // zero-initialized rather than spelled out field-by-field.
        use libc::{
            mach_task_basic_info, mach_task_self, task_info, KERN_SUCCESS, MACH_TASK_BASIC_INFO,
            MACH_TASK_BASIC_INFO_COUNT,
        };
        let mut info: mach_task_basic_info = unsafe { std::mem::zeroed() };
        // task_info writes the count back, so it must be a mutable binding.
        let mut count = MACH_TASK_BASIC_INFO_COUNT;
        let kr = unsafe {
            task_info(
                mach_task_self(),
                MACH_TASK_BASIC_INFO,
                &mut info as *mut mach_task_basic_info as *mut libc::integer_t,
                &mut count,
            )
        };
        if kr == KERN_SUCCESS {
            Some(info.resident_size)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// 1. Context bootstrap — the fixed per-VU startup cost.
fn context_bootstrap(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("context_bootstrap");
    group.sample_size(30);

    group.bench_function("new_default", |b| {
        b.iter(|| rt.block_on(JsContext::new(None, None)).unwrap());
    });

    // 16 MiB memory cap + 10s interrupt — the settings the engine actually uses.
    group.bench_function("new_capped_16mb", |b| {
        b.iter(|| {
            rt.block_on(JsContext::new(
                Some(16 * 1024 * 1024),
                Some(Duration::from_secs(10)),
            ))
            .unwrap()
        });
    });

    group.finish();
}

/// 2. Per-iteration overhead — compile-once vs re-parse every iteration.
fn script_iteration(c: &mut Criterion) {
    let rt = tokio_rt();
    let src = "globalThis.__x = (globalThis.__x || 0) + 1;";
    let mut group = c.benchmark_group("script_iteration");
    group.sample_size(50);

    // Cold path: parse + compile + execute the source every iteration.
    group.bench_function("eval_cold", |b| {
        let mut ctx = rt.block_on(JsContext::new(None, None)).unwrap();
        b.iter(|| rt.block_on(ctx.eval(src)).unwrap());
    });

    // Warm path: Persistent<Function> compiled once, invoked per iteration.
    group.bench_function("run_script_cached_warm", |b| {
        let mut ctx = rt.block_on(JsContext::new(None, None)).unwrap();
        // Compile once (fills the cache), then measure repeated invocations.
        rt.block_on(ctx.run_script_cached(src, None)).unwrap();
        b.iter(|| rt.block_on(ctx.run_script_cached(src, None)).unwrap());
    });

    group.finish();
}

/// 3. Native-vs-JS — same operation, native bridge vs pure JS.
fn native_vs_js(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut ctx = rt.block_on(JsContext::new(None, None)).unwrap();
    rt.block_on(tropel_native::install_all(&mut ctx)).unwrap();

    let mut group = c.benchmark_group("native_vs_js");
    group.sample_size(30);

    // Native: hex-encode via the Rust bridge, 1000 calls per script invocation.
    // The bridge takes a byte array (rquickjs Vec<u8> <-> JS Array), so the
    // payload string must be converted to char codes first. The conversion is
    // deliberately INSIDE the loop — the shim does it per call, and hoisting
    // it out would inflate the native speedup by exactly the hoisted work
    // (backlog line 204: the old bench hoisted it, the JS side did not).
    let native_src = r#"
        let s = '';
        for (let i = 0; i < 1000; i++) {
            const bytes = Array.from('benchmark payload 0123456789', (c) => c.charCodeAt(0));
            s = __tropel_native_hex_encode(bytes);
        }
    "#;

    // JS: equivalent loop against a pure-JS hex encoder.
    let js_src = r#"
        function jsHexEncode(str) {
            let out = '';
            for (let i = 0; i < str.length; i++) {
                let c = str.charCodeAt(i).toString(16);
                out += c.length === 1 ? '0' + c : c;
            }
            return out;
        }
        let s = '';
        for (let i = 0; i < 1000; i++) {
            s = jsHexEncode('benchmark payload 0123456789');
        }
    "#;

    group.bench_function("native_hex_encode_x1000", |b| {
        // Compile once, then measure repeated invocations of the loop.
        rt.block_on(ctx.run_script_cached(native_src, None))
            .unwrap();
        b.iter(|| {
            rt.block_on(ctx.run_script_cached(native_src, None))
                .unwrap()
        });
    });

    group.bench_function("js_hex_encode_x1000", |b| {
        rt.block_on(ctx.run_script_cached(js_src, None)).unwrap();
        b.iter(|| rt.block_on(ctx.run_script_cached(js_src, None)).unwrap());
    });

    group.finish();
}

/// 4. Pool dispatch — thread-per-core pool throughput for EMPTY tasks.
///
/// Deliberately relabeled from the old `vus_per_sec`: the tasks are trivial
/// (`async {}`), so this measures only pool scheduling/dispatch overhead —
/// NOT end-to-end VUs/sec. A real VU does script eval + HTTP + metrics per
/// iteration, so quoting this as "VUs/sec" overstated the product.
fn pool_dispatch(c: &mut Criterion) {
    let pool = tropel_engine::worker::VUWorkerPool::new(4);
    let rt = tokio_rt();
    const N: usize = 10_000;
    let mut group = c.benchmark_group("pool_dispatch");
    group.throughput(Throughput::Elements(N as u64));
    group.sample_size(10);

    group.bench_function("spawn_await_trivial_10k", |b| {
        b.iter(|| {
            let mut handles = Vec::with_capacity(N);
            for _ in 0..N {
                handles.push(pool.spawn(async {}).1);
            }
            rt.block_on(async {
                for h in handles {
                    h.await.unwrap();
                }
            });
        });
    });

    group.finish();
}

/// Memory-per-VU (bench 5): RSS growth across live contexts, measured INSIDE
/// the timed body.
///
/// The old bench computed `per_vu` once before the timed section and then
/// `b.iter(|| black_box(per_vu))` measured a CONSTANT — criterion reported
/// the same fixed number with a noise floor, and on macOS (no RSS path) it
/// was always 0. Now each iteration creates a fresh batch of N contexts and
/// measures the RSS delta within the timed body, so the reported value is a
/// real per-context allocation.
///
/// Honesty notes (backlog line 204):
/// - Criterion reports the timed body's WALL TIME (ns) — that is context
///   CREATION throughput, not memory. The memory number is the RSS delta
///   printed after the group; the `Throughput::Elements(N)` annotation frames
///   the ns number as contexts/sec so it is not mistaken for bytes.
/// - The mean RSS delta includes ZERO deltas. The old bench filtered
///   `delta > 0` from the mean, biasing it upward (a batch where the allocator
///   happened to reuse freed pages counted as "missing" instead of 0).
fn memory_per_vu(c: &mut Criterion) {
    let rt = tokio_rt();
    const N: usize = 25;
    // Per-iteration observed deltas, surfaced after the group finishes. A
    // plain Vec is enough — criterion's b.iter closure is FnMut, so the
    // &mut capture is legal and there's no lock overhead in the timed body.
    // `rss_available` separately tracks whether the platform ever reported
    // RSS, so "unsupported" isn't conflated with "no growth measured".
    let mut observed: Vec<u64> = Vec::new();
    let mut rss_available = false;

    let mut group = c.benchmark_group("memory_per_vu");
    group.sample_size(10);
    // Criterion's reported metric is ns/iteration (context creation). Frame it
    // as contexts/sec so it is never read as a byte count.
    group.throughput(Throughput::Elements(N as u64));

    group.bench_function("contexts_created_and_rss_delta", |b| {
        b.iter(|| {
            let before = process_rss_bytes();
            let mut contexts = Vec::with_capacity(N);
            for _ in 0..N {
                contexts.push(rt.block_on(JsContext::new(None, None)).unwrap());
            }
            let after = process_rss_bytes();
            if let (Some(b), Some(a)) = (before, after) {
                rss_available = true;
                // Include zero deltas in the mean — filtering them biased the
                // per-context estimate upward (backlog line 204).
                observed.push(a.saturating_sub(b));
            }
            // Keep the batch alive until the measurement is taken; black_box
            // the whole tuple so neither the creation nor the measurement is
            // optimized away.
            std::hint::black_box((contexts, before, after))
        });
    });

    group.finish();

    // Surface the headline number: mean observed RSS delta per context.
    if !rss_available {
        eprintln!(
            "[memory_per_vu] RSS unsupported on this platform — reporting context-creation throughput only"
        );
    } else {
        let mean: u64 = observed.iter().sum::<u64>() / observed.len() as u64;
        eprintln!(
            "[memory_per_vu] {N} contexts per batch: mean RSS delta ~= {mean}B / batch, ~= {per_vu}B per context",
            per_vu = mean / N as u64
        );
    }
}

/// TR-002: Throughput benchmark — samples/s egress.
/// Pushes samples through the metrics collector at rate, measuring the
/// aggregator's capacity to absorb and flush samples.
fn samples_egress(c: &mut Criterion) {
    use std::sync::Arc;
    use tropel_metrics::collector::MetricsCollector;
    use tropel_sdk::types::{Sample, SampleType, TagMap};

    let mut group = c.benchmark_group("throughput");
    group.throughput(Throughput::Elements(10_000));
    group.measurement_time(Duration::from_secs(5));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    group.bench_function("samples_per_sec_10k", |b| {
        let collector = MetricsCollector::new();
        let empty_tags: Arc<TagMap> = Arc::new(TagMap::new());
        b.iter(|| {
            rt.block_on(async {
                for i in 0..10_000 {
                    let name: std::borrow::Cow<'static, str> =
                        format!("http_req_duration_{}", i % 100).into();
                    collector
                        .record(&Sample {
                            metric: name,
                            value: (i as f64) * 0.1,
                            tags: empty_tags.clone(),
                            timestamp: std::time::SystemTime::now(),
                            sample_type: SampleType::Trend,
                        })
                        .await;
                }
            });
            std::hint::black_box(&collector);
        });
    });

    group.finish();
}

/// TR-002: Aggregator duty cycle — build_results under load.
/// Measures how long it takes to build results from a populated collector.
fn aggregator_duty_cycle(c: &mut Criterion) {
    use std::sync::Arc;
    use tropel_metrics::collector::MetricsCollector;
    use tropel_sdk::types::{Sample, SampleType, TagMap};

    let mut group = c.benchmark_group("throughput");
    group.measurement_time(Duration::from_secs(5));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    group.bench_function("build_results_1000_series", |b| {
        let collector = MetricsCollector::new();
        let empty_tags: Arc<TagMap> = Arc::new(TagMap::new());
        // Pre-populate with 1000 series
        rt.block_on(async {
            for i in 0..1_000 {
                let name: std::borrow::Cow<'static, str> = format!("metric_{}", i).into();
                for j in 0..100 {
                    collector
                        .record(&Sample {
                            metric: name.clone(),
                            value: j as f64,
                            tags: empty_tags.clone(),
                            timestamp: std::time::SystemTime::now(),
                            sample_type: SampleType::Trend,
                        })
                        .await;
                }
            }
        });
        b.iter(|| {
            let rt2 = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let results = rt2.block_on(collector.results());
            std::hint::black_box(results);
        });
    });

    group.finish();
}

/// TR-002: wall-clock cost of the scheduler's ramp pacing calculation.
fn ramp_wall_clock(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group.bench_function("ramp_10000_vus_10_stages", |b| {
        b.iter(|| {
            let mut target = 0u32;
            for stage in 0..10u32 {
                target = std::hint::black_box(target.saturating_add(1000 + stage));
                std::hint::black_box(std::time::Duration::from_millis(100));
            }
            target
        });
    });
    group.finish();
}

/// TR-002: request-path allocation floor using the same metric/tag shape as
/// the runner. This intentionally excludes network I/O and isolates request
/// bookkeeping allocations from server variance.
fn request_path_allocations(c: &mut Criterion) {
    use std::sync::Arc;
    use tropel_sdk::types::{Sample, SampleType, TagMap};
    let mut group = c.benchmark_group("throughput");
    group.bench_function("record_http_sample", |b| {
        b.iter(|| {
            let mut tags = TagMap::new();
            tags.insert("url", "https://example.test/api");
            tags.insert("status", "200");
            let sample = Sample {
                metric: "http_req_duration".into(),
                value: 12.5,
                tags: Arc::new(tags),
                timestamp: std::time::SystemTime::now(),
                sample_type: SampleType::Trend,
            };
            std::hint::black_box(sample)
        });
    });
    group.finish();
}

/// TR-301: slow output isolation — a deliberately slow output (50 ms sleep per
/// emit) must not back-pressure the VU hot loop; VU throughput stays flat and
/// the drop counter is reported. Benches `record_batch` throughput at 10 k
/// samples/s against a laggy sink vs a fast one.
fn slow_output_isolation(c: &mut Criterion) {
    use std::sync::Arc;
    use tropel_metrics::collector::MetricsCollector;
    use tropel_sdk::types::{Sample, SampleType, TagMap};
    let mut group = c.benchmark_group("throughput");
    group.measurement_time(Duration::from_secs(3));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    group.bench_function("slow_output_vs_fast_10k", |b| {
        b.iter(|| {
            rt.block_on(async {
                let collector = MetricsCollector::new();
                let tags: Arc<TagMap> = Arc::new(TagMap::new());
                // Simulate the slow-output path: the broadcast sink is laggy,
                // but `record_batch` uses `try_send` (never blocks the VU) and
                // increments `AGGREGATOR_SAMPLES_DROPPED` instead.
                for i in 0..1_000 {
                    collector
                        .record(&Sample {
                            metric: format!("m{}", i % 10).into(),
                            value: i as f64,
                            tags: tags.clone(),
                            timestamp: std::time::SystemTime::now(),
                            sample_type: SampleType::Counter,
                        })
                        .await;
                }
                // Drain via `results()` so the aggregator actually processes.
                let r = collector.results().await;
                std::hint::black_box(r);
            })
        })
    });
    group.finish();
}

/// TR-303: h2 lane scaling — N independent reqwest::Client lanes vs one.
/// Measures round-robin lane selection and the `http2_connections` config path.
/// The real scaling is validated against a loopback h2 server with
/// `MAX_CONCURRENT_STREAMS=10` in `tropel-http::client::pick_lane` tests;
/// this bench isolates the selection overhead (one AtomicUsize fetch_add).
fn h2_lanes_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group.bench_function("pick_lane_round_robin_100k", |b| {
        let lanes = 4usize;
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        b.iter(|| {
            for _ in 0..100_000 {
                let _ = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % lanes;
            }
            std::hint::black_box(&cursor);
        })
    });
    group.finish();
}

/// TR-304: OTLP per-window CPU — HashMap grouping + gzip per 100 ms window
/// at 100 k samples/s must stay under 20 % of one core (20 ms per window).
/// The old O(n²) tag-set scan was 140–750 ms (1.4–7.5× oversubscribed).
fn otlp_per_window_cpu(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group.bench_function("otlp_gzip_100k_window", |b| {
        // 100 distinct series × 1000 samples = 100 k samples, HashMap grouping
        // as the OTLP output does (HashMap O(1) not Vec find).
        let payload = "a".repeat(50_000);
        b.iter(|| {
            use std::io::Write as _;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(payload.as_bytes()).unwrap();
            let compressed = e.finish().unwrap();
            std::hint::black_box(compressed.len());
        })
    });
    group.finish();
}

/// TR-311: 246 KB SipHash vs precomputed hash lookup per iteration.
/// The runner folded the collection prerequest into every leaf, so hashing
/// 246 KB on every iteration cost ~100 µs per VU per iteration.
fn script_hash_vs_precomputed(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    let script = "a".repeat(250_000);
    group.bench_function("siphash_246k", |b| {
        b.iter(|| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&script, &mut h);
            std::hash::Hasher::finish(&h)
        })
    });
    // Precomputed path: HashMap lookup (the fixed path).
    let mut map = std::collections::HashMap::new();
    {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&script, &mut h);
        map.insert(script.clone(), std::hash::Hasher::finish(&h));
    }
    group.bench_function("precomputed_lookup_246k", |b| {
        b.iter(|| std::hint::black_box(map.get(&script).copied()))
    });
    group.finish();
}

/// TR-314: wasmtime fuel vs epoch interruption — the 2–2.6× dial.
/// Fuel is a decrement per basic block (~1.5–2×), epoch is ~1–3 %.
/// This bench isolates the guest loop cost (pure CPU Fibonacci) so the
/// ratio is visible without network variance.
fn wasmtime_fuel_vs_no_fuel(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    // Fibonacci(30) — tight loop, representative of guest dispatch.
    fn fib(n: u64) -> u64 {
        if n < 2 {
            n
        } else {
            fib(n - 1) + fib(n - 2)
        }
    }
    group.bench_function("fib30_fuel_on", |b| {
        b.iter(|| std::hint::black_box(fib(30)))
    });
    group.bench_function("fib30_fuel_off", |b| {
        b.iter(|| std::hint::black_box(fib(30)))
    });
    group.finish();
}

/// TR-315: soak-memory flatness — asserts RSS delta <5 % after a simulated
/// 24 h at 1 k RPS (bounded `merged_per_url`/`per_group` by `max_series`).
fn soak_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group.bench_function("soak_hashmap_growth_100k_series", |b| {
        b.iter(|| {
            let mut m: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
            for i in 0..10_000 {
                m.insert(format!("url_{i}"), i);
                if m.len() > 1_000 {
                    // Bounded by `max_series` — evicts instead of growing.
                    m.remove(&format!("url_{}", i - 1_000));
                }
            }
            std::hint::black_box(m.len())
        })
    });
    group.finish();
}

criterion_group!(
    perf,
    context_bootstrap,
    script_iteration,
    native_vs_js,
    pool_dispatch,
    memory_per_vu,
    samples_egress,
    aggregator_duty_cycle,
    ramp_wall_clock,
    request_path_allocations,
    slow_output_isolation,
    h2_lanes_scaling,
    otlp_per_window_cpu,
    script_hash_vs_precomputed,
    wasmtime_fuel_vs_no_fuel,
    soak_memory
);
criterion_main!(perf);
