//! Embedder-facing sandbox configuration (P4b).
//!
//! The script sandbox exposes a **canonical binding** — **`trp.*`**, by
//! Postman convention (`pm.*` is Postman's sole namespace, so `trp.*` is
//! Tropel's; see `TROPEL_MODULARIZATION_TODO.md` P4b) — plus an optional set
//! of **aliases** that are true references to the same object (identical
//! identity, never proxies). `pm.*` is always installed as the frozen
//! Postman-compat peer view; this config only controls the canonical name
//! and its aliases.
//!
//! Third-party embedders (e.g. the API client, or anyone building on
//! `tropel-sandbox`) set the canonical binding name here — that name is what
//! appears in user-visible error strings and on `globalThis` — and wire it
//! through by evaluating [`SandboxConfig::render_js_preamble`] BEFORE the
//! pm.js shim bundle runs.

/// Which namespaces a script context exposes, and under what names.
///
/// - [`SandboxConfig::namespace`] — the canonical binding name. Defaults to
///   `"trp"` (Postman convention); embedders set their own (e.g. the API
///   client's product name).
/// - [`SandboxConfig::aliases`] — extra globals that reference the SAME
///   object as the canonical binding (true aliases, not proxies). Defaults
///   to `[]` — aliases are opt-in per embedder, not part of the stock install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Canonical binding name installed on `globalThis`. Default: `"trp"`.
    pub namespace: String,
    /// Names aliased to the canonical binding (identical object identity).
    /// Default: `[]` (opt-in).
    pub aliases: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            namespace: "trp".into(),
            aliases: Vec::new(),
        }
    }
}

impl SandboxConfig {
    /// Render the JS preamble that installs
    /// `globalThis.__tropel_sandbox_config` for the pm.js install tail to
    /// consume. Evaluate this BEFORE the shim bundle so the canonical name
    /// and aliases are in place when `pm.js` installs its bindings.
    ///
    /// The default config renders to a no-op equivalent of the stock
    /// install (`trp` canonical, no aliases), so the preamble is always safe
    /// to emit.
    pub fn render_js_preamble(&self) -> String {
        let aliases = self
            .aliases
            .iter()
            .map(|a| js_quote(a))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "globalThis.__tropel_sandbox_config = {{ namespace: {}, aliases: [{}] }};",
            js_quote(&self.namespace),
            aliases
        )
    }
}

/// Quote a string as a JS string literal. JSON string escaping is exactly
/// JS-safe for double-quoted string literals.
fn js_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_trp_canonical_only() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.namespace, "trp");
        assert!(cfg.aliases.is_empty());
        assert_eq!(
            cfg.render_js_preamble(),
            "globalThis.__tropel_sandbox_config = { namespace: \"trp\", aliases: [] };"
        );
    }

    #[test]
    fn custom_config_renders_namespace_and_aliases() {
        let cfg = SandboxConfig {
            namespace: "acme".into(),
            aliases: vec!["wire".into(), "product".into()],
        };
        assert_eq!(
            cfg.render_js_preamble(),
            "globalThis.__tropel_sandbox_config = { namespace: \"acme\", aliases: [\"wire\", \"product\"] };"
        );
    }

    #[test]
    fn hostile_names_are_js_escaped() {
        let cfg = SandboxConfig {
            namespace: "my\"prod\"\\x".into(),
            aliases: Vec::new(),
        };
        let preamble = cfg.render_js_preamble();
        assert!(
            preamble.contains("\"my\\\"prod\\\"\\\\x\""),
            "quotes and backslashes must be escaped: {preamble}"
        );
    }
}
