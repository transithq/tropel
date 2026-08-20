//! # tropel-report
//!
//! Reporters consuming aggregated metrics: stdout summary, JSON, CSV.
//! Streaming outputs consuming individual samples during the run.

pub mod csv_reporter;
pub mod influxdb;
pub mod json_reporter;
pub mod json_stream;
pub mod otlp;
pub mod output;
pub mod prometheus;
pub mod statsd;
pub mod stdout;

pub use csv_reporter::*;
pub use influxdb::*;
pub use json_reporter::*;
pub use json_stream::*;
pub use otlp::*;
pub use output::*;
pub use prometheus::*;
pub use statsd::*;
pub use stdout::*;

/// Global counter for samples dropped by output consumer lag.
/// Incremented in each output's `Lagged` handler and surfaced in
/// `MetricsResult` so the summary can warn operators.
pub static OUTPUT_SAMPLES_DROPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

use async_trait::async_trait;
use tropel_metrics::collector::MetricsResult;
use tropel_sdk::Result;

/// A reporter that outputs test results.
#[async_trait]
pub trait Reporter: Send + Sync {
    fn name(&self) -> &str;
    async fn report(&self, result: &MetricsResult) -> Result<()>;
}

/// Create reporters by name, optionally with an output file path from
/// `-o`/`--output`. Without a path, json/csv reporters write to stdout.
pub fn create_reporter(name: &str, output_file: Option<&str>) -> Option<Box<dyn Reporter>> {
    match name {
        "stdout" => Some(Box::new(StdoutReporter)),
        "json" => Some(Box::new(JsonReporter::new(
            output_file.map(|s| s.to_string()),
        ))),
        "csv" => Some(Box::new(CsvReporter::new(
            output_file.map(|s| s.to_string()),
        ))),
        _ => None,
    }
}
