//! # tropel-input-bru
//!
//! Input adapter that reads [Bruno][bruno] `.bru` request files (the v2 block
//! grammar used by every current Bruno release) and produces a
//! protocol-agnostic `Scenario`.
//!
//! [bruno]: https://usebruno.com
//!
//! ## Grammar mapping
//!
//! | .bru block | Scenario field |
//! |------------|-----------------|
//! | `meta { name }` | `ScenarioItem.name` |
//! | `get/post/put/patch/delete/head/options { url, body, auth }` | `request.method` / body mode / auth mode |
//! | `headers { k: v }` (`~k: v` disabled ⇒ dropped) | `request.headers` |
//! | `params:query` (appended to the URL query) / `params:path` (`:id` substituted) | `request.url` |
//! | `body:json` (parsed, invalid ⇒ verbatim) / `body:text`/`body:xml` (`Raw`) / `body:graphql` / `body:formUrlEncoded` / `body:multipartForm` | `request.body` |
//! | `auth:bearer` / `auth:basic` / `auth:apikey { in: header }` | the respective `Authorization`/apikey header |
//! | `script:pre-request` | `prerequest` |
//! | `script:post-response` | prepended to `test` (runs after the response; there is no dedicated field) |
//! | `tests` | `test` |
//! | `assert { path: op value }` | `assertions` (verbatim lines) |
//! | `vars:pre-request` / collection-level vars | `Scenario.variables` (string values) |
//! | `docs { … }` | `Scenario.info.description` |
//!
//! Bruno files are one-request-per-file; a directory import assembles folders
//! from paths (the caller reads every `.bru` under the root). Collection and
//! folder `.bru` files (no method block) contribute headers/scripts/vars that
//! this adapter surfaces through the same fields — requests win conflicts.
//!
//! Out of scope: `@file()` multipart attachments (no file-system context),
//! `@description(...)` annotations (skipped), and oauth2/aws/digest auth
//! (falls back to whatever headers the file itself declares).

use std::collections::HashMap;
use tropel_sdk::{Body, Method, Request};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

// ── Block model ─────────────────────────────────────────────────

/// One parsed `.bru` block: `name` may carry a subtype (`body:json`).
struct Block {
    name: String,
    /// Raw inner lines, verbatim (no closing brace).
    lines: Vec<String>,
}

impl Block {
    /// Key→value pairs for kv-style blocks: `key: value`, `~key: value`
    /// (disabled — returned separately), annotations (`@…`) skipped.
    /// Values are trimmed; quotes are stripped.
    fn pairs(&self) -> Vec<(String, String, bool)> {
        let mut out = Vec::new();
        for line in &self.lines {
            let t = line.trim();
            if t.is_empty() || t.starts_with('@') {
                continue;
            }
            let Some((key, value)) = t.split_once(':') else {
                continue;
            };
            let (disabled, key) = match key.strip_prefix('~') {
                Some(k) => (true, k),
                None => (false, key),
            };
            out.push((key.trim().to_string(), unquote(value.trim()), disabled));
        }
        out
    }

    fn first(&self, key: &str) -> Option<String> {
        self.pairs()
            .into_iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v)
    }

    /// Inner text joined with newlines (raw blocks: bodies, scripts, tests).
    fn text(&self) -> String {
        self.lines.join("\n").trim().to_string()
    }
}

/// Strip balanced single/double quotes and trailing commas from a value.
fn unquote(s: &str) -> String {
    let s = s.trim_end_matches(',').trim();
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = s.len() - 1;
        if (first == b'\'' || first == b'"') && bytes[last] == first {
            return s[1..last].to_string();
        }
    }
    s.to_string()
}

