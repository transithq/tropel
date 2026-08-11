use crate::error::*;
use crate::model::*;
use std::collections::HashMap;
use tropel_sdk::scenario::{Scenario, ScenarioInfo, ScenarioItem};
use tropel_sdk::types::*;

/// Parse a Postman Collection from JSON bytes.
pub fn parse_collection(bytes: &[u8]) -> Result<Collection> {
    let collection: Collection = serde_json::from_slice(bytes)?;
    validate_collection(&collection)?;
    Ok(collection)
}

/// Parse a Postman Collection from a JSON string.
pub fn parse_collection_str(s: &str) -> Result<Collection> {
    let collection: Collection = serde_json::from_str(s)?;
    validate_collection(&collection)?;
    Ok(collection)
}

/// Validate the collection structure.
fn validate_collection(collection: &Collection) -> Result<()> {
    if collection.info.name.is_empty() {
        return Err(CollectionError::MissingField("info.name".into()));
    }
    if collection.info.schema.is_empty() {
        return Err(CollectionError::MissingField("info.schema".into()));
    }
    validate_methods(&collection.item)?;
    Ok(())
}

/// Every request's method must be a valid HTTP token. A genuinely invalid
/// method (empty, whitespace inside, non-tchar chars) fails the whole
/// collection parse loudly instead of silently becoming GET — a write-path
/// request must not degrade into a read-path "test" that reports green.
/// (Valid-but-uncommon tokens like PURGE/LINK parse fine via
/// `Method::Custom`.)
fn validate_methods(items: &[CollectionItem]) -> Result<()> {
    for item in items {
        match item {
            CollectionItem::Request(req) => {
                if Method::parse(&req.request.method).is_none() {
                    return Err(CollectionError::InvalidRequest(format!(
                        "item '{}' has invalid HTTP method {:?}",
                        req.name, req.request.method
                    )));
                }
            }
            CollectionItem::Folder(folder) => validate_methods(&folder.item)?,
        }
    }
    Ok(())
}

/// Convert a Collection into a protocol-agnostic Scenario.
pub fn collection_to_scenario(
    collection: Collection,
    _env_vars: HashMap<String, String>,
) -> Scenario {
    let mut scenario = Scenario {
        info: ScenarioInfo {
            name: collection.info.name.clone(),
            description: collection.info.description.clone(),
            schema: Some(collection.info.schema.clone()),
        },
        items: vec![],
        variables: HashMap::new(),
        auth: convert_auth(collection.auth.as_ref()),
    };

    // Convert collection variables
    for var in &collection.variable {
        if let Some(value) = &var.value {
            scenario.variables.insert(var.key.clone(), value.clone());
        }
    }

    // Convert items, threading collection-level auth down as the inherited
    // scope (Postman inheritance: request > folder > collection).
    scenario.items = convert_items(
        &collection.item,
        &collection.event,
        collection.auth.as_ref(),
    );

    scenario
}

fn convert_items(
    items: &[CollectionItem],
    parent_events: &[Event],
    inherited_auth: Option<&CollectionAuth>,
) -> Vec<ScenarioItem> {
    let mut result = Vec::new();

    for item in items {
        match item {
            CollectionItem::Request(req) => {
                let scenario_item = convert_request_item(req, parent_events, inherited_auth);
                result.push(scenario_item);
            }
            CollectionItem::Folder(folder) => {
                // Folder-level auth overrides the inherited (collection/parent)
                // auth for every request inside the folder. `inherit` passes
                // the parent's auth through; `noauth` explicitly disables it
                // for the whole subtree.
                let folder_auth = match folder.auth.as_ref() {
                    Some(a) if a.auth_type == "inherit" => inherited_auth,
                    Some(a) => Some(a),
                    None => inherited_auth,
                };
                // P0 (backlog): folder events used to REPLACE the inherited
                // chain, so a collection-level prerequest that mints a token
                // ran ZERO times for requests inside folders. Postman is
                // additive — the folder's events APPEND after the inherited
                // collection/parent-folder chain and ALL of them run,
                // outer→inner.
                let mut events = parent_events.to_vec();
                events.extend(folder.event.iter().cloned());
                let scenario_item = ScenarioItem {
                    id: folder.id.clone(),
                    name: folder.name.clone(),
                    request: None,
                    // The folder's own scripts are ALSO folded into every
                    // descendant leaf below, so they run before/after each
                    // request in the folder (Postman's per-request folder
                    // script semantics). `flatten_execution_items` drops a
                    // folder container, so these fields never run directly.
                    prerequest: find_prerequest_script(&folder.event),
                    test: find_test_script(&folder.event),
                    assertions: vec![],
                    items: convert_items(&folder.item, &events, folder_auth),
                };
                result.push(scenario_item);
            }
        }
    }

    result
}

fn convert_request_item(
    req: &RequestItem,
    parent_events: &[Event],
    inherited_auth: Option<&CollectionAuth>,
) -> ScenarioItem {
    // v2.1 schema location for request-level auth is `item.request.auth`
    // (RequestDetail.auth) — `item.auth` is a position the schema doesn't
    // define, but legacy exports use it, so prefer the schema location and
    // fall back to the legacy one.
    let request_auth = req.request.auth.as_ref().or(req.auth.as_ref());
    let request = convert_request(&req.request, request_auth, inherited_auth);

    // P0 (backlog): request events were EITHER/OR with parent events — a
    // request with its own script dropped the collection/folder scripts
    // entirely. Postman is additive: the request's events concatenate AFTER
    // the inherited chain, outer→inner, and ALL of them run.
    let mut events = parent_events.to_vec();
    events.extend(req.event.iter().cloned());

    ScenarioItem {
        id: req.id.clone(),
        name: req.name.clone(),
        request: Some(request),
        prerequest: find_prerequest_script(&events),
        test: find_test_script(&events),
        assertions: vec![],
        items: vec![],
    }
}

