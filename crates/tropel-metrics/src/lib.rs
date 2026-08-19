//! # tropel-metrics
//!
//! Sample ingestion, hdr histogram aggregation, tag/label aggregation,
//! and threshold evaluation.

use serde::{Deserialize, Serialize};

pub mod collector;
pub mod histogram;
pub mod thresholds;

pub use collector::*;
pub use histogram::*;
pub use thresholds::*;

/// Global atomic counter of samples dropped by output consumers due to
/// broadcast lag (RecvError::Lagged).  Every streaming output (InfluxDB,
/// StatsD, JSON-stream, OTLP, Prometheus, extension) increments this on
/// lag; the stdout reporter and MetricsResult surface it so users can
/// see how many samples were silently lost.
pub static OUTPUT_SAMPLES_DROPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The k6 metric unit model (backlog line 32): a metric carries BOTH an
/// aggregation type (`MetricType`: Counter/Gauge/Rate/Trend) AND a unit
/// (`Time`/`Data`/`Default`). `Time` values are fractional milliseconds
/// (rendered with an `ms` suffix, stamped `contains: "time"`); `Data`
/// values are byte counts (`contains: "data"`); `Default` values are raw
/// numbers (`contains: "default"`). Previously the unit was implicit —
/// `MetricType` described aggregation only, `Sample.value` was an
/// undocumented `f64`, and the time registry was a process-global
/// `HashSet<String>` that was never cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricUnit {
    /// Fractional milliseconds — durations, latency, timings.
    Time,
    /// Byte counts — `data_received`/`data_sent` and custom `*_bytes`.
    Data,
    /// Raw numbers — requests, checks, custom counters.
    Default,
}

impl MetricUnit {
    /// k6's `contains` string for the JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            MetricUnit::Time => "time",
            MetricUnit::Data => "data",
            MetricUnit::Default => "default",
        }
    }
}

/// Registry of custom metrics and their declared units.
///
/// k6's `new Trend(name, isTime)` (and the older `metric(name, type,
/// isTime)`) let a script mark a custom metric as containing time so the
/// JSON output stamps `contains: "time"` and summaries render it in ms.
/// The k6 driver registers names here when `isTime` is true; reporters
/// (json-stream's `contains` stamp, the stdout ms unit) consult it via
/// [`unit_of`] in addition to the name-suffix heuristic, so a custom
/// `my_timer` renders as time even though its name doesn't end in
/// `_duration`/`_time`.
pub mod time_metrics {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use super::MetricUnit;

    static UNITS: OnceLock<Mutex<HashMap<String, MetricUnit>>> = OnceLock::new();

    /// Declare a custom metric's unit (`isTime: true` registers [`MetricUnit::Time`]).
    pub fn register(name: &str, unit: MetricUnit) {
        UNITS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(name.to_string(), unit);
    }

    /// Forget all declared units. Called at the start of every engine run so
    /// a previous run's declarations can never leak into the next one (the
    /// old registry was a process-global that was never cleared).
    pub fn clear() {
        if let Some(m) = UNITS.get() {
            m.lock().unwrap().clear();
        }
    }

    /// A metric's unit: explicit [`register`] declaration wins, then k6's
    /// name conventions (byte-count builtins → [`MetricUnit::Data`]; duration
    /// suffixes → [`MetricUnit::Time`]), else [`MetricUnit::Default`]. This is
    /// the SINGLE source of truth — json-stream's `contains` stamp, stdout's
    /// unit suffix, and handleSummary's `contains` field all delegate here so
    /// the classification can never drift between outputs (backlog §0).
    pub fn unit_of(name: &str) -> MetricUnit {
        if let Some(m) = UNITS.get() {
            if let Some(unit) = m.lock().unwrap().get(name) {
                return *unit;
            }
        }
        // k6's byte-count builtins carry `contains: "data"`.
        if name == "data_received"
            || name == "data_sent"
            || name.ends_with("_bytes")
            || name.ends_with("_byte_count")
        {
            return MetricUnit::Data;
        }
        if name.ends_with("duration")
            || name.ends_with("_time")
            || name.ends_with("_waiting")
            || name.ends_with("_receiving")
            || name.ends_with("_sending")
            || name.ends_with("_connecting")
            || name.ends_with("_blocked")
            || name.ends_with("_tls_handshaking")
            || name.ends_with("_lookup")
            || name.contains("ttfb")
            || name.contains("latency")
        {
            return MetricUnit::Time;
        }
        MetricUnit::Default
    }
}

#[cfg(test)]
mod tests {
    use super::{time_metrics, MetricUnit};
    use std::sync::{Mutex, OnceLock};

    // The registry is process-global and tests run in parallel threads — a
    // sibling test's clear() could wipe this test's in-flight register()
    // between the call and the assertion. Serialize the two registry tests.
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn lock_registry() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.get_or_init(Default::default).lock().unwrap()
    }

    #[test]
    fn unit_of_classifies_by_heuristic_and_declaration() {
        let _guard = lock_registry();
        time_metrics::clear();

        // Name-heuristic: byte-count builtins → Data.
        assert_eq!(time_metrics::unit_of("data_received"), MetricUnit::Data);
        assert_eq!(time_metrics::unit_of("data_sent"), MetricUnit::Data);
        // `*_bytes` custom trends carry `contains: "data"` too.
        assert_eq!(time_metrics::unit_of("my_bytes"), MetricUnit::Data);
        assert_eq!(
            time_metrics::unit_of("http_response_body_size"),
            MetricUnit::Default
        );

        // Name-heuristic: duration suffixes → Time.
        assert_eq!(time_metrics::unit_of("http_req_duration"), MetricUnit::Time);
        assert_eq!(
            time_metrics::unit_of("iteration_duration"),
            MetricUnit::Time
        );
        assert_eq!(time_metrics::unit_of("ttfb"), MetricUnit::Time);

        // Explicit declaration wins over heuristics (backlog line 32).
        time_metrics::register("my_timer", MetricUnit::Time);
        time_metrics::register("data_received", MetricUnit::Default);
        assert_eq!(time_metrics::unit_of("my_timer"), MetricUnit::Time);
        assert_eq!(time_metrics::unit_of("data_received"), MetricUnit::Default);

        // Everything else → Default.
        assert_eq!(time_metrics::unit_of("http_reqs"), MetricUnit::Default);
        assert_eq!(time_metrics::unit_of("checks"), MetricUnit::Default);
        assert_eq!(time_metrics::unit_of("my_counter"), MetricUnit::Default);

        // Leave no registrations behind for parallel sibling tests.
        time_metrics::clear();
    }

    #[test]
    fn clear_forgets_previous_runs_declarations() {
        let _guard = lock_registry();
        // Backlog line 32: the old registry was process-global and never
        // cleared — a declaration from run N leaked into run N+1. `clear()`
        // (called by the engine at run start) must forget everything.
        time_metrics::clear();
        time_metrics::register("leaky_timer", MetricUnit::Time);
        assert_eq!(time_metrics::unit_of("leaky_timer"), MetricUnit::Time);
        time_metrics::clear();
        // After clear, a name that only matched via declaration falls back to
        // the heuristic (or Default).
        assert_eq!(time_metrics::unit_of("leaky_timer"), MetricUnit::Default);
    }

    #[test]
    fn unit_as_str_matches_k6_contains() {
        assert_eq!(MetricUnit::Time.as_str(), "time");
        assert_eq!(MetricUnit::Data.as_str(), "data");
        assert_eq!(MetricUnit::Default.as_str(), "default");
    }
}
