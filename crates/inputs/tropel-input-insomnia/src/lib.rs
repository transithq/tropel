//! # tropel-input-insomnia
//!
//! Input adapter that reads [Insomnia][insomnia] v4 export JSON (Desktop app
//! → Export → Insomnia v4) and produces a protocol-agnostic `Scenario`.
//!
//! [insomnia]: https://insomnia.rest
//!
//! ## Mapping
//!
//! | Insomnia resource | Scenario field |
//! |-------------------|-----------------|
//! | `workspace` | `info.name` (root of the tree) |
//! | `request_group` | folder `ScenarioItem` (`request: None`, children in `items`) |
//! | `request` | request `ScenarioItem` |
//! | `request.parameters` | query (deduped → `query_params`, duplicates folded into the URL) |
//! | `request.headers` | `request.headers` (disabled dropped, duplicates `, `-joined) |
//! | `request.body` by `mimeType` | `Body::Json` / `UrlEncoded` / `FormData` / `GraphQL` / `Raw` |
//! | `request.authentication` | `Authorization` header (bearer/basic) or apikey header/param |
//! | `request.scripts.before` / `.after` | `prerequest` / post-response `test` phase |
//! | `environment` (base + subs) | merged into `Scenario.variables` |
//! | `{{ _.name }}` / `{{ name }}` templates | normalized to `{{name}}` everywhere |
//!
//! Sub-environments override their parents (Insomnia's chain order); the
//! workspace's base environment applies first. `api_spec` / `unit_test`
//! resources are ignored.

use serde::Deserialize;
use std::collections::HashMap;
use tropel_sdk::{Body, Method, Request};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

// ── Insomnia export model (minimal) ─────────────────────────────

#[derive(Debug, Deserialize)]
struct ExportRoot {
    resources: Vec<Resource>,
}

