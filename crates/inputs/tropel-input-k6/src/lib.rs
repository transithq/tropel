//! # tropel-input-k6
//!
//! Input adapter + Driver for k6-style JavaScript/TypeScript test scripts.
//!
//! Provides two entry points:
//! - **InputAdapter** (declarative): wraps transpiled JS as a single-item
//!   Scenario, for backward compatibility.
//! - **Driver** (imperative): creates per-VU JS contexts, bootstraps shims,
//!   and runs the user's exported default function per iteration.
//!
//! The Driver is tried first by the engine's input resolution. The InputAdapter
//! serves as a fallback for older execution paths.

pub mod driver;
mod options;

use std::path::Path;
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

/// Input adapter for k6-style JS/TS test scripts.
pub struct K6ScriptAdapter;

/// Is this text an ACTUAL Postman collection? Structural check mirroring the
/// Postman adapter's detect(): a JSON document whose top-level `info.schema`
/// points at the getpostman.com collection schema. Substring matching is
/// forbidden (backlog line 61) — a k6 script hitting k6's own documented
/// postman-echo.com endpoint legitimately contains "postman". Shared by both
/// detect() copies in this crate so they can never drift; the Postman
/// adapter's detect() is the canonical third copy (cross-crate).
pub(crate) fn is_postman_collection(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let schema = value
        .get("info")
        .and_then(|info| info.get("schema"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    schema.contains("getpostman.com") && schema.contains("collection")
}

impl InputAdapter for K6ScriptAdapter {
    fn id(&self) -> &str {
        "k6"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        if let Ok(text) = std::str::from_utf8(bytes) {
            // Detect by looking for k6-like patterns:
            // - `export default function` (k6 default export)
            // - `import { ... } from "k6/..."` (k6 module import)
            // - Common test patterns like `http.get`, `check`, `group`
            //
            // We're lenient here — just check for JS/TS source characteristics
            // that wouldn't be a Postman collection or HAR file. Reject ACTUAL
            // Postman collections (handled by the Postman adapter) using the
            // SAME STRUCTURAL check that adapter uses — a JSON doc whose
            // top-level info.schema points at getpostman.com. Substring
            // matching is forbidden (backlog line 61): a k6 script hitting
            // k6's own documented postman-echo.com endpoint legitimately
            // contains "postman" and was rejected.
            if is_postman_collection(text) {
                return false;
            }

            let has_export_default = text.contains("export default");
            let has_k6_import = text.contains("from \"k6/") || text.contains("from 'k6/");
            let has_test_patterns = text.contains("http.get")
                || text.contains("http.post")
                || text.contains("check(")
                || text.contains("group(");

            has_export_default || has_k6_import || has_test_patterns
        } else {
            false
        }
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        // Without a file path, treat as a plain JS file.
        // TypeScript and ESM imports won't be resolved, but plain JS works.
        let _source = std::str::from_utf8(bytes)
            .map_err(|_| TropelError::Parse("k6 script is not valid UTF-8".into()))?;
        build_scenario_from_source(_source, "script")
    }

    fn parse_with_path(&self, bytes: &[u8], source_path: Option<&Path>) -> Result<Scenario> {
        // Validate UTF-8
        std::str::from_utf8(bytes)
            .map_err(|_| TropelError::Parse("k6 script is not valid UTF-8".into()))?;

        let path = source_path.unwrap_or(Path::new("script.js"));
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("script")
            .to_string();

        let js_code = tropel_es::transpile_file(path)
            .map_err(|e| TropelError::Parse(format!("k6 script transpilation failed: {}", e)))?;

        build_scenario_from_source(&js_code, &name)
    }
}

/// Build a Scenario from transpiled JS source code.
fn build_scenario_from_source(js_code: &str, name: &str) -> Result<Scenario> {
    // Wrap the transpiled code in a self-executing function so it runs
    // regardless of request execution status.
    let wrapped_code = format!(
        "(function() {{\n{}    }})();",
        js_code
            .lines()
            .map(|l| format!("    {}", l))
            .collect::<Vec<_>>()
            .join("\n")
    );

    Ok(Scenario {
        info: ScenarioInfo {
            name: name.to_string(),
            description: Some(format!("k6 script: {}", name)),
            schema: None,
        },
        items: vec![ScenarioItem {
            id: None,
            name: name.to_string(),
            request: None,
            prerequest: vec![],
            test: vec![wrapped_code],
            assertions: vec![],
            items: vec![],
        }],
        variables: std::collections::HashMap::new(),
        auth: None,
    })
}

// Register K6ScriptAdapter for compile-time discovery by the engine.
inventory::submit!(
    InputAdapterRegistration::new("k6", || Box::new(K6ScriptAdapter)).with_priority(10)
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_k6_export_default() {
        let adapter = K6ScriptAdapter;
        let data = br#"export default function() { http.get("https://example.com"); }"#;
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_k6_import() {
        let adapter = K6ScriptAdapter;
        let data = br#"import { check } from "k6"; export default function() {}"#;
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_postman_not_k6() {
        let adapter = K6ScriptAdapter;
        let data = br#"{"info":{"name":"Test","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(
            !adapter.detect(data),
            "Postman JSON should not be detected as k6"
        );
    }

    #[test]
    fn test_detect_plain_js_http() {
        let adapter = K6ScriptAdapter;
        let data = br#"http.get("https://example.com");"#;
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_k6_script_hitting_postman_echo() {
        // THE regression (backlog line 61): the OLD detect() rejected any
        // text containing the substring "postman". k6's own documented
        // example hits postman-echo.com, so a perfectly valid k6 script
        // failed detection and fell through to the wrong adapter.
        let adapter = K6ScriptAdapter;
        let data = br#"import http from "k6/http";
export default function () {
  http.get("https://postman-echo.com/get");
}"#;
        assert!(
            adapter.detect(data),
            "k6 script hitting postman-echo.com must be detected as k6"
        );
    }

    #[test]
    fn test_detect_k6_script_with_item_word() {
        // Same class: the OLD code also rejected any text containing
        // `"item"` — a k6 script iterating an `items` array / object with
        // a quoted "item" key was rejected. Structural collection detection
        // must not false-positive on ordinary JS.
        let adapter = K6ScriptAdapter;
        let data = br#"import http from "k6/http";
export default function () {
  const items = [{ "item": "a" }, { "item": "b" }];
  for (const it of items) { http.get("https://example.com/" + it.item); }
}"#;
        assert!(
            adapter.detect(data),
            "k6 script containing a quoted 'item' key must still be detected"
        );
    }

    #[test]
    fn test_detect_real_postman_collection_still_rejected() {
        // Sanity: the structural check must still reject a REAL Postman
        // collection (info.schema → getpostman.com) so it routes to the
        // Postman adapter, never the k6 one.
        let adapter = K6ScriptAdapter;
        let data = br#"{"info":{"name":"Test","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(
            !adapter.detect(data),
            "a real Postman collection must not be detected as k6"
        );
    }
}