fn convert_request(
    detail: &RequestDetail,
    request_auth: Option<&CollectionAuth>,
    inherited_auth: Option<&CollectionAuth>,
) -> Request {
    // validate_methods() (called from parse_collection) rejects genuinely
    // invalid tokens at parse time. Direct callers of collection_to_scenario
    // bypass that guard, so the fallback here must NOT be a silent GET: the
    // raw token is preserved as Method::Custom, and reqwest::Method::from_bytes
    // (the Custom arm in the HTTP client) rejects non-tchar tokens LOUDLY at
    // request time — the request fails visibly instead of degrading to a
    // read-path GET that reports green.
    let method =
        Method::parse(&detail.method).unwrap_or_else(|| Method::Custom(detail.method.clone()));

    let mut url = build_url(detail);

    let mut headers: HashMap<String, String> = detail
        .header
        .iter()
        .filter(|h| !h.disabled)
        .map(|h| (h.key.clone(), h.value.clone()))
        .collect();

    let query_params = build_query_params(detail, &mut url);

    let body = convert_body(detail.body.as_ref());

    // Backlog line 138: `options.raw.language` selects the Content-Type for
    // a raw body (Postman: language "json" → `application/json`, etc.). The
    // field was parsed and never read — inject the header when the user has
    // not already set one (case-insensitively).
    if let Some(content_type) = raw_content_type(detail.body.as_ref()) {
        let has_ct = headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("content-type"));
        if !has_ct {
            headers.insert("Content-Type".to_string(), content_type);
        }
    }

    Request {
        url,
        method,
        headers,
        query_params,
        body,
        auth: resolve_auth(request_auth, inherited_auth),
        ..Default::default()
    }
}

