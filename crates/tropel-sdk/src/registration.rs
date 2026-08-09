//! Registration types for `inventory`-based extension discovery.
//!
//! Each adapter/driver/output/protocol crate uses `inventory::submit!` to
//! register its factory here; the engine's `ExtensionRegistry` (registry.rs)
//! iterates these collections at startup via `collect_inventory()`.
//!
//! Split out of `traits.rs` as a pure move (no behaviour change) so the P7
//! SDK extraction can relocate the file wholesale — `git filter-repo`
//! follows file moves, not inline splits.

use crate::traits::{Driver, InputAdapter, Output, Protocol};

/// Registration wrapper for protocols.
/// `scheme` is the URL scheme this protocol handles (e.g. `"grpc"`), stored
/// separately from the factory so `collect_inventory()` can read it without
/// instantiating the protocol.
///
/// Uses a `fn` pointer (not `Arc<dyn Fn>`) because `inventory::submit!`
/// requires the expression to be usable in a `const` context — the same
/// pattern as `InputAdapterRegistration`. Protocol factories are simple
/// captureless constructors, so a function pointer is sufficient.
pub struct ProtocolRegistration {
    pub scheme: &'static str,
    pub create: fn() -> Box<dyn Protocol>,
    /// Reserved for dispatch priority (informational; protocols are
    /// scheme-keyed).
    pub priority: u8,
}

impl ProtocolRegistration {
    pub const fn new(scheme: &'static str, create: fn() -> Box<dyn Protocol>) -> Self {
        Self {
            scheme,
            create,
            priority: 0,
        }
    }

    /// Builder-style priority setter (const, for `inventory::submit!`).
    pub const fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

/// Registration wrapper for outputs.
/// Follows the same `fn` pointer pattern as `InputAdapterRegistration`
/// for `const`-compatibility with `inventory::submit!`.
pub struct OutputRegistration {
    pub id: &'static str,
    pub create: fn() -> Box<dyn Output>,
    /// Reserved for future dispatch priority; outputs are name-keyed so
    /// this is currently informational.
    pub priority: u8,
}

impl OutputRegistration {
    pub const fn new(id: &'static str, create: fn() -> Box<dyn Output>) -> Self {
        Self {
            id,
            create,
            priority: 0,
        }
    }

    /// Builder-style priority setter (const, for `inventory::submit!`).
    pub const fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

/// Registration wrapper for input adapters.
/// `id` is the adapter's unique identifier (e.g. "postman", "har"), stored
/// separately from the factory so `collect_inventory()` can read it without
/// instantiating the adapter.
///
/// Uses a `fn` pointer (not `Arc<dyn Fn>`) because `inventory::submit!`
/// requires the expression to be usable in a `const` context. Since adapter
/// factories are always simple captureless closures (no captured state),
/// a function pointer is sufficient and avoids the `Arc` allocation.
pub struct InputAdapterRegistration {
    pub id: &'static str,
    pub create: fn() -> Box<dyn InputAdapter>,
    /// Explicit dispatch priority for content auto-detection. Higher wins
    /// when several adapters' `detect()` claim the same bytes; ties fall back
    /// to registration order. This makes dispatch deterministic and independent
    /// of `inventory` link order.
    pub priority: u8,
}

impl InputAdapterRegistration {
    pub const fn new(id: &'static str, create: fn() -> Box<dyn InputAdapter>) -> Self {
        Self {
            id,
            create,
            priority: 0,
        }
    }

    /// Builder-style priority setter (const, for `inventory::submit!`).
    pub const fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

/// Registration wrapper for imperative drivers.
/// Follows the same `fn` pointer pattern as `InputAdapterRegistration`
/// for `const`-compatibility with `inventory::submit!`.
pub struct DriverRegistration {
    pub id: &'static str,
    pub create: fn() -> Box<dyn Driver>,
    /// Explicit dispatch priority for content auto-detection (see
    /// [`InputAdapterRegistration::priority`]).
    pub priority: u8,
}

impl DriverRegistration {
    pub const fn new(id: &'static str, create: fn() -> Box<dyn Driver>) -> Self {
        Self {
            id,
            create,
            priority: 0,
        }
    }

