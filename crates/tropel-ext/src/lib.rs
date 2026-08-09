//! # tropel-ext
//!
//! The Extension SDK: extension-point traits + the registry.
//! Everything pluggable depends on this crate.

pub mod registration;
pub mod registry;
pub mod traits;

pub use registration::*;
pub use registry::*;
pub use traits::*;