#[derive(Debug, Deserialize)]
struct Resource {
    #[serde(rename = "_type")]
    kind: String,
    #[serde(rename = "_id")]
    id: String,
    #[serde(default, rename = "parentId")]
    parent_id: Option<String>,
    #[serde(default)]
    name: String,
    // request
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    parameters: Vec<InsomniaParam>,
    #[serde(default)]
    headers: Vec<InsomniaParam>,
    #[serde(default)]
    authentication: Option<InsomniaAuth>,
    #[serde(default)]
    body: Option<InsomniaBody>,
    #[serde(default)]
    scripts: Option<InsomniaScripts>,
    // environment
    #[serde(default)]
    data: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct InsomniaParam {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    disabled: bool,
    // multipart file params carry fileName instead of value
    #[serde(default, rename = "fileName")]
    file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum InsomniaAuth {
    Bearer {
        #[serde(default)]
        token: String,
    },
    Basic {
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    },
    Apikey {
        #[serde(default)]
        key: String,
        #[serde(default)]
        value: String,
        #[serde(default)]
        r#in: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct InsomniaBody {
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    params: Vec<InsomniaParam>,
}

#[derive(Debug, Deserialize)]
struct InsomniaScripts {
    #[serde(default)]
    before: String,
    #[serde(default)]
    after: String,
}

// ── InputAdapter implementation ─────────────────────────────────

/// Input adapter for Insomnia v4 export JSON.
pub struct InsomniaInputAdapter;

inventory::submit!(
    InputAdapterRegistration::new("insomnia", || Box::new(InsomniaInputAdapter)).with_priority(28)
);

impl InputAdapter for InsomniaInputAdapter {
    fn id(&self) -> &str {
        "insomnia"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: a JSON object with `_type: "export"`,
        // `__export_format: 4` and a `resources` array.
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return false;
        };
        value.get("_type").and_then(|v| v.as_str()) == Some("export")
            && value.get("__export_format").and_then(|v| v.as_i64()) == Some(4)
            && value
                .get("resources")
                .map(|r| r.is_array())
                .unwrap_or(false)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let root: ExportRoot = serde_json::from_slice(bytes)
            .map_err(|e| TropelError::Parse(format!("Failed to parse Insomnia export: {e}")))?;

        let workspace = root
            .resources
            .iter()
            .find(|r| r.kind == "workspace")
            .ok_or_else(|| TropelError::Parse("Insomnia export has no workspace".into()))?;

        // Environments: base (parented by the workspace) then subs (parented
        // by an environment), later overriding earlier — export order keeps
        // parents before children, matching Insomnia's chain.
        let mut variables: HashMap<String, serde_json::Value> = HashMap::new();
        let env_by_id: HashMap<&str, &Resource> = root
            .resources
            .iter()
            .filter(|r| r.kind == "environment")
            .map(|r| (r.id.as_str(), r))
            .collect();
        for env in root.resources.iter().filter(|r| r.kind == "environment") {
            let parented_by_workspace = env.parent_id.as_deref() == Some(workspace.id.as_str());
            let parented_by_env = env
                .parent_id
                .as_deref()
                .map(|p| env_by_id.contains_key(p))
                .unwrap_or(false);
            if parented_by_workspace || parented_by_env {
                if let Some(data) = &env.data {
                    variables.extend(data.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
            }
        }

        // parentId → children (requests + groups), export order preserved.
        let mut children_of: HashMap<String, Vec<&Resource>> = HashMap::new();
        for r in root.resources.iter() {
            if r.kind != "request" && r.kind != "request_group" {
                continue;
            }
            let parent = r.parent_id.clone().unwrap_or_default();
            children_of.entry(parent).or_default().push(r);
        }

        let items = build_children(&children_of, &workspace.id, 0)?;
        if items.is_empty() {
            return Err(TropelError::Parse(
                "Insomnia export contains no requests".into(),
            ));
        }

        Ok(Scenario {
            info: ScenarioInfo {
                name: workspace.name.clone(),
                description: Some("Imported from Insomnia v4 export".into()),
                schema: None,
            },
            items,
            variables,
            auth: None,
        })
    }
}

/// Recursively build the folder/item tree for one parent id.
fn build_children(
    children_of: &HashMap<String, Vec<&Resource>>,
    parent: &str,
    depth: usize,
) -> Result<Vec<ScenarioItem>> {
    const MAX_DEPTH: usize = 64; // cycle guard for malformed parentId loops
    if depth > MAX_DEPTH {
        return Err(TropelError::Parse(
            "Insomnia export has a parentId cycle".into(),
        ));
    }
    let mut items = Vec::new();
    for r in children_of.get(parent).into_iter().flatten() {
        if r.kind == "request_group" {
            items.push(ScenarioItem {
                name: r.name.clone(),
                id: Some(r.id.clone()),
                request: None,
                prerequest: vec![],
                test: vec![],
                assertions: vec![],
                items: build_children(children_of, &r.id, depth + 1)?,
            });
        } else {
            items.push(request_to_item(r)?);
        }
    }
    Ok(items)
}

/// Convert an Insomnia request resource into a `ScenarioItem`.
fn request_to_item(r: &Resource) -> Result<ScenarioItem> {
    let method = Method::parse(r.method.as_deref().unwrap_or("GET")).ok_or_else(|| {
        TropelError::Parse(format!(
            "Insomnia request {:?} has invalid method {:?}",
            r.name,
            r.method.clone().unwrap_or_default()
        ))
    })?;
    let mut url = normalize_template(r.url.as_deref().unwrap_or(""));

    // Query parameters: deduped sets go to query_params; duplicate keys fold
    // into the URL to preserve order/repeats (HAR-adapter semantics).
    let enabled_params: Vec<(String, String)> = r
        .parameters
        .iter()
        .filter(|p| !p.disabled && !p.name.is_empty())
        .map(|p| (p.name.clone(), normalize_value(&p.value)))
        .collect();
    let has_dupes = {
        let mut seen = std::collections::HashSet::new();
        enabled_params.iter().any(|(k, _)| !seen.insert(k.clone()))
    };
    let query_params = if has_dupes || url.contains('?') {
        if !enabled_params.is_empty() && !url.contains('?') {
            let qs: Vec<String> = enabled_params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        HashMap::new()
    } else {
        enabled_params.into_iter().collect()
    };

    // Headers: disabled dropped, duplicates joined with `, `.
    let mut headers: HashMap<String, String> = HashMap::new();
    for p in &r.headers {
        if p.disabled || p.name.is_empty() {
            continue;
        }
        let value = normalize_value(&p.value);
        match headers.get_mut(&p.name) {
            Some(existing) => {
                existing.push_str(", ");
                existing.push_str(&value);
            }
            None => {
                headers.insert(p.name.clone(), value);
            }
        }
    }

    // Auth → header / query param. An explicit Authorization header wins.
    let mut extra_query: Vec<(String, String)> = Vec::new();
    let has_auth_header = headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("authorization"));
    if !has_auth_header {
        match r.authentication.as_ref() {
            Some(InsomniaAuth::Bearer { token }) => {
                headers.insert(
                    "Authorization".into(),
                    format!("Bearer {}", normalize_value(token)),
                );
            }
            Some(InsomniaAuth::Basic { username, password }) => {
                let creds = format!(
                    "{}:{}",
                    normalize_value(username),
                    normalize_value(password)
                );
                headers.insert(
                    "Authorization".into(),
                    format!("Basic {}", base64_encode(creds.as_bytes())),
                );
            }
            Some(InsomniaAuth::Apikey { key, value, r#in }) => {
                if !key.is_empty() {
                    let value = normalize_value(value);
                    if r#in == "query" {
                        extra_query.push((key.clone(), value));
                    } else {
                        headers.insert(key.clone(), value);
                    }
                }
            }
            _ => {}
        }
    }
    for (k, v) in extra_query {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!("{k}={v}"));
    }

    let body = r.body.as_ref().map(build_body);

    // Insomnia scripts: `before` = pre-request, `after` = post-response
    // (tropel's `test` phase runs post-response).
    let prerequest = r
        .scripts
        .as_ref()
        .map(|s| s.before.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s])
        .unwrap_or_default();
    let test = r
        .scripts
        .as_ref()
        .map(|s| s.after.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s])
        .unwrap_or_default();

    Ok(ScenarioItem {
        name: r.name.clone(),
        id: Some(r.id.clone()),
        request: Some(Request {
            url,
            method,
            headers,
            query_params,
            body,
            auth: None,
            certificate: None,
            follow_redirects: true,
            timeout: None,
            response_type: tropel_sdk::ResponseType::Text,
        }),
        prerequest,
        test,
        assertions: vec![],
        items: vec![],
    })
}

/// Map an Insomnia body by mimeType to a `Body` variant.
fn build_body(b: &InsomniaBody) -> Body {
    let mime = b.mime_type.to_lowercase();
    if mime.contains("json") {
        match serde_json::from_str::<serde_json::Value>(&b.text) {
            Ok(v) => Body::Json(v),
            // Invalid JSON under a json mime → verbatim raw (no re-quoting).
            Err(_) => Body::Raw(b.text.clone()),
        }
    } else if mime.contains("x-www-form-urlencoded") {
        Body::UrlEncoded(
            b.params
                .iter()
                .filter(|p| !p.disabled && !p.name.is_empty())
                .map(|p| (p.name.clone(), normalize_value(&p.value)))
                .collect(),
        )
    } else if mime.contains("multipart/form-data") {
        Body::FormData(
            b.params
                .iter()
                .filter(|p| !p.disabled && !p.name.is_empty())
                .map(|p| {
                    let value = if p.value.is_empty() {
                        // File parts store the file name (content needs FS
                        // context we don't have — keep the name visible).
                        p.file_name.clone().unwrap_or_default()
                    } else {
                        normalize_value(&p.value)
                    };
                    (p.name.clone(), value)
                })
                .collect(),
        )
    } else if mime.contains("graphql") {
        Body::GraphQL {
            query: b.text.clone(),
            variables: None,
        }
    } else {
        Body::Raw(b.text.clone())
    }
}

/// Normalize an Insomnia template to the `{{name}}` form used downstream:
/// strips inner whitespace and the `_.` variable prefix
/// (`{{ _.token }}` / `{{ token }}` → `{{token}}`).
fn normalize_value(s: &str) -> String {
    normalize_template(s)
}

fn normalize_template(s: &str) -> String {
    if !s.contains("{{") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let Some(end_rel) = rest[start..].find("}}") else {
            out.push_str(rest);
            return out;
        };
        let end = start + end_rel;
        out.push_str(&rest[..start]);
        let inner = rest[start + 2..end].trim().trim_start_matches("_.");
        out.push_str("{{");
        out.push_str(inner);
        out.push_str("}}");
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Minimal base64 encoder (mirrors the other adapters).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn export(resources: &str) -> String {
        format!(
            r#"{{"_type":"export","__export_format":4,"__export_date":"2026-01-01","resources":[{resources}]}}"#
        )
    }

    #[test]
    fn test_detect() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{"_type":"export","__export_format":4,"resources":[]}"#;
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_rejects_postman() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{"info":{"name":"x","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_detect_rejects_wrong_format_version() {
        let adapter = InsomniaInputAdapter;
        let data = br#"{"_type":"export","__export_format":3,"resources":[]}"#;
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_parse_simple_request() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"My Workspace"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"List users","method":"GET","url":"https://x.dev/users"}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        assert_eq!(s.info.name, "My Workspace");
        assert_eq!(s.items.len(), 1);
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "https://x.dev/users");
    }

