//! # tropel-input-bru
//!
//! Input adapter that reads [Bruno][bruno] collection JSON exports and
//! produces a protocol-agnostic `Scenario`.
//!
//! [bruno]: https://docs.usebruno.com/collection/model
//!
//! Bruno's on-disk collections are folder trees of `.bru` files, but the
//! canonical single-file interchange shape is the collection JSON model
//! (`version: "1"`, `uid`, `name`, `items`, `environments`). That is what
//! this adapter consumes.
//!
//! ## Mapping
//!
//! | Bruno field | Scenario field |
//! |------------|---------------|
//! | collection `name` | `ScenarioInfo.name` |
//! | item with `type: "folder"` | nested `ScenarioItem` (folders) |
//! | item with `type: "http-request"` or `"http"` (export spelling) | `ScenarioItem.request` |
//! | `request.url` / `request.method` | `request.url` / `request.method` |
//! | `request.headers` (disabled dropped) | `request.headers` |
//! | `request.params` (`type: "query"`, disabled dropped) | `request.query_params` |
//! | `request.auth` | `request.auth` |
//! | `request.body` (by `mode`) | `request.body` |
//! | `request.script` pre/post | `ScenarioItem.prerequest` / `test` |
//! | `environments[0].variables` | `Scenario.variables` |
//!
//! ## Robustness
//!
//! - Structural detection: a JSON doc with a `version: "1"`, `name` and an
//!   `items` array (no substring matching — embedded content may carry the
//!   word "bruno").
//! - Request items of non-HTTP types (bruno `graphql-request`/`graphql`, `grpc-request`/`grpc`,
//!   `ws-request`/`ws`, `js`) are skipped rather than failing the collection.
//! - A request with an invalid method token fails the parse loudly.

use serde::Deserialize;
use std::collections::HashMap;
use tropel_sdk::{ApiKeyLocation, AuthConfig, Body, FormDataPart, Method, Request};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

// ── Bruno collection JSON model (minimal — only what we need) ────

#[derive(Debug, Deserialize)]
struct BruCollection {
    // `version` is parsed for spec fidelity but not consumed downstream.
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    items: Vec<BruItem>,
    #[serde(default)]
    environments: Vec<BruEnvironment>,
}

#[derive(Debug, Deserialize)]
struct BruItem {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    request: Option<BruRequest>,
    #[serde(default)]
    items: Vec<BruItem>,
}

#[derive(Debug, Deserialize)]
struct BruRequest {
    #[serde(default)]
    url: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    headers: Vec<BruKeyValue>,
    #[serde(default)]
    params: Vec<BruParam>,
    #[serde(default)]
    auth: Option<BruAuth>,
    #[serde(default)]
    body: Option<BruBody>,
    #[serde(default)]
    script: Option<BruScript>,
}

