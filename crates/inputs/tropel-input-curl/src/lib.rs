//! # tropel-input-curl
//!
//! Input adapter that parses [cURL][curl] command lines — the format people
//! paste from API docs, `Copy as cURL` browser exports, and READMEs — and
//! produces a protocol-agnostic `Scenario`.
//!
//! [curl]: https://curl.se/docs/manpage.html
//!
//! ## Supported surface
//!
//! | cURL flag | Scenario field |
//! |-----------|----------------|
//! | positional URL | `request.url` |
//! | `-X, --request` | `request.method` (default GET; `-I` ⇒ HEAD; `-d`/`-F` ⇒ POST) |
//! | `-H, --header` | `request.headers` (duplicates joined with `, `; `-H "Name;"` = empty value) |
//! | `-d, --data, --data-raw, --data-binary` | `request.body` (parts joined with `&`; auto `Content-Type: application/x-www-form-urlencoded` when unset) |
//! | `--data-urlencode name=value` | appended to the body, value percent-encoded |
//! | `-F, --form` | `Body::FormData` (a `@file` part is kept verbatim as `"@path"` — no file-system context here) |
//! | `-u, --user user:pass` | `Authorization: Basic base64(user:pass)` header |
//! | `-A, --user-agent`, `-e, --referer`, `-b, --cookie` | their respective headers |
//! | `-L, --location` | `request.follow_redirects` |
//! | `--max-time SECONDS` | `request.timeout` |
//! | `-k -s -i -v -o FILE -w FMT --compressed` | accepted and ignored (transport/client concerns) |
//!
//! Tokenization handles single/double quotes, backslash escapes, `\`
//! line continuations, a leading `$` shell prompt, and multiple commands
//! (each line starting with `curl` becomes its own `ScenarioItem`).
//!
//! Out of scope: `@file` / `<file` body sources and config files
//! (`--config`), which need file-system context `parse()` doesn't have.

use base64::Engine;
use std::collections::HashMap;
use std::time::Duration;
use tropel_sdk::{Body, Method, Request};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, TropelError};
use tropel_sdk::{Scenario, ScenarioInfo, ScenarioItem};

// ── InputAdapter implementation ─────────────────────────────────

/// Input adapter for cURL command lines.
pub struct CurlInputAdapter;

inventory::submit!(
    InputAdapterRegistration::new("curl", || Box::new(CurlInputAdapter)).with_priority(24)
);

impl InputAdapter for CurlInputAdapter {
    fn id(&self) -> &str {
        "curl"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: the first significant line's first token must
        // be `curl` (a leading `$` shell prompt is tolerated). JSON exports,
        // .http request lines and prose never match.
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let mut parts = t.split_whitespace();
            let mut first = parts.next().unwrap_or_default();
            if first == "$" {
                first = parts.next().unwrap_or_default();
            }
            return first == "curl";
        }
        false
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| TropelError::Parse(format!("cURL input is not valid UTF-8: {e}")))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        let commands = split_commands(text);
        if commands.is_empty() {
            return Err(TropelError::Parse("no cURL command found".into()));
        }

        let items: Vec<ScenarioItem> = commands
            .iter()
            .enumerate()
            .map(|(i, cmd)| command_to_item(cmd, i))
            .collect::<Result<Vec<_>>>()?;

        Ok(Scenario {
            info: ScenarioInfo {
                name: "curl-commands".into(),
                description: Some("Imported from cURL command(s)".into()),
                schema: None,
            },
            items,
            variables: HashMap::new(),
            auth: None,
        })
    }
}

// ── Command splitting ───────────────────────────────────────────

/// Split the input into one token list per cURL command.
///
/// Lines ending with `\` are continuations of the same command; a completed
/// command is flushed at its last line. Blank lines and `#` comments between
/// commands are skipped, and only token lists that start with `curl` are
/// kept (stray prose lines are ignored).
fn split_commands(text: &str) -> Vec<Vec<String>> {
    fn flush(pending: &str, commands: &mut Vec<Vec<String>>) {
        if pending.trim().is_empty() {
            return;
        }
        let tokens = tokenize(pending);
        if tokens
            .first()
            .is_some_and(|t| t.as_str() == "curl" || t.as_str() == "curl.exe")
        {
            commands.push(tokens);
        }
    }

    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut pending = String::new();

    for line in text.lines() {
        if let Some(stripped) = line.strip_suffix('\\') {
            pending.push_str(stripped);
            pending.push(' ');
            continue;
        }
        pending.push_str(line);
        flush(&pending, &mut commands);
        pending.clear();
    }
    flush(&pending, &mut commands);
    commands
}

