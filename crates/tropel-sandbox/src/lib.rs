//! # tropel-sandbox
//!
//! The script sandbox: native host functions + the JS glue.
//! Provides `pm.environment`, `pm.variables`, `pm.test`, `pm.expect`,
//! `pm.response`, `pm.sendRequest`, and `pm.iterationData`.
//!
//! P4b layout: the **state model** ([`state`]) is binding-agnostic — scopes,
//! exchange, assertions, flow control — and the **bindings** ([`bindings`])
//! are views over it. `pm.*` is the frozen Postman-compat layer; the
//! canonical binding (default `tropel.*`) is a peer view over the same
//! state, and its name + aliases are configurable by embedders via
//! [`config::SandboxConfig`].

pub mod config;
pub mod state;
pub mod bindings;

pub use config::*;
pub use state::*;
pub use bindings::*;