/// Parse a `.bru` document into blocks. Block headers are
/// `name` or `name:subtype` followed by `{`; a line that is exactly `}` at
/// column 0 closes the current block — an *indented* `}` is block content
/// (JSON bodies close with `  }`). Lines outside blocks are ignored.
fn parse_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current: Option<Block> = None;
    for line in text.lines() {
        let t = line.trim_end_matches('\r');
        if t == "}" {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            continue;
        }
        if current.is_none() {
            if let Some(header) = t.trim().strip_suffix('{') {
                let header = header.trim();
                let valid = !header.is_empty()
                    && header
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_');
                if valid {
                    current = Some(Block {
                        name: header.to_string(),
                        lines: Vec::new(),
                    });
                    continue;
                }
            }
            continue; // stray line outside any block
        }
        current.as_mut().expect("checked").lines.push(t.to_string());
    }
    blocks
}

const METHODS: [&str; 7] = ["get", "post", "put", "patch", "delete", "head", "options"];

// ── InputAdapter implementation ─────────────────────────────────

/// Input adapter for Bruno `.bru` request files.
pub struct BruInputAdapter;

inventory::submit!(
    InputAdapterRegistration::new("bru", || Box::new(BruInputAdapter)).with_priority(26)
);

impl InputAdapter for BruInputAdapter {
    fn id(&self) -> &str {
        "bru"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: the document parses into blocks AND contains
        // either a `meta` block (request/collection/folder files all have one)
        // or a method block with a `url:` line. JSON/curl/.http never parse
        // into that shape.
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        let blocks = parse_blocks(text);
        blocks.iter().any(|b| {
            b.name == "meta" || METHODS.contains(&b.name.as_str()) && b.first("url").is_some()
        })
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| TropelError::Parse(format!(".bru file is not valid UTF-8: {e}")))?;
        let blocks = parse_blocks(text);

        let method_block = blocks
            .iter()
            .find(|b| METHODS.contains(&b.name.as_str()))
            .ok_or_else(|| {
                TropelError::Parse(
                    ".bru file has no method block (get/post/…) — collection and folder files \
                     are not requests"
                        .into(),
                )
            })?;

        let meta_name = blocks
            .iter()
            .find(|b| b.name == "meta")
            .and_then(|b| b.first("name"))
            .unwrap_or_else(|| "bruno-request".into());

        let url_raw = method_block
            .first("url")
            .ok_or_else(|| TropelError::Parse(format!(".bru request {meta_name:?} has no url")))?;
        let method = Method::parse(&method_block.name).ok_or_else(|| {
            TropelError::Parse(format!("unknown method block {:?}", method_block.name))
        })?;

        // headers (disabled `~key` dropped)
        let mut headers: HashMap<String, String> = HashMap::new();
        if let Some(b) = blocks.iter().find(|b| b.name == "headers") {
            for (k, v, disabled) in b.pairs() {
                if !disabled && !k.is_empty() {
                    match headers.get_mut(&k) {
                        Some(existing) => {
                            existing.push_str(", ");
                            existing.push_str(&v);
                        }
                        None => {
                            headers.insert(k, v);
                        }
                    }
                }
            }
        }

