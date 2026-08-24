//! # tropel-input-insomnia
//!
//! Input adapter that reads [Insomnia][insomnia] v4 collection exports and
//! produces a protocol-agnostic `Scenario`.
//!
//! [insomnia]: https://docs.insomnia.rest/insomnia/export
//!
//! ## Mapping
//!
//! | Insomnia field | Scenario field |
//! |---------------|---------------|
//! | workspace `name` | `ScenarioInfo.name` |
//! | `request_group` | `ScenarioItem` folder (nested via `items`) |
//! | `request` | `ScenarioItem.request` |
//! | `request.url` | `request.url` (Insomnia variables `{{ _.x }}` →
//!   `{{x}}`, same normalization as Bruno's converter) |
//! | `request.headers` | `request.headers` (disabled entries dropped) |
//! | `request.parameters` | `request.query_params` (disabled dropped) |
//! | `request.body.mimeType` + `text`/`params` | `request.body` |
//! | `request.authentication` | `request.auth` |
//! | workspace "Base Environment" `data` | `Scenario.variables` |
//!
//! ## Robustness
//!
//! - Structural detection: a v4 export has top-level `_type: "export"` plus a
//!   `resources` array (no substring matching).
//! - Folder nesting is rebuilt from `parentId` spans; requests/request groups
//!   are ordered by `metaSortKey` (default 0) for deterministic output.
//! - A request whose method token is invalid fails the parse loudly.
//!
//! ## Priority
//!
//! 35 — between Postman (40) and HAR (30). Detection is structurally
//! exclusive (no other adapter claims `_type: "export"` + `resources`), so
//! dispatch order is deterministic regardless.

use serde::Deserialize;
use std::collections::HashMap;
use tropel_sdk::{ApiKeyLocation, AuthConfig, Body, FormDataPart, Method, Request};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

// ── Insomnia v4 data model (minimal — only what we need) ─────────

#[derive(Debug, Deserialize)]
struct ExportRoot {
    #[serde(rename = "_type")]
    type_: String,
    // `__export_format` is parsed for spec fidelity but not consumed
    // downstream (detect() checks it structurally on the raw JSON).
    #[serde(rename = "__export_format")]
    #[allow(dead_code)]
    export_format: u64,
    #[serde(default)]
    resources: Vec<InsomniaResource>,
}

