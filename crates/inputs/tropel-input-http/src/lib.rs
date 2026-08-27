//! # tropel-input-http
//!
//! Input adapter that reads [`.http` / `.rest` request files][rest-client] —
//! the plain-text format shared by the VS Code REST Client and the JetBrains
//! HTTP client — and produces a protocol-agnostic `Scenario`.
//!
//! [rest-client]: https://github.com/Huachao/vscode-restclient#request-line
//!
//! ## Grammar (the subset this adapter supports)
//!
//! ```text
//! @host = api.example.com          # file variable (→ Scenario.variables)
//!
//! ### Create user                  # separator; trailing text = request name
//! POST https://{{host}}/users
//! Content-Type: application/json
//! Authorization: Bearer tok
//!
//! {"name": "alice"}
//!
//! ### List users
//! GET https://{{host}}/users
//!   ?page=2                        # query continuation lines
//!   &limit=10
//! ```
//!
//! | .http construct | Scenario field |
//! |-----------------|-----------------|
//! | `###` separator (name) | one `ScenarioItem` per block, name from the separator |
//! | `METHOD URL` / bare `URL` | `request.method` (bare ⇒ GET) / `request.url` |
//! | `?`/`&` continuation lines | appended to the URL query (query_params stays empty) |
//! | `Name: value` until a blank line | `request.headers` (duplicates joined with `, `) |
//! | lines after the blank line | `request.body` (variant picked from Content-Type) |
//! | `@name = value` | `Scenario.variables` (`{{name}}` stays verbatim — the runtime resolves it) |
//! | `#` / `//` comments | skipped before the request line; body content is kept verbatim |
//! | `> {% … %}` response handlers | dropped (client-side scripts, not replayable here) |
//!
//! Out of scope: `< file` / `<> file` body includes (no file-system context
//! in `parse()`), per-request named variables (`# @name value`), and response
//! handler scripts — such lines survive verbatim only where harmless, and
//! body-include directives end up as raw text.

use std::collections::HashMap;
use tropel_sdk::{Body, Method, Request};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

// ── InputAdapter implementation ─────────────────────────────────

/// Input adapter for `.http` / `.rest` request files.
pub struct HttpFileAdapter;

inventory::submit!(
    InputAdapterRegistration::new("http", || Box::new(HttpFileAdapter)).with_priority(25)
);

impl InputAdapter for HttpFileAdapter {
    fn id(&self) -> &str {
        "http"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: the first significant line (skipping blanks,
        // comments, `###` separators and `@variable` definitions) must be a
        // request line — `METHOD URL` or a bare absolute http(s) URL. JSON
        // exports, cURL commands and JS scripts never match that shape.
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
                continue;
            }
            if t.starts_with('@') {
                continue;
            }
            return parse_request_line(t).is_some();
        }
        false
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| TropelError::Parse(format!(".http file is not valid UTF-8: {e}")))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        let mut variables: HashMap<String, serde_json::Value> = HashMap::new();
        // (name hint from the `###` line, raw block lines)
        let mut blocks: Vec<(Option<String>, Vec<&str>)> = vec![(None, Vec::new())];

        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("###") {
                let name = rest.trim();
                blocks.push(((!name.is_empty()).then(|| name.to_string()), Vec::new()));
                continue;
            }
            if let Some(rest) = t.strip_prefix('@') {
                if let Some((name, value)) = parse_variable(rest) {
                    variables.insert(name, serde_json::Value::String(value));
                    continue;
                }
            }
            blocks.last_mut().expect("always one block").1.push(line);
        }

        let mut items: Vec<ScenarioItem> = Vec::new();
        for (name_hint, lines) in blocks {
            let index = items.len();
            if let Some(item) = block_to_item(name_hint, &lines, index)? {
                items.push(item);
            }
        }

        if items.is_empty() {
            return Err(TropelError::Parse(".http file contains no requests".into()));
        }

        Ok(Scenario {
            info: ScenarioInfo {
                name: "http-file".into(),
                description: Some("Imported from .http request file".into()),
                schema: None,
            },
            items,
            variables,
            auth: None,
            conversion_notes: Vec::new(),
        })
    }
}