        // URL assembly: path params substituted, then query params appended.
        let mut url = url_raw.clone();
        if let Some(b) = blocks.iter().find(|b| b.name == "params:path") {
            for (k, v, disabled) in b.pairs() {
                if disabled {
                    continue;
                }
                // `:id` in the URL ← params:path { id: 1 }. Longest-name first
                // would be safer for prefixes (`:id` vs `:id2`); substitute
                // on segment boundaries (`:id/`, `:id?`, end).
                let needle = format!(":{k}");
                let mut replaced = String::new();
                let mut rest = url.as_str();
                while let Some(pos) = rest.find(&needle) {
                    let after = &rest[pos + needle.len()..];
                    let boundary = after
                        .chars()
                        .next()
                        .map(|c| !c.is_ascii_alphanumeric() && c != '_')
                        .unwrap_or(true);
                    replaced.push_str(&rest[..pos]);
                    if boundary {
                        replaced.push_str(&v);
                    } else {
                        replaced.push_str(&needle);
                    }
                    rest = after;
                    if !boundary {
                        break;
                    }
                }
                replaced.push_str(rest);
                url = replaced;
            }
        }
        if let Some(b) = blocks.iter().find(|b| b.name == "params:query") {
            let pairs: Vec<String> = b
                .pairs()
                .into_iter()
                .filter(|(k, _, disabled)| !disabled && !k.is_empty())
                .map(|(k, v, _)| format!("{k}={v}"))
                .collect();
            if !pairs.is_empty() {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&pairs.join("&"));
            }
        }

        // Body: mode from the method block, content from body:<mode>.
        let body_mode = method_block.first("body").unwrap_or_else(|| "none".into());
        let body = build_body(&blocks, &body_mode, &headers);

        // Auth → headers (bearer/basic/apikey-in-header), mirroring the other
        // adapters; an explicit Authorization header from the file wins.
        apply_auth(&blocks, &mut headers);

        // Scripts: post-response BEFORE tests (Bruno ordering — both run
        // after the response; the SDK has no dedicated post-response slot).
        let mut test: Vec<String> = Vec::new();
        if let Some(b) = blocks.iter().find(|b| b.name == "script:post-response") {
            let t = b.text();
            if !t.is_empty() {
                test.push(t);
            }
        }
        if let Some(b) = blocks.iter().find(|b| b.name == "tests") {
            let t = b.text();
            if !t.is_empty() {
                test.push(t);
            }
        }
        let prerequest = blocks
            .iter()
            .find(|b| b.name == "script:pre-request")
            .map(|b| b.text())
            .filter(|t| !t.is_empty())
            .map(|t| vec![t])
            .unwrap_or_default();

        let assertions: Vec<String> = blocks
            .iter()
            .find(|b| b.name == "assert")
            .map(|b| {
                b.lines
                    .iter()
                    .map(|l| l.trim().trim_end_matches(',').to_string())
                    .filter(|l| !l.is_empty() && !l.starts_with('@'))
                    .collect()
            })
            .unwrap_or_default();

        // vars (request/folder/collection .bru all contribute; request wins).
        let mut variables: HashMap<String, serde_json::Value> = HashMap::new();
        for b in blocks.iter().filter(|b| b.name.starts_with("vars:")) {
            for (k, v, disabled) in b.pairs() {
                if !disabled && !k.is_empty() {
                    variables.insert(k, serde_json::Value::String(v));
                }
            }
        }

        let description = blocks
            .iter()
            .find(|b| b.name == "docs")
            .map(|b| b.text())
            .filter(|t| !t.is_empty())
            .or(Some("Imported from Bruno .bru file".into()));

        Ok(Scenario {
            info: ScenarioInfo {
                name: meta_name,
                description,
                schema: None,
            },
            items: vec![ScenarioItem {
                name: "request".into(),
                id: None,
                request: Some(Request {
                    url,
                    method,
                    headers,
                    query_params: HashMap::new(),
                    body,
                    auth: None,
                    certificate: None,
                    follow_redirects: true,
                    timeout: None,
                    response_type: tropel_sdk::ResponseType::Text,
                }),
                prerequest,
                test,
                assertions,
                items: vec![],
            }],
            variables,
            auth: None,
        })
    }
}

