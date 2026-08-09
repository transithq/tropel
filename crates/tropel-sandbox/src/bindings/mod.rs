//! Script bindings over the shared state model.
//!
//! Each binding is a view over the same [`crate::state`] — the namespace is
//! the compat switch (P4b). `pm.*` is the frozen Postman-compat layer;
//! `trp.*` (canonical, Postman convention) and any product aliases are peer
//! views.

pub mod pm;

pub use pm::*;
