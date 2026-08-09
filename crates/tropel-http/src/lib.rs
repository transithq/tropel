//! # tropel-http
//!
//! HTTP Protocol implementation: reqwest client, connection pooling,
//! redirects, per-VU cookie jar. Auth signers live in `tropel-auth`
//! (extracted so the executor and wasm slice can depend on them without
//! pulling in the full HTTP stack).

pub mod blocking;
pub mod client;
pub mod dns;
pub mod rps;
pub mod subtimings;

pub use client::*;
pub use dns::*;
pub use rps::*;