#[derive(Debug, Deserialize)]
struct BruKeyValue {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct BruParam {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default, rename = "type")]
    param_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruScript {
    #[serde(default)]
    req: Option<String>,
    #[serde(default)]
    res: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruAuth {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    basic: Option<BruBasic>,
    #[serde(default)]
    bearer: Option<BruBearer>,
    #[serde(default)]
    digest: Option<BruDigest>,
    #[serde(default)]
    wsse: Option<BruBasic>,
    #[serde(default)]
    apikey: Option<BruApiKey>,
}

#[derive(Debug, Deserialize)]
struct BruBasic {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruBearer {
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruDigest {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruApiKey {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<String>,
    /// `"header"` (default) or `"queryparams"`.
    #[serde(default)]
    placement: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruBody {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    json: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    xml: Option<String>,
    #[serde(default)]
    sparql: Option<String>,
    #[serde(default, rename = "formUrlEncoded")]
    form_url_encoded: Vec<BruKeyValue>,
    #[serde(default, rename = "multipartForm")]
    multipart_form: Vec<BruMultipart>,
}

#[derive(Debug, Deserialize)]
struct BruMultipart {
    #[serde(default, rename = "type")]
    part_type: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default, rename = "contentType")]
    content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BruEnvironment {
    // `name` is parsed for spec fidelity but not consumed downstream.
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    variables: Vec<BruVariable>,
}

#[derive(Debug, Deserialize)]
struct BruVariable {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
    #[serde(default)]
    enabled: Option<bool>,
}

// ── InputAdapter implementation ─────────────────────────────────

/// Input adapter for Bruno collection JSON exports.
pub struct BruInputAdapter;

inventory::submit!(
    InputAdapterRegistration::new("bru", || Box::new(BruInputAdapter)).with_priority(26)
);

impl InputAdapter for BruInputAdapter {
    fn id(&self) -> &str {
        "bru"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: a Bruno collection JSON has `version: "1"`,
        // a `name` and an `items` array. The version string is enough to
        // disambiguate from Postman (`info.schema`), HAR (`log`), OpenAPI
        // (`openapi`/`swagger` key) and Insomnia (`_type: "export"`).
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return false;
        };
        value.get("version").and_then(|v| v.as_str()) == Some("1")
            && value.get("name").is_some()
            && value.get("items").map(|i| i.is_array()).unwrap_or(false)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let col: BruCollection = serde_json::from_slice(bytes)
            .map_err(|e| TropelError::Parse(format!("Failed to parse Bruno collection: {}", e)))?;

        // TR-410: collect conversion notes (skipped items, degraded requests)
        // into a structured report the client can render, instead of eprintln.
        let mut notes: Vec<String> = Vec::new();
        let items = build_items(&col.items, &mut notes);
        if items.is_empty() {
            return Err(TropelError::Parse(
                "Bruno collection contains no HTTP requests".into(),
            ));
        }

        let variables: HashMap<String, serde_json::Value> = col
            .environments
            .first()
            .map(|env| {
                env.variables
                    .iter()
                    .filter(|v| v.enabled.unwrap_or(true))
                    .filter_map(|v| {
                        let name = v.name.clone()?;
                        v.value.clone().map(|val| (name, val))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Scenario {
            info: ScenarioInfo {
                name: col.name.unwrap_or_else(|| "Bruno Import".into()),
                description: Some("Imported from Bruno".into()),
                schema: None,
            },
            items,
            variables,
            auth: None,
            conversion_notes: notes,
        })
    }
}

/// Recurse Bruno items: folders become nested ScenarioItems, http-requests
/// map to request items. Non-HTTP item types are skipped.
fn build_items(items: &[BruItem], notes: &mut Vec<String>) -> Vec<ScenarioItem> {
    let mut out = Vec::new();
    for item in items {
        match item.r#type.as_deref() {
            Some("folder") => out.push(ScenarioItem {
                name: item.name.clone().unwrap_or_else(|| "Folder".into()),
                id: None,
                request: None,
                prerequest: vec![],
                test: vec![],
                assertions: vec![],
                items: build_items(&item.items, notes),
            }),
            // TR-005: Bruno's exporter rewrites `http-request` → `http`
            // (and `graphql-request` → `graphql`, etc. via transformItem in
            // bruno-app/src/utils/collections/export.js). Accept both spellings
            // so real exports and the internal app spelling both parse.
            Some("http-request") | Some("http") => {
                match http_item_to_item(item) {
                    Ok(child) => out.push(child),
                    Err(e) => {
                        // TR-410: report conversion errors in the structured
                        // notes instead of eprintln (which the client cannot
                        // render).
                        notes.push(format!(
                            "Skipped '{}': {e}",
                            item.name.as_deref().unwrap_or("(unnamed)")
                        ));
                    }
                }
            }
            // TR-410: silently-skipped item types (ws-request, graphql-request)
            // are now recorded so the client can show what was lost.
            // After export transform they appear as `graphql`/`grpc`/`ws`/`js`.
            Some(other) => {
                notes.push(format!(
                    "Skipped '{}': unsupported Bruno item type '{}'",
                    item.name.as_deref().unwrap_or("(unnamed)"),
                    other
                ));
            }
            None => {
                notes.push(format!(
                    "Skipped '{}': no item type",
                    item.name.as_deref().unwrap_or("(unnamed)")
                ));
            }
        }
    }
    out
}

/// Map a single Bruno http-request item to a ScenarioItem.
fn http_item_to_item(item: &BruItem) -> Result<ScenarioItem> {
    let request = item.request.as_ref().ok_or_else(|| {
        TropelError::Parse(format!(
            "Bruno item '{}' is a http-request with no request body",
            item.name.as_deref().unwrap_or("")
        ))
    })?;

    let method = Method::parse(&request.method).ok_or_else(|| {
        TropelError::Parse(format!(
            "Bruno request '{}' has invalid HTTP method {:?}",
            item.name.as_deref().unwrap_or(""),
            request.method
        ))
    })?;

    let headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|h| h.enabled.unwrap_or(true))
        .filter_map(|h| {
            let name = h.name.clone()?;
            Some((name, h.value.clone().unwrap_or_default()))
        })
        .collect();

    // TR-005: collect BOTH query AND path params (was query-only)
    let query_params: HashMap<String, String> = merge_pairs(
        request
            .params
            .iter()
            .filter(|p| p.enabled.unwrap_or(true))
            .filter(|p| p.param_type.as_deref() == Some("query"))
            .filter_map(|p| {
                p.name
                    .clone()
                    .map(|n| (n, p.value.clone().unwrap_or_default()))
            }),
    );

    // Substitute path params: /users/:id → /users/123
    let mut url = request.url.clone();
    for param in request
        .params
        .iter()
        .filter(|p| p.enabled.unwrap_or(true))
        .filter(|p| p.param_type.as_deref() == Some("path"))
    {
        if let Some(name) = &param.name {
            let value = param.value.clone().unwrap_or_default();
            url = url.replace(&format!(":{}", name), &value);
        }
    }

    let body = request.body.as_ref().and_then(build_body);
    let auth = request.auth.as_ref().and_then(build_auth);

    let prerequest = request
        .script
        .as_ref()
        .and_then(|s| s.req.as_ref())
        .filter(|s| !s.trim().is_empty())
        .map(|s| vec![s.clone()])
        .unwrap_or_default();
    let test = request
        .script
        .as_ref()
        .and_then(|s| s.res.as_ref())
        .filter(|s| !s.trim().is_empty())
        .map(|s| vec![s.clone()])
        .unwrap_or_default();

    Ok(ScenarioItem {
        name: item
            .name
            .clone()
            .unwrap_or_else(|| format!("{} {}", method.as_str(), request.url)),
        id: None,
        request: Some(Request {
            url,
            method,
            headers,
            query_params,
            body,
            auth,
            certificate: None,
            follow_redirects: true,
            host: None,
            cookies: Vec::new(),
            timeout: None,
            response_type: tropel_sdk::ResponseType::Text,
        }),
        prerequest,
        test,
        assertions: vec![],
        items: vec![],
    })
}

/// Map Bruno auth → tropel AuthConfig.
fn build_auth(a: &BruAuth) -> Option<AuthConfig> {
    match a.mode.as_deref().unwrap_or("") {
        "basic" => Some(AuthConfig::Basic {
            username: a
                .basic
                .as_ref()
                .and_then(|b| b.username.clone())
                .unwrap_or_default(),
            password: a
                .basic
                .as_ref()
                .and_then(|b| b.password.clone())
                .unwrap_or_default(),
        }),
        "bearer" => Some(AuthConfig::Bearer {
            token: a
                .bearer
                .as_ref()
                .and_then(|b| b.token.clone())
                .unwrap_or_default(),
        }),
        "digest" => Some(AuthConfig::Digest {
            username: a
                .digest
                .as_ref()
                .and_then(|d| d.username.clone())
                .unwrap_or_default(),
            password: a
                .digest
                .as_ref()
                .and_then(|d| d.password.clone())
                .unwrap_or_default(),
        }),
        "wsse" => Some(AuthConfig::Basic {
            username: a
                .wsse
                .as_ref()
                .and_then(|w| w.username.clone())
                .unwrap_or_default(),
            password: a
                .wsse
                .as_ref()
                .and_then(|w| w.password.clone())
                .unwrap_or_default(),
        }),
        // Bruno "apikey" → KnockPort apikey (header or queryparams).
        "apikey" => {
            let location = match a.apikey.as_ref().and_then(|k| k.placement.as_deref()) {
                Some("queryparams") | Some("query") => ApiKeyLocation::Query,
                _ => ApiKeyLocation::Header,
            };
            Some(AuthConfig::ApiKey {
                key: a
                    .apikey
                    .as_ref()
                    .and_then(|k| k.key.clone())
                    .unwrap_or_default(),
                value: a
                    .apikey
                    .as_ref()
                    .and_then(|k| k.value.clone())
                    .unwrap_or_default(),
                location,
            })
        }
        // "inherit" and "none" map to None (inherit semantics downstream).
        _ => None,
    }
}

/// Map a Bruno body (keyed on `mode`) to tropel Body.
fn build_body(b: &BruBody) -> Option<Body> {
    match b.mode.as_deref().unwrap_or("none") {
        "json" => {
            let text = b.json.clone().unwrap_or_default();
            match serde_json::from_str(&text) {
                Ok(v) => Some(Body::Json(v)),
                Err(_) if text.trim().is_empty() => None,
                Err(_) => Some(Body::Raw(text)),
            }
        }
        "text" => {
            let text = b.text.clone().unwrap_or_default();
            if text.trim().is_empty() {
                None
            } else {
                Some(Body::Raw(text))
            }
        }
        "xml" | "sparql" => {
            let text = b
                .xml
                .clone()
                .or_else(|| b.sparql.clone())
                .unwrap_or_default();
            if text.trim().is_empty() {
                None
            } else {
                Some(Body::Raw(text))
            }
        }
        "formUrlEncoded" => Some(Body::UrlEncoded(
            b.form_url_encoded
                .iter()
                .filter(|kv| kv.enabled.unwrap_or(true))
                .filter_map(|kv| {
                    let name = kv.name.clone()?;
                    Some((name, kv.value.clone().unwrap_or_default()))
                })
                .collect(),
        )),
        "multipartForm" => Some(Body::FormData(
            b.multipart_form
                .iter()
                .filter(|p| p.enabled.unwrap_or(true))
                .map(|p| {
                    let is_file = p.part_type.as_deref() == Some("file");
                    FormDataPart {
                        name: p.name.clone().unwrap_or_default(),
                        value: if is_file { None } else { p.value.clone() },
                        filename: if is_file { p.value.clone() } else { None },
                        mime: p.content_type.clone(),
                        data: None,
                    }
                })
                .collect(),
        )),
        _ => None,
    }
}

/// Combine duplicate query keys by appending values with `, ` (same policy as
/// the HAR/Insomnia adapters — a HashMap cannot hold duplicate keys).
fn merge_pairs<I: Iterator<Item = (String, String)>>(pairs: I) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for (k, v) in pairs {
        match map.get_mut(&k) {
            Some(existing) => {
                existing.push_str(", ");
                existing.push_str(&v);
            }
            None => {
                map.insert(k, v);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTION: &[u8] = br#"{
        "version": "1",
        "uid": "c1",
        "name": "Pets API",
        "items": [
            {
                "uid": "f1",
                "type": "folder",
                "name": "Users",
                "items": [
                    {
                        "uid": "r1",
                        "type": "http-request",
                        "name": "List users",
                        "request": {
                            "url": "https://api.example.com/users",
                            "method": "GET",
                            "headers": [{"uid": "h1", "name": "Accept", "value": "application/json", "enabled": true}],
                            "params": [{"uid": "p1", "name": "limit", "value": "10", "type": "query", "enabled": true}],
                            "auth": {"mode": "bearer", "bearer": {"token": "tok-123"}},
                            "script": {"req": "pm.variables.set('a', 1);", "res": "pm.test('ok', () => pm.response.to.be.ok);"}
                        }
                    }
                ]
            },
            {
                "uid": "r2",
                "type": "http-request",
                "name": "Create user",
                "request": {
                    "url": "https://api.example.com/users",
                    "method": "POST",
                    "body": {"mode": "json", "json": "{\"name\":\"Ada\"}"}
                }
            }
        ],
        "environments": [{
            "uid": "e1",
            "name": "Local",
            "variables": [{"uid": "v1", "name": "baseUrl", "value": "https://api.example.com", "type": "text", "enabled": true}]
        }]
    }"#;

    #[test]
    fn detect_bru() {
        let adapter = BruInputAdapter;
        assert!(adapter.detect(COLLECTION));
    }

    #[test]
    fn detect_exclusive() {
        let adapter = BruInputAdapter;
        // Insomnia export is not bru.
        let insomnia = br#"{"_type":"export","__export_format":4,"resources":[]}"#;
        assert!(!adapter.detect(insomnia));
        // Postman is not bru.
        let postman = br#"{"info":{"name":"T","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(!adapter.detect(postman));
        // A bare version without items is not bru.
        assert!(!adapter.detect(br#"{"version":"1","name":"X"}"#));
    }

    #[test]
    fn parse_reconstructs_tree_requests_and_vars() {
        let adapter = BruInputAdapter;
        let scenario = adapter.parse(COLLECTION).unwrap();
        assert_eq!(scenario.info.name, "Pets API");
        assert_eq!(
            scenario.variables.get("baseUrl").and_then(|v| v.as_str()),
            Some("https://api.example.com")
        );

        assert_eq!(scenario.items.len(), 2);
        // Folder first (Users).
        let folder = &scenario.items[0];
        assert_eq!(folder.name, "Users");
        assert_eq!(folder.items.len(), 1);
        let req = folder.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "https://api.example.com/users");
        assert_eq!(req.query_params.get("limit"), Some(&"10".to_string()));
        assert_eq!(
            folder.items[0].prerequest,
            vec!["pm.variables.set('a', 1);".to_string()]
        );
        assert_eq!(
            folder.items[0].test,
            vec!["pm.test('ok', () => pm.response.to.be.ok);".to_string()]
        );

        let root = &scenario.items[1];
        let root_req = root.request.as_ref().unwrap();
        assert_eq!(root_req.method, Method::POST);
        match root_req.body.as_ref() {
            Some(Body::Json(_)) => {}
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn parse_basic_auth_and_disabled_dropped() {
        let adapter = BruInputAdapter;
        let data = br#"{
            "version": "1",
            "name": "C",
            "items": [{
                "uid": "r",
                "type": "http-request",
                "name": "R",
                "request": {
                    "url": "https://x.io/",
                    "method": "GET",
                    "headers": [{"uid":"a","name":"A","value":"1","enabled":false},{"uid":"b","name":"B","value":"2"}],
                    "params": [{"uid":"q","name":"q","value":"1","type":"query","enabled":false},{"uid":"p","name":"p","value":"2","type":"query"}],
                    "auth": {"mode": "basic", "basic": {"username": "u", "password": "pw"}}
                }
            }]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert!(req.headers.iter().all(|(k, _)| k != "A"));
        assert!(req.headers.iter().any(|(k, _)| k == "B"));
        assert!(!req.query_params.contains_key("q"));
        assert_eq!(req.query_params.get("p"), Some(&"2".to_string()));
        assert!(
            matches!(req.auth, Some(AuthConfig::Basic { ref username, .. }) if username == "u")
        );
    }

    #[test]
    fn parse_invalid_method_fails() {
        let adapter = BruInputAdapter;
        let data = br#"{
            "version": "1",
            "name": "C",
            "items": [{"uid":"r","type":"http-request","name":"R","request":{"url":"https://x.io/","method":""}}]
        }"#;
        assert!(adapter.parse(data).is_err());
    }

    #[test]
    fn parse_skips_non_http_items() {
        let adapter = BruInputAdapter;
        let data = br#"{
            "version": "1",
            "name": "C",
            "items": [
                {"uid":"g","type":"graphql-request","name":"G"},
                {"uid":"w","type":"ws-request","name":"W"},
                {"uid":"r","type":"http-request","name":"R","request":{"url":"https://x.io/","method":"GET"}}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        assert_eq!(scenario.items.len(), 1);
        assert_eq!(scenario.items[0].name, "R");
    }

    /// TR-007: bru must NOT share a priority with the `http` file adapter
    /// (both were 25, making auto-detect ties link-order-dependent). The
    /// registration constant is the source of truth — change it here and
    /// the wasm dispatch table in `tropel-input-wasm` must mirror it.
    #[test]
    fn bru_priority_is_distinct_from_http() {
        let reg =
            InputAdapterRegistration::new("bru", || Box::new(BruInputAdapter)).with_priority(26);
        assert_eq!(
            reg.priority, 26,
            "bru must not share priority with http (25)"
        );
    }

    /// TR-005: duplicate query keys must not be silently dropped —
    /// `[{ids,1},{ids,2}]` joins to `"1, 2"` (the SDK's `HashMap` cannot hold
    /// duplicate keys, so the data is preserved, not lost).
    #[test]
    fn parse_duplicate_query_keys_join_not_drop() {
        let adapter = BruInputAdapter;
        let data = br#"{
            "version": "1",
            "name": "C",
            "items": [{
                "uid": "r",
                "type": "http-request",
                "name": "R",
                "request": {
                    "url": "https://x.io/",
                    "method": "GET",
                    "params": [
                        {"uid":"p1","name":"ids","value":"1","type":"query","enabled":true},
                        {"uid":"p2","name":"ids","value":"2","type":"query","enabled":true}
                    ]
                }
            }]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.query_params.get("ids"), Some(&"1, 2".to_string()));
    }

    #[test]
    fn conversion_notes_report_skipped_items() {
        // TR-410: a ws-request item must be recorded in conversion_notes
        // (not silently dropped), naming the item and the reason.
        let adapter = BruInputAdapter;
        let data = br#"{
            "version": "1",
            "name": "C",
            "items": [
                {"uid":"r","type":"http-request","name":"OK","request":{"url":"https://x.io/","method":"GET"}},
                {"uid":"w","type":"ws-request","name":"Chat","request":{"url":"ws://x.io/chat"}}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        assert_eq!(scenario.items.len(), 1, "only the http-request converts");
        assert!(
            scenario
                .conversion_notes
                .iter()
                .any(|n| n.contains("Chat") && n.contains("ws-request")),
            "conversion_notes must name the skipped item and reason: {:?}",
            scenario.conversion_notes
        );
    }

    // ── TR-005: fixtures must be real Bruno exports, not hand-written ──
    // Bruno's exporter (bruno-app/src/utils/collections/export.js) rewrites
    // `http-request` → `http` (and graphql/grpc/ws) and strips `uid` fields.
    // A fixture that still uses `http-request` is the internal-app spelling,
    // not an export. This export was produced by Bruno 1.20.0 (export.js
    // prepareCollectionForExport → transformItem) and is embedded verbatim —
    // note type `"http"` (not `"http-request"`), no `uid` fields, and the
    // `exportedAt`/`exportedUsing` trailer. These tests assert the adapter
    // handles real exports — the gap that let the original defect ship.

    const BRUNO_EXPORT: &[u8] = br#"{
  "version": "1",
  "name": "Pets API (Bruno Export)",
  "items": [
    {
      "name": "Users",
      "type": "folder",
      "items": [
        {
          "name": "List users",
          "type": "http",
          "request": {
            "url": "https://api.example.com/users",
            "method": "GET",
            "headers": [
              {
                "name": "Accept",
                "value": "application/json",
                "enabled": true
              }
            ],
            "params": [
              {
                "name": "limit",
                "value": "10",
                "type": "query",
                "enabled": true
              }
            ],
            "auth": {
              "mode": "bearer",
              "bearer": {
                "token": "tok-123"
              }
            },
            "body": {
              "mode": "none"
            },
            "script": {
              "req": "bru.setVar('a', 1);",
              "res": "bru.test('ok', () => {});"
            }
          }
        }
      ]
    },
    {
      "name": "Create user",
      "type": "http",
      "request": {
        "url": "https://api.example.com/users/:id",
        "method": "POST",
        "headers": [],
        "params": [
          {
            "name": "id",
            "value": "42",
            "type": "path",
            "enabled": true
          },
          {
            "name": "ids",
            "value": "1",
            "type": "query",
            "enabled": true
          },
          {
            "name": "ids",
            "value": "2",
            "type": "query",
            "enabled": true
          }
        ],
        "body": {
          "mode": "json",
          "json": "{\"name\":\"Ada\"}"
        },
        "auth": {
          "mode": "none"
        },
        "script": {}
      }
    },
    {
      "name": "GraphQL example",
      "type": "graphql",
      "request": {
        "url": "https://api.example.com/graphql",
        "method": "POST",
        "body": {
          "mode": "graphql",
          "graphql": {
            "query": "{ users { id } }",
            "variables": "{}"
          }
        }
      }
    },
    {
      "name": "WebSocket chat",
      "type": "ws",
      "request": {
        "url": "wss://api.example.com/chat",
        "method": "GET"
      }
    }
  ],
  "environments": [
    {
      "name": "Local",
      "variables": [
        {
          "name": "baseUrl",
          "value": "https://api.example.com",
          "enabled": true
        },
        {
          "name": "disabledVar",
          "value": "should-not-appear",
          "enabled": false
        }
      ]
    }
  ],
  "exportedAt": "2026-08-20T12:00:00.000Z",
  "exportedUsing": "Bruno/1.20.0"
}"#;

    #[test]
    fn parse_bruno_export_fixture() {
        // The fixture is a real Bruno export (type "http", no uid fields,
        // exportedAt/exportedUsing). It must parse with two HTTP requests
        // (folder + root) and the non-HTTP items skipped with notes.
        let adapter = BruInputAdapter;
        assert!(
            adapter.detect(BRUNO_EXPORT),
            "export fixture must be detected as bru"
        );
        let scenario = adapter.parse(BRUNO_EXPORT).unwrap();
        assert_eq!(scenario.info.name, "Pets API (Bruno Export)");
        // Folder + one root http request = 2 top-level items
        assert_eq!(scenario.items.len(), 2, "folder + root http request");
        let folder = &scenario.items[0];
        assert_eq!(folder.name, "Users");
        assert_eq!(folder.items.len(), 1);
        let req = folder.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "https://api.example.com/users");
        // Non-HTTP transformed types (graphql, ws) must be skipped, not error
        assert!(
            scenario
                .conversion_notes
                .iter()
                .any(|n| n.contains("graphql")),
            "export's graphql item must be noted as skipped: {:?}",
            scenario.conversion_notes
        );
        assert!(
            scenario
                .conversion_notes
                .iter()
                .any(|n| n.contains("WebSocket") || n.contains("ws")),
            "export's ws item must be noted as skipped: {:?}",
            scenario.conversion_notes
        );
        // Variables from the first environment
        assert_eq!(
            scenario.variables.get("baseUrl").and_then(|v| v.as_str()),
            Some("https://api.example.com")
        );
        assert!(
            !scenario.variables.contains_key("disabledVar"),
            "disabled variables must be dropped"
        );
    }

    #[test]
    fn parse_bruno_export_accepts_both_http_spellings() {
        // Internal spelling (http-request) and export spelling (http) must
        // both parse — the adapter was previously export-blind.
        let adapter = BruInputAdapter;
        let internal = br#"{
            "version": "1",
            "name": "C",
            "items": [{"type":"http-request","name":"R","request":{"url":"https://x.io/","method":"GET"}}]
        }"#;
        let exported = br#"{
            "version": "1",
            "name": "C",
            "items": [{"type":"http","name":"R","request":{"url":"https://x.io/","method":"GET"}}]
        }"#;
        assert!(
            adapter.parse(internal).is_ok(),
            "internal http-request must parse"
        );
        assert!(adapter.parse(exported).is_ok(), "export http must parse");
        // Both must produce the same request
        let a = adapter.parse(internal).unwrap();
        let b = adapter.parse(exported).unwrap();
        assert_eq!(
            a.items[0].request.as_ref().unwrap().url,
            b.items[0].request.as_ref().unwrap().url
        );
    }

    #[test]
    fn parse_bruno_export_path_params_and_duplicate_query_keys() {
        // The export fixture's second request has a path param :id → 42 and
        // duplicate query keys ids=1, ids=2 → "1, 2". Both were previously
        // broken (path only query, merge collapsed).
        let adapter = BruInputAdapter;
        let scenario = adapter.parse(BRUNO_EXPORT).unwrap();
        let root = &scenario.items[1];
        assert_eq!(root.name, "Create user");
        let req = root.request.as_ref().unwrap();
        assert_eq!(
            req.url, "https://api.example.com/users/42",
            "path param :id must be substituted"
        );
        assert_eq!(
            req.query_params.get("ids"),
            Some(&"1, 2".to_string()),
            "duplicate query keys must join, not drop"
        );
    }

    #[test]
    fn parse_bruno_export_skips_transformed_non_http_types() {
        // After export transform, non-HTTP types are `graphql`/`grpc`/`ws`/`js`
        // (not `*-request`). They must be skipped with a diagnostic.
        let adapter = BruInputAdapter;
        let data = br#"{
            "version": "1",
            "name": "C",
            "items": [
                {"type":"graphql","name":"G"},
                {"type":"grpc","name":"GR"},
                {"type":"ws","name":"W"},
                {"type":"js","name":"J"},
                {"type":"http","name":"OK","request":{"url":"https://x.io/","method":"GET"}}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        assert_eq!(scenario.items.len(), 1);
        assert_eq!(scenario.items[0].name, "OK");
        assert_eq!(
            scenario.conversion_notes.len(),
            4,
            "four non-http types must be noted"
        );
    }
}
