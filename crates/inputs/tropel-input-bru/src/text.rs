//! Bruno's `.bru` TEXT format — the on-disk one.
//!
//! TR-458. The crate previously read only Bruno's collection-JSON interchange,
//! so a user pointing at a real Bruno folder got nothing. KnockPort filled the
//! gap with a 683-line TypeScript parser (`packages/format/src/bruno-file.ts`)
//! whose own header calls it a stopgap "until the crate lands". This is that
//! crate landing; the stopgap is deleted in the same change.
//!
//! ## Shape
//!
//! A `.bru` file is ONE request: brace-delimited blocks of `key: value` pairs,
//! with `~key` marking a disabled pair and `name:subtype` naming the block —
//! `body:json`, `auth:bearer`, `vars:pre-request`. Some blocks carry raw text
//! rather than pairs (bodies, scripts, docs).
//!
//! ```text
//! meta { name: Login  type: http  seq: 2 }
//! post { url: https://api.test/login  body: form-urlencoded  auth: basic }
//! headers { content-type: application/x-www-form-urlencoded }
//! auth:basic { username: john  password: s3cret }
//! ```
//!
//! ## Why it produces JSON rather than a Scenario
//!
//! It builds the same shape Bruno's own EXPORT produces, and hands it to the
//! JSON path that already exists. That is the point: the mapping from Bruno to
//! a `Scenario` — methods, headers, param merging, auth modes, body modes, the
//! path-param substitution — stays in ONE place. A second mapping here is the
//! invariant #3 failure this crate exists to prevent, and it is exactly the
//! shape the TypeScript stopgap had (its own mapping, drifting quietly).

use serde_json::{json, Map, Value};

/// Blocks whose body is raw text rather than `key: value` pairs.
const TEXT_BLOCKS: &[&str] = &[
    "body",
    "body:json",
    "body:text",
    "body:xml",
    "body:sparql",
    "body:graphql",
    "body:graphql:vars",
    "body:file",
    "script:pre-request",
    "script:post-response",
    "tests",
    "docs",
];

/// Does this text open with a `meta {` block?
///
/// Every `.bru` file starts with one, and no JSON, YAML or cURL document does
/// — a YAML mapping would need `meta:` with a colon. Cheap and specific enough
/// to keep the import chain from mis-claiming foreign text.
pub fn looks_like_bru_text(text: &str) -> bool {
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(rest) = t.strip_prefix("meta") else {
            return false;
        };
        return rest.trim_start().starts_with('{');
    }
    false
}

#[derive(Debug)]
struct Pair {
    key: String,
    value: String,
    enabled: bool,
}

#[derive(Debug)]
struct Block {
    name: String,
    pairs: Vec<Pair>,
    text: String,
}

/// Strip the common leading indent, and the blank edges.
fn outdent(lines: &[String]) -> String {
    let cut = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let body: Vec<&str> = lines
        .iter()
        .map(|l| {
            if l.len() >= cut {
                &l[cut..]
            } else {
                l.as_str()
            }
        })
        .collect();
    body.join("\n").trim_matches('\n').to_string()
}

/// `name {` on a line of its own — the only way a block opens.
fn block_opener(line: &str) -> Option<&str> {
    let t = line.trim();
    let name_end = t.find('{')?;
    let name = t[..name_end].trim();
    if !t[name_end + 1..].trim().is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-') {
        return None;
    }
    Some(name)
}

/// One `key: value` line. `None` for a line that carries no pair.
///
/// Returns `Some((key, None, enabled))` when the value opens a `'''` multiline.
fn parse_pair_line(raw: &str) -> Option<(String, Option<String>, bool)> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    let (enabled, rest) = match line.strip_prefix('~') {
        Some(r) => (false, r),
        None => (true, line),
    };
    let (key, value) = if let Some(after_quote) = rest.strip_prefix('"') {
        // A quoted key may itself contain a colon, so the closing quote is
        // found first and the separator looked for after it.
        let end = after_quote.find('"')?;
        let key = &after_quote[..end];
        let colon = after_quote[end..].find(':')?;
        (key, after_quote[end + colon + 1..].trim())
    } else {
        let colon = rest.find(':')?;
        (rest[..colon].trim(), rest[colon + 1..].trim())
    };
    // Bruno prefixes request-local vars with `@` (vars:pre-request); the
    // marker is dropped and the pair kept, so a local var maps like any other.
    let key = key.strip_prefix('@').unwrap_or(key).trim();
    if key.is_empty() {
        return None;
    }
    if value == "'''" {
        return Some((key.to_string(), None, enabled));
    }
    Some((key.to_string(), Some(value.to_string()), enabled))
}

