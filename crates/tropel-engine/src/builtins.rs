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
    /// TR-102: no host bridge may carry its own reserved-metric list.
    ///
    /// The guard existed in four places and the four copies drifted:
    /// `http_req_dns` was guarded on the k6 path only, `iteration_duration`
    /// and `dropped_iterations` on the pm/trp path only, `ws_*`/`browser_*`
    /// on two of three. A k6 script could therefore forge
    /// `dropped_iterations` — the counter that decides whether a run reports
    /// itself verified.
    ///
    /// Fails on the pre-fix code: it greps the shipped sources for a local
    /// list literal, and pre-fix there were four. Source-level because the
    /// failure mode is a *new copy* appearing, which no runtime assertion on
    /// the current call sites can see.
    #[test]
    fn no_crate_carries_a_private_reserved_metric_list() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("crates");

        fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // The SDK is where the one canonical list lives.
                    if path.file_name().is_some_and(|n| n == "tropel-sdk") {
                        continue;
                    }
                    walk(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let Ok(src) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    // The shape all four copies had: a `const RESERVED:
                    // &[&str]` slice literal.
                    //
                    // Both needles are assembled with `concat!` so this
                    // detector does not match its own source. Spelling them
                    // literally here makes the test fail on itself, which is
                    // a decorative failure that teaches the next person to
                    // add an exclusion rather than fix a real hit.
                    let name_needle = concat!("const ", "RESERVED");
                    let type_needle = concat!("&[&", "str]");
                    for (i, line) in src.lines().enumerate() {
                        if line.contains(name_needle) && line.contains(type_needle) {
                            hits.push(format!("{}:{}", path.display(), i + 1));
                        }
                    }
                }
            }
        }

        let mut hits = Vec::new();
        walk(&root, &mut hits);
        assert!(
            hits.is_empty(),
            "these files carry a private reserved-metric list; call \
             tropel_sdk::is_reserved_builtin_metric instead (TR-102): {hits:#?}"
        );
    }

    /// Adapters that legitimately have no static `inventory::submit!`, with the
    /// reason. Anything not listed here MUST be reachable from the CLI.
    ///
    /// Data rather than an `if`, so adding an exemption is a visible diff that
    /// has to carry a justification.
    const REGISTRATION_EXEMPT: &[(&str, &str)] = &[(
        "tropel-input-subprocess",
        "factory-only: takes a runtime --subprocess-adapter <cmd> argument, so it cannot \
         be a compile-time registration. A static placeholder would be probed on every \
         auto-detect and spawn a bogus `echo` (see its lib.rs Registration section).",
    )];

    /// TR-007: every adapter shipped in this workspace must be reachable from
    /// the CLI. A new adapter that is never force-linked (or never wired into
    /// `link_builtins`) registers in `inventory` but gets dead-stripped from
    /// the binary, so `tropel run file` reports "no input adapter recognized".
    /// This test walks the real `collect_inventory()` path the CLI uses.
    ///
    /// It ENUMERATES `crates/inputs/` rather than asserting a hardcoded list.
    /// The criterion is "otherwise the next one ships unwired too", and a
    /// hardcoded list cannot deliver that — the next adapter is by definition
    /// not in it, so it passes silently. bru and insomnia shipped unreachable
    /// for exactly this reason: they were in the wasm dispatch table and not
    /// the native one, and nothing compared the two populations.
    #[test]
    fn every_adapter_in_the_workspace_is_reachable_from_the_cli() {
        register_builtins();
        let registry = ExtensionRegistry::new();
        let inputs = registry.list_inputs();

        let inputs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("crates/inputs");

        let mut unreachable = Vec::new();
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&inputs_dir)
            .expect("crates/inputs must exist")
            .flatten()
        {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let crate_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if REGISTRATION_EXEMPT.iter().any(|(n, _)| *n == crate_name) {
                continue;
            }
            // The adapter id is what `list_inputs()` reports; by convention it
            // is the crate name minus the `tropel-input-` prefix.
            let Some(id) = crate_name.strip_prefix("tropel-input-") else {
                continue;
            };
            checked += 1;
            if !inputs.iter().any(|got| got == id) {
                unreachable.push(id.to_string());
            }
        }

        assert!(
            checked >= 7,
            "expected to enumerate at least the 7 known adapters, found {checked} — the \
             directory layout changed and this test is no longer looking at anything \
             (that is how a reachability test rots into a no-op)"
        );
        assert!(
            unreachable.is_empty(),
            "these adapters exist in crates/inputs but are NOT reachable from the CLI: \
             {unreachable:?}. Add each to builtins::link_builtins(), or add an entry to \
             REGISTRATION_EXEMPT with the reason it cannot be statically registered."
        );
    }

    /// The exemption list must not become a way to hide a real gap: every crate
    /// named in it has to still exist, and carry a real justification.
    #[test]
    fn registration_exemptions_still_exist_and_carry_a_reason() {
        let inputs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("crates/inputs");
        for (crate_name, reason) in REGISTRATION_EXEMPT {
            assert!(
                inputs_dir.join(crate_name).is_dir(),
                "REGISTRATION_EXEMPT names '{crate_name}', which no longer exists — remove \
                 the stale exemption"
            );
            assert!(
                reason.len() > 40,
                "exemption for '{crate_name}' needs a real justification, not a stub"
            );
        }
    }
}