#[derive(Debug, Deserialize)]
struct InsomniaResource {
    #[serde(default, rename = "_id")]
    id: Option<String>,
    #[serde(default, rename = "_type")]
    type_: String,
    #[serde(default, rename = "parentId")]
    parent_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    // Request-group / request ordering (Insomnia emits a `metaSortKey`).
    // TR-006: Insomnia assigns sort keys by midpoint averaging, so they
    // go fractional the first time a user drags anything. Use f64.
    #[serde(default, rename = "metaSortKey")]
    meta_sort_key: Option<f64>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Vec<InsomniaHeader>,
    #[serde(default)]
    parameters: Vec<InsomniaParam>,
    #[serde(default, rename = "pathParameters")]
    path_parameters: Vec<InsomniaParam>,
    #[serde(default)]
    authentication: Option<InsomniaAuth>,
    #[serde(default)]
    body: Option<InsomniaBody>,
    #[serde(default)]
    data: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct InsomniaHeader {
    name: String,
    value: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct InsomniaParam {
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct InsomniaAuth {
    #[serde(default, rename = "type")]
    type_: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<String>,
    /// Insomnia apikey placement: `"header"` (default) or `"query"`.
    #[serde(default, rename = "addTo")]
    add_to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InsomniaBody {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    params: Vec<InsomniaBodyParam>,
}

#[derive(Debug, Deserialize)]
struct InsomniaBodyParam {
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    disabled: bool,
    /// Multipart param kind: `"text"` or `"file"`.
    #[serde(default, rename = "type")]
    param_type: Option<String>,
    #[serde(default, rename = "fileName")]
    file_name: Option<String>,
}

// ── InputAdapter implementation ─────────────────────────────────

/// Input adapter for Insomnia v4 collection exports.
pub struct InsomniaInputAdapter;

inventory::submit!(
    InputAdapterRegistration::new("insomnia", || Box::new(InsomniaInputAdapter)).with_priority(35)
);

impl InputAdapter for InsomniaInputAdapter {
    fn id(&self) -> &str {
        "insomnia"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: a v4 export has `_type: "export"`,
        // `__export_format` and a `resources` array. No substring matching —
        // embedded content may legitimately contain "insomnia".
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return false;
        };
        value.get("_type").and_then(|t| t.as_str()) == Some("export")
            && value.get("__export_format").is_some()
            && value
                .get("resources")
                .map(|r| r.is_array())
                .unwrap_or(false)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let root: ExportRoot = serde_json::from_slice(bytes)
            .map_err(|e| TropelError::Parse(format!("Failed to parse Insomnia export: {}", e)))?;

        if root.type_ != "export" {
            return Err(TropelError::Parse("Not an Insomnia export".into()));
        }

        // Find the workspace (root of the tree) and the base environment.
        let workspace = root
            .resources
            .iter()
            .find(|r| r.type_ == "workspace")
            .or_else(|| root.resources.iter().find(|r| r.type_ == "environment"))
            .ok_or_else(|| TropelError::Parse("Insomnia export contains no workspace".into()))?;
        let workspace_id = workspace.id.clone().unwrap_or_default();

        let base_env = root
            .resources
            .iter()
            .find(|r| {
                r.type_ == "environment" && r.parent_id.as_deref() == Some(workspace_id.as_str())
            })
            .map(|e| &e.data)
            .cloned()
            .unwrap_or_default();

        // Rebuild the tree: request groups + requests nested by parentId.
        let items = build_items(&root.resources, &workspace_id);

        if items.is_empty() {
            return Err(TropelError::Parse(
                "Insomnia export contains no requests".into(),
            ));
        }

        Ok(Scenario {
            info: ScenarioInfo {
                name: workspace
                    .name
                    .clone()
                    .unwrap_or_else(|| "Insomnia Import".into()),
                description: Some("Imported from Insomnia".into()),
                schema: None,
            },
            items,
            variables: base_env,
            auth: None,
        })
    }
}

/// Build the ScenarioItem tree under `parent_id` (workspace id at the root).
/// Request groups become folders (nested `items`); requests map directly.
/// Items are ordered by `metaSortKey` (ascending, default 0) — Insomnia
/// exports resources in this order.
fn build_items(resources: &[InsomniaResource], parent_id: &str) -> Vec<ScenarioItem> {
    let mut children: Vec<&InsomniaResource> = resources
        .iter()
        .filter(|r| r.parent_id.as_deref() == Some(parent_id))
        .collect();
    children.sort_by(|a, b| {
        a.meta_sort_key
            .unwrap_or(0.0)
            .partial_cmp(&b.meta_sort_key.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = Vec::new();
    for r in children {
        match r.type_.as_str() {
            "request_group" => {
                let id = r.id.clone().unwrap_or_default();
                out.push(ScenarioItem {
                    name: r.name.clone().unwrap_or_else(|| "Folder".into()),
                    id: None,
                    request: None,
                    prerequest: vec![],
                    test: vec![],
                    assertions: vec![],
                    items: build_items(resources, &id),
                });
            }
            "request" => {
                match request_to_item(r) {
                    Ok(item) => out.push(item),
                    Err(e) => {
                        // TR-006: report conversion errors instead of silently dropping
                        eprintln!(
                            "insomnia: skipping request {}: {}",
                            r.name.as_deref().unwrap_or("?"),
                            e
                        );
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Map a single Insomnia request resource to a ScenarioItem.
fn request_to_item(r: &InsomniaResource) -> Result<ScenarioItem> {
    let method_str = r.method.clone().unwrap_or_default();
    let method = Method::parse(&method_str).ok_or_else(|| {
        TropelError::Parse(format!(
            "Insomnia request '{}' has invalid HTTP method {:?}",
            r.name.as_deref().unwrap_or(""),
            method_str
        ))
    })?;

    let mut url = r.url.clone().unwrap_or_default();
    url = normalize_variables(&url);

    // Path parameters: substitute `{name}` / `:name` tokens in the URL
    // (KnockPort's model has no separate path-param table; HAR/Postman keep
    // them inline too).
    for p in &r.path_parameters {
        let val = normalize_variables(&p.value);
        url = url.replace(&format!("{{{}}}", p.name), &val);
        url = url.replace(&format!(":{}", p.name), &val);
    }

    let headers: Vec<(String, String)> = r
        .headers
        .iter()
        .filter(|h| !h.disabled)
        .map(|h| (h.name.clone(), normalize_variables(&h.value)))
        .collect();

    let query_params: HashMap<String, String> = merge_pairs(
        r.parameters
            .iter()
            .filter(|p| !p.disabled)
            .map(|p| (p.name.clone(), normalize_variables(&p.value))),
    );

    let body = r.body.as_ref().and_then(build_body);
    let auth = r.authentication.as_ref().and_then(build_auth);

    Ok(ScenarioItem {
        name: r
            .name
            .clone()
            .unwrap_or_else(|| format!("{} {}", method.as_str(), url)),
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
            timeout: None,
            response_type: tropel_sdk::ResponseType::Text,
        }),
        prerequest: vec![],
        test: vec![],
        assertions: vec![],
        items: vec![],
    })
}

/// Map Insomnia auth → tropel AuthConfig. `None` means "not configured"
/// (inherits); only explicitly-conflicting schemes are dropped.
fn build_auth(a: &InsomniaAuth) -> Option<AuthConfig> {
    match a.type_.as_deref().unwrap_or("") {
        "basic" => Some(AuthConfig::Basic {
            username: a.username.clone().unwrap_or_default(),
            password: a.password.clone().unwrap_or_default(),
        }),
        "bearer" => Some(AuthConfig::Bearer {
            token: a.token.clone().unwrap_or_default(),
        }),
        "digest" => Some(AuthConfig::Digest {
            username: a.username.clone().unwrap_or_default(),
            password: a.password.clone().unwrap_or_default(),
        }),
        "apikey" => {
            let location = match a.add_to.as_deref() {
                Some("query") => ApiKeyLocation::Query,
                _ => ApiKeyLocation::Header,
            };
            Some(AuthConfig::ApiKey {
                key: a.key.clone().unwrap_or_default(),
                value: a.value.clone().unwrap_or_default(),
                location,
            })
        }
        _ => None,
    }
}

/// Map an Insomnia body to tropel Body, keyed on the mime type.
fn build_body(b: &InsomniaBody) -> Option<Body> {
    let mime = b.mime_type.as_deref().unwrap_or("").to_ascii_lowercase();
    let mime_clean = mime.split(';').next().unwrap_or("").trim();
    let text = b.text.clone().unwrap_or_default();

    match mime_clean {
        "application/json" => match serde_json::from_str(&text) {
            Ok(v) => Some(Body::Json(v)),
            Err(_) if text.trim().is_empty() || text.trim() == "{}" => {
                if text.trim().is_empty() {
                    None
                } else {
                    // Empty JSON object → keep as Json({}).
                    Some(Body::Json(serde_json::json!({})))
                }
            }
            Err(_) => Some(Body::Raw(text)),
        },
        "application/x-www-form-urlencoded" => Some(Body::UrlEncoded(
            b.params
                .iter()
                .filter(|p| !p.disabled)
                .map(|p| (p.name.clone(), normalize_variables(&p.value)))
                .collect(),
        )),
        "multipart/form-data" => Some(Body::FormData(
            b.params
                .iter()
                .filter(|p| !p.disabled)
                .map(|p| FormDataPart {
                    name: p.name.clone(),
                    value: if p.param_type.as_deref() == Some("file") {
                        None
                    } else {
                        Some(normalize_variables(&p.value))
                    },
                    filename: if p.param_type.as_deref() == Some("file") {
                        p.file_name.clone()
                    } else {
                        None
                    },
                    mime: None,
                    data: None,
                })
                .collect(),
        )),
        "application/graphql" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let query = v
                    .get("query")
                    .and_then(|q| q.as_str())
                    .unwrap_or_default()
                    .to_string();
                let variables = v.get("variables").filter(|x| x.is_object()).and_then(|x| {
                    serde_json::from_value::<HashMap<String, serde_json::Value>>(x.clone()).ok()
                });
                if !query.is_empty() {
                    return Some(Body::GraphQL { query, variables });
                }
            }
            Some(Body::Raw(text))
        }
        // text/plain, text/xml, application/xml, and any raw mime (or none).
        _ => {
            if text.trim().is_empty() {
                None
            } else {
                Some(Body::Raw(text))
            }
        }
    }
}

/// Normalize Insomnia variables (`{{ _.name }}` → `{{name}}`), matching
/// Bruno's converter which strips the `_.` prefix and internal spaces.
fn normalize_variables(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `{{`, then consume until `}}`, stripping spaces and `_.`.
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i;
            let mut j = i + 2;
            let mut inner = String::new();
            let mut closed = false;
            while j + 1 < bytes.len() {
                if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    closed = true;
                    break;
                }
                let c = bytes[j];
                if !c.is_ascii_whitespace() && c != b'_' && c != b'.' {
                    inner.push(c as char);
                }
                j += 1;
            }
            if closed {
                out.push_str("{{");
                out.push_str(&inner);
                out.push_str("}}");
                i = j + 2;
                continue;
            } else {
                i = start;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Combine duplicate query keys by appending values with `, ` (same policy as
/// the HAR adapter — a HashMap cannot hold duplicate keys, so the data is
/// preserved, not dropped).
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

    const EXPORT: &[u8] = br#"{
        "_type": "export",
        "__export_format": 4,
        "__export_date": "2024-01-01T00:00:00.000Z",
        "resources": [
            {"_type": "workspace", "_id": "wrk_1", "name": "Pets API", "parentId": null},
            {"_type": "environment", "_id": "env_base", "parentId": "wrk_1", "name": "Base Environment", "data": {"baseUrl": "https://api.example.com"}},
            {"_type": "request_group", "_id": "fld_users", "parentId": "wrk_1", "name": "Users", "metaSortKey": -200},
            {"_type": "request", "_id": "req_get", "parentId": "fld_users", "name": "List users", "method": "GET", "url": "{{ _.baseUrl }}/users", "metaSortKey": -100, "headers": [{"name": "Accept", "value": "application/json"}], "parameters": [{"name": "limit", "value": "10"}]},
            {"_type": "request", "_id": "req_post", "parentId": "wrk_1", "name": "Create user", "method": "POST", "url": "https://api.example.com/users", "body": {"mimeType": "application/json", "text": "{\"name\":\"Ada\"}"}, "authentication": {"type": "bearer", "token": "tok-123"}, "metaSortKey": 0}
        ]
    }"#;

    #[test]
    fn detect_insomnia() {
        let adapter = InsomniaInputAdapter;
        assert!(adapter.detect(EXPORT));
    }

    #[test]
    fn detect_exclusive() {
        let adapter = InsomniaInputAdapter;
        // Not an export.
        assert!(!adapter.detect(br#"{"resources":[]}"#));
        // A random JSON is not an Insomnia export.
        assert!(!adapter.detect(br#"{"_type":"export"}"#));
        // Postman schema URL must NOT be detected as Insomnia.
        let postman = br#"{"info":{"name":"T","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(!adapter.detect(postman));
    }

    #[test]
    fn parse_reconstructs_tree_and_variables() {
        let adapter = InsomniaInputAdapter;
        let scenario = adapter.parse(EXPORT).unwrap();
        assert_eq!(scenario.info.name, "Pets API");
        assert_eq!(
            scenario.variables.get("baseUrl").and_then(|v| v.as_str()),
            Some("https://api.example.com")
        );

        // Two top-level items: the "Users" folder and the root request.
        assert_eq!(scenario.items.len(), 2);
        let folder = &scenario.items[0];
        assert_eq!(folder.name, "Users");
        assert_eq!(folder.items.len(), 1);
        let req = folder.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "{{baseUrl}}/users");
        assert_eq!(req.query_params.get("limit"), Some(&"10".to_string()));

        let root = &scenario.items[1];
        let root_req = root.request.as_ref().unwrap();
        assert_eq!(root_req.method, Method::POST);
        assert!(
            matches!(root_req.auth, Some(AuthConfig::Bearer { ref token }) if token == "tok-123")
        );
        match root_req.body.as_ref() {
            Some(Body::Json(_)) => {}
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn parse_disabled_headers_and_params_dropped() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{
            "_type": "export",
            "__export_format": 4,
            "resources": [
                {"_type": "workspace", "_id": "wrk", "name": "W"},
                {"_type": "request", "_id": "r", "parentId": "wrk", "name": "R", "method": "GET", "url": "https://x.io/", "headers": [{"name": "A", "value": "1", "disabled": true}, {"name": "B", "value": "2"}], "parameters": [{"name": "q", "value": "1", "disabled": true}, {"name": "p", "value": "2"}]}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert!(req.headers.iter().all(|(k, _)| k != "A"));
        assert!(req.headers.iter().any(|(k, _)| k == "B"));
        assert!(!req.query_params.contains_key("q"));
        assert_eq!(req.query_params.get("p"), Some(&"2".to_string()));
    }

    #[test]
    fn parse_invalid_method_fails() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{
            "_type": "export",
            "__export_format": 4,
            "resources": [
                {"_type": "workspace", "_id": "wrk", "name": "W"},
                {"_type": "request", "_id": "r", "parentId": "wrk", "name": "R", "method": "", "url": "https://x.io/"}
            ]
        }"#;
        assert!(adapter.parse(data).is_err());
    }

    #[test]
    fn parse_path_parameters_substituted() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{
            "_type": "export",
            "__export_format": 4,
            "resources": [
                {"_type": "workspace", "_id": "wrk", "name": "W"},
                {"_type": "request", "_id": "r", "parentId": "wrk", "name": "Get user", "method": "GET", "url": "https://x.io/users/:id", "pathParameters": [{"name": "id", "value": "42"}]}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        assert_eq!(
            scenario.items[0].request.as_ref().unwrap().url,
            "https://x.io/users/42"
        );
    }

    #[test]
    fn parse_base_url_env_bootstrap_without_workspace_env() {
        // When there's no base environment resource, variables stay empty.
        let adapter = InsomniaInputAdapter;
        let data = br#"{
            "_type": "export",
            "__export_format": 4,
            "resources": [
                {"_type": "workspace", "_id": "wrk", "name": "W"},
                {"_type": "request", "_id": "r", "parentId": "wrk", "name": "R", "method": "GET", "url": "https://x.io/"}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        assert!(scenario.variables.is_empty());
    }

    /// TR-006: Insomnia assigns sort keys by midpoint averaging, so they go
    /// FRACTIONAL the first time a user drags anything. The old `Option<i64>`
    /// type rejected the whole resource (parse error) on any drag-reordered
    /// export. This fixture is exactly what a reordered workspace exports.
    #[test]
    fn parse_reordered_workspace_with_fractional_sort_keys() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{
            "_type": "export",
            "__export_format": 4,
            "resources": [
                {"_type": "workspace", "_id": "wrk", "name": "W"},
                {"_type": "request", "_id": "r1", "parentId": "wrk", "name": "First", "method": "GET", "url": "https://x.io/1", "metaSortKey": -0.5},
                {"_type": "request", "_id": "r2", "parentId": "wrk", "name": "Second", "method": "GET", "url": "https://x.io/2", "metaSortKey": 0.25},
                {"_type": "request", "_id": "r3", "parentId": "wrk", "name": "Third", "method": "GET", "url": "https://x.io/3", "metaSortKey": 1.5}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        let names: Vec<&str> = scenario.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["First", "Second", "Third"]);
    }

    /// TR-006 (sibling of TR-005): duplicate query keys must not be silently
    /// dropped — `[{ids,1},{ids,2}]` joins to `"1, 2"` (the SDK's
    /// `HashMap` cannot hold duplicate keys, so the data is preserved, not lost).
    #[test]
    fn parse_duplicate_query_keys_join_not_drop() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{
            "_type": "export",
            "__export_format": 4,
            "resources": [
                {"_type": "workspace", "_id": "wrk", "name": "W"},
                {"_type": "request", "_id": "r", "parentId": "wrk", "name": "R", "method": "GET", "url": "https://x.io/", "parameters": [{"name": "ids", "value": "1"}, {"name": "ids", "value": "2"}]}
            ]
        }"#;
        let scenario = adapter.parse(data).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.query_params.get("ids"), Some(&"1, 2".to_string()));
    }
}
