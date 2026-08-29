//! TR-315 — soak harness: does memory stay flat under sustained load?
//!
//! # Why this exists
//!
//! TR-315's acceptance criterion read *"A 24 h soak benchmark, run in CI
//! weekly, asserting flat memory"*, and was closed with a note citing an
//! `#[ignore] soak` test, a weekly `ci.yml` `soak` job, and an RSS assertion.
//! **None of the three existed.** `grep -rn '#\[ignore\]' crates/` found
//! nothing, `ci.yml` had no `soak` job, and the artifact was a criterion
//! bench (`soak_memory`) that inserted and removed 10 000 HashMap entries.
//!
//! criterion is the wrong tool regardless: it cannot assert, and a soak is a
//! trend over hours, not a per-iteration timing.
//!
//! # What it measures
//!
//! Sustained load through the real `MetricsCollector` — the component that
//! actually retains state across a run — sampling process RSS on a fixed
//! interval, then fitting a least-squares line to the samples.
//!
//! The **slope** is the signal, not the endpoints. A leak is a persistent
//! upward trend; a single high reading is allocator noise or a GC that has
//! not run. Comparing first-to-last (which is what a naive soak does) is
//! dominated by both.
//!
//! Warm-up samples are discarded: the first seconds include lazily-built
//! caches and allocator arena growth, which are one-off and not a leak.
//!
//! # Configuration
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `TROPEL_SOAK_SECS` | 60 | total run duration |
//! | `TROPEL_SOAK_SAMPLES_PER_SEC` | 20000 | offered load |
//! | `TROPEL_SOAK_SERIES` | 500 | distinct tag-sets (cardinality) |
//! | `TROPEL_SOAK_MAX_GROWTH_BYTES_PER_MIN` | 2097152 | failing slope (2 MiB/min) |
//! | `TROPEL_SOAK_INJECT_LEAK` | 0 | `1` = deliberately leak, for the negative control |
//!
//! # It can fail
//!
//! `TROPEL_SOAK_INJECT_LEAK=1` retains every sample in a growing `Vec`, so the
//! detector must report a leak. That is the negative control: a soak test that
//! has never been shown to fail is decoration, which is precisely how the
//! previous one passed while measuring nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tropel_metrics::collector::MetricsCollector;
use tropel_sdk::types::{Sample, SampleType, TagMap};

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn process_rss_bytes() -> Option<u64> {
    use libc::{
        mach_task_basic_info, task_info, KERN_SUCCESS, MACH_TASK_BASIC_INFO,
        MACH_TASK_BASIC_INFO_COUNT,
    };
    use mach2::traps::mach_task_self;
    let mut info: mach_task_basic_info = unsafe { std::mem::zeroed() };
    let mut count = MACH_TASK_BASIC_INFO_COUNT;
    let kr = unsafe {
        task_info(
            mach_task_self() as libc::mach_port_t,
            MACH_TASK_BASIC_INFO,
            &mut info as *mut mach_task_basic_info as *mut libc::integer_t,
            &mut count,
        )
    };
    (kr == KERN_SUCCESS).then_some(info.resident_size)
}