// ── Shell-ish tokenizer ─────────────────────────────────────────

/// Tokenize a command line: whitespace separates tokens, single quotes are
/// fully literal, double quotes allow backslash escapes, and a backslash
/// escapes the next character outside quotes. A leading `$` prompt token is
/// dropped.
fn tokenize(s: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum State {
        Plain,
        Single,
        Double,
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut state = State::Plain;
    let mut has_token = false;

    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::Plain => match c {
                '\'' => {
                    state = State::Single;
                    has_token = true;
                }
                '"' => {
                    state = State::Double;
                    has_token = true;
                }
                '\\' => {
                    if let Some(&next) = chars.peek() {
                        current.push(next);
                        chars.next();
                        has_token = true;
                    }
                }
                c if c.is_whitespace() => {
                    if has_token {
                        tokens.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                }
                c => {
                    current.push(c);
                    has_token = true;
                }
            },
            State::Single => {
                if c == '\'' {
                    state = State::Plain;
                } else {
                    current.push(c);
                }
            }
            State::Double => match c {
                '"' => state = State::Plain,
                '\\' => {
                    if let Some(&next) = chars.peek() {
                        // Inside double quotes curl's shell only escapes
                        // \"$` and backslash; escaping any single char is a
                        // harmless superset for a command-line parser.
                        current.push(next);
                        chars.next();
                    }
                }
                c => current.push(c),
            },
        }
    }
    if has_token {
        tokens.push(current);
    }

    // Drop a leading shell-prompt `$`.
    if tokens.first().map(String::as_str) == Some("$") {
        tokens.remove(0);
    }
    tokens
}

// ── Command → ScenarioItem ──────────────────────────────────────

/// Flags that take a value in the next token or glued (`-XPOST`).
const VALUE_FLAGS: [&str; 20] = [
    "-X",
    "--request",
    "-H",
    "--header",
    "-d",
    "--data",
    "--data-raw",
    "--data-binary",
    "--data-urlencode",
    "-F",
    "--form",
    "-u",
    "--user",
    "-A",
    "--user-agent",
    "-e",
    "--referer",
    "-b",
    "--cookie",
    "--max-time",
];

/// Flags accepted and ignored (no replayable semantic).
const IGNORED_FLAGS: [&str; 9] = [
    "-k",
    "--insecure",
    "-s",
    "--silent",
    "-i",
    "--include",
    "-v",
    "--verbose",
    "--compressed",
];