/// Parse a request line: `METHOD URL`, a bare `URL`, or `METHOD URL HTTP/x.y`
/// (the trailing protocol token is tolerated and ignored). Returns `None` for
/// anything that is not a request line.
fn parse_request_line(line: &str) -> Option<(Method, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let first = parts.next()?;
    let (method, url) = if first.starts_with("http://") || first.starts_with("https://") {
        // Bare URL — the REST Client default method is GET.
        (Method::GET, first)
    } else {
        let method = Method::parse(first)?;
        let url = parts.next()?;
        (method, url)
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    Some((method, url.to_string()))
}

/// Parse a file-variable definition body (the part after `@`):
/// `name = value`. The value is kept verbatim (trimmed).
fn parse_variable(rest: &str) -> Option<(String, String)> {
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

/// Parse a header line `Name: value`. Returns `None` when the line is not a
/// header (no colon, empty name, or invalid header-name characters).
fn parse_header_line(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once(':')?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
    {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

/// Convert one `###`-delimited block into a `ScenarioItem`.
///
/// Returns `Ok(None)` for blocks that hold no request (only comments /
/// blanks / variable lines live at file level, but a leading pre-separator
/// block may be empty). A block whose first significant line is neither a
/// request line nor a comment is a parse error — silently skipping it would
/// hide a malformed request from the user.
fn block_to_item(
    name_hint: Option<String>,
    lines: &[&str],
    index: usize,
) -> Result<Option<ScenarioItem>> {
    // Skip leading blanks / comments before the request line.
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
            i += 1;
            continue;
        }
        break;
    }
    if i >= lines.len() {
        return Ok(None);
    }

    let Some((method, mut url)) = parse_request_line(lines[i]) else {
        return Err(TropelError::Parse(format!(
            "Request #{}: expected `METHOD URL` line, found {:?}",
            index + 1,
            lines[i].trim()
        )));
    };
    i += 1;

    // Query continuation lines: `?page=2` / `&limit=10` following the
    // request line extend the URL query (REST Client multi-line queries).
    while i < lines.len() {
        let t = lines[i].trim();
        if t.starts_with('?') || t.starts_with('&') {
            let fragment = t.trim_start_matches(['?', '&']);
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(fragment);
            i += 1;
        } else {
            break;
        }
    }

    // Header lines until a blank line. A non-header line before the blank
    // means the author omitted the separator — treat it as body start.
    // W2 #203: ordered Vec in file order; duplicates fold (`, ` join) so no
    // header value is dropped.
    let mut headers: Vec<(String, String)> = Vec::new();
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() {
            i += 1;
            break;
        }
        let Some((name, value)) = parse_header_line(t) else {
            break;
        };
        merge_header(&mut headers, name, value);
        i += 1;
    }

    // Body: everything else. JetBrains response-handler blocks (`> {% … %}`)
    // are client-side scripts — stop the body at the first `>` line.
    let mut body_lines: Vec<&str> = Vec::new();
    while i < lines.len() {
        let t = lines[i].trim();
        if t.starts_with('>') {
            break;
        }
        body_lines.push(lines[i]);
        i += 1;
    }
    let body_text = body_lines.join("\n").trim_end().to_string();
    let body = pick_body(&body_text, &headers);

    let item_name = name_hint.unwrap_or_else(|| generate_item_name(&url, index));

    Ok(Some(ScenarioItem {
        name: item_name,
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
            host: None,
            cookies: Vec::new(),
            timeout: None,
            response_type: tropel_sdk::ResponseType::Text,
        }),
        prerequest: vec![],
        test: vec![],
        assertions: vec![],
        items: vec![],
    }))
}