/// Split a `.bru` document into its blocks.
fn split_blocks(text: &str) -> Result<Vec<Block>, String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<(String, Vec<Pair>, Vec<String>, bool)> = None;
    let mut multiline: Option<(String, bool, Vec<String>)> = None;

    for raw in text.lines() {
        if let Some((key, enabled, lines)) = multiline.as_mut() {
            match raw.find("'''") {
                None => {
                    lines.push(raw.to_string());
                    continue;
                }
                Some(end) => {
                    let tail = &raw[..end];
                    if !tail.trim().is_empty() {
                        lines.push(tail.to_string());
                    }
                    if let Some((_, pairs, _, _)) = current.as_mut() {
                        pairs.push(Pair {
                            key: std::mem::take(key),
                            value: outdent(lines),
                            enabled: *enabled,
                        });
                    }
                    multiline = None;
                    continue;
                }
            }
        }

        if let Some((name, pairs, text_lines, is_text)) = current.as_mut() {
            // A text block closes on a `}` at column 0, so a `}` INSIDE a JSON
            // body does not end it. A pair block tolerates indentation.
            let closing = if *is_text {
                raw.strip_prefix('}').is_some_and(|r| r.trim().is_empty())
            } else {
                raw.trim() == "}"
            };
            if closing {
                blocks.push(Block {
                    name: std::mem::take(name),
                    pairs: std::mem::take(pairs),
                    text: outdent(text_lines),
                });
                current = None;
                continue;
            }
            if *is_text {
                text_lines.push(raw.to_string());
                continue;
            }
            match parse_pair_line(raw) {
                None => continue,
                Some((key, None, enabled)) => {
                    multiline = Some((key, enabled, Vec::new()));
                    continue;
                }
                Some((key, Some(value), enabled)) => {
                    pairs.push(Pair {
                        key,
                        value,
                        enabled,
                    });
                    continue;
                }
            }
        }

        if let Some(name) = block_opener(raw) {
            let is_text = TEXT_BLOCKS.contains(&name);
            current = Some((name.to_string(), Vec::new(), Vec::new(), is_text));
        }
        // Lines outside a block (`tags [ … ]` list items) carry no request
        // data and are skipped.
    }

    if multiline.is_some() {
        return Err("unterminated ''' multiline value in the .bru document".into());
    }
    if let Some((name, _, _, _)) = current {
        return Err(format!("unterminated .bru block {name:?}"));
    }
    Ok(blocks)
}

fn find<'a>(blocks: &'a [Block], name: &str) -> Option<&'a Block> {
    blocks.iter().find(|b| b.name == name)
}

/// Enabled + disabled pairs as `[{name, value, enabled}]`.
fn pairs_to_kv(block: &Block) -> Value {
    Value::Array(
        block
            .pairs
            .iter()
            .map(|p| json!({"name": p.key, "value": p.value, "enabled": p.enabled}))
            .collect(),
    )
}

/// Convert a `.bru` document into the shape Bruno's own JSON export produces.
///
/// Unconvertible blocks are RECORDED in `notes` rather than dropped in
/// silence: an import that quietly loses a body or an auth block is the data
/// loss invariant #7 forbids, and the caller renders these.
pub fn bru_text_to_json(text: &str, notes: &mut Vec<String>) -> Result<Value, String> {
    let blocks = split_blocks(text)?;

    let meta = find(&blocks, "meta")
        .ok_or_else(|| "a .bru document must open with a meta block".to_string())?;
    let name = meta
        .pairs
        .iter()
        .find(|p| p.key == "name")
        .map(|p| p.value.clone())
        .unwrap_or_else(|| "Request".to_string());

    // The method block IS the method: `get {`, `post {`, … Exactly one.
    const METHODS: &[&str] = &[
        "get", "post", "put", "delete", "patch", "options", "head", "connect", "trace",
    ];
    let method_block = blocks
        .iter()
        .find(|b| METHODS.contains(&b.name.as_str()))
        .ok_or_else(|| {
            format!("the .bru request {name:?} has no method block (expected one of get/post/put/delete/patch/options/head/connect/trace)")
        })?;
    let method = method_block.name.to_uppercase();
    let method_dict: std::collections::HashMap<&str, &str> = method_block
        .pairs
        .iter()
        .map(|p| (p.key.as_str(), p.value.as_str()))
        .collect();
    let url = method_dict.get("url").copied().unwrap_or_default();

    let mut request = Map::new();
    request.insert("url".into(), json!(url));
    request.insert("method".into(), json!(method));

    if let Some(h) = find(&blocks, "headers") {
        request.insert("headers".into(), pairs_to_kv(h));
    }

    // `query` pairs are typed so the shared mapper merges them the same way it
    // merges an export's params.
    if let Some(q) = find(&blocks, "query") {
        request.insert(
            "params".into(),
            Value::Array(
                q.pairs
                    .iter()
                    .map(|p| {
                        json!({"name": p.key, "value": p.value, "enabled": p.enabled, "type": "query"})
                    })
                    .collect(),
            ),
        );
    }

    if let Some(auth) = build_auth(&blocks, &method_dict, notes) {
        request.insert("auth".into(), auth);
    }
    if let Some(body) = build_body(&blocks, &method_dict, notes) {
        request.insert("body".into(), body);
    }

    let mut script = Map::new();
    if let Some(b) = find(&blocks, "script:pre-request") {
        script.insert("req".into(), json!(b.text));
    }
    if let Some(b) = find(&blocks, "script:post-response") {
        script.insert("res".into(), json!(b.text));
    }
    if !script.is_empty() {
        request.insert("script".into(), Value::Object(script));
    }

    note_dropped_blocks(&blocks, &name, notes);

    Ok(json!({
        "version": "1",
        "name": name,
        "items": [{ "type": "http", "name": name, "request": Value::Object(request) }],
    }))
}