fn command_to_item(tokens: &[String], index: usize) -> Result<ScenarioItem> {
    // tokens[0] is `curl` — skip it.
    let mut url: Option<String> = None;
    let mut method: Option<Method> = None;
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut data_parts: Vec<String> = Vec::new();
    let mut form_parts: Vec<(String, String)> = Vec::new();
    let mut follow_redirects = false;
    let mut timeout: Option<Duration> = None;
    let mut has_data = false;
    let mut has_form = false;
    let mut head_request = false;

    let mut i = 1;
    while i < tokens.len() {
        let tok = tokens[i].as_str();

        // --flag=value long form.
        if let Some(eq_value) = tok.strip_prefix("--") {
            if let Some((name, value)) = eq_value.split_once('=') {
                let flag = format!("--{name}");
                if VALUE_FLAGS.contains(&flag.as_str()) {
                    apply_flag(
                        &flag,
                        Some(value),
                        &mut method,
                        &mut headers,
                        &mut data_parts,
                        &mut form_parts,
                        &mut timeout,
                        &mut has_data,
                        &mut has_form,
                        index,
                    )?;
                    i += 1;
                    continue;
                }
            }
        }

        if VALUE_FLAGS.contains(&tok) {
            let value = tokens.get(i + 1).ok_or_else(|| {
                TropelError::Parse(format!(
                    "Command #{}: flag {} requires a value",
                    index + 1,
                    tok
                ))
            })?;
            apply_flag(
                tok,
                Some(value),
                &mut method,
                &mut headers,
                &mut data_parts,
                &mut form_parts,
                &mut timeout,
                &mut has_data,
                &mut has_form,
                index,
            )?;
            i += 2;
            continue;
        }

        // Short flag with glued value: -XPOST, -H'Name: v', -ddata…
        if tok.len() > 2 && tok.starts_with('-') && !tok.starts_with("--") {
            let flag = &tok[..2];
            if VALUE_FLAGS.contains(&flag) {
                apply_flag(
                    flag,
                    Some(&tok[2..]),
                    &mut method,
                    &mut headers,
                    &mut data_parts,
                    &mut form_parts,
                    &mut timeout,
                    &mut has_data,
                    &mut has_form,
                    index,
                )?;
                i += 1;
                continue;
            }
        }

        if IGNORED_FLAGS.contains(&tok)
            || tok == "-o"
            || tok == "--output"
            || tok == "-w"
            || tok == "--write-out"
        {
            // -o/-w take a value that is not replayable — skip it.
            let skips_value =
                tok == "-o" || tok == "--output" || tok == "-w" || tok == "--write-out";
            i += 1 + usize::from(skips_value);
            continue;
        }
        if tok == "-L" || tok == "--location" {
            follow_redirects = true;
            i += 1;
            continue;
        }
        if tok == "-I" || tok == "--head" {
            head_request = true;
            i += 1;
            continue;
        }
        if tok == "-G" || tok == "--get" {
            // -G moves --data to the query string; rare in pasted commands.
            // Treated as a no-op here (documented limitation).
            i += 1;
            continue;
        }

        // Positional argument = the URL.
        if !tok.starts_with('-') && url.is_none() {
            url = Some(tokens[i].clone());
            i += 1;
            continue;
        }

        // Unknown flag: skip it (and its value if it looks like one) rather
        // than failing the whole import for an exotic-but-harmless option.
        i += 1;
    }

    let url = url.ok_or_else(|| {
        TropelError::Parse(format!("Command #{}: no URL argument found", index + 1))
    })?;

    // curl method resolution: explicit -X wins; -I forces HEAD; -d/-F imply
    // POST; otherwise GET.
    let method = method
        .or({
            if head_request {
                Some(Method::HEAD)
            } else if has_data || has_form {
                Some(Method::POST)
            } else {
                None
            }
        })
        .unwrap_or(Method::GET);

    // -d sends urlencoded data unless a Content-Type was set explicitly.
    if has_data
        && !headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("content-type"))
    {
        headers.insert(
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        );
    }

    // Body variant: -F wins over -d (curl errors on both; last one kept).
    let body = if has_form {
        Some(Body::FormData(form_parts.into_iter().collect()))
    } else if has_data {
        let text = data_parts.join("&");
        pick_body(&text, &headers)
    } else {
        None
    };

    let item_name = generate_item_name(&url, index);

    Ok(ScenarioItem {
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
            follow_redirects,
            timeout,
            response_type: tropel_sdk::ResponseType::Text,
        }),
        prerequest: vec![],
        test: vec![],
        assertions: vec![],
        items: vec![],
    })
}

/// Apply a value-taking flag. `value` is `Some` for every call site (all
/// VALUE_FLAGS take values); kept as `Option` to mirror glued/long forms.
#[allow(clippy::too_many_arguments)]
fn apply_flag(
    flag: &str,
    value: Option<&str>,
    method: &mut Option<Method>,
    headers: &mut HashMap<String, String>,
    data_parts: &mut Vec<String>,
    form_parts: &mut Vec<(String, String)>,
    timeout: &mut Option<Duration>,
    has_data: &mut bool,
    has_form: &mut bool,
    index: usize,
) -> Result<()> {
    let value = value.ok_or_else(|| {
        TropelError::Parse(format!(
            "Command #{}: flag {} requires a value",
            index + 1,
            flag
        ))
    })?;
    match flag {
        "-X" | "--request" => {
            *method = Some(Method::parse(value).ok_or_else(|| {
                TropelError::Parse(format!(
                    "Command #{}: invalid method {:?}",
                    index + 1,
                    value
                ))
            })?);
        }
        "-H" | "--header" => {
            // `-H "Name;"` (semicolon, no value) sets an EMPTY header value.
            if let Some(name) = value.strip_suffix(';') {
                let name = name.trim();
                if !name.is_empty() {
                    headers.insert(name.to_string(), String::new());
                }
            } else if let Some((name, val)) = value.split_once(':') {
                let name = name.trim();
                if name.is_empty() {
                    return Err(TropelError::Parse(format!(
                        "Command #{}: malformed header {:?}",
                        index + 1,
                        value
                    )));
                }
                match headers.get_mut(name) {
                    Some(existing) => {
                        existing.push_str(", ");
                        existing.push_str(val.trim());
                    }
                    None => {
                        headers.insert(name.to_string(), val.trim().to_string());
                    }
                }
            } else {
                return Err(TropelError::Parse(format!(
                    "Command #{}: malformed header {:?}",
                    index + 1,
                    value
                )));
            }
        }
        "-d" | "--data" | "--data-raw" | "--data-binary" => {
            data_parts.push(value.to_string());
            *has_data = true;
        }
        "--data-urlencode" => {
            // `name=value` → url-encode the value; a bare value is encoded
            // whole (curl semantics).
            let encoded = match value.split_once('=') {
                Some((name, val)) => format!("{}={}", name, percent_encode(val)),
                None => percent_encode(value),
            };
            data_parts.push(encoded);
            *has_data = true;
        }
        "-F" | "--form" => {
            match value.split_once('=') {
                Some((name, val)) => form_parts.push((name.to_string(), val.to_string())),
                None => {
                    return Err(TropelError::Parse(format!(
                        "Command #{}: malformed form part {:?}",
                        index + 1,
                        value
                    )))
                }
            }
            *has_form = true;
        }
        "-u" | "--user" => {
            let creds = value.trim_end_matches(':'); // `-u user:` = no password
            let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
            headers.insert("Authorization".into(), format!("Basic {encoded}"));
        }
        "-A" | "--user-agent" => {
            headers.insert("User-Agent".into(), value.to_string());
        }
        "-e" | "--referer" => {
            headers.insert("Referer".into(), value.to_string());
        }
        "-b" | "--cookie" => {
            // A literal `k=v; k2=v2` string (a jar-file path would also land
            // here — documented limitation: no file-system context).
            headers.insert("Cookie".into(), value.to_string());
        }
        "--max-time" => {
            let secs: f64 = value.parse().map_err(|_| {
                TropelError::Parse(format!(
                    "Command #{}: invalid --max-time {:?}",
                    index + 1,
                    value
                ))
            })?;
            *timeout = Some(Duration::from_secs_f64(secs));
        }
        _ => {}
    }
    Ok(())
}

