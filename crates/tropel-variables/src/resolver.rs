use crate::catalog::DynamicCatalog;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;

/// Variable scope.
#[derive(Debug, Clone, Default)]
pub struct VariableScope {
    /// Local variables (pm.variables) — HIGHEST priority, Postman's local
    /// scope (backlog line 137). Script-set values here shadow data/env.
    pub local: HashMap<String, serde_json::Value>,
    /// Iteration data.
    pub data: HashMap<String, serde_json::Value>,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Collection variables (backlog line 346: wrapped in Arc to avoid
    /// deep-cloning the entire HashMap on every build_scope call).
    pub collection: Arc<HashMap<String, serde_json::Value>>,
    /// Global variables (same Arc optimization).
    pub globals: Arc<HashMap<String, serde_json::Value>>,
}

/// The `{{var}}` placeholder regex, compiled ONCE per process. `VariableResolver`
/// is constructed per iteration / per VU on the hot path (see the runner), so a
/// `Regex::new` on every construction was pure waste — compiled once, a `Regex`
/// is `Sync` and reused by all threads.
static VAR_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

fn var_re() -> &'static Regex {
    // `[^{}]+` matches only the INNERMOST placeholder: a variable name
    // contains no braces, so a name with `{`/`}` inside cannot be a valid
    // reference. `{{host_{{suffix}}}}` therefore resolves `{{suffix}}`
    // first, then `{{host_dev}}` on the next deep pass — the greedy
    // `[^}]+` matched `host_{{suffix` (the OUTER span) and could never
    // recurse inward (backlog line 135).
    VAR_RE.get_or_init(|| Regex::new(r"\{\{([^{}]+)\}\}").expect("valid variable regex"))
}

/// Postman resolves nested `{{a}}` → `{{b}}` chains up to 20 levels deep
/// (its docs call this the "maximum nesting depth" — 19 levels + the outer
/// reference). The runner previously hardcoded a bare `5` at ten call sites,
/// so chains 6+ deep left a literal `{{x}}` on the wire. A single named
/// constant keeps every call site in lockstep with Postman.
pub const MAX_VARIABLE_RESOLUTION_PASSES: usize = 20;

/// Resolves {{variable}} references with scope precedence.
pub struct VariableResolver {
    dynamic_catalog: DynamicCatalog,
}

impl VariableResolver {
    pub fn new() -> Self {
        Self {
            dynamic_catalog: DynamicCatalog::new(),
        }
    }

    /// Resolve all variable references in the input string.
    pub fn resolve(&self, input: &str, scope: &VariableScope) -> String {
        self.resolve_with(input, scope, EscapeMode::None)
    }

    /// Resolve variable references, escaping each substituted value for
    /// embedding inside a JSON string literal (`"` `\` control chars are
    /// escaped). This is what makes `{"s":"{{name}}"}` with
    /// `name = he said "hi"` produce VALID JSON instead of a broken document
    /// (backlog line 96: substituted values weren't escaped for their
    /// context — a CSV column containing a quote/backslash/newline made the
    /// error rate a function of the data file).
    pub fn resolve_json(&self, input: &str, scope: &VariableScope) -> String {
        self.resolve_with(input, scope, EscapeMode::Json)
    }

    /// Resolve variable references for a URL. Postman-compatible: substituted
    /// values are inserted RAW with no percent-encoding, so `{{endpoint}}` =
    /// `https://api.test/search?a=1` survives intact (backlog line 136 — the
    /// old behavior percent-encoded `?` `&` `=` `#` in every value and
    /// destroyed structural URLs). Data values that need encoding are the
    /// caller's job, exactly as in Postman/k6.
    pub fn resolve_url(&self, input: &str, scope: &VariableScope) -> String {
        self.resolve_with(input, scope, EscapeMode::Url)
    }

