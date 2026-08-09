//! # tropel-pm
//!
//! The `pm.*` API bridge: native functions + the JS glue.
//! Provides `pm.environment`, `pm.variables`, `pm.test`, `pm.expect`,
//! `pm.response`, `pm.sendRequest`, and `pm.iterationData`.
//!
//! P4b layout: the **state model** ([`state`]) is binding-agnostic — scopes,
//! exchange, assertions, flow control — and the **bindings** ([`bindings`])
//! are views over it. `pm.*` is the frozen Postman-compat layer; the
//! canonical `tropel.*` binding is a peer view over the same state.

pub mod state;
pub mod bindings;

pub use state::*;
pub use bindings::*;