fn build_auth(
    blocks: &[Block],
    method_dict: &std::collections::HashMap<&str, &str>,
    notes: &mut Vec<String>,
) -> Option<Value> {
    let mode = method_dict.get("auth").copied().unwrap_or("none");
    if mode == "none" || mode == "inherit" {
        return None;
    }
    let dict = |name: &str| -> std::collections::HashMap<String, String> {
        find(blocks, name)
            .map(|b| {
                b.pairs
                    .iter()
                    .map(|p| (p.key.clone(), p.value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut auth = Map::new();
    auth.insert("mode".into(), json!(mode));
    match mode {
        "basic" => {
            let d = dict("auth:basic");
            auth.insert(
                "basic".into(),
                json!({"username": d.get("username"), "password": d.get("password")}),
            );
        }
        "bearer" => {
            let d = dict("auth:bearer");
            auth.insert("bearer".into(), json!({"token": d.get("token")}));
        }
        "digest" => {
            let d = dict("auth:digest");
            auth.insert(
                "digest".into(),
                json!({"username": d.get("username"), "password": d.get("password")}),
            );
        }
        "wsse" => {
            let d = dict("auth:wsse");
            auth.insert(
                "wsse".into(),
                json!({"username": d.get("username"), "password": d.get("password")}),
            );
        }
        "apikey" => {
            let d = dict("auth:apikey");
            auth.insert(
                "apikey".into(),
                json!({
                    "key": d.get("key"),
                    "value": d.get("value"),
                    "placement": d.get("placement").cloned().unwrap_or_else(|| "header".into()),
                }),
            );
        }
        other => {
            // Named, not guessed. An auth mode silently downgraded to "none"
            // is an unauthenticated request on the wire (invariant #7).
            notes.push(format!(
                "auth mode {other:?} is not supported by the Bruno importer — the request will carry no credentials"
            ));
            return None;
        }
    }
    Some(Value::Object(auth))
}

fn build_body(
    blocks: &[Block],
    method_dict: &std::collections::HashMap<&str, &str>,
    notes: &mut Vec<String>,
) -> Option<Value> {
    let mode = method_dict.get("body").copied().unwrap_or("none");
    if mode == "none" {
        return None;
    }
    let mut body = Map::new();
    body.insert("mode".into(), json!(mode));
    match mode {
        "json" | "text" | "xml" | "sparql" => {
            let key = if mode == "sparql" { "sparql" } else { mode };
            if let Some(b) = find(blocks, &format!("body:{mode}")) {
                body.insert(key.into(), json!(b.text));
            }
        }
        "form-urlencoded" => {
            let entries = find(blocks, "body:form-urlencoded")
                .map(pairs_to_kv)
                .unwrap_or_else(|| Value::Array(vec![]));
            // The TEXT format spells this `form-urlencoded`; Bruno's own
            // EXPORT spells it `formUrlEncoded`, and the shared mapper matches
            // on the export spelling. Translating here is the whole reason
            // this function exists — without it the body silently became
            // `None`, which is a POST going out with no body at all.
            body.insert("mode".into(), json!("formUrlEncoded"));
            body.insert("formUrlEncoded".into(), entries);
        }
        "multipartForm" | "multipart-form" => {
            let parts = find(blocks, "body:multipart-form")
                .map(|b| {
                    Value::Array(
                        b.pairs
                            .iter()
                            .map(|p| {
                                json!({
                                    "type": "text",
                                    "name": p.key,
                                    "value": p.value,
                                    "enabled": p.enabled,
                                })
                            })
                            .collect(),
                    )
                })
                .unwrap_or_else(|| Value::Array(vec![]));
            body.insert("mode".into(), json!("multipartForm"));
            body.insert("multipartForm".into(), parts);
        }
        other => {
            notes.push(format!(
                "body mode {other:?} is not supported by the Bruno importer — the request will be sent with no body"
            ));
            return None;
        }
    }
    Some(Value::Object(body))
}

/// Record the blocks that carry data the `Scenario` model has no home for.
///
/// Listed rather than ignored, because "the import worked" and "the import
/// worked and dropped your assertions" look identical otherwise.
fn note_dropped_blocks(blocks: &[Block], name: &str, notes: &mut Vec<String>) {
    for b in blocks {
        let dropped = match b.name.as_str() {
            "assert" => "assertions",
            "vars:pre-request" | "vars:post-response" => "request-local variables",
            "tests" => "the tests block",
            "settings" => "request settings",
            _ => continue,
        };
        if b.pairs.is_empty() && b.text.trim().is_empty() {
            continue;
        }
        notes.push(format!(
            "'{name}': {dropped} were dropped — the Scenario model carries no field for the .bru {:?} block",
            b.name
        ));
    }
}
