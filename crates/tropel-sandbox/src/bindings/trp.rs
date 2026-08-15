use crate::state::SharedPmState;
use rquickjs::function::Func;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tropel_js::JsContext;
use tropel_sdk::error::TropelError;
#[cfg(feature = "send-request")]
use tropel_sdk::traits::DriverHttpClient;
#[cfg(feature = "send-request")]
use tropel_sdk::types::Request;
use tropel_sdk::types::{AuthConfig, Body, Method};
use tropel_sdk::Result;

/// Convert a serde_json::Value to a string suitable for JS consumption.
/// Always JSON-encodes the value so the JS shim can JSON.parse() to
/// restore the correct type. This ensures "123" (string) survives as
/// the string "123" rather than being parsed as the number 123.
/// All variable scopes (env, collection, globals) use this same path
/// for type-safe round-tripping.
fn variable_value_to_string(val: &Value) -> String {
    serde_json::to_string(val).unwrap_or_default()
}

/// Convert a plain string to its JSON-encoded form for type-safe JS round-tripping.
/// `&str` implements `Serialize`, so `serde_json::to_string` produces
/// `'"123"'` which `JSON.parse()` restores as the string `"123"` — not
/// the number `123` or boolean `true`.
fn string_to_json_encoded(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Decode a value the JS shim JSON-encoded on set back to a plain string.
/// Backlog line 89: set/get must be INVERSES. `pm.environment.set('x',
/// '1234')` sends `JSON.stringify('1234')` = `'"1234"'`; decoding yields the
/// plain string `1234`. Non-string JSON (numbers, booleans, objects) is
/// stored as its JSON text — Postman environment variables are strings-only,
/// so this is the type-safe representation. Unparseable input (e.g. legacy
/// seeded plain strings) passes through verbatim.
fn decode_json_encoded(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(Value::String(v)) => v,
        Ok(other) => other.to_string(),
        Err(_) => s.to_string(),
    }
}