    /// Builder-style priority setter (const, for `inventory::submit!`).
    pub const fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

// `fn` pointers are automatically Send + Sync.

// Register collection types for compile-time inventory registration.
// Each adapter/driver crate uses `inventory::submit!` to register,
// and the registry's `collect_inventory()` iterates them at startup.
// This must be in the crate that defines the type (`tropel-ext`).
inventory::collect!(InputAdapterRegistration);
inventory::collect!(DriverRegistration);
inventory::collect!(OutputRegistration);
inventory::collect!(ProtocolRegistration);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;
    use crate::traits::{DriverInstance, ProtocolOutcome};
    use crate::types::{Request, Sample};
    use crate::{Result, TropelError};
    use async_trait::async_trait;
    use serde_json::Value;

    // ── Stub impls for registration tests ──
    struct StubAdapter;
    impl InputAdapter for StubAdapter {
        fn id(&self) -> &str {
            "stub"
        }
        fn detect(&self, _bytes: &[u8]) -> bool {
            false
        }
        fn parse(&self, _bytes: &[u8]) -> Result<Scenario> {
            Err(TropelError::Other("stub".into()))
        }
    }

    struct StubDriver;
    #[async_trait]
    impl Driver for StubDriver {
        fn id(&self) -> &str {
            "stub"
        }
        fn detect(&self, _bytes: &[u8]) -> bool {
            false
        }
        async fn init(
            &self,
            _bytes: &[u8],
            _source_path: Option<&std::path::Path>,
            _exec: Option<&str>,
        ) -> Result<Box<dyn DriverInstance>> {
            Err(TropelError::Other("stub".into()))
        }
    }

    struct StubProtocol;
    #[async_trait]
    impl Protocol for StubProtocol {
        fn scheme(&self) -> &str {
            "stub"
        }
        async fn execute(
            &self,
            _req: &Request,
            _config: Option<&Value>,
        ) -> Result<ProtocolOutcome> {
            Err(TropelError::Other("stub".into()))
        }
    }

    struct StubOutput;
    #[async_trait]
    impl Output for StubOutput {
        fn name(&self) -> &str {
            "stub"
        }
        async fn emit(&self, _batch: &[Sample]) -> Result<()> {
            Ok(())
        }
        async fn flush(&self) -> Result<()> {
            Ok(())
        }
    }

    fn stub_adapter() -> Box<dyn InputAdapter> {
        Box::new(StubAdapter)
    }
    fn stub_driver() -> Box<dyn Driver> {
        Box::new(StubDriver)
    }
    fn stub_protocol() -> Box<dyn Protocol> {
        Box::new(StubProtocol)
    }
    fn stub_output() -> Box<dyn Output> {
        Box::new(StubOutput)
    }

    #[test]
    fn registration_builders_default_priority_zero() {
        assert_eq!(ProtocolRegistration::new("grpc", stub_protocol).priority, 0);
        assert_eq!(OutputRegistration::new("stdout", stub_output).priority, 0);
        assert_eq!(
            InputAdapterRegistration::new("postman", stub_adapter).priority,
            0
        );
        assert_eq!(DriverRegistration::new("k6", stub_driver).priority, 0);
    }

    #[test]
    fn with_priority_is_const_builder_pattern() {
        let p = ProtocolRegistration::new("grpc", stub_protocol).with_priority(5);
        assert_eq!(p.scheme, "grpc");
        assert_eq!(p.priority, 5);
        let a = InputAdapterRegistration::new("x", stub_adapter).with_priority(9);
        assert_eq!(a.id, "x");
        assert_eq!(a.priority, 9);
    }
}
