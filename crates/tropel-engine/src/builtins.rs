//! # Built-in extension wiring
//!
//! The built-in input adapters (postman, har, openapi, k6) register
//! themselves via `inventory::submit!`. However, Rust's linker only pulls
//! object code from a dependency crate that is actually *referenced* from
//! the final binary. Since nothing else in `tropel-engine` mentions these
//! adapter types, their registration statics get dead-stripped and the
//! binary reports "no input adapter recognized" / "none registered".
//!
//! The functions below construct each built-in adapter/driver type, forcing
//! the linker to include the crate's object code — and therefore its
//! `inventory::submit!` registration. This mirrors what `tropel build` does
//! when it emits `extern crate {name};` lines into the generated `main.rs`.
//!
//! `register_builtins()` is invoked from the CLI at startup, before the
//! `ExtensionRegistry` performs its `collect_inventory()` pass.

use tropel_sdk::traits::{Driver, InputAdapter, Protocol};

/// Force-link every built-in input adapter and driver by constructing it.
/// Returns the total number of built-ins so the call is observable.
pub fn link_builtins() -> usize {
    let adapters: Vec<Box<dyn InputAdapter>> = vec![
        Box::new(tropel_input_postman::PostmanInputAdapter),
        Box::new(tropel_input_har::HarInputAdapter),
        Box::new(tropel_input_openapi::OpenApiInputAdapter),
        Box::new(tropel_input_k6::K6ScriptAdapter),
        Box::new(tropel_input_http::HttpFileAdapter),
        Box::new(tropel_input_bru::BruInputAdapter),
        Box::new(tropel_input_insomnia::InsomniaInputAdapter),
    ];
    let drivers: Vec<Box<dyn Driver>> = vec![
        Box::new(tropel_input_k6::driver::K6Driver),
        // Force-link the imperative WASM driver so its inventory registration
        // survives dead-stripping (tropel-wasm is also linked for
        // discover_plugins, but the DriverRegistration static must be pulled
        // into the binary for `tropel run plugin.wasm` to resolve).
        Box::new(tropel_wasm::driver::WasmDriver::default()),
    ];
    // Force-link the protocol extensions so their `inventory::submit!`
    // registrations survive dead-stripping — this is what makes `grpc://` /
    // `grpcs://` and `ws://` / `wss://` URLs reachable through the VU
    // runner's scheme dispatch.
    let protocols: Vec<Box<dyn Protocol>> = vec![
        Box::new(tropel_x_grpc::GrpcProtocol::default()),
        Box::new(tropel_x_websocket::WebSocketProtocol),
    ];
    adapters.len() + drivers.len() + protocols.len()
}

/// Call from the CLI before registry collection so the linker keeps the
/// built-in `inventory::submit!` registrations alive.
pub fn register_builtins() {
    let count = link_builtins();
    tracing::debug!("Force-linked {count} built-in adapter/driver type(s)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tropel_ext::registry::ExtensionRegistry;

    /// TR-007: every adapter shipped in this workspace must be reachable from
    /// the CLI. A new adapter that is never force-linked (or never wired into
    /// `link_builtins`) registers in `inventory` but gets dead-stripped from
    /// the binary, so `tropel run file` reports "no input adapter recognized".
    /// This test walks the real `collect_inventory()` path the CLI uses.
    #[test]
    fn every_builtin_adapter_is_reachable_from_the_cli() {
        register_builtins();
        let registry = ExtensionRegistry::new();
        let inputs = registry.list_inputs();
        for expected in ["postman", "har", "openapi", "k6", "http", "bru", "insomnia"] {
            assert!(
                inputs.iter().any(|id| id == expected),
                "adapter '{expected}' is not reachable from the CLI — it must be added to \
                 builtins::link_builtins() or the binary dead-strips its registration"
            );
        }
    }
}