/// Decode a value the JS shim JSON-encoded on set back into a serde_json
/// Value for the collection/global stores. `'{"a":1}'` → object, `'42'` →
/// number, `'"42"'` → the string "42"; unparseable input → String(raw).
fn decode_json_value(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

/// Resolve a variable across ALL scopes with Postman precedence:
/// iteration data > environment > collection > globals (backlog line 145).
///
/// Returns the value JSON-encoded for the JS shim to `JSON.parse()` back to
/// the correct type. Extracted as a pure function so the precedence contract
/// is unit-testable without a live JS context.
fn variables_lookup(
    key: &str,
    local_vars: &HashMap<String, Value>,
    iteration_data: Option<&HashMap<String, Value>>,
    environment: &HashMap<String, String>,
    collection_vars: &HashMap<String, Value>,
    globals: &HashMap<String, Value>,
) -> Option<String> {
    // Postman precedence: local (pm.variables) > data (iteration) > env >
    // collection > globals. Backlog line 137: pm.variables.set wrote to
    // collection while get read data first — set-then-get disagreed when a
    // data row had the same key. Local is its own store now, checked first.
    if let Some(val) = local_vars.get(key) {
        return Some(variable_value_to_string(val));
    }
    // Backlog line 145: iteration data used to be ignored entirely, so a CSV
    // row could never override an environment/collection value.
    if let Some(val) = iteration_data.and_then(|d| d.get(key)) {
        return Some(variable_value_to_string(val));
    }
    // Environment variables are HashMap<String, String>
    if let Some(val) = environment.get(key) {
        return Some(string_to_json_encoded(val));
    }
    // Collection and global variables are serde_json::Value
    if let Some(val) = collection_vars.get(key) {
        return Some(variable_value_to_string(val));
    }
    globals.get(key).map(variable_value_to_string)
}

/// Parse an HTTP method string (case-insensitive) into a `Method`.
/// Falls back to `Custom` for non-standard tokens (PURGE, LINK, …).
fn parse_method(s: &str) -> Method {
    let t = s.trim();
    if t.is_empty() {
        return Method::GET;
    }
    match t.to_uppercase().as_str() {
        "GET" => Method::GET,
        "HEAD" => Method::HEAD,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        "OPTIONS" => Method::OPTIONS,
        "TRACE" => Method::TRACE,
        "CONNECT" => Method::CONNECT,
        other => Method::Custom(other.to_string()),
    }
}

/// Return the response body as a JSON string for the `pm.response.json()`
/// bridge, validating that it IS valid JSON first.
///
/// The body is validated against a SCRATCH copy and the ORIGINAL bytes are
/// returned. simd-json parses in-place and REWRITES its input buffer
/// (in-situ de-escaping), so validating-and-returning the same buffer
/// corrupts any body containing `\n`, `\"`, `\\` or `\uXXXX` escapes: the
/// de-escaped bytes overwrite the escape sequences but the buffer length is
/// unchanged, leaving stale bytes — `{"a":"x\ny"}` becomes invalid JSON.
/// The JS shim then JSON.parses the returned text, so it must be pristine.
fn response_json_string(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let mut scratch = body.to_vec();
    simd_json::serde::from_slice::<serde_json::Value>(&mut scratch).ok()?;
    String::from_utf8(body.to_vec()).ok()
}

/// Resolve {{variable}} references in a URL using the current PM state.
/// Searches environment, collection vars, and globals in order.
/// Uses a cursor-based approach that builds the result string by pushing
/// segments — no in-place mutation, no infinite-loop risk.
///
/// Only used by the `send-request` bridge (`pm.sendRequest`); gated so the
/// browser slice (`--no-default-features`) doesn't carry dead code.
#[cfg(feature = "send-request")]
fn resolve_vars(
    url: &str,
    local_vars: &HashMap<String, serde_json::Value>,
    environment: &HashMap<String, String>,
    collection_vars: &HashMap<String, serde_json::Value>,
    globals: &HashMap<String, serde_json::Value>,
) -> String {
    if !url.contains("{{") {
        return url.to_string();
    }

    let mut result = String::with_capacity(url.len());
    let mut pos = 0;

    while pos < url.len() {
        // Find the next {{ marker
        if let Some(start) = url[pos..].find("{{") {
            let abs_start = pos + start;

            // Copy everything before the marker
            result.push_str(&url[pos..abs_start]);

            // Look for the closing }}
            if let Some(end) = url[abs_start + 2..].find("}}") {
                let key_start = abs_start + 2;
                let key_end = abs_start + 2 + end;

                // Extract and normalize the key — trim whitespace
                let key = url[key_start..key_end].trim();

                // Try to resolve from scopes in order: local → env →
                // collection → globals (backlog line 137: pm.variables is
                // the LOCAL scope — sendRequest must see script-set values).
                let resolved = local_vars
                    .get(key)
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .or_else(|| environment.get(key).cloned())
                    .or_else(|| {
                        collection_vars.get(key).map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .or_else(|| {
                        globals.get(key).map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    });

                match resolved {
                    Some(val) => result.push_str(&val),
                    None => {
                        // Unresolved — emit the original {{key}} literal
                        result.push_str(&url[abs_start..key_end + 2]);
                    }
                }

                // Advance cursor past the {{key}}
                pos = key_end + 2;
            } else {
                // No closing }} — emit the rest as-is and stop
                result.push_str(&url[abs_start..]);
                break;
            }
        } else {
            // No more {{ markers — emit the tail
            result.push_str(&url[pos..]);
            break;
        }
    }

    result
}

/// Resolve `{{var}}` placeholders across URL, headers, and body for
/// `pm.sendRequest` (backlog line 147: only the URL was resolved, so a
/// `Bearer {{token}}` header or `{{"key":"{{val}}"}}` body went out with the
/// literal placeholder). Extracted as a pure function so the resolution
/// contract is unit-testable without a live JS context.
#[cfg(feature = "send-request")]
fn resolve_send_request(
    url: &str,
    headers_json: &str,
    body: &str,
    local_vars: &HashMap<String, serde_json::Value>,
    environment: &HashMap<String, String>,
    collection_vars: &HashMap<String, serde_json::Value>,
    globals: &HashMap<String, serde_json::Value>,
) -> (String, HashMap<String, String>, Option<Body>) {
    let resolved_url = resolve_vars(url, local_vars, environment, collection_vars, globals);
    let headers: HashMap<String, String> = parse_headers(headers_json)
        .into_iter()
        .map(|(k, v)| {
            (
                k.clone(),
                resolve_vars(&v, local_vars, environment, collection_vars, globals),
            )
        })
        .collect();
    let request_body = if body.is_empty() {
        None
    } else {
        Some(Body::Raw(resolve_vars(
            body,
            local_vars,
            environment,
            collection_vars,
            globals,
        )))
    };
    (resolved_url, headers, request_body)
}

/// Parses headers from a JSON string that may be either:
/// - Object form: {"Content-Type": "application/json"}
/// - Postman array form: [{"key": "Content-Type", "value": "application/json"}]
///
/// Object-form values may be non-strings (e.g. `{"Content-Length": 123}` or
/// `{"X-Bool": true}`). The old code parsed the object as
/// `HashMap<String, String>` and fell through to the (failing) array form
/// whenever ANY value was non-string — silently dropping EVERY header
/// (backlog P3). Non-string scalars are now stringified; null/complex values
/// are skipped.
#[cfg(feature = "send-request")]
fn parse_headers(json: &str) -> HashMap<String, String> {
    if json.is_empty() || json == "{}" || json == "[]" {
        return HashMap::new();
    }

    // Try object form first — tolerant of non-string values.
    if json.trim_start().starts_with('{') {
        if let Ok(map) = serde_json::from_str::<HashMap<String, serde_json::Value>>(json) {
            let mut headers = HashMap::new();
            for (k, v) in map {
                match v {
                    serde_json::Value::String(s) => {
                        headers.insert(k, s);
                    }
                    serde_json::Value::Number(n) => {
                        headers.insert(k, n.to_string());
                    }
                    serde_json::Value::Bool(b) => {
                        headers.insert(k, b.to_string());
                    }
                    // Null and complex values cannot be header strings.
                    _ => {}
                }
            }
            return headers;
        }
    }

    // Try Postman array form: [{"key": ..., "value": ...}]
    if json.trim_start().starts_with('[') {
        if let Ok(arr) = serde_json::from_str::<Vec<HashMap<String, serde_json::Value>>>(json) {
            let mut headers = HashMap::new();
            for entry in arr {
                let key = entry.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if !key.is_empty() {
                    headers.insert(key.to_string(), value.to_string());
                }
            }
            return headers;
        }
    }

    HashMap::new()
}

/// Register all `pm.*` bridge functions as global JS functions in a JsContext.
/// Functions like `__tropel_pm_test`, `__tropel_pm_environment_get`, etc.
/// are registered so the JS shims in scripting-api/pm.js can call them.
///
/// With rquickjs 0.12+, the following complex types are supported as
/// Func::from parameter/return types via FromJs/IntoJs: HashMap<String, String>,
/// Vec<(String, String)>, Option<T>, Vec<T>, and all primitive types.
pub struct TrpBridge {
    state: SharedPmState,
    /// Per-VU HTTP client for executing pm.sendRequest synchronously.
    /// Stored as `Arc<dyn DriverHttpClient>` (the SDK trait) so the sandbox
    /// does not directly depend on `tropel-http` (F1, review fix).
    ///
    /// `Option` so the always-available [`TrpBridge::new`] constructor
    /// compiles under BOTH feature states (P5b: feature unification turns
    /// `send-request` on for the whole workspace when the engine opts in,
    /// so the web slice's 1-arg call must not disappear). When `None`, the
    /// `send-request` bridge returns a descriptive error instead of
    /// panicking.
    #[cfg(feature = "send-request")]
    http_client: Option<Arc<dyn DriverHttpClient>>,
}

impl TrpBridge {
    /// Always-available constructor with no HTTP client (browser slice,
    /// `--no-default-features`). Compiles in both feature states.
    pub fn new(state: SharedPmState) -> Self {
        Self {
            state,
            #[cfg(feature = "send-request")]
            http_client: None,
        }
    }

    /// Constructor with a per-VU HTTP client (enables `pm.sendRequest`).
    /// Accepts `Arc<dyn DriverHttpClient>` so the sandbox does not need a
    /// direct dependency on `tropel-http` (F1, review fix).
    #[cfg(feature = "send-request")]
    pub fn with_http_client(state: SharedPmState, http_client: Arc<dyn DriverHttpClient>) -> Self {
        Self {
            state,
            http_client: Some(http_client),
        }
    }

    /// Register all bridge functions into the given JS context.
    pub fn install(&self, ctx: &mut JsContext) -> Result<()> {
        let state = self.state.clone();

        let failures: Vec<String> = ctx.with_ctx(|rq_ctx| {
            let globals = rq_ctx.globals();
            let mut failures: Vec<String> = Vec::new();

            // A failed registration must NOT degrade silently into pm.js's
            // `typeof` fallbacks (`pm.response.code()` -> 0). Collect every
            // failing `globals.set` and surface them as an Err below.
            macro_rules! set_global {
                ($name:expr, $func:expr $(,)?) => {
                    if let Err(e) = globals.set($name, $func) {
                        failures.push(format!("{}: {}", $name, e));
                    }
                };
            }

            // ── Environment ──
            // Backlog line 89: set/get must be INVERSES. The shim JSON-encodes
            // on set (string '1234' → '"1234"', number 42 → '42') and
            // JSON-parses on get; these bridges store PLAIN values (decode on
            // set) and JSON-encode on get, so '1234' survives a round trip as
            // the STRING '1234' — never the number 1234 — while script-set
            // values stay plain for {{var}} substitution (which reads the
            // maps directly).
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_environment_get",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.environment
                        .get(&key)
                        .map(|v| string_to_json_encoded(v.as_str()))
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_environment_set",
                Func::from(move |key: String, value: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.environment.insert(key, decode_json_encoded(&value));
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_environment_unset",
                Func::from(move |key: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.environment.remove(&key);
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_environment_clear",
                Func::from(move || {
                    let mut st = state_clone.lock().unwrap();
                    st.environment.clear();
                }),
            );

            // ── Variables ──
            // Returns Option<String>: ALL variable scopes return JSON-encoded
            // strings so the JS shim can `JSON.parse()` to restore the correct
            // JS type. Without encoding, an env var like "123" would be parsed
            // as the number 123 instead of the string "123".
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_variables_get",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    variables_lookup(
                        &key,
                        &st.local_vars,
                        st.iteration_data.as_ref(),
                        &st.environment,
                        &st.collection_vars,
                        &st.globals,
                    )
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_variables_set",
                Func::from(move |key: String, value: String| {
                    let mut st = state_clone.lock().unwrap();
                    // Backlog line 137: pm.variables is the LOCAL scope —
                    // writes land here (highest priority), not in collection.
                    st.local_vars.insert(key, decode_json_value(&value));
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_variables_unset",
                Func::from(move |key: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.local_vars.remove(&key);
                    st.collection_vars.remove(&key);
                    st.environment.remove(&key);
                    st.globals.remove(&key);
                }),
            );

            // ── Environment: has / toObject ──
            // Backlog line 145: Postman's pm.environment exposes has() and
            // toObject() alongside get/set/unset.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_environment_has",
                Func::from(move |key: String| -> bool {
                    let st = state_clone.lock().unwrap();
                    st.environment.contains_key(&key)
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_environment_to_object",
                Func::from(move || -> HashMap<String, String> {
                    let st = state_clone.lock().unwrap();
                    st.environment
                        .iter()
                        .map(|(k, v)| (k.clone(), string_to_json_encoded(v)))
                        .collect()
                }),
            );

            // ── Collection Variables (pm.collectionVariables) ──
            // Backlog line 145: one of the top-3 most-used pm.* members was
            // entirely missing. Values are JSON-encoded for type-safe
            // round-tripping, same as variables.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_collection_vars_get",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.collection_vars.get(&key).map(variable_value_to_string)
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_collection_vars_set",
                Func::from(move |key: String, value: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.collection_vars.insert(key, decode_json_value(&value));
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_collection_vars_unset",
                Func::from(move |key: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.collection_vars.remove(&key);
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_collection_vars_has",
                Func::from(move |key: String| -> bool {
                    let st = state_clone.lock().unwrap();
                    st.collection_vars.contains_key(&key)
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_collection_vars_to_object",
                Func::from(move || -> HashMap<String, String> {
                    let st = state_clone.lock().unwrap();
                    st.collection_vars
                        .iter()
                        .map(|(k, v)| (k.clone(), variable_value_to_string(v)))
                        .collect()
                }),
            );

            // ── Global Variables (pm.globals) ──
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_globals_get",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.globals.get(&key).map(variable_value_to_string)
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_globals_set",
                Func::from(move |key: String, value: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.globals.insert(key, decode_json_value(&value));
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_globals_unset",
                Func::from(move |key: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.globals.remove(&key);
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_globals_has",
                Func::from(move |key: String| -> bool {
                    let st = state_clone.lock().unwrap();
                    st.globals.contains_key(&key)
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_globals_to_object",
                Func::from(move || -> HashMap<String, String> {
                    let st = state_clone.lock().unwrap();
                    st.globals
                        .iter()
                        .map(|(k, v)| (k.clone(), variable_value_to_string(v)))
                        .collect()
                }),
            );

            // ── Request (pm.request) ──
            // Backlog line 145: prerequest scripts could not add an auth
            // header or sign a request because pm.request didn't exist AND
            // the runner rebuilt the wire request from the static collection
            // item, discarding any state.request mutations. The runner now
            // reads state.request (seeded from item.request before prerequest)
            // when building the outgoing request, so these bridges actually
            // change what goes out on the wire.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_url",
                Func::from(move || -> String {
                    let st = state_clone.lock().unwrap();
                    st.request
                        .as_ref()
                        .map(|r| r.url.clone())
                        .unwrap_or_default()
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_url_set",
                Func::from(move |url: String| {
                    let mut st = state_clone.lock().unwrap();
                    if let Some(r) = st.request.as_mut() {
                        r.url = url;
                    }
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_method",
                Func::from(move || -> String {
                    let st = state_clone.lock().unwrap();
                    st.request
                        .as_ref()
                        .map(|r| r.method.to_string())
                        .unwrap_or_else(|| "GET".to_string())
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_method_set",
                Func::from(move |method: String| {
                    let mut st = state_clone.lock().unwrap();
                    if let Some(r) = st.request.as_mut() {
                        r.method = parse_method(&method);
                    }
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_headers",
                Func::from(move || -> HashMap<String, String> {
                    let st = state_clone.lock().unwrap();
                    st.request
                        .as_ref()
                        .map(|r| r.headers.clone())
                        .unwrap_or_default()
                }),
            );

            // Case-insensitive read (Postman's HeaderList.get is case-insensitive).
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_header_get",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.request.as_ref().and_then(|r| {
                        r.headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                            .map(|(_, v)| v.clone())
                    })
                }),
            );

            // Upsert case-insensitively: replace an existing differently-cased
            // header rather than creating a duplicate (Postman HeaderList.add).
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_header_set",
                Func::from(move |key: String, value: String| {
                    let mut st = state_clone.lock().unwrap();
                    if let Some(r) = st.request.as_mut() {
                        let existing = r
                            .headers
                            .keys()
                            .find(|k| k.eq_ignore_ascii_case(&key))
                            .cloned();
                        match existing {
                            Some(ek) => {
                                r.headers.insert(ek, value);
                            }
                            None => {
                                r.headers.insert(key, value);
                            }
                        }
                    }
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_header_unset",
                Func::from(move |key: String| {
                    let mut st = state_clone.lock().unwrap();
                    if let Some(r) = st.request.as_mut() {
                        r.headers.retain(|k, _| !k.eq_ignore_ascii_case(&key));
                    }
                }),
            );

            // Body as raw text (get) / raw text (set).
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_body",
                Func::from(move || -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.request.as_ref().and_then(|r| match &r.body {
                        Some(Body::Raw(s)) => Some(s.clone()),
                        Some(Body::Json(v)) => Some(serde_json::to_string(v).unwrap_or_default()),
                        _ => None,
                    })
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_body_set",
                Func::from(move |body: String| {
                    let mut st = state_clone.lock().unwrap();
                    if let Some(r) = st.request.as_mut() {
                        r.body = Some(Body::Raw(body));
                    }
                }),
            );

            // Auth: accepts the internally-tagged AuthConfig JSON form
            // ({"type":"bearer","token":...}) — the same shape the
            // postman/k6 inputs produce. A prerequest script can therefore
            // sign the outgoing request (the primary purpose of pm.request).
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_auth_set",
                Func::from(move |auth_json: String| {
                    let parsed = serde_json::from_str::<AuthConfig>(&auth_json);
                    let mut st = state_clone.lock().unwrap();
                    if let Some(r) = st.request.as_mut() {
                        match parsed {
                            Ok(auth) => r.auth = Some(auth),
                            Err(e) => {
                                tracing::warn!(
                                    "pm.request.auth: could not parse auth config: {}",
                                    e
                                );
                            }
                        }
                    }
                }),
            );

            // Live auth READ (backlog line 101): the JS shim kept auth in a
            // module-scope `_pmRequestAuth` singleton, so a request that had
            // NO auth (or a DIFFERENT auth) on the next iteration still read
            // the previous iteration's value. The getter returns the CURRENT
            // request's auth from state (None when unset) so reads are
            // always per-request — no cross-iteration leakage.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_auth",
                Func::from(move || -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.request.as_ref().and_then(|r| {
                        r.auth
                            .as_ref()
                            .map(|a| serde_json::to_string(a).unwrap_or_default())
                    })
                }),
            );

            // Live body-mode READ (backlog line 101): the JS shim's
            // `_pmRequestBody.mode` module-scope value persisted across
            // iterations (a fresh request per iteration re-seeded the raw
            // text but not the mode). The getter derives the mode from the
            // CURRENT request's Body variant, so `pm.request.body.mode`
            // always describes the actual request — no leakage.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_request_body_mode",
                Func::from(move || -> String {
                    let st = state_clone.lock().unwrap();
                    match st.request.as_ref().and_then(|r| r.body.as_ref()) {
                        Some(Body::Raw(_)) | Some(Body::Json(_)) => "raw".to_string(),
                        Some(Body::FormData(_)) => "formdata".to_string(),
                        Some(Body::UrlEncoded(_)) => "urlencoded".to_string(),
                        Some(Body::Binary(_)) => "file".to_string(),
                        Some(Body::GraphQL { .. }) => "graphql".to_string(),
                        None => "raw".to_string(),
                    }
                }),
            );

            // ── pm.info (live, backlog line 101) ──
            // The shim previously shipped a hardcoded stub (eventName
            // 'test', iteration 0, iterationCount 1, requestName ''). All
            // five fields are now read from PmState so a test script sees
            // the real iteration, the real request name, and the configured
            // iteration count.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_info",
                Func::from(move || -> String {
                    let st = state_clone.lock().unwrap();
                    let request_id = st
                        .request_names
                        .iter()
                        .position(|n| n == &st.current_request_name)
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| st.current_request_name.clone());
                    serde_json::json!({
                        "eventName": st.event_name,
                        "iteration": st.iteration_index,
                        "iterationCount": st.total_iterations.unwrap_or(1),
                        "requestName": st.current_request_name,
                        "requestId": request_id,
                    })
                    .to_string()
                }),
            );

            // ── Test skip (pm.test.skip) ──
            // Backlog line 145: pm.test.skip(name, fn) marks a test skipped
            // WITHOUT running it — a collection using it used to throw
            // "pm.test.skip is not a function" and fail the whole run. Skipped
            // tests are not pass/fail checks, so nothing is recorded.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_test_skip",
                Func::from(move |name: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.assertions.skipped += 1;
                    tracing::debug!("pm.test.skip: {}", name);
                }),
            );

            // ── Response ──
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_code",
                Func::from(move || -> u16 {
                    let st = state_clone.lock().unwrap();
                    st.response.as_ref().map(|r| r.status_code).unwrap_or(0)
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_status",
                Func::from(move || -> String {
                    let st = state_clone.lock().unwrap();
                    st.response
                        .as_ref()
                        .map(|r| r.status_text.clone())
                        .unwrap_or_default()
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_body",
                Func::from(move || -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.response.as_ref().and_then(|r| r.body_text())
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_time",
                Func::from(move || -> f64 {
                    let st = state_clone.lock().unwrap();
                    st.response
                        .as_ref()
                        .map(|r| r.response_time.as_secs_f64() * 1000.0)
                        .unwrap_or(0.0)
                }),
            );

            // ── Response Headers (full map) ──
            // rquickjs 0.12+ supports HashMap<String,String> as IntoJs -> JS object
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_headers",
                Func::from(move || -> HashMap<String, String> {
                    let st = state_clone.lock().unwrap();
                    st.response
                        .as_ref()
                        .map(|r| r.headers.clone())
                        .unwrap_or_default()
                }),
            );

            // ── Response Header (individual header access, widely used) ──
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_header",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.response.as_ref().and_then(|r| {
                        // Postman semantics: pm.response.header('content-type')
                        // and pm.response.headers.get('Content-Type') are
                        // case-insensitive. The map is canonical (Content-Type)
                        // after the client.rs fix, but a script may ask in any
                        // case — look up case-insensitively.
                        r.headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                            .map(|(_, v)| v.clone())
                    })
                }),
            );

            // ── Response Cookies (name → value map)
            // rquickjs 0.12+ supports HashMap<String,String> as IntoJs -> JS object
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_cookies",
                Func::from(move || -> HashMap<String, String> {
                    let st = state_clone.lock().unwrap();
                    st.response
                        .as_ref()
                        .map(|r| {
                            r.cookies
                                .iter()
                                .map(|c| (c.name.clone(), c.value.clone()))
                                .collect()
                        })
                        .unwrap_or_default()
                }),
            );

            // ── Response JSON (returns JSON string, parsed by JS shim) ──
            // rquickjs 0.12 still doesn't support returning serde_json::Value directly,
            // but returning Option<String> (JSON text) works. The pm.js shim parses
            // this string via JSON.parse() to produce the expected object.
            // We validate the body is valid JSON using simd-json (fast, from bytes)
            // before returning, so the JS shim can throw a descriptive error on
            // invalid JSON.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_response_json",
                Func::from(move || -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.response
                        .as_ref()
                        .and_then(|r| response_json_string(&r.body))
                }),
            );

            // ── Iteration Data ──
            // Returns Option<String>: JSON-encoded value so the JS shim can
            // JSON.parse() to restore the correct type.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_iteration_data_get",
                Func::from(move |key: String| -> Option<String> {
                    let st = state_clone.lock().unwrap();
                    st.iteration_data
                        .as_ref()
                        .and_then(|data| data.get(&key).map(variable_value_to_string))
                }),
            );

            // ── Test ──
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_test",
                // 3rd arg: optional k6 check() tags JSON (backlog line 149).
                Func::from(
                    move |name: String, passed: bool, tags_json: Option<String>| {
                        let extra = tags_json
                            .as_deref()
                            .and_then(|j| serde_json::from_str::<HashMap<String, String>>(j).ok())
                            .unwrap_or_default();
                        let mut st = state_clone.lock().unwrap();
                        st.record_test_tagged(&name, passed, extra);
                    },
                ),
            );

            // ── Flow Control ──
            // setNextRequest resolution order (backlog §4):
            //   1. null / empty / "null"  → END the iteration (the runner
            //      breaks when the pending index is out of range; usize::MAX
            //      is the sentinel for "stop walking this iteration"). The
            //      old code treated null as a NO-OP, so a collection whose
            //      last item jumped to an earlier one never consumed the
            //      jump, leaked it into the next iteration, and ran one
            //      request forever.
            //   2. item ID  → Postman resolves ids FIRST (the v2.1 schema
            //      keys items by id; name is the fallback).
            //   3. item NAME → LAST match wins (Postman is last-wins on
            //      duplicate names; the old first-wins position() jumped the
            //      wrong item).
            //   4. numeric index (legacy, LAST) — only when no id/name
            //      matches, so a request literally named "2" is resolved by
            //      name, not hijacked by the numeric parse.
            //   5. anything else → END the iteration (Postman: an unknown
            //      name stops the flow rather than silently continuing).
            // Sentinel: an out-of-range jump target means "end the current
            // iteration" — the runner breaks on any target >= item_count.
            const END_ITERATION: usize = usize::MAX;
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_set_next_request",
                Func::from(move |request_id: Option<String>| {
                    let mut st = state_clone.lock().unwrap();

                    let Some(request_id) = request_id else {
                        st.next_request = Some(END_ITERATION);
                        return;
                    };
                    if request_id == "null" || request_id.is_empty() {
                        st.next_request = Some(END_ITERATION);
                        return;
                    }

                    // 1. Item id first (Postman resolves ids before names).
                    if let Some(pos) = st.request_ids.iter().position(|i| i == &request_id) {
                        st.next_request = Some(pos);
                        return;
                    }

                    // 2. Name — last-wins on duplicates (Postman).
                    if let Some(pos) = st.request_names.iter().rposition(|n| n == &request_id) {
                        st.next_request = Some(pos);
                        return;
                    }

                    // 3. Legacy numeric index — only after id/name miss, so a
                    //    request named "2" is not hijacked by the parse.
                    if let Ok(index) = request_id.parse::<usize>() {
                        st.next_request = Some(index);
                        return;
                    }

                    // 4. Unknown → end the iteration (Postman semantics).
                    st.next_request = Some(usize::MAX);
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_skip_tests",
                Func::from(move || {
                    let mut st = state_clone.lock().unwrap();
                    st.skip_tests = true;
                }),
            );

            // ── skipRequest (backlog line 146) ──
            // pm.execution.skipRequest() must skip ONLY the current item and
            // move to the next — the old shim routed it through
            // setNextRequest(null), which threw on the strict String param
            // AND semantically "stopped the whole run". The runner reads this
            // flag after the prerequest script and skips send + test script.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_skip_request",
                Func::from(move || {
                    let mut st = state_clone.lock().unwrap();
                    st.skip_request = true;
                }),
            );

            // ── Group (for nesting groups with group_duration metric) ──
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_group_start",
                Func::from(move |name: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.group_stack.push(name);
                    // Rebuild the current group path from the stack
                    st.current_group = if st.group_stack.is_empty() {
                        None
                    } else {
                        Some(st.group_stack.join("::"))
                    };
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_group_end",
                Func::from(move |name: String, duration_ms: f64| {
                    let mut st = state_clone.lock().unwrap();
                    // Pop the matching group from the stack
                    if st.group_stack.last().map(|n| n == &name).unwrap_or(false) {
                        st.group_stack.pop();
                    }
                    // Rebuild current group path
                    st.current_group = if st.group_stack.is_empty() {
                        None
                    } else {
                        Some(st.group_stack.join("::"))
                    };

                    // Emit group_duration sample (Trend) in ms — the public
                    // unit end-to-end (backlog §0). The JS side already
                    // measures in ms, so no µs conversion here.
                    let mut tags = tropel_sdk::types::TagMap::new();
                    tags.insert("group", name.clone());
                    if let Some(ref path) = st.current_group {
                        tags.insert("group_path", path.clone());
                    }
                    st.samples.push(tropel_sdk::types::Sample {
                        metric: "group_duration".into(),
                        value: duration_ms,
                        tags: Arc::new(tags),
                        timestamp: tropel_js::clock::monotonic_wall_now(),
                        sample_type: tropel_sdk::types::SampleType::Trend,
                    });
                }),
            );

            // ── Custom Metrics ──
            // Add a custom metric sample (Postman-style, no tags).
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_metrics_add",
                Func::from(move |name: String, value: f64, metric_type_str: String| {
                    let mut st = state_clone.lock().unwrap();
                    // Track current value
                    st.custom_metrics.insert(name.clone(), value);
                    // Emit a metric sample with the appropriate type
                    let sample_type = match metric_type_str.as_str() {
                        "counter" => tropel_sdk::types::SampleType::Counter,
                        "gauge" => tropel_sdk::types::SampleType::Point,
                        "rate" => tropel_sdk::types::SampleType::Rate,
                        _ => tropel_sdk::types::SampleType::Trend,
                    };
                    st.samples.push(tropel_sdk::types::Sample {
                        metric: name.into(),
                        value,
                        tags: Arc::new(tropel_sdk::types::TagMap::new()),
                        timestamp: tropel_js::clock::monotonic_wall_now(),
                        sample_type,
                    });
                }),
            );

            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_metrics_get",
                Func::from(move |name: String| -> Option<f64> {
                    let st = state_clone.lock().unwrap();
                    st.custom_metrics.get(&name).copied()
                }),
            );

            // ── Custom Metric with Tags (k6-style .add(value, tags)) ──
            // Called by Counter/Gauge/Rate/Trend JS constructors.
            // tags_json is a JSON-encoded object like '{"status":"200","method":"GET"}'.
            let state_clone = state.clone();
            set_global!(
                "__tropel_pm_custom_metric_add",
                Func::from(
                    move |name: String, value: f64, tags_json: String, metric_type_str: String| {
                        let mut st = state_clone.lock().unwrap();
                        // Parse tags from JSON string
                        let tags = if tags_json.is_empty() || tags_json == "{}" {
                            tropel_sdk::types::TagMap::new()
                        } else {
                            let parsed: std::collections::HashMap<String, String> =
                                serde_json::from_str(&tags_json).unwrap_or_default();
                            tropel_sdk::types::TagMap::from_pairs(parsed)
                        };

                        // Track the current value per metric+tags combo
                        st.custom_metrics.insert(name.clone(), value);

                        // Determine sample type from type string
                        let sample_type = match metric_type_str.as_str() {
                            "counter" => tropel_sdk::types::SampleType::Counter,
                            "gauge" => tropel_sdk::types::SampleType::Point,
                            "rate" => tropel_sdk::types::SampleType::Rate,
                            _ => tropel_sdk::types::SampleType::Trend,
                        };

                        st.samples.push(tropel_sdk::types::Sample {
                            metric: name.into(),
                            value,
                            tags: Arc::new(tags),
                            timestamp: tropel_js::clock::monotonic_wall_now(),
                            sample_type,
                        });
                    },
                ),
            );

            // ═══════════════════════════════════════════════════
            // Execution context (k6 exec.* / test.abort())
            // ═══════════════════════════════════════════════════
            // exec.vu.idInTest — VU ID
            let state_clone = state.clone();
            set_global!(
                "__tropel_exec_vu_id",
                Func::from(move || -> u32 { state_clone.lock().unwrap().vu_id }),
            );

            // exec.scenario.name — scenario name
            let state_clone = state.clone();
            set_global!(
                "__tropel_exec_scenario_name",
                Func::from(move || -> String { state_clone.lock().unwrap().scenario_name.clone() }),
            );

            // exec.scenario.executor — executor type string (e.g.,
            // "constant-vus", "ramping-vus"). Piped into PmState by the engine
            // via ScenarioRunner::with_exec_context. Falls back to "" when the
            // engine hasn't attached it (e.g. script-only test harnesses).
            let state_clone = state.clone();
            set_global!(
                "__tropel_exec_scenario_executor",
                Func::from(move || -> String { state_clone.lock().unwrap().executor_name.clone() }),
            );

            // exec.vu.iterationInScenario — current iteration index
            let state_clone = state.clone();
            set_global!(
                "__tropel_exec_iteration",
                Func::from(move || -> u64 { state_clone.lock().unwrap().iteration_index }),
            );

            // exec.instance.iterationsCompleted — GLOBAL total across all VUs.
            // The engine attaches the scheduler's shared atomic counter to
            // PmState; the closure reads it live (lock-free). Falls back to the
            // per-VU iteration index when no handle is attached.
            let state_clone = state.clone();
            set_global!(
                "__tropel_exec_iterations_completed",
                Func::from(move || -> u64 {
                    let st = state_clone.lock().unwrap();
                    st.global_iterations
                        .as_ref()
                        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(st.iteration_index)
                }),
            );

            // exec.instance.vusActive — currently active VUs, read live from
            // the scheduler's shared atomic counter (lock-free). Falls back to
            // 0 when no handle is attached.
            let state_clone = state.clone();
            set_global!(
                "__tropel_exec_vus_active",
                Func::from(move || -> u32 {
                    let st = state_clone.lock().unwrap();
                    st.active_vus
                        .as_ref()
                        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(0)
                }),
            );

            // test.abort(message) — requests engine to abort the test
            let state_clone = state.clone();
            set_global!(
                "__tropel_test_abort",
                Func::from(move |message: String| {
                    let mut st = state_clone.lock().unwrap();
                    st.abort_requested = true;
                    st.abort_message = Some(message);
                }),
            );

            // ── sendRequest ──
            // Executes an HTTP request synchronously using the per-VU HTTP client
            // (routed through the SDK's `DriverHttpClient` trait, F1 — no direct
            // tropel-http dependency). The bridge closure runs inside ctx.with()
            // (synchronous), so it spawns a dedicated thread with its own tokio
            // runtime and block_on: no tokio runtime is entered on the caller's
            // thread (→ no "cannot block from within a runtime" panic), and the
            // fresh runtime avoids deadlock with the current-thread VU runtime.
            // reqwest's own per-request timeout still fires normally.
            //
            // Supports the auth-token-fetch pattern: scripts can call pm.sendRequest
            // to obtain auth tokens or session data, then store them via pm.variables.set().
            // Variable references ({{var}}) in the URL are resolved against the current
            // environment/collection/global variables.
            //
            // Parameters:
            //   method: HTTP method string (GET, POST, etc.)
            //   url: Request URL with optional {{variable}} references
            //   headers_json: JSON string of headers (supports both object and array formats)
            //   body: Request body string (empty string = no body)
            //   timeout_ms: Request timeout in milliseconds (0 = no timeout, default 30000)
            //   response_type: k6 responseType ("text"/"binary"/"none") — "none"
            //     skips reading the response body (saves bandwidth/memory)
            // Returns: JSON-encoded response with code, statusText, body, headers, responseTime
            #[cfg(feature = "send-request")]
            {
                let http = self.http_client.clone();
                let state_for_send = self.state.clone();
                set_global!(
                    "__tropel_pm_send_request",
                    Func::from(
                        move |method: String,
                              url: String,
                              headers_json: String,
                              body: String,
                              timeout_ms: f64,
                              response_type: String|
                              -> String {
                            // Resolve {{variables}} across URL, headers, and body
                            // (backlog line 147: only the URL was resolved — a
                            // header like "Authorization: Bearer {{token}}" went
                            // out with the literal placeholder).
                            let (resolved_url, headers, request_body) = {
                                let st = state_for_send.lock().unwrap();
                                resolve_send_request(
                                    &url,
                                    &headers_json,
                                    &body,
                                    &st.local_vars,
                                    &st.environment,
                                    &st.collection_vars,
                                    &st.globals,
                                )
                            };

                            let timeout = if timeout_ms > 0.0 {
                                Some(std::time::Duration::from_millis(timeout_ms as u64))
                            } else {
                                Some(std::time::Duration::from_secs(30)) // default 30s
                            };

                            // A genuinely invalid method token must not silently
                            // become GET (a write-path "PURGE" must not degrade
                            // into a read-path GET that reports green). Surfaced
                            // as a status-0 error response. Valid-but-uncommon
                            // tokens (PURGE/LINK/…) parse fine via Method::Custom.
                            let Some(method) = Method::parse(&method) else {
                                return serde_json::json!({
                                    "error": format!("invalid HTTP method {}", method),
                                    "code": 0,
                                    "statusText": format!("invalid HTTP method {}", method),
                                    "body": "",
                                    "headers": {},
                                    "responseTime": 0,
                                })
                                .to_string();
                            };

                            let req = Request {
                                url: resolved_url,
                                method,
                                headers,
                                query_params: HashMap::new(),
                                body: request_body,
                                auth: None,
                                certificate: None,
                                follow_redirects: true,
                                timeout,
                                response_type: tropel_sdk::types::ResponseType::from_k6(
                                    &response_type,
                                ),
                            };

                            // Execute on the dedicated I/O runtime via a
                            // fresh tokio runtime on a spawned thread — safe
                            // from inside ctx.with() on a current-thread VU
                            // runtime. When the bridge was built without a
                            // client (browser slice), surface a descriptive
                            // error rather than panicking on None.
                            let Some(http) = http.clone() else {
                                return serde_json::json!({
                                    "error": "pm.sendRequest unavailable in this build (no HTTP client)",
                                    "code": 0,
                                    "statusText": "pm.sendRequest unavailable in this build (no HTTP client)",
                                    "body": "",
                                    "headers": {},
                                    "responseTime": 0,
                                })
                                .to_string();
                            };
                            let result = std::thread::spawn(move || {
                                let rt = tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()
                                    .expect("sandbox send-request tokio runtime");
                                rt.block_on(async move { http.execute(&req).await })
                            })
                            .join()
                            .expect("send-request thread panicked");
                            match result {
                                Ok(http_resp) => {
                                    let body_text = String::from_utf8(http_resp.body.clone())
                                        .unwrap_or_default();
                                    serde_json::json!({
                                    "code": http_resp.status_code,
                                    "statusText": http_resp.status_text,
                                    "body": body_text,
                                    "headers": http_resp.headers,
                                    "responseTime": http_resp.response_time.as_secs_f64() * 1000.0,
                                })
                                .to_string()
                                }
                                // Backlog line 147: transport failures must be
                                // visible to the universal `if (err)` guard in the
                                // shim. The `error` field is what the JS side
                                // checks — code 0 alone is not enough (a 0-status
                                // reply looks like a "success" to naive callers).
                                Err(e) => serde_json::json!({
                                    "error": format!("Request failed: {}", e),
                                    "code": 0,
                                    "statusText": format!("Request failed: {}", e),
                                    "body": "",
                                    "headers": {},
                                    "responseTime": 0,
                                })
                                .to_string(),
                            }
                        },
                    ),
                );
            }
            failures
        });

        if !failures.is_empty() {
            return Err(TropelError::Js(format!(
                "Tropel bridge (trp.*) registration failed: {}",
                failures.join("; ")
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JS shim round-trips a variable through `JSON.parse`:
    /// bridge returns a JSON-encoded string → shim JSON.parses it. This test
    /// locks the typing contract so an env var "123" stays the STRING "123"
    /// (never the number 123) and a collection/global object survives.
    #[test]
    fn test_variable_json_roundtrip_preserves_type() {
        // Env vars are HashMap<String, String> — the bridge JSON-encodes them.
        let env_str = string_to_json_encoded("123");
        assert_eq!(env_str, "\"123\"");
        // Shim does JSON.parse → must come back as the string "123".
        let parsed: serde_json::Value = serde_json::from_str(&env_str).unwrap();
        assert!(parsed.is_string());
        assert_eq!(parsed.as_str().unwrap(), "123");

        // Boolean-looking and null-looking env values stay strings too.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&string_to_json_encoded("true")).unwrap(),
            serde_json::Value::String("true".into())
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&string_to_json_encoded("null")).unwrap(),
            serde_json::Value::String("null".into())
        );

        // Plain string (the common case) round-trips unchanged.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&string_to_json_encoded("hello")).unwrap(),
            serde_json::Value::String("hello".into())
        );

        // Collection/global vars are serde_json::Value — objects round-trip
        // as objects, numbers as numbers, strings as strings.
        let obj = serde_json::json!({ "a": 1, "b": [true, null] });
        let encoded = variable_value_to_string(&obj);
        let parsed: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(parsed, obj);

        let num = serde_json::json!(123);
        let parsed: serde_json::Value =
            serde_json::from_str(&variable_value_to_string(&num)).unwrap();
        assert!(parsed.is_number());
    }

    /// Regression (backlog line 89): setters String()-coerced and getters
    /// JSON.parse'd were NOT inverses — a plain string '1234' set through
    /// pm.environment round-tripped as the NUMBER 1234 (and objects became
    /// "[object Object]"). The shim now JSON-encodes on set; the bridges
    /// decode-on-set / encode-on-get, so the full cycle (shim encode → bridge
    /// decode → bridge encode → shim parse) must restore the exact value.
    #[test]
    fn test_set_get_json_roundtrip_preserves_type() {
        // Env bridge: decode on set. JSON string → plain string.
        assert_eq!(decode_json_encoded("\"1234\""), "1234");
        // Number/bool/object → stored as JSON text (env is strings-only).
        assert_eq!(decode_json_encoded("42"), "42");
        assert_eq!(decode_json_encoded("{\"a\":1}"), "{\"a\":1}");
        // Unparseable (legacy seeded plain string) → verbatim.
        assert_eq!(
            decode_json_encoded("https://api.example.com"),
            "https://api.example.com"
        );

        // Full env cycle: set('s','1234') → shim sends '"1234"' → bridge
        // stores plain '1234' → get bridge returns '"1234"' → shim JSON.parse
        // → the STRING '1234' (never the number 1234).
        let stored = decode_json_encoded("\"1234\"");
        assert_eq!(stored, "1234");
        let on_get = string_to_json_encoded(&stored);
        assert_eq!(on_get, "\"1234\"");
        let parsed: serde_json::Value = serde_json::from_str(&on_get).unwrap();
        assert!(parsed.is_string());
        assert_eq!(parsed.as_str().unwrap(), "1234");

        // Collection/global bridge: decode on set to a Value.
        assert_eq!(decode_json_value("\"42\""), serde_json::json!("42"));
        assert_eq!(decode_json_value("42"), serde_json::json!(42));
        assert_eq!(decode_json_value("{\"a\":1}"), serde_json::json!({"a": 1}));
        assert_eq!(decode_json_value("plain"), serde_json::json!("plain"));

        // Full collection cycle: object set → object out.
        let obj: Value = decode_json_value("{\"a\":1}");
        let encoded = variable_value_to_string(&obj);
        assert_eq!(encoded, "{\"a\":1}");
        let back: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back, serde_json::json!({"a": 1}));
    }

    /// Regression (backlog line 145): `pm.variables.get` must resolve with
    /// Postman precedence — iteration data beats environment beats collection
    /// beats globals. The old code skipped iteration data entirely, so a CSV
    /// row could never override an environment/collection value.
    #[test]
    fn test_variables_lookup_postman_precedence() {
        let data = HashMap::from([("k".to_string(), Value::String("from-data".into()))]);
        let env = HashMap::from([("k".to_string(), "from-env".to_string())]);
        let collection =
            HashMap::from([("k".to_string(), Value::String("from-collection".into()))]);
        let globals = HashMap::from([("k".to_string(), Value::String("from-globals".into()))]);

        // Full shadow chain: data wins.
        let got = variables_lookup(
            "k",
            &HashMap::new(),
            Some(&data),
            &env,
            &collection,
            &globals,
        );
        assert_eq!(
            got.as_deref(),
            Some("\"from-data\""),
            "iteration data must beat env/collection/globals"
        );

        // No data: env wins.
        let got = variables_lookup("k", &HashMap::new(), None, &env, &collection, &globals);
        assert_eq!(
            got.as_deref(),
            Some("\"from-env\""),
            "environment must beat collection/globals"
        );

        // No data, no env: collection wins.
        let got = variables_lookup(
            "k",
            &HashMap::new(),
            None,
            &HashMap::new(),
            &collection,
            &globals,
        );
        assert_eq!(
            got.as_deref(),
            Some("\"from-collection\""),
            "collection must beat globals"
        );

        // Only globals.
        let got = variables_lookup(
            "k",
            &HashMap::new(),
            None,
            &HashMap::new(),
            &HashMap::new(),
            &globals,
        );
        assert_eq!(
            got.as_deref(),
            Some("\"from-globals\""),
            "globals last resort"
        );

        // No scope has it.
        let got = variables_lookup(
            "missing",
            &HashMap::new(),
            Some(&data),
            &env,
            &collection,
            &globals,
        );
        assert_eq!(got, None, "unknown key resolves to None");
    }

    /// Regression (backlog line 147): `pm.sendRequest` must resolve
    /// `{{var}}` placeholders in the URL, headers, AND body — the old code
    /// resolved the URL only, so `Authorization: Bearer {{token}}` or a
    /// body containing a placeholder went out with the literal braces.
    #[cfg(feature = "send-request")]
    #[test]
    fn test_resolve_send_request_resolves_url_headers_body() {
        let env = HashMap::from([("token".to_string(), "s3cret".to_string())]);
        let collection = HashMap::new();
        let globals = HashMap::new();

        let (url, headers, body) = resolve_send_request(
            "https://api.example.com/v1?key={{token}}",
            "{\"Authorization\":\"Bearer {{token}}\",\"X-Static\":\"v\"}",
            "{\"token\":\"{{token}}\"}",
            &HashMap::new(),
            &env,
            &collection,
            &globals,
        );

        assert_eq!(
            url, "https://api.example.com/v1?key=s3cret",
            "URL must resolve"
        );

        // Backlog line 137: pm.variables is the LOCAL scope — sendRequest
        // must resolve a script-set value, and it must beat environment.
        let local = HashMap::from([("token".to_string(), Value::String("local-tok".into()))]);
        let (url_local, headers_local, body_local) = resolve_send_request(
            "https://api.example.com/v1?key={{token}}",
            "{\"Authorization\":\"Bearer {{token}}\"}",
            "{\"token\":\"{{token}}\"}",
            &local,
            &env,
            &collection,
            &globals,
        );
        assert_eq!(url_local, "https://api.example.com/v1?key=local-tok");
        assert_eq!(
            headers_local.get("Authorization").map(String::as_str),
            Some("Bearer local-tok")
        );
        assert_eq!(
            body_local.map(|b| match b {
                Body::Raw(s) => s,
                _ => String::new(),
            }),
            Some("{\"token\":\"local-tok\"}".to_string())
        );
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer s3cret"),
            "header values must resolve"
        );
        assert_eq!(
            headers.get("X-Static").map(String::as_str),
            Some("v"),
            "non-placeholder headers must pass through untouched"
        );
        assert_eq!(
            body.map(|b| match b {
                Body::Raw(s) => s,
                _ => String::new(),
            }),
            Some("{\"token\":\"s3cret\"}".to_string()),
            "raw body must resolve"
        );

        // Empty body stays None.
        let (_, _, body) =
            resolve_send_request("u", "{}", "", &HashMap::new(), &env, &collection, &globals);
        assert!(body.is_none(), "empty body must stay None");
    }

    /// Regression (backlog line 68): `pm.response.json()` returned
    /// simd-json's IN-PLACE scratch buffer. simd-json rewrites the input
    /// bytes while de-escaping, so the returned text contained raw control
    /// chars / stale bytes for any body with `\n`, `\"`, `\\` or `\uXXXX`.
    /// The bridge must validate against a scratch copy and return the
    /// ORIGINAL bytes, which the shim JSON.parses cleanly.
    #[test]
    fn test_response_json_returns_pristine_body() {
        // The classic failing body: a real newline escape. The old code
        // de-escaped it in-place, shortening the string but keeping the
        // buffer length → stale byte after the newline → invalid JSON.
        // NOTE: the \n, \", \\ and \uXXXX here are JSON ESCAPE sequences
        // inside the raw string (backslash + char), NOT real control bytes —
        // a literal newline byte would be invalid JSON.
        let body = br#"{"a":"x\ny","b":"\"q\"","c":"path\\to","d":"\u00e9"}"#;
        let s = response_json_string(body).expect("valid JSON must be returned");
        // The returned text must be EXACTLY the original bytes — pristine
        // for the shim's JSON.parse().
        assert_eq!(s.as_bytes(), body);
        // And it must actually parse.
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["a"], "x\ny");
        assert_eq!(v["b"], "\"q\"");
        assert_eq!(v["c"], "path\\to");
        assert_eq!(v["d"], "é");

        // Non-JSON body → None (shim throws its descriptive error).
        assert!(response_json_string(b"not json").is_none());
        // Empty body → None.
        assert!(response_json_string(b"").is_none());
    }

    /// Regression (backlog P3): `parse_headers` returned an EMPTY map on any
    /// object form containing a non-string value (e.g. {"Content-Length":
    /// 123}) — the `HashMap<String, String>` parse failed, the array fallback
    /// failed, and EVERY header was silently dropped. Non-string scalars must
    /// be stringified and the rest preserved.
    #[cfg(feature = "send-request")]
    #[test]
    fn parse_headers_non_string_object_values_are_stringified() {
        let headers = parse_headers(
            r#"{"Content-Type":"application/json","Content-Length":123,"X-Bool":true}"#,
        );
        assert_eq!(
            headers.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            headers.get("Content-Length").map(String::as_str),
            Some("123")
        );
        assert_eq!(headers.get("X-Bool").map(String::as_str), Some("true"));
        assert_eq!(headers.len(), 3, "no header may be dropped");

        // Null/complex values are skipped, not fatal.
        let headers = parse_headers(r#"{"A":"keep","B":null,"C":[1,2]}"#);
        assert_eq!(headers.get("A").map(String::as_str), Some("keep"));
        assert!(!headers.contains_key("B"));
        assert!(!headers.contains_key("C"));

        // Array form still works unchanged.
        let headers = parse_headers(r#"[{"key":"X","value":"y"}]"#);
        assert_eq!(headers.get("X").map(String::as_str), Some("y"));
    }

    /// P4b conformance: the pm.js binding is namespace-parameterized. `pm`
    /// (frozen Postman-compat) and `trp` (canonical, Postman convention) are
    /// peer views over the same state. The STOCK install exposes ONLY those
    /// two — no default aliases (aliases are opt-in via SandboxConfig).
    /// Error strings name the invoked namespace, and the top-level bindings
    /// are installed read-only.
    #[test]
    fn test_binding_namespace_conformance() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");

            // Stock install: pm + trp only — no aliases, no tropel.
            let installed: bool = ctx
                .eval(
                    "typeof pm === 'object' && typeof trp === 'object' \
                     && typeof wire === 'undefined' && typeof tropel === 'undefined'",
                )
                .unwrap();
            assert!(
                installed,
                "pm + trp must be installed; no default aliases or tropel globals"
            );

            // pm and trp are distinct peer views (not the same object).
            let peer_views: bool = ctx.eval("pm !== trp").unwrap();
            assert!(
                peer_views,
                "pm and trp must be peer views, not the same object"
            );

            // Error strings name the invoked namespace.
            let pm_err: String = ctx
                .eval(
                    "(function(){ try { pm.response.json(); return 'no-error'; } \
                      catch (e) { return e.message; } })()",
                )
                .unwrap();
            assert!(
                pm_err.contains("pm.response.json()"),
                "pm error must name pm: {pm_err}"
            );

            let tr_err: String = ctx
                .eval(
                    "(function(){ try { trp.response.json(); return 'no-error'; } \
                      catch (e) { return e.message; } })()",
                )
                .unwrap();
            assert!(
                tr_err.contains("trp.response.json()"),
                "trp error must name trp: {tr_err}"
            );

            // Top-level bindings installed read-only (writable: false).
            let readonly: bool = ctx
                .eval(
                    "Object.getOwnPropertyDescriptor(globalThis, 'pm').writable === false \
                     && Object.getOwnPropertyDescriptor(globalThis, 'trp').writable === false",
                )
                .unwrap();
            assert!(readonly, "pm/trp must be non-writable");
        });
    }

    /// P4b open item: alias configuration is part of the public API.
    /// Evaluating [`SandboxConfig::render_js_preamble`] BEFORE the pm.js
    /// shim must make the canonical binding appear under the configured
    /// namespace, with the configured aliases as TRUE aliases (identical
    /// object, not a proxy). The default `trp` canonical is absent when a
    /// custom namespace replaces it, and error strings still name the
    /// invoked namespace.
    #[test]
    fn test_sandbox_config_drives_namespace_and_aliases() {
        use crate::config::SandboxConfig;

        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            // A namespace distinct from the `trp` default proves the config
            // actually drives the canonical name.
            let cfg = SandboxConfig {
                namespace: "acme".into(),
                aliases: vec!["product".into(), "wire".into()],
            };
            ctx.eval::<(), _>(cfg.render_js_preamble())
                .expect("config preamble should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");

            // Custom canonical installed; aliases are true aliases of it.
            let installed: bool = ctx
                .eval(
                    "typeof acme === 'object' && typeof product === 'object' \
                     && typeof wire === 'object' && product === acme && wire === acme",
                )
                .unwrap();
            assert!(
                installed,
                "acme canonical with product/wire aliases must be installed"
            );

            // The DEFAULT trp binding must NOT be present when the config
            // renames the canonical namespace.
            let no_default: bool = ctx
                .eval("typeof trp === 'undefined' && typeof tropel === 'undefined'")
                .unwrap();
            assert!(
                no_default,
                "default trp must not leak alongside the custom namespace"
            );

            // pm is still the frozen Postman-compat peer view, distinct object.
            let peer_views: bool = ctx.eval("typeof pm === 'object' && pm !== acme").unwrap();
            assert!(peer_views, "pm must remain a peer view, not acme's alias");

            // Error strings name the CONFIGURED namespace.
            let err: String = ctx
                .eval(
                    "(function(){ try { acme.response.json(); return 'no-error'; } \
                      catch (e) { return e.message; } })()",
                )
                .unwrap();
            assert!(
                err.contains("acme.response.json()"),
                "error must name acme: {err}"
            );

            // Configured bindings installed read-only too.
            let readonly: bool = ctx
                .eval(
                    "Object.getOwnPropertyDescriptor(globalThis, 'acme').writable === false \
                     && Object.getOwnPropertyDescriptor(globalThis, 'product').writable === false",
                )
                .unwrap();
            assert!(readonly, "acme/product must be non-writable");
        });
    }

    /// Regression (backlog line 101): `pm.info` was a hardcoded stub and
    /// `pm.request.auth` / `pm.request.body.mode` were module-scope
    /// singletons — a fresh request on the next iteration (no auth, a
    /// different body) still read the PREVIOUS iteration's values. The shim
    /// now reads LIVE from the __tropel_pm_* bridges: pm.info fields come
    /// from __tropel_pm_info, auth from __tropel_pm_request_auth (null when
    /// the current request has none), and mode from
    /// __tropel_pm_request_body_mode. Stub bridges simulate the per-
    /// iteration state change; reads must follow the bridge, not a stale
    /// JS-side copy.
    #[test]
    fn test_pm_info_auth_and_body_mode_read_live_per_iteration() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");

            // Iteration 1: a prerequest script SETS auth and a body mode.
            ctx.eval::<(), _>(
                r#"
                // Simulate the sandbox bridges. Per-iteration values change
                // to prove reads are live, not cached.
                globalThis.__tropel_pm_info = function () {
                    return JSON.stringify({ eventName: 'test', iteration: 4,
                        iterationCount: 25, requestName: 'get-user', requestId: 'r-9' });
                };
                globalThis.__tropel_pm_request_auth = function () {
                    // The CURRENT request has no auth.
                    return null;
                };
                globalThis.__tropel_pm_request_body_mode = function () {
                    // The CURRENT request's body is urlencoded.
                    return 'urlencoded';
                };
                globalThis.__tropel_pm_request_auth_set = function (j) {
                    globalThis.__auth_set = j;
                };

                // Iteration 1: prerequest writes auth + a body mode.
                pm.request.auth = { type: 'bearer', token: 'stale-token' };
                pm.request.body = { mode: 'raw', raw: 'x' };

                // Iteration 2: a fresh request with NO auth and a urlencoded
                // body. Reads must come from the live bridges.
                var authNow = pm.request.auth;
                var modeNow = pm.request.body.mode;
                var info = pm.info;
                globalThis.__out = JSON.stringify([
                    authNow, modeNow,
                    info.eventName, info.iteration, info.iterationCount,
                    info.requestName, info.requestId
                ]);
                "#,
            )
            .expect("setup should eval");

            // Parse the exact output so assertions are unambiguous.
            let v: serde_json::Value =
                serde_json::from_str(&ctx.eval::<String, _>("__out").unwrap())
                    .expect("__out must be JSON");
            let arr = v.as_array().expect("__out must be an array");
            // auth must be NULL (fresh request has none), NOT the stale
            // 'stale-token' singleton from iteration 1.
            assert!(
                arr[0].is_null(),
                "pm.request.auth must read the live (absent) auth, got: {arr:?}"
            );
            assert!(
                arr[0].as_str().is_none(),
                "pm.request.auth must not leak the previous iteration's auth, got: {arr:?}"
            );
            // body.mode must follow the live bridge, not the 'raw' written
            // by iteration 1's body setter.
            assert_eq!(
                arr[1].as_str(),
                Some("urlencoded"),
                "pm.request.body.mode must read the live mode, got: {arr:?}"
            );
            // pm.info fields must be live, not the hardcoded stub.
            assert_eq!(
                arr[2].as_str(),
                Some("test"),
                "eventName live, got: {arr:?}"
            );
            assert_eq!(arr[3], serde_json::json!(4), "iteration live, got: {arr:?}");
            assert_eq!(
                arr[4],
                serde_json::json!(25),
                "iterationCount live, got: {arr:?}"
            );
            assert_eq!(
                arr[5].as_str(),
                Some("get-user"),
                "requestName live, got: {arr:?}"
            );
            assert_eq!(arr[6].as_str(), Some("r-9"), "requestId live, got: {arr:?}");

            // The setter still forwards to the auth bridge.
            let auth_set: String = ctx.eval("__auth_set").unwrap();
            assert_eq!(
                auth_set, r#"{"type":"bearer","token":"stale-token"}"#,
                "pm.request.auth setter must still JSON-encode to the bridge"
            );
        });
    }
}
