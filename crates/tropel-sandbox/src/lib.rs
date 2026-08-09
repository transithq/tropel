#![doc = "Internal to tropel-runtime. No stability guarantee — depend on tropel-runtime instead."]
//! # tropel-sandbox
//!
//! The script sandbox: native host functions + the JS glue.
//! Provides `pm.environment`, `pm.variables`, `pm.test`, `pm.expect`,
//! `pm.response`, `pm.sendRequest`, and `pm.iterationData`.
//!
//! P4b layout: the **state model** ([`state`]) is binding-agnostic — scopes,
//! exchange, assertions, flow control — and the **bindings** ([`bindings`])
//! are views over it. `pm.*` is the frozen Postman-compat layer; the
//! canonical binding (`trp.*`, Postman convention) is a peer view over the
//! same state, and its name + aliases are configurable by embedders via
//! [`config::SandboxConfig`].

pub mod bindings;
pub mod config;
pub mod state;

pub use bindings::*;
pub use config::*;
pub use state::*;
