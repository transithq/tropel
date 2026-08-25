//! `tropel-input-wasm` — the lazy collection-import slice for browser
//! embedders (KnockPort). See `API_CLIENT_WEB_PAYLOAD.md` §2.3: the eager
//! `tropel-core-wasm` tier stays small (variables + auth, under its budget
//! gate), while the bulky collection parsers live HERE, loaded only when the
//! import UI opens.
//!
//! Exports mirror the engine's `ExtensionRegistry::resolve_input` dispatch:
//! iterate the registered input adapters in priority order and pick the
//! HIGHEST-priority one whose `detect()` claims the bytes (ties → listed
//! order). The adapter set is enumerated explicitly (not via `inventory`)
//! so the dispatch is deterministic and independent of link order — same
//! guarantee the engine's explicit-priority design provides natively.
//!
//! The output is a protocol-agnostic `Scenario` JSON (the `tropel-sdk`
//! shape: `info` / `items` / `variables` / `auth`); the embedder maps it to
//! its own collection model in TypeScript (KnockPort's `packages/format`).

use tropel_sdk::{InputAdapter, Scenario};
use wasm_bindgen::prelude::*;

// Pure, testable core — the wasm-bindgen exports below are thin wrappers.
// Kept separate because `JsValue` cannot be constructed on native test
// builds (wasm-bindgen stubs panic), so all parse logic lives here and the
// native tests exercise THIS layer; the exports are covered end-to-end by
// packages/input-wasm/smoke.mjs against the real wasm.

fn err_text(e: impl std::fmt::Display) -> String {
    // ASCII-only (same rationale as tropel-core-wasm): the web-target glue
    // truncates strings at their UTF-16 code unit count, so any multi-byte
    // character would corrupt the message at the JS boundary.
    e.to_string()
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}

/// Highest-priority adapter whose `detect()` claims the bytes, if any.
/// P1 line 152: ADAPTERS are sorted by priority descending so the
/// highest-priority adapter (postman=40) is checked first. On a match
/// we still scan all adapters to find the true highest priority, but
/// the common case (postman) hits on the first probe.
fn resolve(bytes: &[u8]) -> Option<Box<dyn InputAdapter>> {
    let mut best: Option<(u8, Box<dyn InputAdapter>)> = None;
    for (_, priority, create) in ADAPTERS {
        let adapter = create();
        if adapter.detect(bytes) && best.as_ref().map(|(p, _)| *priority > *p).unwrap_or(true) {
            best = Some((*priority, adapter));
        }
    }
    best.map(|(_, adapter)| adapter)
}

/// Detect the input format id (`"openapi"`, `"postman"`, `"har"`,
/// `"insomnia"`, `"bru"`) or `""`.
pub(crate) fn detect_impl(bytes: &[u8]) -> String {
    resolve(bytes)
        .map(|adapter| adapter.id().to_string())
        .unwrap_or_default()
}

/// Auto-detect + parse → Scenario.
pub(crate) fn import_any_impl(bytes: &[u8]) -> Result<Scenario, String> {
    let adapter = resolve(bytes).ok_or_else(|| {
        "Unrecognized import - expected OpenAPI, Swagger 2.0, Postman collection, Insomnia export, Bruno collection, or HAR".to_string()
    })?;
    adapter.parse(bytes).map_err(err_text)
}

/// Explicit-format parse → Scenario.
pub(crate) fn import_by_id_impl(id: &str, bytes: &[u8]) -> Result<Scenario, String> {
    let create = ADAPTERS
        .iter()
        .find(|(adapter_id, _, _)| *adapter_id == id)
        .map(|(_, _, create)| create)
        .ok_or_else(|| format!("unknown import format: {id}"))?;
    create().parse(bytes).map_err(err_text)
}

fn err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&err_text(e))
}