/// Build the request body from the method block's `body:` mode and the
/// matching `body:<mode>` block. Invalid JSON falls back to verbatim `Raw`.
fn build_body(blocks: &[Block], mode: &str, headers: &HashMap<String, String>) -> Option<Body> {
    let block_text = |name: &str| blocks.iter().find(|b| b.name == name).map(|b| b.text());
    match mode {
        "none" => None,
        "json" => {
            let text = block_text("body:json").unwrap_or_default();
            if text.is_empty() {
                return None;
            }
            match serde_json::from_str(&text) {
                Ok(v) => Some(Body::Json(v)),
                Err(_) => Some(Body::Raw(text)),
            }
        }
        "text" | "xml" => block_text("body:text")
            .or_else(|| block_text("body:xml"))
            .filter(|t| !t.is_empty())
            .map(Body::Raw),
        "graphql" => {
            let text = block_text("body:graphql").unwrap_or_default();
            if text.is_empty() {
                return None;
            }
            // Bruno splits the GraphQL body into query + variables fields in
            // its JSON shape; the .bru text form stores the query. Variables
            // block (`body:graphql:vars`) parses as a JSON object when valid.
            let variables = block_text("body:graphql:vars")
                .and_then(|v| serde_json::from_str::<HashMap<String, serde_json::Value>>(&v).ok());
            Some(Body::GraphQL {
                query: text,
                variables,
            })
        }
        "formUrlEncoded" => {
            let fields = kv_fields(blocks, "body:formUrlEncoded");
            (!fields.is_empty()).then_some(Body::UrlEncoded(fields))
        }
        "multipartForm" => {
            let fields = kv_fields(blocks, "body:multipartForm");
            (!fields.is_empty()).then_some(Body::FormData(fields))
        }
        _ => {
            // Unknown mode (or none declared but a body block exists):
            // replay the first body:* block verbatim as raw text.
            let _ = headers;
            blocks
                .iter()
                .find(|b| b.name.starts_with("body:"))
                .map(|b| b.text())
                .filter(|t| !t.is_empty())
                .map(Body::Raw)
        }
    }
}

/// Key-value fields of a block, skipping disabled (`~key`) entries.
fn kv_fields(blocks: &[Block], name: &str) -> HashMap<String, String> {
    blocks
        .iter()
        .find(|b| b.name == name)
        .map(|b| {
            b.pairs()
                .into_iter()
                .filter(|(k, _, disabled)| !disabled && !k.is_empty())
                .map(|(k, v, _)| (k, v))
                .collect()
        })
        .unwrap_or_default()
}

/// Apply Bruno auth blocks as headers. An explicit `Authorization` header in
/// the file is never overwritten.
fn apply_auth(blocks: &[Block], headers: &mut HashMap<String, String>) {
    let mode = blocks
        .iter()
        .find(|b| b.name == "auth")
        .and_then(|b| b.first("mode"))
        .or_else(|| {
            // The method block's `auth:` line is the shorthand form.
            blocks
                .iter()
                .find(|b| METHODS.contains(&b.name.as_str()))
                .and_then(|b| b.first("auth"))
        });
    let has_auth_header = headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("authorization"));
    if has_auth_header {
        return;
    }
    match mode.as_deref() {
        Some("bearer") => {
            if let Some(token) = blocks
                .iter()
                .find(|b| b.name == "auth:bearer")
                .and_then(|b| b.first("token"))
            {
                headers.insert("Authorization".into(), format!("Bearer {token}"));
            }
        }
        Some("basic") => {
            let b = blocks.iter().find(|b| b.name == "auth:basic");
            if let Some(b) = b {
                let user = b.first("username").unwrap_or_default();
                let pass = b.first("password").unwrap_or_default();
                let encoded = base64_encode(format!("{user}:{pass}").as_bytes());
                headers.insert("Authorization".into(), format!("Basic {encoded}"));
            }
        }
        Some("apikey") => {
            let b = blocks.iter().find(|b| b.name == "auth:apikey");
            if let Some(b) = b {
                let in_ = b.first("in").unwrap_or_else(|| "header".into());
                let key = b.first("key").unwrap_or_default();
                let value = b.first("value").unwrap_or_default();
                if in_ == "header" && !key.is_empty() {
                    headers.insert(key, value);
                }
                // `in: query` needs URL assembly — skipped (documented).
            }
        }
        _ => {}
    }
}