/// Pick the `Body` variant from the body text and Content-Type header:
/// JSON bodies parse to `Body::Json` (invalid JSON falls back to verbatim
/// `Raw` — re-quoting would change the payload), everything else is `Raw`
/// (urlencoded / multipart bodies are kept in wire format with their
/// Content-Type header preserved, mirroring the HAR adapter).
fn pick_body(text: &str, headers: &[(String, String)]) -> Option<Body> {
    if text.is_empty() {
        return None;
    }
    let content_type = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_lowercase())
        .unwrap_or_default();
    if content_type.contains("json") {
        match serde_json::from_str(text) {
            Ok(v) => Some(Body::Json(v)),
            Err(_) => Some(Body::Raw(text.to_string())),
        }
    } else {
        Some(Body::Raw(text.to_string()))
    }
}

/// Insert a header, joining duplicate names with `, ` (RFC 9110 field-line
/// combination) instead of silently dropping data.
fn merge_header(headers: &mut Vec<(String, String)>, name: String, value: String) {
    match headers.iter_mut().find(|(n, _)| n == &name) {
        Some((_, existing)) => {
            existing.push_str(", ");
            existing.push_str(&value);
        }
        None => headers.push((name, value)),
    }
}

/// Generate a human-readable item name from a URL.
fn generate_item_name(url: &str, index: usize) -> String {
    if let Some(path_start) = url.find("://") {
        let after_scheme = &url[path_start + 3..];
        if let Some(path_pos) = after_scheme.find('/') {
            let path = &after_scheme[path_pos..];
            let path = path.split(['?', '#']).next().unwrap_or(path);
            let path = path.trim_end_matches('/');
            if let Some(last_seg) = path.rsplit('/').find(|s: &&str| !s.is_empty()) {
                return format!("request #{} ({})", index + 1, last_seg);
            }
        }
    }
    format!("request #{}", index + 1)
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_simple_file() {
        let adapter = HttpFileAdapter;
        let data = b"### List users\nGET https://api.example.com/users\nAccept: application/json\n";
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_bare_url() {
        let adapter = HttpFileAdapter;
        let data = b"https://api.example.com/health\n";
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_skips_comments_and_variables() {
        let adapter = HttpFileAdapter;
        let data =
            b"# A comment\n// another comment\n@host = example.com\n\nGET https://{{host}}/x\n";
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_rejects_curl() {
        let adapter = HttpFileAdapter;
        let data = b"curl -X POST https://api.example.com/users -d '{\"a\":1}'";
        assert!(!adapter.detect(data), "curl commands are not .http files");
    }

    #[test]
    fn test_detect_rejects_json() {
        let adapter = HttpFileAdapter;
        let data = br#"{"log": {"version": "1.2", "entries": []}}"#;
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_detect_rejects_plain_text() {
        let adapter = HttpFileAdapter;
        let data = b"hello world\nthis is just text\n";
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_detect_rejects_relative_url() {
        // .http requests must be absolute in this adapter — a bare path is
        // not distinguishable from prose.
        let adapter = HttpFileAdapter;
        let data = b"GET /users\n";
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_parse_multiple_named_requests() {
        let adapter = HttpFileAdapter;
        let data = b"### List users\nGET https://api.example.com/users\n\n### Create user\nPOST https://api.example.com/users\n\n";
        let scenario = adapter.parse(data).unwrap();
        assert_eq!(scenario.items.len(), 2);
        assert_eq!(scenario.items[0].name, "List users");
        assert_eq!(scenario.items[1].name, "Create user");
        assert_eq!(
            scenario.items[1].request.as_ref().unwrap().method,
            Method::POST
        );
    }

    #[test]
    fn test_parse_bare_url_defaults_get() {
        let adapter = HttpFileAdapter;
        let data = b"https://api.example.com/health\n";
        let scenario = adapter.parse(data).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "https://api.example.com/health");
    }

    #[test]
    fn test_parse_headers_and_duplicates() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "GET https://api.example.com/users\n",
            "Accept: application/json\n",
            "X-Trace: a\n",
            "X-Trace: b\n",
            "\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        let get = |k: &str| {
            req.headers
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert_eq!(get("Accept"), "application/json");
        assert_eq!(get("X-Trace"), "a, b");
    }

    #[test]
    fn test_parse_json_body() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "POST https://api.example.com/users\n",
            "Content-Type: application/json\n",
            "\n",
            "{\"name\": \"alice\"}\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        match req.body.as_ref().expect("body missing") {
            Body::Json(v) => assert_eq!(v["name"], "alice"),
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_invalid_json_falls_back_to_raw() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "POST https://api.example.com/echo\n",
            "Content-Type: application/json\n",
            "\n",
            "not actually json\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "not actually json"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_urlencoded_body_kept_verbatim() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "POST https://api.example.com/login\n",
            "Content-Type: application/x-www-form-urlencoded\n",
            "\n",
            "user=alice&pass=secret\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "user=alice&pass=secret"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_query_continuation_lines() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "GET https://api.example.com/users\n",
            "  ?page=2\n",
            "  &limit=10\n",
            "\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://api.example.com/users?page=2&limit=10");
        // Query stays in the URL — query_params must not re-append it.
        assert!(req.query_params.is_empty());
    }

    #[test]
    fn test_parse_query_continuation_with_existing_query() {
        let adapter = HttpFileAdapter;
        let data = concat!("GET https://api.example.com/search?q=a\n", "  &sort=asc\n");
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://api.example.com/search?q=a&sort=asc");
    }

    #[test]
    fn test_parse_file_variables() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "@host = api.example.com\n",
            "@page_size = 25\n",
            "\n",
            "### List\n",
            "GET https://{{host}}/users?page={{page_size}}\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        assert_eq!(
            scenario.variables.get("host"),
            Some(&serde_json::Value::String("api.example.com".into()))
        );
        assert_eq!(
            scenario.variables.get("page_size"),
            Some(&serde_json::Value::String("25".into()))
        );
        // {{var}} placeholders stay verbatim — the runtime resolves them.
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://{{host}}/users?page={{page_size}}");
    }

    #[test]
    fn test_parse_response_handlers_dropped() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "GET https://api.example.com/users\n",
            "\n",
            "> {%\n",
            "  client.log(response.body('name'));\n",
            "%}\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert!(req.body.is_none(), "handler script must not leak into body");
    }

    #[test]
    fn test_parse_body_without_blank_line_after_headers() {
        // Lenient recovery: a non-header line in the header section means the
        // author skipped the blank separator — it becomes the body.
        let adapter = HttpFileAdapter;
        let data = concat!(
            "POST https://api.example.com/echo\n",
            "Content-Type: text/plain\n",
            "hello body\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "hello body"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_comments_before_request_skipped() {
        let adapter = HttpFileAdapter;
        let data = concat!(
            "# leading comment\n",
            "// another\n",
            "\n",
            "GET https://api.example.com/users\n"
        );
        let scenario = adapter.parse(data.as_bytes()).unwrap();
        assert_eq!(scenario.items.len(), 1);
    }

    #[test]
    fn test_parse_generates_name_from_path() {
        let adapter = HttpFileAdapter;
        let data = b"GET https://api.example.com/users/123?verbose=1\n";
        let scenario = adapter.parse(data).unwrap();
        assert_eq!(scenario.items[0].name, "request #1 (123)");
    }

    #[test]
    fn test_parse_empty_file_errors() {
        let adapter = HttpFileAdapter;
        assert!(adapter.parse(b"").is_err());
        assert!(adapter.parse(b"# only a comment\n").is_err());
    }

    #[test]
    fn test_parse_malformed_block_errors() {
        let adapter = HttpFileAdapter;
        let data = b"this is not a request line\n";
        let err = adapter.parse(data).unwrap_err();
        assert!(
            err.to_string().contains("METHOD URL"),
            "error should point at the malformed line: {err}"
        );
    }

    #[test]
    fn test_parse_http_version_token_ignored() {
        // Raw HTTP-style request lines (`GET /x HTTP/1.1`) carry a protocol
        // token that .http files don't use — tolerated and ignored when the
        // URL is absolute.
        let adapter = HttpFileAdapter;
        let data = b"GET https://api.example.com/x HTTP/1.1\n";
        let scenario = adapter.parse(data).unwrap();
        let req = scenario.items[0].request.as_ref().unwrap();
        assert_eq!(req.url, "https://api.example.com/x");
    }
}