#[cfg(windows)]
fn process_rss_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    (ok != 0).then_some(pmc.WorkingSetSize as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_rss_bytes() -> Option<u64> {
    None
}

/// Least-squares slope of `(seconds, bytes)` in bytes per second.
///
/// The slope is what distinguishes a leak from noise: a leak is a persistent
/// trend, and endpoints alone cannot tell the two apart.
fn slope_bytes_per_sec(samples: &[(f64, f64)]) -> f64 {
    let n = samples.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let mean_x = samples.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = samples.iter().map(|(_, y)| y).sum::<f64>() / n;
    let num: f64 = samples
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let den: f64 = samples.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let secs = env_u64("TROPEL_SOAK_SECS", 60);
    let rate = env_u64("TROPEL_SOAK_SAMPLES_PER_SEC", 20_000);
    let series = env_u64("TROPEL_SOAK_SERIES", 500).max(1);
    let max_slope_per_min = env_u64("TROPEL_SOAK_MAX_GROWTH_BYTES_PER_MIN", 2 * 1024 * 1024) as f64;
    let inject_leak = env_u64("TROPEL_SOAK_INJECT_LEAK", 0) == 1;

    if process_rss_bytes().is_none() {
        // Fail closed. "RSS unsupported" and "memory is flat" must not look
        // the same — that equivalence is what let the previous gate pass on
        // macOS while measuring nothing.
        eprintln!(
            "FAIL: RSS is not measurable on this platform, so the soak cannot \
             assert anything. Failing rather than reporting a pass."
        );
        std::process::exit(1);
    }

    eprintln!(
        "soak: {secs}s at {rate} samples/s over {series} series \
         (leak injection: {inject_leak})"
    );

    let collector = MetricsCollector::new();
    // Pre-build the tag sets so per-iteration allocation is the collector's,
    // not the harness's.
    let tag_sets: Vec<Arc<TagMap>> = (0..series)
        .map(|i| {
            let mut t = TagMap::with_capacity(7);
            t.insert("url", format!("https://example.test/api/resource/{i}"));
            t.insert("method", "GET");
            t.insert("status", "200");
            t.insert("name", format!("resource_{i}"));
            t.insert("group", "::soak");
            t.insert("scenario", "default");
            t.insert("expected_response", "true");
            Arc::new(t)
        })
        .collect();

    // The negative control's leak lives here.
    let mut leaked: Vec<Sample> = Vec::new();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    // Discard the first 20% (min 5s): lazily-built caches and allocator arena
    // growth are one-off, not a trend.
    let warmup = Duration::from_secs((secs / 5).max(5).min(secs));
    let mut measurements: Vec<(f64, f64)> = Vec::new();
    let mut next_sample_at = start + Duration::from_secs(1);
    let batch = (rate / 100).max(1);
    let mut emitted: u64 = 0;

    while Instant::now() < deadline {
        for i in 0..batch {
            let tags = tag_sets[(emitted.wrapping_add(i) as usize) % tag_sets.len()].clone();
            let sample = Sample {
                metric: "http_req_duration".into(),
                value: (emitted % 250) as f64 + 0.5,
                tags,
                timestamp: std::time::SystemTime::now(),
                sample_type: SampleType::Trend,
            };
            if inject_leak {
                leaked.push(sample.clone());
            }
            collector.record(&sample).await;
        }
        emitted += batch;

        let now = Instant::now();
        if now >= next_sample_at {
            if now.duration_since(start) >= warmup {
                if let Some(rss) = process_rss_bytes() {
                    measurements.push((now.duration_since(start).as_secs_f64(), rss as f64));
                }
            }
            next_sample_at = now + Duration::from_secs(1);
        }
        // Pace to roughly the offered rate.
        let expected = Duration::from_secs_f64(emitted as f64 / rate as f64);
        let elapsed = Instant::now().duration_since(start);
        if expected > elapsed {
            tokio::time::sleep(expected - elapsed).await;
        }
    }

    std::hint::black_box(&leaked);

    if measurements.len() < 5 {
        eprintln!(
            "FAIL: only {} RSS samples after warm-up — too few to fit a trend. \
             Raise TROPEL_SOAK_SECS.",
            measurements.len()
        );
        std::process::exit(1);
    }

    let slope_per_sec = slope_bytes_per_sec(&measurements);
    let slope_per_min = slope_per_sec * 60.0;
    let first = measurements.first().map(|(_, y)| *y).unwrap_or(0.0);
    let last = measurements.last().map(|(_, y)| *y).unwrap_or(0.0);

    println!(
        "{{\"secs\":{},\"samples_emitted\":{},\"rss_samples\":{},\
         \"rss_first\":{},\"rss_last\":{},\"slope_bytes_per_min\":{:.0},\
         \"budget_bytes_per_min\":{:.0},\"leak_injected\":{},\"passed\":{}}}",
        secs,
        emitted,
        measurements.len(),
        first as u64,
        last as u64,
        slope_per_min,
        max_slope_per_min,
        inject_leak,
        slope_per_min <= max_slope_per_min
    );

    if slope_per_min > max_slope_per_min {
        eprintln!(
            "FAIL: RSS is trending up at {:.0} B/min, budget {:.0} B/min \
             ({} samples over {}s, {} -> {} bytes)",
            slope_per_min,
            max_slope_per_min,
            measurements.len(),
            secs,
            first as u64,
            last as u64
        );
        std::process::exit(1);
    }
    eprintln!("ok: RSS slope {slope_per_min:.0} B/min is within {max_slope_per_min:.0} B/min");
}
