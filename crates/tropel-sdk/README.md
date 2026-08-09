# Tropel SDK

Stable public API for Tropel extension authors. `tropel-sdk` is the **single
dependency** a third-party adapter/driver/output author needs — it re-exports
the types and traits that form the public contract without exposing internal
crate structure.

The SDK is a **leaf**: it depends on zero `tropel-*` crates. Backing crates
(`tropel-core`, `tropel-ext`) depend on *it*, not the other way around.

## Semver policy

This crate follows strict semver. Breaking changes (trait method added or
removed, type renamed, field visibility changed) bump the major version.
Before 1.0.0, breaking changes bump the minor version (0.x → 0.y).

<!-- Maintained in sync with the crate-level doc comment in src/lib.rs
     (which docs.rs renders). Update both when the public surface changes. -->

## Quick start — writing an input adapter

```rust,ignore
// Single import: `tropel-sdk` re-exports everything you need.
use tropel_sdk::{
    InputAdapter, InputAdapterRegistration, Scenario, ScenarioInfo,
    ScenarioItem, Request, Method, Result, inventory,
};

/// Adapter for the (fictional) `.apig` format.
pub struct ApiGatewayAdapter;

impl InputAdapter for ApiGatewayAdapter {
    fn id(&self) -> &str { "apig" }

    fn detect(&self, bytes: &[u8]) -> bool {
        bytes.starts_with(b"APIG\n")
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let _text = std::str::from_utf8(bytes)
            .map_err(|e| /* ... */)?;
        Ok(Scenario {
            info: ScenarioInfo {
                name: "my-api".into(),
                description: None,
                schema: None,
            },
            items: vec![/* request items */],
            variables: Default::default(),
            auth: None,
        })
    }
}

// Register for compile-time discovery by the engine.
// The `inventory` crate is re-exported through the SDK.
inventory::submit!(InputAdapterRegistration::new(
    "apig",
    || Box::new(ApiGatewayAdapter),
));
```

## Features

| Feature | Default | Purpose |
|---|---|---|
| `registration` | ✅ on | `*Registration` structs + the `inventory` dependency (compile-time discovery) |
| `unstable-protocol` | off | `Protocol` extension trait (engine dispatches by URL scheme — built-ins `grpc://`/`ws://`) |
| `unstable-output` | off | `Output` extension trait (engine drives registered outputs from the sample stream) |

The quick-start above uses the default `registration` feature
(`inventory::submit!`); drop it only if you implement traits without
registering them. A consumer that only writes adapters against the traits
can set `default-features = false` and drop the `inventory` dependency
entirely:

```toml
tropel-sdk = { version = "0.1", default-features = false }
```

## Stability guarantees

| Item | Status |
|------|--------|
| `Scenario`, `ScenarioInfo`, `ScenarioItem` | ✅ Stable — schema is the engine's core IR |
| `InputAdapter` trait | ✅ Stable — used by all declarative adapters |
| `InputAdapterRegistration` | ✅ Stable — const fn, `fn()` pointer |
| `Request`, `Response`, `Method`, `Body` | ✅ Stable — used in scenario items |
| `Result<T>` / `TropelError` | ✅ Stable — error handling |
| `Sample`, `SampleType`, `TagMap` | ✅ Stable — metric sample surface (tags use `Arc<str>` interning) |
| `Protocol` trait | ✅ Stable — engine dispatches by URL scheme via the registry (`instantiate_protocols`); built-ins `grpc://`/`ws://` ship through it |
| `Output` trait | ✅ Wired — engine drives registered outputs from the sample stream (emit per batch, flush on close); see the `prometheus` reference extension (`unstable-output`) |
| `Driver` / `DriverInstance` / `VuContext` | ✅ Stable — re-exported and used by the k6 driver (`tropel-input-k6`) |
| `WASM` / `WIT` interface | 🔶 Resolves (parses) — `wit/adapter.wit` in the crate root is validated as a *parseable* WIT package; the forward-looking Component-Model contract, intentionally lagging the shipped C-ABI path (`tropel-wasm`) |
| Engine config (`config` module) | 🔒 NOT re-exported at the root — engine-owned; only the contract-referenced subset lives here, reachable via `tropel_sdk::config::*` |

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)).
