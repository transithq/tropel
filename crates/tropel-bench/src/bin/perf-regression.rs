//! Release-only machine-readable benchmark gate.
//!
//! Criterion remains the detailed benchmark report. This small gate provides
//! stable JSON for CI and fails when a measured value exceeds its threshold.
//! Thresholds are nanoseconds per operation and may be overridden per machine.

use std::time::Instant;

/// TR-501: per-VU QuickJS heap, measured on a context that actually carries
/// the shims a VU carries.
///
/// This used to create a **bare** `JsContext::new(None, None)` — no shims —
/// and measure RSS delta against the 900 KB budget. Its own comment said so:
/// *"Use bare JsContext for budget — shims add ~734k, bare is smaller."* The
/// gate therefore could not fail for the thing TR-501 exists to guard: shim
/// loading is the whole budget. It also returned `None` on macOS, so the
/// check silently passed there.
///
/// Now it measures QuickJS's own accounting (`JS_ComputeMemoryUsage` via
/// `quickjs_heap_bytes`) on a context built through the real
/// `create_vu_js_context` path, which is what a VU gets. That number is
/// available on every platform and is not polluted by allocator arenas or by
/// whatever else the process is doing, so the budget means the same thing
/// everywhere.
fn memory_per_vu_bytes() -> Option<u64> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async { tropel_engine::bench_support::vu_context_heap_bytes().await })
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("perf-regression must run with --release");
        std::process::exit(2);
    }
    let machine = std::env::var("CI_RUNNER_OS")
        .or_else(|_| std::env::var("OS"))
        .unwrap_or_else(|_| std::env::consts::OS.to_string());
    let machine = format!(
        "{}-{}-{}c",
        machine,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    // Each gate drives PRODUCTION code. The four that used to be here did not:
    // `egress` allocated a `Vec::with_capacity(256)`, `aggregator` summed
    // 0..32, `ramp` added integers, and `allocations` built a two-element vec
    // of string literals. All four were compared against one 1000 ns threshold
    // they passed trivially, so the "release performance regression gate"
    // gated nothing at all (TR-002).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");

    // Egress: nanoseconds per sample through the real MetricsCollector. The
    // 100 k samples/s budget is 10 000 ns/sample; the gate is set well inside
    // that so a regression is caught before the budget is actually breached.
    let egress = {
        use std::sync::Arc;
        use tropel_metrics::collector::MetricsCollector;
        use tropel_sdk::types::{Sample, SampleType, TagMap};
        const N: u64 = 50_000;
        let collector = MetricsCollector::new();
        let tags: Arc<TagMap> = Arc::new(TagMap::new());
        let start = Instant::now();
        rt.block_on(async {
            for i in 0..N {
                collector
                    .record(&Sample {
                        metric: format!("http_req_duration_{}", i % 100).into(),
                        value: i as f64 * 0.1,
                        tags: tags.clone(),
                        timestamp: std::time::SystemTime::now(),
                        sample_type: SampleType::Trend,
                    })
                    .await;
            }
        });
        start.elapsed().as_nanos() as u64 / N
    };

    // Request path: nanoseconds to build the twelve-sample set one HTTP hop
    // emits, over one shared Arc<TagMap> of seven tags (TR-312).
    let request_path = {
        use std::sync::Arc;
        use tropel_sdk::types::{Sample, SampleType, TagMap};
        const N: u64 = 100_000;
        const HOP_METRICS: [&str; 12] = [
            "http_req_duration",
            "http_reqs",
            "http_req_failed",
            "http_req_blocked",
            "http_req_dns",
            "http_req_connecting",
            "http_req_tls_handshaking",
            "http_req_sending",
            "http_req_waiting",
            "http_req_receiving",
            "data_sent",
            "data_received",
        ];
        let start = Instant::now();
        for _ in 0..N {
            let mut tags = TagMap::with_capacity(7);
            tags.insert("url", "https://example.test/api/resource/42");
            tags.insert("method", "GET");
            tags.insert("status", "200");
            tags.insert("name", "getResource");
            tags.insert("group", "::checkout");
            tags.insert("scenario", "default");
            tags.insert("expected_response", "true");
            let tags = Arc::new(tags);
            let now = std::time::SystemTime::now();
            let mut samples = Vec::with_capacity(HOP_METRICS.len());
            for name in HOP_METRICS {
                samples.push(Sample {
                    metric: name.into(),
                    value: 12.5,
                    tags: tags.clone(),
                    timestamp: now,
                    sample_type: SampleType::Trend,
                });
            }
            std::hint::black_box(&samples);
        }
        start.elapsed().as_nanos() as u64 / N
    };

    // Per-gate thresholds. One shared 1000 ns number across four unrelated
    // measurements was how the old gate passed everything.
    let ns_threshold = |name: &str, default: u64| -> u64 {
        std::env::var(format!("TROPEL_PERF_MAX_NS_{name}"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    let egress_budget = ns_threshold("EGRESS", 4_000);
    let request_path_budget = ns_threshold("REQUEST_PATH", 4_000);
    let passed_ns = egress <= egress_budget && request_path <= request_path_budget;

    // TR-501: per-VU QuickJS heap budget, measured on a REAL VU context.
    // Budget 900 KB, above the measured ~486 KB with headroom for a shim to
    // grow, tight enough to catch a regression. Unlike the previous RSS-based
    // check, an unmeasurable value now FAILS rather than passing: "we could
    // not measure it" and "it is within budget" must not look the same.
    let memory_per_vu = memory_per_vu_bytes();
    let memory_budget: u64 = 900 * 1024;
    let memory_passed = memory_per_vu.is_some_and(|v| v <= memory_budget);
    let passed = passed_ns && memory_passed;
    println!(
        "{{\"machine\":\"{}\",\"profile\":\"release\",\"benchmarks\":{{\"samples_egress_ns\":{},\"samples_egress_budget_ns\":{},\"request_path_ns\":{},\"request_path_budget_ns\":{},\"memory_per_vu\":{},\"memory_budget\":{}}},\"passed\":{}}}",
        machine.replace('"', "'"),
        egress,
        egress_budget,
        request_path,
        request_path_budget,
        memory_per_vu.unwrap_or(0),
        memory_budget,
        passed
    );
    if egress > egress_budget {
        eprintln!("egress regression: {egress} ns/sample > budget {egress_budget} ns");
    }
    if request_path > request_path_budget {
        eprintln!(
            "request-path regression: {request_path} ns/hop > budget {request_path_budget} ns"
        );
    }
    if !memory_passed {
        match memory_per_vu {
            Some(v) => eprintln!(
                "TR-501 memory budget failed: per-VU QuickJS heap {v} B > budget {memory_budget} B (900 KB)"
            ),
            None => eprintln!(
                "TR-501 memory budget failed: per-VU QuickJS heap could not be measured. \
                 This gate fails closed — an unmeasurable budget that reports PASS is how \
                 the shim-loading path went ungated in the first place."
            ),
        }
    }
    if !passed {
        std::process::exit(1);
    }
}