    /// Shared resolution core. `mode` decides how each substituted VALUE is
    /// escaped for its destination context (raw / JSON string / URL query).
    /// The placeholder itself is never touched, and an unresolved variable
    /// stays literal `{{name}}` in every mode.
    fn resolve_with(&self, input: &str, scope: &VariableScope, mode: EscapeMode) -> String {
        if !input.contains("{{") {
            return input.to_owned();
        }

        // First resolve dynamic variables ({{$xxx}})
        // TR-403: a total-output cap error means the input is too large to
        // resolve safely — fall back to the ORIGINAL input (no partial
        // substitution) and log loudly. The truncation warning was already
        // emitted by the catalog.
        let after_dynamic = self.dynamic_catalog.resolve(input).unwrap_or_else(|e| {
            tracing::error!("dynamic variable resolution failed: {e}");
            input.to_string()
        });

        // Then resolve scoped variables ({{var_name}})
        let result = var_re().replace_all(&after_dynamic, |caps: &regex::Captures| {
            let var_name = caps.get(1).unwrap().as_str().trim();

            // Skip dynamic vars (already handled)
            if var_name.starts_with('$') {
                return caps.get(0).unwrap().as_str().to_string();
            }

            let value = self.resolve_variable(var_name, scope);
            if value.contains("{{") {
                // The value is an unresolved placeholder (keep it literal)
                // OR a nested template that later passes must resolve —
                // `{{base_url}}` → `https://{{host}}`. Escaping it NOW
                // would corrupt the inner placeholder (`{` → `%7B`), so it
                // is inserted raw; the destination-context escape applies
                // only once the value is fully resolved (backlog line 135).
                value
            } else {
                match mode {
                    EscapeMode::None => value,
                    EscapeMode::Json => {
                        if placeholder_in_json_string(&after_dynamic, caps.get(0).unwrap().range())
                        {
                            json_escape(&value)
                        } else {
                            // A bare JSON-fragment variable —
                            // `"filter": {{filterJson}}` — must stay RAW:
                            // escaping it turns `{"a":1}` into `{\"a\":1}`
                            // and corrupts the document (backlog line 135).
                            value
                        }
                    }
                    EscapeMode::Url => value,
                }
            }
        });

        // Backlog line 349: .to_string() on Cow::Owned allocates a fresh
        // String + memcpy; .into_owned() is free for Cow::Owned.
        result.into_owned()
    }

    /// Resolve a single variable name against the scope.
    pub fn resolve_variable(&self, var_name: &str, scope: &VariableScope) -> String {
        // Postman scope priority: local (pm.variables) > data > env >
        // collection > globals.

        // Local variables first — pm.variables is the highest-priority scope
        // (backlog line 137: set-then-get must be consistent, so a local
        // value shadows same-named iteration data).
        if let Some(val) = scope.local.get(var_name) {
            return value_to_string(val);
        }

        // Check iteration data
        if let Some(val) = scope.data.get(var_name) {
            return value_to_string(val);
        }

        // Check environment
        if let Some(val) = scope.env.get(var_name) {
            return val.clone();
        }

        // Check collection variables
        if let Some(val) = scope.collection.get(var_name) {
            return value_to_string(val);
        }

        // Check globals
        if let Some(val) = scope.globals.get(var_name) {
            return value_to_string(val);
        }

        // Not found — return the original placeholder
        format!("{{{{{}}}}}", var_name)
    }

    /// Resolve an entire string — including nested variable references.
    /// Multiple passes to handle {{var1_{{var2}}}} style nesting.
    pub fn resolve_deep(&self, input: &str, scope: &VariableScope, max_passes: usize) -> String {
        self.resolve_deep_with(input, scope, max_passes, EscapeMode::None)
    }

    /// [`resolve_deep`] with JSON-string escaping of substituted values.
    pub fn resolve_json_deep(
        &self,
        input: &str,
        scope: &VariableScope,
        max_passes: usize,
    ) -> String {
        self.resolve_deep_with(input, scope, max_passes, EscapeMode::Json)
    }

    /// [`resolve_deep`] with Postman-compatible URL semantics: substituted
    /// values are inserted RAW (no percent-encoding — see [`resolve_url`]).
    pub fn resolve_url_deep(
        &self,
        input: &str,
        scope: &VariableScope,
        max_passes: usize,
    ) -> String {
        self.resolve_deep_with(input, scope, max_passes, EscapeMode::Url)
    }

