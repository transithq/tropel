//! # tropel-scheduler
//!
//! How many VUs, how fast, how long — ramping, arrival rate, VU lifecycle
//! (the `VUScheduler`). Entirely load-specific: the API client does not use
//! this crate.
//!
//! Split from the old `tropel-executor` (P5): the scenario pass — resolve,
//! script, sign, send, assert, jump — lives in `tropel-runtime`.

pub mod scheduler;

pub use scheduler::*;