/// Harvest the structured `url.query` list into `query_params` — but ONLY
/// when the URL itself does not already carry a query (same convention as
/// the HAR adapter; the HTTP client re-appends `query_params`, so doing
/// both would send every param twice: `?page=2&page=2`).
///
/// Postman's `url.raw` is the URL exactly as typed and ALWAYS contains the
/// query when one exists — so the common case leaves `query_params` empty
/// and the query rides in the URL. When the URL has no `?` but the
/// structured query has DUPLICATE keys (a HashMap cannot represent
/// `a=1&a=2`), fold the query string into the URL instead, preserving
/// order and duplicates.
fn build_query_params(detail: &RequestDetail, url: &mut String) -> HashMap<String, String> {
    if url.contains('?') {
        return HashMap::new();
    }
    let pairs: Vec<(String, String)> = detail
        .url
        .as_ref()
        .map(|u| {
            u.query
                .iter()
                .filter(|q| !q.disabled)
                .map(|q| (q.key.clone(), q.value.clone().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    if pairs.is_empty() {
        return HashMap::new();
    }
    if has_duplicate_keys(&pairs) {
        let qs: Vec<String> = pairs.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        url.push('?');
        url.push_str(&qs.join("&"));
        HashMap::new()
    } else {
        pairs.into_iter().collect()
    }
}

fn has_duplicate_keys(pairs: &[(String, String)]) -> bool {
    let mut seen = std::collections::HashSet::new();
    pairs.iter().any(|(k, _)| !seen.insert(k.clone()))
}

/// Resolve a request's effective auth following Postman inheritance
/// (request > folder > collection):
/// - explicit `noauth` → `Some(AuthConfig::NoAuth)` — disables auth and
///   BLOCKS inheritance (the old `None` mapping made noauth inherit the
///   parent scope, the inverse of Postman);
/// - explicit `inherit` or no request-level auth → the inherited scope auth;
/// - a real auth type → its config.
fn resolve_auth(
    request_auth: Option<&CollectionAuth>,
    inherited_auth: Option<&CollectionAuth>,
) -> Option<AuthConfig> {
    match request_auth {
        None => convert_auth(inherited_auth),
        Some(a) if a.auth_type == "inherit" => convert_auth(inherited_auth),
        Some(a) if a.auth_type == "noauth" => Some(AuthConfig::NoAuth),
        Some(a) => convert_auth(Some(a)),
    }
}

fn build_url(detail: &RequestDetail) -> String {
    let url = match detail.url.as_ref() {
        Some(u) => u,
        None => return String::new(),
    };

    let mut result = if let Some(raw) = &url.raw {
        if !raw.is_empty() {
            raw.clone()
        } else {
            assemble_url(url)
        }
    } else {
        assemble_url(url)
    };

    // Backlog line 138: Postman path variables are declared in
    // `url.variable` as `{key, value}` and referenced in the URL as `:key`
    // segments (e.g. `https://api.test/users/:id`). They were parsed and
    // never read — substitute each declared variable into the URL now.
    for v in &url.variable {
        // Guard against malformed declarations: an empty key would build
        // ":" and replace EVERY colon in the URL (protocol + port).
        if v.key.is_empty() {
            continue;
        }
        if let Some(value) = &v.value {
            let key = format!(":{}", v.key);
            result = result.replace(&key, value);
        }
    }

    result
}

/// Assemble a URL from the structured fields (used when `url.raw` is absent).
fn assemble_url(url: &UrlDetail) -> String {
    let proto = url.protocol.as_deref().unwrap_or("https");
    let host = url.host.join(".");
    let port = url
        .port
        .as_ref()
        .map(|p| format!(":{}", p))
        .unwrap_or_default();
    let path = if url.path.is_empty() {
        String::new()
    } else {
        format!("/{}", url.path.join("/"))
    };

    format!("{}://{}{}{}", proto, host, port, path)
}

fn convert_body(body: Option<&RequestBody>) -> Option<Body> {
    match body {
        Some(b) => match b.mode.as_str() {
            "raw" => b.raw.clone().map(Body::Raw),
            "urlencoded" => b.urlencoded.as_ref().map(|params| {
                Body::UrlEncoded(
                    params
                        .iter()
                        .filter(|p| !p.disabled)
                        .map(|p| (p.key.clone(), p.value.clone().unwrap_or_default()))
                        .collect(),
                )
            }),
            "formdata" => b.formdata.as_ref().map(|params| {
                Body::FormData(
                    params
                        .iter()
                        .filter(|p| !p.disabled)
                        .map(|p| {
                            let value = if p.param_type.as_deref() == Some("file") {
                                // Backlog line 138: a file part's content
                                // comes from `src` (a path on disk), not
                                // `value` — it used to become an EMPTY text
                                // field. Read the file's text content
                                // (lossy for binary files — the FormData
                                // model is HashMap<String, String>).
                                p.src
                                    .as_ref()
                                    .and_then(|s| std::fs::read(s).ok())
                                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                                    .unwrap_or_default()
                            } else {
                                p.value.clone().unwrap_or_default()
                            };
                            (p.key.clone(), value)
                        })
                        .collect(),
                )
            }),
            "graphql" => b.graphql.as_ref().map(|gql| {
                let variables = gql
                    .variables
                    .as_ref()
                    .and_then(|v| serde_json::from_str(v).ok());
                Body::GraphQL {
                    query: gql.query.clone().unwrap_or_default(),
                    variables,
                }
            }),
            "file" => {
                if let Some(f) = b.file.as_ref() {
                    // Exported `content` wins when present (literal body
                    // text). Backlog line 138: mode:"file" with only `src`
                    // used to yield NO body at all — read the file from
                    // disk as binary bytes.
                    if let Some(content) = &f.content {
                        if !content.is_empty() {
                            return Some(Body::Raw(content.clone()));
                        }
                    }
                    if let Some(src) = &f.src {
                        if let Ok(bytes) = std::fs::read(src) {
                            return Some(Body::Binary(bytes));
                        }
                    }
                }
                None
            }
            _ => b.raw.clone().map(Body::Raw),
        },
        None => None,
    }
}

/// Postman raw-body `options.raw.language` → Content-Type header (backlog
/// line 138): a raw body declared as `"language": "json"` must send
/// `application/json`. Only known languages map; unknown ones get no header.
fn raw_content_type(body: Option<&RequestBody>) -> Option<String> {
    let b = body?;
    if b.mode != "raw" {
        return None;
    }
    let language = b
        .options
        .as_ref()?
        .raw
        .as_ref()?
        .language
        .as_deref()?
        .to_ascii_lowercase();
    Some(match language.as_str() {
        "json" => "application/json".to_string(),
        "text" => "text/plain".to_string(),
        "xml" => "application/xml".to_string(),
        "html" => "text/html".to_string(),
        "javascript" => "application/javascript".to_string(),
        _ => return None,
    })
}

fn convert_auth(auth: Option<&CollectionAuth>) -> Option<AuthConfig> {
    let auth = auth.as_ref()?;
    match auth.auth_type.as_str() {
        // Explicit noauth at ANY scope (request/folder/collection) must
        // yield Some(NoAuth) — never None. None means "no auth configured →
        // inherit the parent scope" and the runner falls back to scenario
        // auth on None, which would silently re-apply collection auth to a
        // folder/request explicitly marked noauth (the inverse of Postman).
        "noauth" => Some(AuthConfig::NoAuth),
        "bearer" => {
            let token = get_auth_attr(&auth.bearer, "token")
                .or_else(|| get_auth_attr(&auth.bearer, "bearerToken"))
                .unwrap_or_default();
            Some(AuthConfig::Bearer { token })
        }
        "basic" => {
            let username = get_auth_attr(&auth.basic, "username").unwrap_or_default();
            let password = get_auth_attr(&auth.basic, "password").unwrap_or_default();
            Some(AuthConfig::Basic { username, password })
        }
        "apikey" => {
            let key = get_auth_attr(&auth.apikey, "key").unwrap_or_default();
            let value = get_auth_attr(&auth.apikey, "value").unwrap_or_default();
            let location_str = get_auth_attr(&auth.apikey, "in").unwrap_or_default();
            let location = if location_str == "query" {
                ApiKeyLocation::Query
            } else {
                ApiKeyLocation::Header
            };
            Some(AuthConfig::ApiKey {
                key,
                value,
                location,
            })
        }
        "digest" => {
            let username = get_auth_attr(&auth.digest, "username").unwrap_or_default();
            let password = get_auth_attr(&auth.digest, "password").unwrap_or_default();
            Some(AuthConfig::Digest { username, password })
        }
        "oauth1" => {
            let consumer_key = get_auth_attr(&auth.oauth1, "consumerKey").unwrap_or_default();
            let consumer_secret = get_auth_attr(&auth.oauth1, "consumerSecret").unwrap_or_default();
            let token = get_auth_attr(&auth.oauth1, "token");
            let token_secret = get_auth_attr(&auth.oauth1, "tokenSecret");
            Some(AuthConfig::OAuth1 {
                consumer_key,
                consumer_secret,
                token,
                token_secret,
            })
        }
        "oauth2" => {
            let access_token = get_auth_attr(&auth.oauth2, "accessToken").unwrap_or_default();
            let token_type = get_auth_attr(&auth.oauth2, "tokenType");
            Some(AuthConfig::OAuth2 {
                access_token,
                token_type,
            })
        }
        "awsv4" => {
            let access_key = get_auth_attr(&auth.awsv4, "accessKey").unwrap_or_default();
            let secret_key = get_auth_attr(&auth.awsv4, "secretKey").unwrap_or_default();
            let region = get_auth_attr(&auth.awsv4, "region");
            let service = get_auth_attr(&auth.awsv4, "service");
            let session_token = get_auth_attr(&auth.awsv4, "sessionToken");
            Some(AuthConfig::AwsSigV4 {
                access_key,
                secret_key,
                region,
                service,
                session_token,
            })
        }
        "hawk" => {
            let auth_id = get_auth_attr(&auth.hawk, "authId").unwrap_or_default();
            let auth_key = get_auth_attr(&auth.hawk, "authKey").unwrap_or_default();
            let algorithm = get_auth_attr(&auth.hawk, "algorithm");
            Some(AuthConfig::Hawk {
                auth_id,
                auth_key,
                algorithm,
            })
        }
        // Explicit auth with an UNKNOWN type (e.g. NTLM, edgegrid, ...) must
        // NOT fall back to None — None means "no auth configured → inherit
        // the parent scope", and the runner re-applies collection auth on
        // None, silently sending the collection's bearer token to an
        // endpoint that explicitly declared a different scheme (P1,
        // credential-leak shaped). Postman overrides, never inherits, when a
        // request declares its own auth; an unsupported scheme sends no
        // auth header at all.
        _ => Some(AuthConfig::NoAuth),
    }
}

fn get_auth_attr(attrs: &[AuthAttribute], key: &str) -> Option<String> {
    attrs.iter().find(|a| a.key == key).map(|a| match &a.value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// ALL prerequest scripts in the event chain, outer (collection) → inner
/// (request), as a LIST — one entry per script, NOT concatenated. Postman is
/// additive: a collection, each enclosing folder, and the request itself may
/// each contribute a prerequest script and EVERY one runs, before the
/// request, in that order. The old find-first behavior silently dropped
/// outer scripts the moment a deeper level had its own (P0).
///
/// Each script stays its own element so the runner compiles it into its own
/// lexical scope (backlog §4): a `const baseUrl` at collection level and at
/// request level must NOT collide, a top-level `return` only exits its own
/// script, and each script caches independently — the old single joined
/// string (`"\n;\n"`) shared one scope, so a redeclared const killed the
/// whole chain.
fn find_prerequest_script(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.listen == "prerequest")
        .filter_map(|e| e.script.as_ref())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// ALL test scripts in the event chain, outer → inner, as a LIST — one
/// entry per script, NOT concatenated (same per-script lexical-scope
/// semantics as `find_prerequest_script`).
fn find_test_script(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.listen == "test")
        .filter_map(|e| e.script.as_ref())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_collection() {
        let json = r#"{
            "info": {
                "name": "Test Collection",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": []
        }"#;

        let collection = parse_collection_str(json).unwrap();
        assert_eq!(collection.info.name, "Test Collection");
        assert!(collection.item.is_empty());
    }

    #[test]
    fn test_parse_single_request() {
        let json = r#"{
            "info": {
                "name": "Simple API",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [
                {
                    "name": "Get Users",
                    "request": {
                        "method": "GET",
                        "header": [
                            {"key": "Accept", "value": "application/json"}
                        ],
                        "url": {
                            "raw": "https://api.example.com/users",
                            "protocol": "https",
                            "host": ["api", "example", "com"],
                            "path": ["users"]
                        }
                    }
                }
            ]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        assert_eq!(collection.item.len(), 1);
    }

    #[test]
    fn test_parse_with_variables() {
        let json = r#"{
            "info": {
                "name": "Environments",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "variable": [
                {"key": "base_url", "value": "https://api.example.com", "type": "string"},
                {"key": "api_key", "value": "secret123", "type": "string"}
            ],
            "item": []
        }"#;

        let collection = parse_collection_str(json).unwrap();
        assert_eq!(collection.variable.len(), 2);
    }

    #[test]
    fn test_path_variables_substituted_in_url() {
        // Backlog line 138: url.variable declares path variables referenced
        // as `:key` segments; they were parsed and never read.
        let json = r#"{
            "info": {
                "name": "PV",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [{
                "name": "Get User",
                "request": {
                    "method": "GET",
                    "url": {
                        "raw": "https://api.test/users/:id",
                        "host": ["api", "test"],
                        "path": ["users", ":id"],
                        "variable": [{"key": "id", "value": "42"}]
                    }
                }
            }]
        }"#;
        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        let req = scenario.items[0].request.as_ref().expect("request");
        assert_eq!(req.url, "https://api.test/users/42");
    }

    #[test]
    fn test_raw_language_sets_content_type() {
        // Backlog line 138: options.raw.language ("json") must set
        // Content-Type: application/json — the field was parsed, never read.
        let json = r#"{
            "info": {
                "name": "CT",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [{
                "name": "Post",
                "request": {
                    "method": "POST",
                    "url": "https://api.test/echo",
                    "body": {
                        "mode": "raw",
                        "raw": "{\"a\":1}",
                        "options": {"raw": {"language": "json"}}
                    }
                }
            }]
        }"#;
        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        let req = scenario.items[0].request.as_ref().expect("request");
        assert_eq!(
            req.headers.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn test_file_body_reads_src_from_disk() {
        // Backlog line 138: mode:"file" with only `src` (no exported
        // content) yielded NO body at all — read the file from disk.
        let path = std::env::temp_dir().join("tropel-test-upload.txt");
        std::fs::write(&path, b"file-bytes-123").expect("write temp file");
        let src = path.display().to_string().replace('\\', "\\\\");
        let json = format!(
            r#"{{
                "info": {{
                    "name": "F",
                    "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
                }},
                "item": [{{
                    "name": "Upload",
                    "request": {{
                        "method": "POST",
                        "url": "https://api.test/upload",
                        "body": {{"mode": "file", "file": {{"src": "{src}"}}}}
                    }}
                }}]
            }}"#
        );
        let collection = parse_collection_str(&json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        let req = scenario.items[0].request.as_ref().expect("request");
        match &req.body {
            Some(Body::Binary(bytes)) => assert_eq!(bytes, b"file-bytes-123"),
            other => panic!("expected Binary body, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_formdata_file_part_reads_src() {
        // Backlog line 138: a formdata part with type:"file" became an
        // EMPTY text field — the content comes from `src` on disk.
        let path = std::env::temp_dir().join("tropel-test-form.txt");
        std::fs::write(&path, b"part-contents").expect("write temp file");
        let src = path.display().to_string().replace('\\', "\\\\");
        let json = format!(
            r#"{{
                "info": {{
                    "name": "FD",
                    "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
                }},
                "item": [{{
                    "name": "Form",
                    "request": {{
                        "method": "POST",
                        "url": "https://api.test/form",
                        "body": {{
                            "mode": "formdata",
                            "formdata": [
                                {{"key": "name", "value": "alice"}},
                                {{"key": "file", "type": "file", "src": "{src}"}}
                            ]
                        }}
                    }}
                }}]
            }}"#
        );
        let collection = parse_collection_str(&json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        let req = scenario.items[0].request.as_ref().expect("request");
        match &req.body {
            Some(Body::FormData(map)) => {
                assert_eq!(map.get("name").map(String::as_str), Some("alice"));
                assert_eq!(map.get("file").map(String::as_str), Some("part-contents"));
            }
            other => panic!("expected FormData body, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_convert_to_scenario() {
        let json = r#"{
            "info": {
                "name": "Test",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "variable": [
                {"key": "base_url", "value": "https://api.example.com"}
            ],
            "item": [
                {
                    "name": "Get Users",
                    "request": {
                        "method": "GET",
                        "url": {"raw": "{{base_url}}/users"}
                    }
                }
            ]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());

        assert_eq!(scenario.info.name, "Test");
        assert_eq!(
            scenario.variables.get("base_url").unwrap(),
            "https://api.example.com"
        );
        assert_eq!(scenario.items.len(), 1);
    }

    #[test]
    fn test_parse_graphql_request() {
        let json = r#"{
            "info": {
                "name": "GraphQL",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [
                {
                    "name": "GraphQL Query",
                    "request": {
                        "method": "POST",
                        "url": {"raw": "https://api.example.com/graphql"},
                        "body": {
                            "mode": "graphql",
                            "graphql": {
                                "query": "query { users { id name } }",
                                "variables": "{\"limit\": 10}"
                            }
                        }
                    }
                }
            ]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());

        if let Some(request) = &scenario.items[0].request {
            assert_eq!(request.method, Method::POST);
            if let Some(Body::GraphQL { query, variables }) = &request.body {
                assert_eq!(query, "query { users { id name } }");
                assert!(variables.is_some());
            } else {
                panic!("Expected GraphQL body");
            }
        }
    }

    #[test]
    fn test_parse_with_events() {
        let json = r#"{
            "info": {
                "name": "With Scripts",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [
                {
                    "name": "Test Request",
                    "event": [
                        {
                            "listen": "prerequest",
                            "script": {
                                "exec": ["pm.environment.set('key', 'value')"],
                                "type": "text/javascript"
                            }
                        },
                        {
                            "listen": "test",
                            "script": {
                                "exec": [
                                    "pm.test('Status 200', function() {",
                                    "    pm.response.to.have.status(200);",
                                    "});"
                                ],
                                "type": "text/javascript"
                            }
                        }
                    ],
                    "request": {
                        "method": "GET",
                        "url": {"raw": "https://api.example.com/test"}
                    }
                }
            ]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());

        assert!(!scenario.items[0].prerequest.is_empty());
        assert!(!scenario.items[0].test.is_empty());
        assert!(scenario.items[0].test[0].contains("pm.test"));
    }

    #[test]
    fn test_collection_and_folder_scripts_reach_leaves_in_order() {
        // P0 (backlog): collection- and folder-level scripts never ran for
        // folder-organized collections. Three interacting facts: folder
        // events REPLACED collection events; request events were either/or
        // with parent events (Postman is additive); and
        // flatten_execution_items discards a folder that has children, so
        // its script was dead. A top-level prerequest that mints a token
        // ran zero times → every request sent literal `Bearer {{token}}`.
        //
        // Fix: the inherited event chain (collection → folder → request)
        // is folded into each leaf at convert time, outer→inner, ALL of
        // them concatenated — so per-request scripts run exactly the way
        // Postman runs them.
        let json = r#"{
            "info": {"name": "Scripts", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "event": [
                {"listen": "prerequest", "script": {"exec": ["COLLECTION_PREREQUEST"], "type": "text/javascript"}},
                {"listen": "test", "script": {"exec": ["COLLECTION_TEST"], "type": "text/javascript"}}
            ],
            "item": [{
                "name": "Folder",
                "event": [
                    {"listen": "prerequest", "script": {"exec": ["FOLDER_PREREQUEST"], "type": "text/javascript"}},
                    {"listen": "test", "script": {"exec": ["FOLDER_TEST"], "type": "text/javascript"}}
                ],
                "item": [{
                    "name": "Inner Req",
                    "request": {"method": "GET", "url": {"raw": "https://api.example.com/inner"}},
                    "event": [
                        {"listen": "prerequest", "script": {"exec": ["INNER_PREREQUEST"], "type": "text/javascript"}},
                        {"listen": "test", "script": {"exec": ["INNER_TEST"], "type": "text/javascript"}}
                    ]
                }]
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let folder = &scenario.items[0];
        assert_eq!(folder.name, "Folder");
        let inner = &folder.items[0];

        // Prerequest: ALL three levels folded as a LIST, outer→inner, each
        // in its own element (per-script lexical scope, backlog §4).
        let pre = &inner.prerequest;
        assert_eq!(pre.len(), 3, "all three levels must fold, got: {pre:?}");
        assert!(pre[0].contains("COLLECTION_PREREQUEST"));
        assert!(pre[1].contains("FOLDER_PREREQUEST"));
        assert!(pre[2].contains("INNER_PREREQUEST"));

        // Test: ALL three levels folded, outer→inner.
        let t = &inner.test;
        assert_eq!(t.len(), 3, "all three test levels must fold, got: {t:?}");
        assert!(t[0].contains("COLLECTION_TEST"));
        assert!(t[1].contains("FOLDER_TEST"));
        assert!(t[2].contains("INNER_TEST"));
    }

    #[test]
    fn test_request_scripts_append_to_inherited_not_replace() {
        // P0 (backlog): a request with its OWN event dropped the inherited
        // collection/folder scripts (either/or). Postman is additive — the
        // request's script must run IN ADDITION to the outer chain.
        let json = r#"{
            "info": {"name": "Additive", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "event": [
                {"listen": "prerequest", "script": {"exec": ["COLL_PRE"], "type": "text/javascript"}}
            ],
            "item": [{
                "name": "Req",
                "request": {"method": "GET", "url": {"raw": "https://api.example.com/x"}},
                "event": [
                    {"listen": "prerequest", "script": {"exec": ["REQ_PRE"], "type": "text/javascript"}}
                ]
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let pre = &scenario.items[0].prerequest;
        assert_eq!(pre.len(), 2, "inherited + own script, got: {pre:?}");
        assert!(pre[0].contains("COLL_PRE"), "inherited script first");
        assert!(pre[1].contains("REQ_PRE"), "request's own script second");
    }

    #[test]
    fn test_parse_folder_nesting() {
        // A folder containing a request: the request must surface as a
        // nested ScenarioItem, not be flattened or dropped.
        let json = r#"{
            "info": {"name": "Nested", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Folder",
                "item": [{
                    "name": "Inner Req",
                    "request": {"method": "GET", "url": {"raw": "https://api.example.com/inner"}}
                }]
            }]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        assert_eq!(scenario.items.len(), 1);
        assert_eq!(scenario.items[0].name, "Folder");
        assert_eq!(scenario.items[0].items.len(), 1);
        let inner = &scenario.items[0].items[0];
        assert_eq!(inner.name, "Inner Req");
        let req = inner.request.as_ref().expect("inner request parsed");
        assert_eq!(req.url, "https://api.example.com/inner");
    }

    #[test]
    fn test_parse_query_params_and_raw_body() {
        // Structured URL with query params + a raw JSON body + string-form
        // URL variant must all survive the round-trip.
        let json = r#"{
            "info": {"name": "Full", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Create",
                "request": {
                    "method": "POST",
                    "url": {
                        "raw": "https://api.example.com/items",
                        "host": ["api", "example", "com"],
                        "path": ["items"],
                        "query": [
                            {"key": "page", "value": "2"},
                            {"key": "per_page", "value": "50"}
                        ]
                    },
                    "body": {
                        "mode": "raw",
                        "raw": "{\"name\":\"x\"}"
                    }
                }
            }, {
                "name": "StringUrl",
                "request": {"method": "GET", "url": "https://api.example.com/str"}
            }]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        assert_eq!(scenario.items.len(), 2);

        let create = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(create.url, "https://api.example.com/items");
        assert_eq!(
            create.query_params.get("page").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            create.query_params.get("per_page").map(String::as_str),
            Some("50")
        );
        assert!(matches!(create.body, Some(Body::Raw(ref s)) if s == "{\"name\":\"x\"}"));

        // String-form URL: the custom UrlDetail deserializer handles it.
        let str_req = scenario.items[1].request.as_ref().unwrap();
        assert_eq!(str_req.url, "https://api.example.com/str");
    }

    #[test]
    fn test_request_level_auth_read_from_request_detail() {
        // Regression (backlog line 69): the v2.1 schema puts request-level
        // auth at `item.request.auth` (RequestDetail.auth), but the parser
        // only read the non-schema `item.auth` position — per-request bearer
        // tokens were silently dropped, so no auth was sent at all.
        let json = r#"{
            "info": {"name": "Auth", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Secure",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/secure"},
                    "auth": {
                        "type": "bearer",
                        "bearer": [{"key": "token", "value": "tok123", "type": "string"}]
                    }
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        match req.auth.as_ref() {
            Some(AuthConfig::Bearer { token }) => assert_eq!(token, "tok123"),
            other => panic!("expected Bearer auth, got {:?}", other),
        }
    }

    #[test]
    fn test_folder_auth_inherited_by_children() {
        // Folder-level auth must be inherited by every request in the folder
        // (Postman inheritance: request > folder > collection).
        let json = r#"{
            "info": {"name": "Folders", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "auth": {"type": "basic", "basic": [{"key": "username", "value": "coll_user"}, {"key": "password", "value": "coll_pass"}]},
            "item": [{
                "name": "Folder",
                "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "folder_tok"}]},
                "item": [{
                    "name": "Child",
                    "request": {"method": "GET", "url": {"raw": "https://api.example.com/child"}}
                }]
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].items[0].request.as_ref().unwrap();
        match req.auth.as_ref() {
            Some(AuthConfig::Bearer { token }) => assert_eq!(token, "folder_tok"),
            other => panic!("child must inherit folder bearer auth, got {:?}", other),
        }
    }

    #[test]
    fn test_collection_auth_inherited_by_requests() {
        // No folder or request auth → the collection-level auth applies.
        let json = r#"{
            "info": {"name": "Coll", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "auth": {"type": "apikey", "apikey": [{"key": "key", "value": "k1"}, {"key": "value", "value": "v1"}, {"key": "in", "value": "header"}]},
            "item": [{
                "name": "Req",
                "request": {"method": "GET", "url": {"raw": "https://api.example.com/x"}}
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        assert!(
            matches!(req.auth.as_ref(), Some(AuthConfig::ApiKey { .. })),
            "request must inherit collection api-key auth, got {:?}",
            req.auth
        );
    }

    #[test]
    fn test_noauth_blocks_inheritance() {
        // Regression (backlog line 69): `{"type":"noauth"}` mapped to None,
        // indistinguishable from inherit, so an explicitly unauthenticated
        // request inherited collection auth — the INVERSE of Postman. noauth
        // must yield AuthConfig::NoAuth so the runner sends no auth.
        let json = r#"{
            "info": {"name": "NoAuth", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "coll_tok"}]},
            "item": [{
                "name": "Public",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/public"},
                    "auth": {"type": "noauth"}
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        assert!(
            matches!(req.auth.as_ref(), Some(AuthConfig::NoAuth)),
            "noauth must block inheritance (AuthConfig::NoAuth), got {:?}",
            req.auth
        );
    }

    #[test]
    fn test_unknown_auth_type_does_not_leak_collection_token() {
        // Regression (backlog line 139): a request explicitly configured for
        // an auth type we don't support (NTLM) mapped to None, which the
        // runner treats as "inherit" — sending the collection's bearer token
        // to that endpoint. An explicit but unsupported scheme must yield
        // AuthConfig::NoAuth (send nothing), never the collection's token.
        let json = r#"{
            "info": {"name": "Ntlm", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "coll_tok"}]},
            "item": [{
                "name": "Windows",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/windows"},
                    "auth": {"type": "ntlm", "ntlm": [{"key": "username", "value": "u"}, {"key": "password", "value": "p"}]}
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        assert!(
            matches!(req.auth.as_ref(), Some(AuthConfig::NoAuth)),
            "unknown explicit auth must NOT inherit collection auth (NoAuth), got {:?}",
            req.auth
        );
    }

    #[test]
    fn test_oauth_export_with_non_string_auth_values_parses() {
        // Regression (backlog line 131): AuthAttribute.value was a required
        // String, but the v2.1 schema types it any-type and doesn't require
        // it — real Postman OAuth1 exports carry booleans
        // (`addParamsToHeader`) and modern OAuth2 exports carry arrays
        // (`tokenRequestParams: []`). A boolean/array value made serde fail
        // the *whole collection* → zero requests ran. The whole collection
        // must parse and the auth must resolve to the string fields.
        let json = r#"{
            "info": {"name": "OAuth", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "OAuth1",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/oauth1"},
                    "auth": {
                        "type": "oauth1",
                        "oauth1": [
                            {"key": "consumerKey", "value": "ck", "type": "string"},
                            {"key": "addParamsToHeader", "value": true, "type": "boolean"}
                        ]
                    }
                }
            }, {
                "name": "OAuth2",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/oauth2"},
                    "auth": {
                        "type": "oauth2",
                        "oauth2": [
                            {"key": "accessToken", "value": "tok", "type": "string"},
                            {"key": "tokenRequestParams", "value": [], "type": "array"}
                        ]
                    }
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        assert_eq!(scenario.items.len(), 2, "both OAuth requests must parse");

        let oauth1 = scenario.items[0].request.as_ref().unwrap();
        match oauth1.auth.as_ref() {
            Some(AuthConfig::OAuth1 { consumer_key, .. }) => assert_eq!(consumer_key, "ck"),
            other => panic!("expected OAuth1 auth, got {:?}", other),
        }

        let oauth2 = scenario.items[1].request.as_ref().unwrap();
        match oauth2.auth.as_ref() {
            Some(AuthConfig::OAuth2 { access_token, .. }) => assert_eq!(access_token, "tok"),
            other => panic!("expected OAuth2 auth, got {:?}", other),
        }
    }

    #[test]
    fn test_folder_noauth_blocks_collection_inheritance() {
        // Regression: a folder marked noauth inside a bearer-authenticated
        // collection must NOT re-inherit the collection bearer. convert_auth
        // maps "noauth" → Some(AuthConfig::NoAuth) at every scope level so
        // the runner's `.or(scenario.auth)` fallback can't re-apply it.
        let json = r#"{
            "info": {"name": "NoAuthFolder", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "coll_tok"}]},
            "item": [{
                "name": "Public Folder",
                "auth": {"type": "noauth"},
                "item": [{
                    "name": "Public Req",
                    "request": {"method": "GET", "url": {"raw": "https://api.example.com/public"}}
                }]
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].items[0].request.as_ref().unwrap();
        assert!(
            matches!(req.auth.as_ref(), Some(AuthConfig::NoAuth)),
            "folder noauth must block collection inheritance, got {:?}",
            req.auth
        );
    }

    #[test]
    fn test_legacy_item_auth_fallback() {
        // Some exports put request auth at the non-schema `item.auth` slot;
        // it must still be honored when `item.request.auth` is absent.
        let json = r#"{
            "info": {"name": "Legacy", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Legacy Auth",
                "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "legacy_tok"}]},
                "request": {"method": "GET", "url": {"raw": "https://api.example.com/legacy"}}
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        match req.auth.as_ref() {
            Some(AuthConfig::Bearer { token }) => assert_eq!(token, "legacy_tok"),
            other => panic!("legacy item.auth must be honored, got {:?}", other),
        }
    }

    #[test]
    fn test_query_not_sent_twice() {
        // Regression (backlog line 72): build_url returns url.raw verbatim
        // (which ALWAYS contains the query) AND query_params harvested the
        // structured url.query list — the HTTP client re-appends
        // query_params, so `GET /items?page=2` went out as
        // `/items?page=2&page=2`. When the URL already carries a query,
        // query_params must stay empty (same convention as the HAR adapter).
        let json = r#"{
            "info": {"name": "Q", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "List",
                "request": {
                    "method": "GET",
                    "url": {
                        "raw": "https://api.example.com/items?page=2",
                        "host": ["api", "example", "com"],
                        "path": ["items"],
                        "query": [{"key": "page", "value": "2"}]
                    }
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://api.example.com/items?page=2");
        assert!(
            req.query_params.is_empty(),
            "query_params must be empty when the URL already has the query — got {:?}",
            req.query_params
        );
    }

    #[test]
    fn test_query_populated_when_url_has_no_query() {
        // Structured query WITHOUT a query in the raw URL must still be
        // harvested into query_params (the client appends them once).
        let json = r#"{
            "info": {"name": "Q", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "List",
                "request": {
                    "method": "GET",
                    "url": {
                        "raw": "https://api.example.com/items",
                        "host": ["api", "example", "com"],
                        "path": ["items"],
                        "query": [{"key": "page", "value": "2"}]
                    }
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://api.example.com/items");
        assert_eq!(req.query_params.get("page").map(String::as_str), Some("2"));
    }

    #[test]
    fn test_query_duplicate_keys_folded_into_url() {
        // A HashMap cannot represent `tag=a&tag=b`; when the URL has no
        // query and the structured list has duplicate keys, fold the full
        // query string into the URL (order + duplicates preserved) and keep
        // query_params empty — mirroring the HAR adapter's convention.
        let json = r#"{
            "info": {"name": "Q", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Tags",
                "request": {
                    "method": "GET",
                    "url": {
                        "raw": "https://api.example.com/search",
                        "host": ["api", "example", "com"],
                        "path": ["search"],
                        "query": [{"key": "tag", "value": "a"}, {"key": "tag", "value": "b"}]
                    }
                }
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://api.example.com/search?tag=a&tag=b");
        assert!(req.query_params.is_empty());
    }

    #[test]
    fn test_schema_legal_shapes_parse_as_requests() {
        // Regression (backlog line 93): object-form `description`,
        // string-form `script.exec`, a header with no `value`, a numeric
        // `responseTime`, and a missing response `code` are ALL schema-legal
        // Postman shapes. Before the fix each one made RequestItem fail to
        // parse, the untagged CollectionItem silently fell through to
        // FolderItem (which only requires `name`), and the request was
        // dropped as an empty folder.
        let json = r#"{
            "info": {"name": "Shapes", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Shape Req",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/shapes"},
                    "description": {"content": "object-form description", "type": "text/plain"},
                    "header": [{"key": "X-No-Value"}]
                },
                "event": [{
                    "listen": "test",
                    "script": {"exec": "pm.test('ok', () => {});", "type": "text/javascript"}
                }],
                "response": [{"name": "r1", "status": "OK", "responseTime": 123}]
            }]
        }"#;

        let scenario = collection_to_scenario(parse_collection_str(json).unwrap(), HashMap::new());
        assert_eq!(
            scenario.items.len(),
            1,
            "request must not fall through to a folder"
        );
        assert_eq!(scenario.items[0].name, "Shape Req");
        let req = scenario.items[0].request.as_ref().expect("request parsed");
        assert_eq!(req.url, "https://api.example.com/shapes");
        assert_eq!(req.headers.get("X-No-Value").map(String::as_str), Some(""));
        // String-form exec must still surface as a test script.
        let test = &scenario.items[0].test;
        assert!(
            test[0].contains("pm.test"),
            "string-form exec must surface as a test script"
        );
    }

    #[test]
    fn test_malformed_request_fails_loudly() {
        // Regression (backlog line 93): a request with a genuinely
        // malformed sub-field (here: a non-string header value) must ERROR
        // loudly — not silently become an empty folder that vanishes from
        // the run. The request-key discriminator guarantees RequestItem is
        // attempted whenever a `request` key is present.
        let json = r#"{
            "info": {"name": "Bad", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Broken Req",
                "request": {
                    "method": "GET",
                    "url": {"raw": "https://api.example.com/broken"},
                    "header": [{"key": "X", "value": 42}]
                }
            }]
        }"#;

        assert!(
            parse_collection_str(json).is_err(),
            "malformed request must fail loudly, not become an empty folder"
        );
    }

    #[test]
    fn test_parse_urlencoded_body() {
        let json = r#"{
            "info": {"name": "Form", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{
                "name": "Login",
                "request": {
                    "method": "POST",
                    "url": {"raw": "https://api.example.com/login"},
                    "body": {
                        "mode": "urlencoded",
                        "urlencoded": [
                            {"key": "user", "value": "alice"},
                            {"key": "pass", "value": "secret", "disabled": true}
                        ]
                    }
                }
            }]
        }"#;

        let collection = parse_collection_str(json).unwrap();
        let scenario = collection_to_scenario(collection, HashMap::new());
        let req = scenario.items[0].request.as_ref().unwrap();
        match &req.body {
            Some(Body::UrlEncoded(params)) => {
                assert_eq!(params.len(), 1, "disabled param dropped");
                assert_eq!(params.get("user").map(String::as_str), Some("alice"));
                assert!(params.get("pass").is_none());
            }
            other => panic!("expected UrlEncoded body, got {:?}", other),
        }
    }
}