/// A single (id, priority, factory) slot in the adapter dispatch table.
type AdapterEntry = (&'static str, u8, fn() -> Box<dyn InputAdapter>);

/// The collection parsers exposed by this slice, in dispatch order.
/// Priority mirrors each input crate's `with_priority` registration:
/// postman 40, har 30, insomnia 35, openapi 20, bru 26. `detect` claims must
/// be mutually exclusive (structural checks, no substring matching) — see
/// each crate. bru is 26, NOT 25, so it does not tie with the `http` file
/// adapter (TR-007: both registering at 25 made the tie-break
/// link-order-dependent).
// P1 line 152: sorted by priority DESCENDING so the highest-priority
// adapter (postman=40) is probed first. The old order was arbitrary
// and har(30)/bru(25) were checked before postman(40) for no reason.
const ADAPTERS: &[AdapterEntry] = &[
    // TR-263: the wasm/browser tier parses SUBMITTED (untrusted)
    // collections — use the untrusted Postman adapter so mode:"file"
    // bodies / form-data file parts cannot read arbitrary paths.
    ("postman", 40, || {
        Box::new(tropel_input_postman::PostmanUntrustedInputAdapter)
    }),
    ("insomnia", 35, || {
        Box::new(tropel_input_insomnia::InsomniaInputAdapter)
    }),
    ("har", 30, || Box::new(tropel_input_har::HarInputAdapter)),
    ("bru", 26, || Box::new(tropel_input_bru::BruInputAdapter)),
    ("openapi", 20, || {
        Box::new(tropel_input_openapi::OpenApiInputAdapter)
    }),
];

/// Detect the input format: returns the adapter id (`"openapi"`, `"postman"`
/// or `"har"`) when the bytes are recognized, otherwise an empty string.
/// Safe to call before init resolution matters — no allocation beyond the
/// adapter probe.
#[wasm_bindgen(js_name = "detect")]
pub fn detect(bytes: &[u8]) -> String {
    detect_impl(bytes)
}

/// Parse arbitrary import bytes via content auto-detection → Scenario JSON.
/// Dispatches to the highest-priority adapter whose `detect()` claims the
/// bytes; errors when nothing matches or the matched parser rejects the
/// content.
#[wasm_bindgen(js_name = "importAny")]
pub fn import_any(bytes: &[u8]) -> Result<String, JsValue> {
    import_any_impl(bytes)
        .and_then(|scenario| serde_json::to_string(&scenario).map_err(|e| e.to_string()))
        .map_err(err)
}

/// Parse import bytes as an explicitly-named format (`"openapi"`, `"postman"`,
/// `"har"`, `"insomnia"`, `"bru"`) → Scenario JSON. Skips detection; errors
/// when the format is unknown or the parser rejects the content.
#[wasm_bindgen(js_name = "importById")]
pub fn import_by_id(id: &str, bytes: &[u8]) -> Result<String, JsValue> {
    import_by_id_impl(id, bytes)
        .and_then(|scenario| serde_json::to_string(&scenario).map_err(|e| e.to_string()))
        .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSTMAN: &[u8] = br#"{
        "info": {
            "name": "Test Collection",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": [
            {
                "name": "GET Users",
                "request": {"method": "GET", "url": {"raw": "https://api.example.com/users"}}
            }
        ]
    }"#;

    const OPENAPI: &[u8] = br#"{
        "openapi": "3.0.3",
        "info": {"title": "Pets", "version": "1.0.0"},
        "paths": {
            "/pets": {
                "get": {
                    "summary": "List pets",
                    "responses": {"200": {"description": "ok"}}
                }
            }
        }
    }"#;

    const HAR: &[u8] = br#"{
        "log": {
            "version": "1.2",
            "entries": [{
                "request": {"method": "GET", "url": "https://example.com/", "headers": [], "queryString": []},
                "response": {"status": 200, "statusText": "OK"}
            }]
        }
    }"#;

    const INSOMNIA: &[u8] = br#"{
        "_type": "export",
        "__export_format": 4,
        "resources": [
            {"_type": "workspace", "_id": "wrk_1", "name": "Pets API", "parentId": null},
            {"_type": "request", "_id": "req_1", "parentId": "wrk_1", "name": "List pets", "method": "GET", "url": "https://api.example.com/pets"}
        ]
    }"#;

    const BRU: &[u8] = br#"{
        "version": "1",
        "uid": "c1",
        "name": "Pets API",
        "items": [
            {"uid": "r1", "type": "http-request", "name": "List pets", "request": {"url": "https://api.example.com/pets", "method": "GET"}}
        ]
    }"#;

    #[test]
    fn detect_is_exclusive() {
        assert_eq!(detect_impl(POSTMAN), "postman");
        assert_eq!(detect_impl(OPENAPI), "openapi");
        assert_eq!(detect_impl(HAR), "har");
        assert_eq!(detect_impl(INSOMNIA), "insomnia");
        assert_eq!(detect_impl(BRU), "bru");
        assert_eq!(detect_impl(b"hello"), "");
    }

    #[test]
    fn import_any_dispatches_and_round_trips() {
        let postman = import_any_impl(POSTMAN).unwrap();
        assert_eq!(postman.info.name, "Test Collection");
        assert_eq!(postman.items.len(), 1);
        assert_eq!(
            postman.items[0].request.as_ref().unwrap().method.as_str(),
            "GET"
        );

        let openapi = import_any_impl(OPENAPI).unwrap();
        assert_eq!(openapi.info.name, "Pets");
        assert_eq!(openapi.items.len(), 1);

        let har = import_any_impl(HAR).unwrap();
        assert_eq!(har.items.len(), 1);

        let insomnia = import_any_impl(INSOMNIA).unwrap();
        assert_eq!(insomnia.info.name, "Pets API");
        assert_eq!(insomnia.items.len(), 1);
        assert_eq!(
            insomnia.items[0].request.as_ref().unwrap().method.as_str(),
            "GET"
        );

        let bru = import_any_impl(BRU).unwrap();
        assert_eq!(bru.info.name, "Pets API");
        assert_eq!(bru.items.len(), 1);
    }

    #[test]
    fn import_by_id_is_explicit() {
        let s = import_by_id_impl("openapi", OPENAPI).unwrap();
        assert_eq!(s.info.name, "Pets");
        assert!(import_by_id_impl("postman", OPENAPI).is_err());
        assert_eq!(
            import_by_id_impl("insomnia", INSOMNIA).unwrap().items.len(),
            1
        );
        assert_eq!(import_by_id_impl("bru", BRU).unwrap().items.len(), 1);
        assert!(import_by_id_impl("bogus", OPENAPI).is_err());
    }

    /// TR-007: the wasm dispatch table must mirror each crate's `with_priority`
    /// registration, and priorities must be pairwise distinct. `bru` and `http`
    /// both used to register at 25 (tie-break link-order-dependent); bru is now
    /// 26 everywhere — here AND in tropel-input-bru.
    #[test]
    fn dispatch_table_priorities_are_distinct_and_mirror_native() {
        let mut priorities: Vec<u8> = ADAPTERS.iter().map(|(_, p, _)| *p).collect();
        let before = priorities.len();
        priorities.sort_unstable();
        priorities.dedup();
        assert_eq!(
            priorities.len(),
            before,
            "dispatch priorities must be distinct: {:?}",
            ADAPTERS
                .iter()
                .map(|(id, p, _)| (*id, *p))
                .collect::<Vec<_>>()
        );
        let bru_prio = ADAPTERS
            .iter()
            .find(|(id, _, _)| *id == "bru")
            .map(|(_, p, _)| *p)
            .expect("bru in dispatch table");
        assert_eq!(bru_prio, 26, "bru must be 26, distinct from http's 25");
    }
}