/// Minimal base64 encoder (avoids a dependency for one call site).
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

    #[test]
    fn test_detect_bru_file() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "meta {\n  name: Login\n  type: http\n  seq: 1\n}\n\n",
            "post {\n  url: {{baseUrl}}/login\n  body: json\n  auth: none\n}\n"
        );
        assert!(adapter.detect(data.as_bytes()));
    }

    #[test]
    fn test_detect_collection_bru() {
        // Collection .bru has meta + headers but no method block — still a
        // .bru file (parse() will reject it as a non-request, detect passes).
        let adapter = BruInputAdapter;
        let data = concat!(
            "meta {\n  name: My Collection\n  type: collection\n}\n\n",
            "headers {\n  check: again\n}\n"
        );
        assert!(adapter.detect(data.as_bytes()));
    }

    #[test]
    fn test_detect_rejects_json() {
        let adapter = BruInputAdapter;
        assert!(!adapter.detect(br#"{"meta": {"name": "x"}}"#));
    }

    #[test]
    fn test_detect_rejects_http_file() {
        let adapter = BruInputAdapter;
        assert!(!adapter.detect(b"### List\nGET https://x.dev/a\n"));
    }

    #[test]
    fn test_detect_rejects_prose() {
        let adapter = BruInputAdapter;
        assert!(!adapter.detect(b"just some text with { braces }\n"));
    }

    #[test]
    fn test_parse_simple_request() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "meta {\n  name: Get Users\n  type: http\n  seq: 2\n}\n\n",
            "get {\n  url: https://x.dev/users\n  body: none\n  auth: none\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        assert_eq!(s.info.name, "Get Users");
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "https://x.dev/users");
        assert!(req.body.is_none());
    }

    #[test]
    fn test_parse_json_body() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "post {\n  url: https://x.dev/users\n  body: json\n  auth: none\n}\n\n",
            "body:json {\n  {\n    \"name\": \"alice\"\n  }\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Json(v) => assert_eq!(v["name"], "alice"),
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_json_body_with_braces_inside_strings() {
        // Bodies containing `}` characters inside strings/lines must not end
        // the block early — only a line that is EXACTLY `}` closes it.
        let adapter = BruInputAdapter;
        let data = concat!(
            "post {\n  url: https://x.dev/a\n  body: json\n  auth: none\n}\n\n",
            "body:json {\n  {\"a\": \"}\"}\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Json(v) => assert_eq!(v["a"], "}"),
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_headers_and_disabled() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "get {\n  url: https://x.dev/a\n  body: none\n  auth: none\n}\n\n",
            "headers {\n  Accept: application/json\n  ~X-Disabled: yes\n  token: {{token}}\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("Accept").unwrap(), "application/json");
        assert!(!req.headers.contains_key("X-Disabled"));
        assert_eq!(req.headers.get("token").unwrap(), "{{token}}");
    }

    #[test]
    fn test_parse_query_and_path_params() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "get {\n  url: https://x.dev/users/:id\n  body: none\n  auth: none\n}\n\n",
            "params:path {\n  id: 42\n}\n\n",
            "params:query {\n  page: 2\n  ~verbose: true\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://x.dev/users/42?page=2");
        assert!(req.query_params.is_empty());
    }

    #[test]
    fn test_parse_form_urlencoded() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "post {\n  url: https://x.dev/login\n  body: formUrlEncoded\n  auth: none\n}\n\n",
            "body:formUrlEncoded {\n  user: alice\n  ~secret: nope\n  pass: pw\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::UrlEncoded(fields) => {
                assert_eq!(fields.get("user").unwrap(), "alice");
                assert_eq!(fields.get("pass").unwrap(), "pw");
                assert!(!fields.contains_key("secret"));
            }
            other => panic!("Expected Body::UrlEncoded, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_multipart_form() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "post {\n  url: https://x.dev/upload\n  body: multipartForm\n  auth: none\n}\n\n",
            "body:multipartForm {\n  name: alice\n  avatar: @photo.png\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::FormData(fields) => {
                assert_eq!(fields.get("name").unwrap(), "alice");
                assert_eq!(fields.get("avatar").unwrap(), "@photo.png");
            }
            other => panic!("Expected Body::FormData, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_graphql_body() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "post {\n  url: https://x.dev/graphql\n  body: graphql\n  auth: none\n}\n\n",
            "body:graphql {\n  query {\n    user(id: 1) { name }\n  }\n}\n\n",
            "body:graphql:vars {\n  {\"id\": 1}\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::GraphQL { query, variables } => {
                assert!(query.contains("user(id: 1)"));
                assert_eq!(variables.as_ref().unwrap()["id"], 1);
            }
            other => panic!("Expected Body::GraphQL, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_bearer_auth() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "get {\n  url: https://x.dev/me\n  body: none\n  auth: bearer\n}\n\n",
            "auth:bearer {\n  token: {{token}}\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(
            req.headers.get("Authorization").unwrap(),
            "Bearer {{token}}"
        );
    }

    #[test]
    fn test_parse_basic_auth() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "post {\n  url: https://x.dev/p\n  body: none\n  auth: basic\n}\n\n",
            "auth:basic {\n  username: bruno\n  password: s3cret\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        let expected = base64_encode(b"bruno:s3cret");
        assert_eq!(
            req.headers.get("Authorization").unwrap(),
            &format!("Basic {expected}")
        );
    }

    #[test]
    fn test_explicit_authorization_header_wins() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "get {\n  url: https://x.dev/me\n  body: none\n  auth: bearer\n}\n\n",
            "headers {\n  Authorization: Custom value\n}\n\n",
            "auth:bearer {\n  token: t\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("Authorization").unwrap(), "Custom value");
    }

    #[test]
    fn test_parse_scripts_tests_assert_vars() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "meta {\n  name: full\n}\n",
            "post {\n  url: https://x.dev/a\n  body: none\n  auth: none\n}\n\n",
            "script:pre-request {\n  bru.setVar('t', '1')\n}\n\n",
            "script:post-response {\n  bru.setVar('id', res.body.id)\n}\n\n",
            "tests {\n  test('ok', () => expect(res.status).to.eql(200))\n}\n\n",
            "assert {\n  res.status: eq 200\n}\n\n",
            "vars:pre-request {\n  token: abc\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let item = &s.items[0];
        assert_eq!(item.prerequest.len(), 1);
        assert!(item.prerequest[0].contains("bru.setVar"));
        // post-response script runs BEFORE tests (Bruno ordering).
        assert_eq!(item.test.len(), 2);
        assert!(item.test[0].contains("res.body.id"));
        assert!(item.test[1].contains("expect(res.status)"));
        assert_eq!(item.assertions, vec!["res.status: eq 200"]);
        assert_eq!(
            s.variables.get("token"),
            Some(&serde_json::Value::String("abc".into()))
        );
    }

    #[test]
    fn test_parse_docs_block_becomes_description() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "get {\n  url: https://x.dev/a\n  body: none\n  auth: none\n}\n\n",
            "docs {\n  # Hello\n  World\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let d = s.info.description.unwrap();
        assert!(d.contains("# Hello"));
    }

    #[test]
    fn test_annotations_skipped() {
        let adapter = BruInputAdapter;
        let data = concat!(
            "get {\n  url: https://x.dev/a\n  body: none\n  auth: none\n}\n\n",
            "headers {\n  @description('''note''')\n  X-Real: 1\n}\n"
        );
        let s = adapter.parse(data.as_bytes()).unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("X-Real").unwrap(), "1");
        assert_eq!(req.headers.len(), 1);
    }

    #[test]
    fn test_collection_bru_without_method_errors() {
        let adapter = BruInputAdapter;
        let data = "meta {\n  name: c\n  type: collection\n}\n";
        let err = adapter.parse(data.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("method block"), "got: {err}");
    }

    #[test]
    fn test_missing_url_errors() {
        let adapter = BruInputAdapter;
        let data = "get {\n  body: none\n  auth: none\n}\n";
        assert!(adapter.parse(data.as_bytes()).is_err());
    }
}
