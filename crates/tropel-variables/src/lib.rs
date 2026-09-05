#![doc = "Internal to tropel-runtime. No stability guarantee — depend on tropel-runtime instead."]
//! # tropel-variables
//!
//! {{var}} resolution with scope precedence and dynamic-variable catalog.

pub mod assertions;
pub mod catalog;
pub mod resolver;

pub use catalog::*;
pub use resolver::*;
