//! `set_clock_source` override test — deliberately isolated in its own
//! integration-test process.
//!
//! Installing the process-global, un-removable clock source (`OnceLock` can't
//! be unset) inside the lib test binary would poison every parallel test that
//! relies on the clock advancing — most critically
//! `infinite_loop_interrupted_within_two_seconds`, whose interrupt deadline
//! would never trip against a constant clock, hanging the whole suite forever.
//! A separate process makes the override harmless.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use tropel_js::clock::{monotonic_now_nanos, monotonic_wall_now, set_clock_source};

/// Holds the deterministic values the fn-pointer source reports.
static FAKE: OnceLock<(u64, SystemTime)> = OnceLock::new();

fn fake_source() -> (u64, SystemTime) {
    *FAKE.get().expect("fake source values set before install")
}

#[test]
fn set_clock_source_overrides_time() {
    // P6 differential harness: a deterministic source must be honored.
    // Capture real values first, then install a source that reports them
    // exactly. Deriving from the real clock (rather than a fixed epoch) keeps
    // the module's invariant tests valid regardless of execution order.
    let wall = monotonic_wall_now() + Duration::from_secs(5);
    let nanos = monotonic_now_nanos() + 42;
    FAKE.set((nanos, wall))
        .expect("fake values set exactly once");
    set_clock_source(fake_source);
    assert_eq!(monotonic_now_nanos(), nanos);
    assert_eq!(monotonic_wall_now(), wall);
}
