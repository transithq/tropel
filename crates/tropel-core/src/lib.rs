//! # tropel-core
//!
//! Engine-internal domain types: configuration, execution segments.
//! After the P1 inversion, the shared contract types (`types`, `scenario`,
//! `error`, `duration`) and extension traits (`traits`, `registration`)
//! live in `tropel-sdk` — this crate now depends on the leaf and keeps only
//! what the engine (not adapter authors) needs. P3c moved the clock to
//! `tropel-js` and the HTTP config types to `tropel-http` so the runtime
//! publish set stops resolving this crate.

pub mod config;
pub mod segment;

pub use segment::*;

// Explicit re-exports (not a glob — a glob over a module that itself
// re-exports from tropel-sdk triggers an unused-import lint).
pub use config::{
    status_is_expected, ArrivalRateStage, ExecutionConfig, ExpectedStatus, HttpConfig, JobConfig,
    OutputConfig, ScenarioConfig, Stage, ThinkTimeConfig, ThresholdConfig, TlsConfig,
};
