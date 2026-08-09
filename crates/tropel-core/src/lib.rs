//! # tropel-core
//!
//! Engine-internal domain types: configuration, clock, execution segments.
//! After the P1 inversion, the shared contract types (`types`, `scenario`,
//! `error`, `duration`) and extension traits (`traits`, `registration`)
//! live in `tropel-sdk` — this crate now depends on the leaf and keeps only
//! what the engine (not adapter authors) needs.

pub mod clock;
pub mod config;
pub mod segment;

pub use clock::*;
pub use segment::*;

// Explicit re-exports (not a glob — a glob over a module that itself
// re-exports from tropel-sdk triggers an unused-import lint).
pub use config::{
    ArrivalRateStage, ExecutionConfig, ExpectedStatus, HttpConfig, JobConfig, OutputConfig,
    ScenarioConfig, Stage, status_is_expected, ThinkTimeConfig, ThresholdConfig, TlsConfig,
};