/// Pick the `Body` variant from the body text and Content-Type header
/// (mirrors tropel-input-http): JSON parses to `Body::Json`, invalid JSON
/// falls back to verbatim `Raw`, everything else is `Raw`.
fn pick_body(text: &str, headers: &HashMap<String, String>) -> Option<Body> {
    if text.is_empty() {
        return None;
    }
    let content_type = headers
        .get("Content-Type")
        .or_else(|| headers.get("content-type"))
        .map(|v| v.to_lowercase())
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

/// Percent-encode a form value (RFC 3986 unreserved characters kept).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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
    fn test_detect_simple() {
        let adapter = CurlInputAdapter;
        assert!(adapter.detect(b"curl https://api.example.com/users"));
    }

    #[test]
    fn test_detect_with_prompt_and_flags() {
        let adapter = CurlInputAdapter;
        assert!(adapter.detect(b"$ curl -X POST https://x.dev/a -d 'k=v'"));
    }

    #[test]
    fn test_detect_rejects_http_file() {
        let adapter = CurlInputAdapter;
        assert!(!adapter.detect(b"### List\nGET https://api.example.com/users\n"));
    }

    #[test]
    fn test_detect_rejects_prose() {
        let adapter = CurlInputAdapter;
        assert!(!adapter.detect(b"run the downloader to fetch things\n"));
    }

    #[test]
    fn test_simple_get() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl https://api.example.com/users")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(req.url, "https://api.example.com/users");
        assert!(req.body.is_none());
    }

    #[test]
    fn test_post_with_data_and_auto_content_type() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl -X POST https://api.example.com/users -d 'name=alice&role=admin'")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(
            req.headers.get("Content-Type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "name=alice&role=admin"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_data_implies_post_without_x() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl https://api.example.com/tokens -d 'grant_type=client_credentials'")
            .unwrap();
        assert_eq!(s.items[0].request.as_ref().unwrap().method, Method::POST);
    }

    #[test]
    fn test_json_content_type_parses_body() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(
                concat!(
                    "curl -X POST https://api.example.com/users ",
                    "-H 'Content-Type: application/json' ",
                    "-d '{\"name\": \"alice\"}'"
                )
                .as_bytes(),
            )
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Json(v) => assert_eq!(v["name"], "alice"),
            other => panic!("Expected Body::Json, got {:?}", other),
        }
    }

    #[test]
    fn test_multiple_data_joined_with_ampersand() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl https://x.dev/a -d 'a=1' -d 'b=2' -d 'c=3'")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "a=1&b=2&c=3"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_data_urlencode_encodes_value() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl https://x.dev/search --data-urlencode 'q=hello world&more'")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "q=hello%20world%26more"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_headers_and_duplicates() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(
                concat!(
                    "curl https://x.dev/a ",
                    "-H 'Accept: application/json' ",
                    "-H 'X-Trace: 1' -H 'X-Trace: 2' ",
                    "-H 'X-Empty;'"
                )
                .as_bytes(),
            )
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("Accept").unwrap(), "application/json");
        assert_eq!(req.headers.get("X-Trace").unwrap(), "1, 2");
        assert_eq!(req.headers.get("X-Empty").unwrap(), "");
    }

    #[test]
    fn test_glued_short_flags() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl -XPOST https://x.dev/a -H'Accept: text/plain' -dbody=1")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.headers.get("Accept").unwrap(), "text/plain");
        match req.body.as_ref().unwrap() {
            Body::Raw(t) => assert_eq!(t, "body=1"),
            other => panic!("Expected Body::Raw, got {:?}", other),
        }
    }

    #[test]
    fn test_long_flag_equals_form() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl --request=DELETE https://x.dev/a/1")
            .unwrap();
        assert_eq!(s.items[0].request.as_ref().unwrap().method, Method::DELETE);
    }

    #[test]
    fn test_basic_auth() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl -u alice:secret https://x.dev/private")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        let expected = base64::engine::general_purpose::STANDARD.encode("alice:secret");
        assert_eq!(
            req.headers.get("Authorization").unwrap(),
            &format!("Basic {expected}")
        );
    }

    #[test]
    fn test_agent_referer_cookie() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(
                concat!(
                    "curl https://x.dev/a ",
                    "-A 'KnockPort/1.0' ",
                    "-e https://x.dev/home ",
                    "-b 'session=abc; theme=dark'"
                )
                .as_bytes(),
            )
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.headers.get("User-Agent").unwrap(), "KnockPort/1.0");
        assert_eq!(req.headers.get("Referer").unwrap(), "https://x.dev/home");
        assert_eq!(
            req.headers.get("Cookie").unwrap(),
            "session=abc; theme=dark"
        );
    }

    #[test]
    fn test_form_parts() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl -F name=alice -F 'avatar=@photo.png;type=image/png' https://x.dev/upload")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::POST);
        match req.body.as_ref().unwrap() {
            Body::FormData(fields) => {
                assert_eq!(fields.get("name").unwrap(), "alice");
                // @file parts kept verbatim — no FS context to read them.
                assert_eq!(fields.get("avatar").unwrap(), "@photo.png;type=image/png");
            }
            other => panic!("Expected Body::FormData, got {:?}", other),
        }
    }

    #[test]
    fn test_head_request() {
        let adapter = CurlInputAdapter;
        let s = adapter.parse(b"curl -I https://x.dev/a").unwrap();
        assert_eq!(s.items[0].request.as_ref().unwrap().method, Method::HEAD);
    }

    #[test]
    fn test_follow_redirects_and_timeout() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl -L --max-time 5 https://x.dev/a")
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert!(req.follow_redirects);
        assert_eq!(req.timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn test_multiple_commands() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(
                concat!(
                    "curl https://x.dev/a\n",
                    "curl -X POST https://x.dev/b -d 'k=1'\n"
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items[1].request.as_ref().unwrap().method, Method::POST);
    }

    #[test]
    fn test_line_continuation() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(
                concat!(
                    "curl -X POST https://x.dev/a \\\n",
                    "  -H 'Content-Type: application/json' \\\n",
                    "  -d '{\"a\": 1}'\n"
                )
                .as_bytes(),
            )
            .unwrap();
        let req = s.items[0].request.as_ref().unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.headers.get("Content-Type").unwrap(), "application/json");
        assert!(matches!(req.body.as_ref().unwrap(), Body::Json(_)));
    }

    #[test]
    fn test_missing_url_errors() {
        let adapter = CurlInputAdapter;
        assert!(adapter.parse(b"curl -X POST -d 'k=1'").is_err());
    }

    #[test]
    fn test_missing_flag_value_errors() {
        let adapter = CurlInputAdapter;
        assert!(adapter.parse(b"curl https://x.dev/a -X").is_err());
    }

    #[test]
    fn test_names_generated_from_path() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl https://x.dev/users/42?verbose=1")
            .unwrap();
        assert_eq!(s.items[0].name, "request #1 (42)");
    }

    #[test]
    fn test_ignored_flags_skip_value() {
        let adapter = CurlInputAdapter;
        let s = adapter
            .parse(b"curl -s -k --compressed -o out.json -w '%{http_code}' https://x.dev/a")
            .unwrap();
        assert_eq!(s.items[0].request.as_ref().unwrap().url, "https://x.dev/a");
    }
}
