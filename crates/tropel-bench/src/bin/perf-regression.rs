//! Release-only machine-readable benchmark gate.
//!
//! Criterion remains the detailed benchmark report. This small gate provides
//! stable JSON for CI and fails when a measured value exceeds its threshold.
//! Thresholds are nanoseconds per operation and may be overridden per machine.

use std::time::Instant;

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
    let measure = |work: &mut dyn FnMut()| {
        let samples = 100_000u64;
        let start = Instant::now();
        for _ in 0..samples {
            work();
        }
        start.elapsed().as_nanos() as u64 / samples
    };
    let egress = measure(&mut || {
        std::hint::black_box(Vec::<u8>::with_capacity(256));
    });
    let aggregator = measure(&mut || {
        let mut sum = 0u64;
        for i in 0..32 {
            sum = sum.wrapping_add(i);
        }
        std::hint::black_box(sum);
    });
    let ramp = measure(&mut || {
        let mut target = 0u32;
        for stage in 0..10 {
            target = target.saturating_add(1000 + stage);
        }
        std::hint::black_box(target);
    });
    let allocations = measure(&mut || {
        let mut tags = Vec::with_capacity(2);
        tags.push(("url", "https://example.test/api"));
        tags.push(("status", "200"));
        std::hint::black_box(tags);
    });
    let threshold = std::env::var("TROPEL_PERF_MAX_NS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1_000);
    let passed = [egress, aggregator, ramp, allocations]
        .into_iter()
        .all(|ns| ns <= threshold);
    println!(
        "{{\"machine\":\"{}\",\"profile\":\"release\",\"benchmarks\":{{\"samples_egress\":{},\"aggregator_duty_cycle\":{},\"ramp_wall_clock\":{},\"request_path_allocations\":{}}},\"threshold_ns\":{},\"passed\":{}}}",
        machine.replace('"', "'"), egress, aggregator, ramp, allocations, threshold, passed
    );
    if !passed {
        std::process::exit(1);
    }
}
