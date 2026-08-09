#![doc = "Internal to tropel-runtime. No stability guarantee — depend on tropel-runtime instead."]
//! # tropel-runtime
//!
//! What happens during one pass through a `Scenario` — resolve, script,
//! sign, send, assert, jump (the `ScenarioRunner`). Named after
//! `postman-runtime`: the scheduler and the API client both run this crate.
//!
//! Split from the old `tropel-executor` (P5): the load-specific half — how
//! many VUs, how fast, how long — lives in `tropel-scheduler`.

pub mod runner;

pub use runner::*;
