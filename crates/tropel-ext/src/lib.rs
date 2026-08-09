//! # tropel-ext
//!
//! The engine's extension *resolver*. After the P1 inversion the extension
//! traits and `*Registration` structs live in `tropel-sdk` (the leaf); this
//! crate keeps only `ExtensionRegistry`, the engine-side resolver that
//! collects inventory-registered extensions at startup. An extension
//! **registers** (via the SDK); only the engine **resolves** (here).

pub mod registry;

pub use registry::*;