    #[test]
    fn test_parse_folder_tree() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request_group","_id":"fld_1","parentId":"wrk_1","name":"Auth"},
               {"_type":"request","_id":"req_1","parentId":"fld_1","name":"Login","method":"POST","url":"https://x.dev/login"},
               {"_type":"request","_id":"req_2","parentId":"wrk_1","name":"Ping","method":"GET","url":"https://x.dev/ping"}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        assert_eq!(s.items.len(), 2);
        // Folders keep export order relative to sibling requests.
        let folder = s.items.iter().find(|i| i.request.is_none()).unwrap();
        assert_eq!(folder.name, "Auth");
        assert_eq!(folder.items.len(), 1);
        assert_eq!(folder.items[0].name, "Login");
    }

    #[test]
    fn test_parse_nested_folders() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request_group","_id":"fld_1","parentId":"wrk_1","name":"A"},
               {"_type":"request_group","_id":"fld_2","parentId":"fld_1","name":"B"},
               {"_type":"request","_id":"req_1","parentId":"fld_2","name":"Deep","method":"GET","url":"https://x.dev/d"}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let a = &s.items[0];
        let b = &a.items[0];
        assert_eq!(b.name, "B");
        assert_eq!(b.items[0].name, "Deep");
    }

    #[test]
    fn test_parse_parameters_to_query() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"Q","method":"GET","url":"https://x.dev/search",
                "parameters":[{"name":"q","value":"a b","disabled":false},{"name":"verbose","value":"1","disabled":true}]}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.query_params.get("q").unwrap(), "a b");
        assert!(!req.query_params.contains_key("verbose"));
        assert_eq!(req.url, "https://x.dev/search");
    }

    #[test]
    fn test_parse_duplicate_query_params_folded_into_url() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"Q","method":"GET","url":"https://x.dev/search",
                "parameters":[{"name":"tag","value":"a"},{"name":"tag","value":"b"}]}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://x.dev/search?tag=a&tag=b");
        assert!(req.query_params.is_empty());
    }

    #[test]
    fn test_parse_headers_disabled_and_duplicates() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"H","method":"GET","url":"https://x.dev/a",
                "headers":[{"name":"Accept","value":"application/json","disabled":false},
                           {"name":"X-Trace","value":"1","disabled":false},
                           {"name":"X-Trace","value":"2","disabled":false},
                           {"name":"X-Off","value":"x","disabled":true}]}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("Accept").unwrap(), "application/json");
        assert_eq!(req.headers.get("X-Trace").unwrap(), "1, 2");
        assert!(!req.headers.contains_key("X-Off"));
    }

    #[test]
    fn test_parse_json_body() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"C","method":"POST","url":"https://x.dev/users",
                "body":{"mimeType":"application/json","text":"{\"name\": \"alice\"}"}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Json(v) => assert_eq!(v["name"], "alice"),
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_urlencoded_body() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"F","method":"POST","url":"https://x.dev/login",
                "body":{"mimeType":"application/x-www-form-urlencoded","params":[
                   {"name":"user","value":"alice","disabled":false},
                   {"name":"off","value":"x","disabled":true}]}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::UrlEncoded(f) => {
                assert_eq!(f.get("user").unwrap(), "alice");
                assert!(!f.contains_key("off"));
            }
            other => panic!("Expected Body::UrlEncoded, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_multipart_body_keeps_filenames() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"U","method":"POST","url":"https://x.dev/up",
                "body":{"mimeType":"multipart/form-data","params":[
                   {"name":"field","value":"v","type":"string"},
                   {"name":"file","value":"","fileName":"doc.pdf","type":"file"}]}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::FormData(f) => {
                assert_eq!(f.get("field").unwrap(), "v");
                assert_eq!(f.get("file").unwrap(), "doc.pdf");
            }
            other => panic!("Expected Body::FormData, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_graphql_body() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"G","method":"POST","url":"https://x.dev/graphql",
                "body":{"mimeType":"application/graphql","text":"query { user { name } }"}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::GraphQL { query, .. } => assert!(query.contains("user")),
            other => panic!("Expected Body::GraphQL, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_bearer_auth() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"B","method":"GET","url":"https://x.dev/me",
                "authentication":{"type":"bearer","token":"tok123"}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("Authorization").unwrap(), "Bearer tok123");
    }

    #[test]
    fn test_parse_basic_auth() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"B","method":"GET","url":"https://x.dev/me",
                "authentication":{"type":"basic","username":"u","password":"p"}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        let expected = base64_encode(b"u:p");
        assert_eq!(
            req.headers.get("Authorization").unwrap(),
            &format!("Basic {expected}")
        );
    }

    #[test]
    fn test_parse_apikey_in_query() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"K","method":"GET","url":"https://x.dev/a",
                "authentication":{"type":"apikey","key":"api_key","value":"xyz","in":"query"}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://x.dev/a?api_key=xyz");
        assert!(!req.headers.contains_key("api_key"));
    }

    #[test]
    fn test_parse_environments_merged() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"environment","_id":"env_base","parentId":"wrk_1","name":"Base","data":{"base_url":"https://x.dev","token":"t1"}},
               {"_type":"environment","_id":"env_sub","parentId":"env_base","name":"Prod","data":{"base_url":"https://prod.x.dev"}},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"R","method":"GET","url":"{{ base_url }}/a"}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        // Sub-environment overrides the base; untouched keys survive.
        assert_eq!(
            s.variables.get("base_url"),
            Some(&serde_json::Value::String("https://prod.x.dev".into()))
        );
        assert_eq!(
            s.variables.get("token"),
            Some(&serde_json::Value::String("t1".into()))
        );
        // Templates normalized to {{name}}.
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "{{base_url}}/a");
    }

    #[test]
    fn test_template_normalization_underscore_and_spaces() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"T","method":"GET","url":"{{ _.host }}/x?tok={{ token }}"}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "{{host}}/x?tok={{token}}");
    }

    #[test]
    fn test_parse_scripts() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"workspace","_id":"wrk_1","name":"W"},
               {"_type":"request","_id":"req_1","parentId":"wrk_1","name":"S","method":"GET","url":"https://x.dev/a",
                "scripts":{"before":"insomnia.send();","after":"console.log(response.code);"}}"#,
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let item = &s.items[0];
        assert_eq!(item.prerequest, vec!["insomnia.send();".to_string()]);
        assert_eq!(item.test, vec!["console.log(response.code);".to_string()]);
    }

    #[test]
    fn test_no_workspace_errors() {
        let adapter = InsomniaInputAdapter;
        let data = export(
            r#"{"_type":"request","_id":"req_1","parentId":null,"name":"x","method":"GET","url":"https://x.dev/a"}"#,
        );
        assert!(adapter.parse(data.as_bytes()).is_err());
    }

    #[test]
    fn test_no_requests_errors() {
        let adapter = InsomniaInputAdapter;
        let data = export(r#"{"_type":"workspace","_id":"wrk_1","name":"Empty"}"#);
        assert!(adapter.parse(data.as_bytes()).is_err());
    }
}
