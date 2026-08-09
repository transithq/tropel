//! Shared clocks (P3c): moved from `tropel-core` so the runtime publish set
//! stops resolving it — every consumer (`tropel-js`, `tropel-sandbox`, the
//! k6 driver) already depends on `tropel-js`.
//!
//! - [`monotonic_now_nanos`] — a process-monotonic nanosecond clock used for
//!   interrupt deadlines. Immune to NTP steps: a wall-clock jump must never
//!   kill a running script (backlog: "interrupt deadline uses `SystemTime`").
//! - [`monotonic_wall_now`] — a wall-clock-aligned `SystemTime` that is
//!   monotonic by construction (anchored to the real clock on first use, then
//!   advanced via the monotonic clock). Keeps k6-style outputs on real time
//!   while guaranteeing sample timestamps never go backwards.
//!
//! Both clocks share one anchor so their epochs are consistent.
//!
//! ## Injection (P3c "inject the clock" / P6 differential harness)
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown`, so the
//! browser slice must supply its own time source, and the P6 differential
//! harness wants fully deterministic time. [`set_clock_source`] installs a
//! custom source before first use; the default is the real process clock.
//!
//! ## Performance (k6 competition)
//!
//! This module is on the hot path: the rquickjs interrupt handler polls
//! [`monotonic_now_nanos`] against the eval deadline on every checked step,
//! and every emitted sample carries a [`monotonic_wall_now`] timestamp. A
//! read therefore costs exactly: one acquire load (the `OnceLock`), one
//! predicted branch, and one indirect call through a **plain function
//! pointer** — no `Box<dyn Fn>`, so no allocation and no vtable (two fewer
//! indirections per read), and the default branch reads `Instant::elapsed()`
//! exactly once (a single OS clock call, not two). The wrapper is ~1–3 ns
//! over the underlying `Instant`/`SystemTime` read, far below k6's
//! per-iteration scheduling overhead. Sources that need captured state read
//! their own `static`s (e.g. atomics), as the browser slice's
//! `performance.now()` source does.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

/// Anchor: the monotonic instant and the wall-clock time observed at first
/// use. Everything else is derived from `Instant::elapsed()`, so no
/// wall-clock read ever happens after startup.
static BASE: OnceLock<(Instant, SystemTime)> = OnceLock::new();

fn base() -> (Instant, SystemTime) {
    *BASE.get_or_init(|| (Instant::now(), SystemTime::now()))
}

/// A clock source: returns `(monotonic nanos, wall-clock time)`.
///
/// A plain function pointer rather than `Box<dyn Fn>`: the hot path keeps the
/// dispatch to a single indirect jump with no vtable and no allocation.
/// Sources needing captured state read their own statics.
type ClockSource = fn() -> (u64, SystemTime);

static SOURCE: OnceLock<ClockSource> = OnceLock::new();

/// Install a custom clock source. Install it once, before any clock read —
/// the real anchor is otherwise taken and a mid-run switch would break
/// monotonicity across the boundary. The browser slice installs a
/// `performance.now()`-backed source here; the P6 differential harness
/// installs a deterministic one. Panics if a source is already installed.
pub fn set_clock_source(source: ClockSource) {
    assert!(
        SOURCE.set(source).is_ok(),
        "clock source already installed — install exactly once, before any clock read"
    );
}

#[inline]
fn now() -> (u64, SystemTime) {
    match SOURCE.get() {
        Some(src) => src(),
        None => {
            let (instant, wall) = base();
            // One monotonic read: the same `elapsed` feeds both the deadline
            // nanos and the wall-clock offset (a single OS clock call, not two).
            let elapsed = instant.elapsed();
            (elapsed.as_nanos() as u64, wall + elapsed)
        }
    }
}

/// Monotonic nanoseconds since the clock's first use.
///
/// Safe for deadline arithmetic (the interrupt handler compares against this):
/// a system clock step cannot move it backwards or forwards.
#[inline]
pub fn monotonic_now_nanos() -> u64 {
    now().0
}

/// Wall-clock-aligned, monotonic `SystemTime`.
///
/// Equal to the real wall clock at the moment of first use, then advances at
/// real time via the monotonic clock — so a backward NTP step never yields a
/// timestamp earlier than a previous one, and a forward step never jumps
/// ahead of elapsed real time. k6-compatible outputs (`json_stream`,
/// `influxdb`, `otlp`) stay on real time; sample ordering stays monotonic.
#[inline]
pub fn monotonic_wall_now() -> SystemTime {
    now().1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_now_is_monotonic() {
        let a = monotonic_wall_now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = monotonic_wall_now();
        assert!(b >= a, "timestamps must never go backwards");
    }

    #[test]
    fn now_nanos_is_monotonic() {
        let a = monotonic_now_nanos();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = monotonic_now_nanos();
        assert!(b >= a, "deadline clock must never go backwards");
    }

    #[test]
    fn wall_now_is_aligned_to_real_clock() {
        // Anchored at first use, so it must be within a few minutes of the
        // real wall clock (not relative-to-process-start).
        let diff = SystemTime::now()
            .duration_since(monotonic_wall_now())
            .unwrap_or_default();
        assert!(
            diff < std::time::Duration::from_secs(300),
            "wall_now drifted from the real clock by {diff:?}"
        );
    }

    // NOTE: the set_clock_source override test deliberately does NOT live
    // here. Installing the process-global, un-removable source inside this
    // lib test binary would poison parallel tests that rely on the clock
    // advancing — most critically infinite_loop_interrupted_within_two_seconds,
    // whose interrupt deadline would never trip against a constant clock,
    // hanging the suite forever. It lives in tests/clock_override.rs, a
    // separate process.
}
