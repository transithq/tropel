//! `tropel-core-wasm` — the eager-loaded core tier for browser embedders
//! (KnockPort). wasm32-unknown-unknown + wasm-bindgen; deliberately NO
//! QuickJS: see `API_CLIENT_WEB_PAYLOAD.md` §2.3 (two-tier wasm). The
//! website/web app only ever talks to HTTP through a relay (CORS), so the
//! heavy `tropel-web` (wasip1 + QuickJS) scenario slice is extension/native
//! territory; this crate covers the pure compute the page always needs —
//! starting with the dynamic-variable catalog.
//!
//! Exports are thin adapters over `tropel-variables` — the catalog itself is
//! NOT duplicated here, so the website, the extension and the native runner
//! cannot drift.

use std::sync::OnceLock;

use tropel_variables::{DynamicCatalog, PREDEFINED_VARIABLE_META};
use wasm_bindgen::prelude::*;

static CATALOG: OnceLock<DynamicCatalog> = OnceLock::new();

fn catalog() -> &'static DynamicCatalog {
    CATALOG.get_or_init(DynamicCatalog::new)
}

/// Resolve every predefined dynamic variable (`{{$guid}}`, `{{$timestamp}}`,
/// …) in the input. Each occurrence generates a fresh value; unknown `{{$…}}`
/// names survive as literal placeholders (Tropel semantics). Plain `{{var}}`
/// references are untouched — the embedder resolves those against its own
/// environment/collection maps.
#[wasm_bindgen(js_name = "resolveVariables")]
pub fn resolve_variables(input: &str) -> String {
    catalog().resolve(input)
}

/// Catalog metadata as a JSON string: `[{"name":"$guid","description":…},…]`.
/// Feed the names into editor autocomplete; the descriptions into tooltips.
#[wasm_bindgen(js_name = "predefinedVariablesMeta")]
pub fn predefined_variables_meta() -> String {
    let entries = PREDEFINED_VARIABLE_META
        .iter()
        .map(|m| format!("{{\"name\":\"{}\",\"description\":\"{}\"}}", m.name, m.description));
    format!("[{}]", entries.collect::<Vec<_>>().join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_guid() {
        let out = resolve_variables("id={{$guid}}");
        assert!(out.starts_with("id="));
        let guid = out.strip_prefix("id=").unwrap();
        assert_eq!(guid.len(), 36);
        assert!(!out.contains("{{$guid}}"));
    }

    #[test]
    fn plain_vars_survive() {
        assert_eq!(resolve_variables("{{baseUrl}}/x"), "{{baseUrl}}/x");
    }

    #[test]
    fn meta_is_well_formed_json() {
        let json = predefined_variables_meta();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.len() >= 30, "metadata covers the catalog");
        assert_eq!(parsed[0]["name"], "$guid");
        for entry in &parsed {
            assert!(entry["name"].as_str().unwrap().starts_with('$'));
            assert!(!entry["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn meta_names_all_resolve() {
        let json = predefined_variables_meta();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        for entry in parsed {
            let name = entry["name"].as_str().unwrap();
            // Parameterized entries carry an argument in their description
            // example (`{{$randomString:16}}`); resolve the bare form.
            let resolved = catalog().resolve(&format!("{{{{{name}}}}}"));
            assert!(!resolved.contains(&format!("{{{{{name}}}}}")), "{name} must resolve");
        }
    }
}