    /// Shared deep-resolution core that threads the escape mode through
    /// every pass.
    fn resolve_deep_with(
        &self,
        input: &str,
        scope: &VariableScope,
        max_passes: usize,
        mode: EscapeMode,
    ) -> String {
        // Fast path: no {{ means no variable reference — return unchanged
        // without allocating.
        if !input.contains("{{") {
            return input.to_string();
        }
        let mut result = input.to_string();
        for _ in 0..max_passes {
            if !result.contains("{{") {
                break;
            }
            let resolved = self.resolve_with(&result, scope, mode);
            if resolved == result {
                break;
            }
            result = resolved;
        }
        result
    }
}

/// How a substituted variable VALUE is escaped for its destination context.
/// The placeholder text itself is never modified.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeMode {
    /// Raw insertion (headers, plain-text bodies) — no escaping.
    None,
    /// Escape for embedding inside a JSON string literal.
    Json,
    /// URL context — Postman-compatible RAW insertion: no percent-encoding
    /// (backlog line 136: Postman does no encoding at all), so structural
    /// URLs inside substituted values survive resolution.
    Url,
}

/// Escape a value for safe embedding inside a JSON string literal: `"` and
/// `\` get backslash-escaped, control chars become \n \r \t or \uXXXX.
/// JSON bodies built from `{{var}}` templates stay parseable even when the
/// data (e.g. a CSV column) contains quotes or newlines.
fn json_escape(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Is the placeholder at `range` inside a JSON string literal? A template
/// has an ODD number of unescaped `"` before a placeholder that sits inside
/// a string (each pair of quotes opens+closes a string literal, so an odd
/// count means the last `"` was an opening one). Escaped quotes (`\"`) are
/// skipped so `{"a":"say \"hi\" {{name}}"}` still counts correctly. This is
/// what lets `"{{name}}"` get value-escaped while a bare JSON fragment —
/// `"filter": {{filterJson}}` — stays raw (backlog line 135).
fn placeholder_in_json_string(template: &str, range: std::ops::Range<usize>) -> bool {
    let before = &template[..range.start];
    let bytes = before.as_bytes();
    let mut quote_count = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped character (e.g. \")
            b'"' => {
                quote_count += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    quote_count % 2 == 1
}

impl Default for VariableResolver {
    fn default() -> Self {
        Self::new()
    }
}

fn value_to_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_variable() {
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([("base_url".into(), "https://api.example.com".into())]),
            ..Default::default()
        };

        let result = resolver.resolve("{{base_url}}/users", &scope);
        assert_eq!(result, "https://api.example.com/users");
    }

    #[test]
    fn test_multiple_variables() {
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([
                ("host".into(), "api.example.com".into()),
                ("port".into(), "443".into()),
            ]),
            ..Default::default()
        };

        let result = resolver.resolve("https://{{host}}:{{port}}/v1", &scope);
        assert_eq!(result, "https://api.example.com:443/v1");
    }

    #[test]
    fn test_scope_priority() {
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            local: HashMap::new(),
            data: HashMap::from([("key".into(), serde_json::Value::String("data-value".into()))]),
            env: HashMap::from([("key".into(), "env-value".into())]),
            collection: Arc::new(HashMap::from([(
                "key".into(),
                serde_json::Value::String("col-value".into()),
            )])),
            globals: Arc::new(HashMap::from([(
                "key".into(),
                serde_json::Value::String("global-value".into()),
            )])),
        };

        // Data takes priority
        let result = resolver.resolve("{{key}}", &scope);
        assert_eq!(result, "data-value");
    }

    #[test]
    fn test_local_variable_highest_priority() {
        // Backlog line 137: pm.variables is the LOCAL scope — Postman's
        // highest priority. A local value must shadow iteration data, env,
        // collection and globals for {{var}} substitution.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            local: HashMap::from([("key".into(), serde_json::Value::String("local".into()))]),
            data: HashMap::from([("key".into(), serde_json::Value::String("data".into()))]),
            env: HashMap::from([("key".into(), "env".into())]),
            collection: Arc::new(HashMap::from([(
                "key".into(),
                serde_json::Value::String("col".into()),
            )])),
            globals: Arc::new(HashMap::from([(
                "key".into(),
                serde_json::Value::String("global".into()),
            )])),
        };
        assert_eq!(resolver.resolve("{{key}}", &scope), "local");
    }

    #[test]
    fn test_missing_variable() {
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();
        let result = resolver.resolve("{{missing}}", &scope);
        assert_eq!(result, "{{missing}}");
    }

    #[test]
    fn test_dynamic_variable() {
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();
        let result = resolver.resolve("id={{$guid}}", &scope);
        assert!(result.starts_with("id="));
        assert_eq!(result.len(), 39); // "id=" + 36-char UUID
    }

    #[test]
    fn test_no_variables() {
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();
        let result = resolver.resolve("plain text", &scope);
        assert_eq!(result, "plain text");
    }

    #[test]
    fn test_deep_resolve() {
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([
                ("host".into(), "{{base_host}}".into()),
                ("base_host".into(), "api.example.com".into()),
            ]),
            ..Default::default()
        };

        let result = resolver.resolve_deep("https://{{host}}/v1", &scope, 5);
        assert_eq!(result, "https://api.example.com/v1");
    }

    #[test]
    fn test_collection_then_globals_priority() {
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            collection: Arc::new(HashMap::from([(
                "key".into(),
                serde_json::Value::String("col-value".into()),
            )])),
            globals: Arc::new(HashMap::from([(
                "key".into(),
                serde_json::Value::String("global-value".into()),
            )])),
            ..Default::default()
        };

        // env > data > collection > globals; with env absent, collection wins.
        assert_eq!(resolver.resolve("{{key}}", &scope), "col-value");

        // Collection value type is preserved through the value form.
        let scope_num = VariableScope {
            collection: Arc::new(HashMap::from([("n".into(), serde_json::json!(42))])),
            ..Default::default()
        };
        assert_eq!(resolver.resolve("n={{n}}", &scope_num), "n=42");
    }

    #[test]
    fn test_dynamic_guid_fresh_per_occurrence() {
        // Regression: {{$guid}}-{{$guid}} once produced the SAME value both
        // times (str::replace of a single resolved string). Each occurrence
        // must be independently fresh.
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();
        // Delimiter is a comma — NOT a hyphen, because UUIDs themselves are
        // hyphenated, so splitting on '-' would split inside the first UUID.
        let result = resolver.resolve("{{$guid}},{{$guid}}", &scope);

        let (first, second) = result.split_once(',').expect("comma separator present");
        assert_eq!(first.len(), 36, "first is a UUID: {first}");
        assert_eq!(second.len(), 36, "second is a UUID: {second}");
        assert_ne!(first, second, "each occurrence is fresh");
    }

    #[test]
    fn test_dynamic_timestamp_and_random_int() {
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();

        // {{$timestamp}} — 10-digit Unix seconds.
        let ts = resolver.resolve("ts={{$timestamp}}", &scope);
        let ts_val = ts.strip_prefix("ts=").unwrap();
        assert_eq!(ts_val.len(), 10, "timestamp is 10 digits: {ts}");
        let secs: u64 = ts_val.parse().unwrap();
        assert!(secs > 1_700_000_000, "timestamp is recent: {ts}");

        // {{$randomInt}} — fresh integer in [0, 1000) per occurrence.
        for _ in 0..20 {
            let ri = resolver.resolve("{{$randomInt}}", &scope);
            let n: i64 = ri
                .parse()
                .unwrap_or_else(|_| panic!("randomInt is numeric: {ri}"));
            assert!((0..1000).contains(&n), "randomInt in range: {ri}");
        }
    }

    #[test]
    fn test_unresolved_variable_left_literal_in_deep() {
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();
        let result = resolver.resolve_deep("/api/{{missing}}/v1", &scope, 5);
        assert_eq!(result, "/api/{{missing}}/v1");
    }

    #[test]
    fn test_resolve_json_escapes_quotes() {
        // Backlog line 96: `{"s":"{{name}}"}` with `name = he said "hi"`
        // produced INVALID JSON (the quote terminated the string). The value
        // must be JSON-escaped so the document stays parseable.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([("name".into(), "he said \"hi\"".into())]),
            ..Default::default()
        };

        let result = resolver.resolve_json(r#"{"s":"{{name}}"}"#, &scope);
        assert_eq!(result, r#"{"s":"he said \"hi\""}"#);
        // The result must round-trip as valid JSON with the value intact.
        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");
        assert_eq!(parsed["s"], "he said \"hi\"");
    }

    #[test]
    fn test_resolve_json_escapes_backslash_and_newline() {
        // CSV data with a backslash or embedded newline must not corrupt a
        // JSON body (the error rate was a function of the data file).
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([
                ("path".into(), "C:\\tmp\\f".into()),
                ("note".into(), "line1\nline2".into()),
            ]),
            ..Default::default()
        };

        let result = resolver.resolve_json(r#"{"p":"{{path}}","n":"{{note}}"}"#, &scope);
        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");
        assert_eq!(parsed["p"], "C:\\tmp\\f");
        assert_eq!(parsed["n"], "line1\nline2");
    }

    #[test]
    fn test_resolve_json_unresolved_stays_literal() {
        let resolver = VariableResolver::new();
        let scope = VariableScope::default();
        // An unresolved variable inside a JSON body must stay `{{name}}`
        // (literal placeholder), not be escaped into garbage.
        let result = resolver.resolve_json(r#"{"s":"{{missing}}"}"#, &scope);
        assert_eq!(result, r#"{"s":"{{missing}}"}"#);
    }

    #[test]
    fn test_resolve_url_inserts_values_raw_postman_style() {
        // Backlog line 136: Postman does no percent-encoding — substituted
        // values are inserted RAW. (The old behavior encoded `?`/`&`/`=`/`#`
        // in every value to stop query splitting, but that destroyed
        // structural URLs; Postman/k6 parity means the caller controls
        // encoding.)
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([("q".into(), "a&b=c".into())]),
            ..Default::default()
        };

        let result = resolver.resolve_url("/search?q={{q}}", &scope);
        assert_eq!(result, "/search?q=a&b=c");
    }

    #[test]
    fn test_resolve_url_leaves_structural_urls_intact() {
        // The item-136 symptom: `{{endpoint}}` holding a full URL with a
        // query string + fragment must survive resolution unmodified.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([(
                "endpoint".into(),
                "https://api.test/search?a=1&b=2#frag".into(),
            )]),
            ..Default::default()
        };

        let result = resolver.resolve_url("{{endpoint}}", &scope);
        assert_eq!(result, "https://api.test/search?a=1&b=2#frag");

        // `+`, space and `#` in a value stay raw too (Postman semantics).
        let scope2 = VariableScope {
            env: HashMap::from([("token".into(), "tok+1 #2".into())]),
            ..Default::default()
        };
        assert_eq!(resolver.resolve_url("?t={{token}}", &scope2), "?t=tok+1 #2");
    }

    #[test]
    fn test_nested_innermost_first() {
        // Backlog line 135: the greedy `[^}]+` matched `host_{{suffix` (the
        // OUTER span) and could never recurse inward, so `{{host_{{suffix}}}}`
        // never resolved. The innermost `{{suffix}}` must resolve first,
        // then `{{host_dev}}` on the next deep pass.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([
                ("suffix".into(), "dev".into()),
                ("host_dev".into(), "api-dev.example.com".into()),
            ]),
            ..Default::default()
        };
        let result = resolver.resolve_deep("https://{{host_{{suffix}}}}/v1", &scope, 5);
        assert_eq!(result, "https://api-dev.example.com/v1");
    }

    #[test]
    fn test_deep_chain_beyond_five_resolves_to_postman_depth() {
        // Backlog line 204: the runner hardcoded resolution depth 5, so a
        // 6-level chain left `{{v1}}` literal on the wire. Postman resolves
        // up to 20 levels — a 10-level chain must resolve fully.
        let resolver = VariableResolver::new();
        let mut env = HashMap::new();
        for i in 0..10 {
            let key = format!("v{i}");
            let val = if i == 9 {
                "final".to_string()
            } else {
                format!("{{{{v{}}}}}", i + 1)
            };
            env.insert(key, val);
        }
        let scope = VariableScope {
            env,
            ..Default::default()
        };
        let result = resolver.resolve_deep("{{v0}}", &scope, MAX_VARIABLE_RESOLUTION_PASSES);
        assert_eq!(result, "final");
    }

    #[test]
    fn test_chain_deeper_than_postman_cap_stops_at_cap() {
        // A 25-level chain exceeds Postman's 20-level cap: resolution stops
        // at the cap and the deepest reference stays literal (matching
        // Postman, which also gives up at 20 levels).
        let resolver = VariableResolver::new();
        let mut env = HashMap::new();
        for i in 0..25 {
            let key = format!("w{i}");
            let val = if i == 24 {
                "deep".to_string()
            } else {
                format!("{{{{w{}}}}}", i + 1)
            };
            env.insert(key, val);
        }
        let scope = VariableScope {
            env,
            ..Default::default()
        };
        let result = resolver.resolve_deep("{{w0}}", &scope, MAX_VARIABLE_RESOLUTION_PASSES);
        // Exactly 20 passes run: references w0..w19 are consumed, so the
        // string is `{{w20}}` — w20's value ("{{w21}}") is never substituted.
        assert_eq!(result, "{{w20}}");
    }

    #[test]
    fn test_url_deep_no_escape_of_nested_template() {
        // Backlog line 135: URL escaping applied on EVERY deep pass turned
        // `https://{{host}}` into `https://%7B%7Bhost%7D%7D` before the inner
        // placeholder could resolve. A value that is itself a template must
        // be inserted raw; only the FINAL resolved value is escaped.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([
                ("base_url".into(), "https://{{host}}".into()),
                ("host".into(), "api.example.com".into()),
            ]),
            ..Default::default()
        };
        let result = resolver.resolve_url_deep("{{base_url}}/users?x=1", &scope, 5);
        assert_eq!(result, "https://api.example.com/users?x=1");
    }

    #[test]
    fn test_url_deep_keeps_structural_urls() {
        // Backlog line 136 via the deep path: `{{endpoint}}` resolving to a
        // full URL keeps its query string instead of `search%3Fa%3D1`.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([("endpoint".into(), "https://api.test/search?a=1".into())]),
            ..Default::default()
        };
        let result = resolver.resolve_url_deep("{{endpoint}}", &scope, 5);
        assert_eq!(result, "https://api.test/search?a=1");
    }

    #[test]
    fn test_json_fragment_variable_stays_raw() {
        // Backlog line 135: blanket JSON escaping corrupted bare JSON
        // fragments — `{"filter": {{filterJson}}}` became invalid JSON. A
        // placeholder OUTSIDE a string literal must be inserted raw; only
        // placeholders INSIDE a string get value-escaped.
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([(
                "filterJson".into(),
                r#"{"status": 200, "tags": ["a"]}"#.into(),
            )]),
            ..Default::default()
        };
        let result = resolver.resolve_json_deep(r#"{"filter": {{filterJson}}}"#, &scope, 5);
        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");
        assert_eq!(parsed["filter"]["status"], 200);
    }

    #[test]
    fn test_json_escape_still_applies_inside_string() {
        // The string-context escape must survive the fix: a value with a
        // quote substituted INSIDE a JSON string literal is still escaped
        // (three quotes precede the placeholder → inside a string).
        let resolver = VariableResolver::new();
        let scope = VariableScope {
            env: HashMap::from([("name".into(), "he said \"hi\"".into())]),
            ..Default::default()
        };
        let result = resolver.resolve_json_deep(r#"{"s":"{{name}}"}"#, &scope, 5);
        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");
        assert_eq!(parsed["s"], "he said \"hi\"");
    }
}
