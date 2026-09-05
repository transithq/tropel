//! The declarative assertion vocabulary (KT-303, tropel ask #16).
//!
//! # Why this table is DATA and not a `match`
//!
//! KnockPort's `packages/engine/src/assertions.ts` holds 28 operators, and its
//! editor renders a dropdown from the same table it evaluates with — arity
//! decides whether the "expected" input is shown at all, and `summary` is the
//! failure-message wording. Porting only the evaluator would leave the editor
//! reading a SECOND copy of the vocabulary, which is the divergence this
//! workstream exists to remove: an operator added on one side and not the
//! other is a dropdown entry that always fails, or a rule the UI cannot offer.
//!
//! So the table is serde-serializable crate data. One list, consumed by the
//! evaluator here and by the editor across the wasm boundary.
//!
//! # Scope of THIS module
//!
//! The vocabulary and its shape. The predicates themselves are the next slice
//! — see the note on [`AssertionOperator::arity`] about why arity had to come
//! first.

use serde::{Deserialize, Serialize};

/// Whether an operator compares against an expected value.
///
/// Load-bearing beyond evaluation: the editor hides the "expected" input for
/// a unary operator, and a mismatch here is a form that asks for a value the
/// evaluator will ignore, or omits one it requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssertionArity {
    /// Takes no expected value: `isEmpty`, `isNull`, …
    Unary,
    /// Compares against one: `eq`, `contains`, …
    Binary,
}

/// One row of the vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionOperator {
    /// The wire name, exactly as it appears in a collection's `assert:` block.
    /// camelCase because that is what is already written in user files —
    /// renaming it would be a data migration, not a refactor.
    pub name: &'static str,
    pub arity: AssertionArity,
    /// One-line wording used by failure messages AND the editor hint. Kept
    /// byte-identical to KnockPort's so migrating the evaluator does not
    /// silently reword every existing failure message.
    pub summary: &'static str,
}

/// The 28 operators, in KnockPort's declaration order.
///
/// Order is part of the contract: the editor's dropdown renders it directly,
/// so re-sorting alphabetically here would reorder every user's menu.
pub static ASSERTION_OPERATORS: &[AssertionOperator] = &[
    op("eq", AssertionArity::Binary, "equals"),
    op("neq", AssertionArity::Binary, "does not equal"),
    op("gt", AssertionArity::Binary, "greater than"),
    op("gte", AssertionArity::Binary, "greater than or equal"),
    op("lt", AssertionArity::Binary, "less than"),
    op("lte", AssertionArity::Binary, "less than or equal"),
    op("in", AssertionArity::Binary, "is one of"),
    op("notIn", AssertionArity::Binary, "is not one of"),
    op("contains", AssertionArity::Binary, "contains"),
    op("notContains", AssertionArity::Binary, "does not contain"),
    op("length", AssertionArity::Binary, "has length"),
    op("matches", AssertionArity::Binary, "matches the regex"),
    op(
        "notMatches",
        AssertionArity::Binary,
        "does not match the regex",
    ),
    op("startsWith", AssertionArity::Binary, "starts with"),
    op("endsWith", AssertionArity::Binary, "ends with"),
    op("between", AssertionArity::Binary, "is between"),
    op("isEmpty", AssertionArity::Unary, "is empty"),
    op("isNotEmpty", AssertionArity::Unary, "is not empty"),
    op("isNull", AssertionArity::Unary, "is null"),
    op("isUndefined", AssertionArity::Unary, "is undefined"),
    op("isDefined", AssertionArity::Unary, "is defined"),
    op("isTruthy", AssertionArity::Unary, "is truthy"),
    op("isFalsy", AssertionArity::Unary, "is falsy"),
    op("isJson", AssertionArity::Unary, "is parseable JSON"),
    op("isNumber", AssertionArity::Unary, "is a number"),
    op("isString", AssertionArity::Unary, "is a string"),
    op("isBoolean", AssertionArity::Unary, "is a boolean"),
    op("isArray", AssertionArity::Unary, "is an array"),
];

const fn op(name: &'static str, arity: AssertionArity, summary: &'static str) -> AssertionOperator {
    AssertionOperator {
        name,
        arity,
        summary,
    }
}

/// Look an operator up by its wire name.
///
/// Returns `None` for anything unknown — the caller REJECTS BY NAME rather
/// than falling back to `eq`. A silent fallback is the TR-004/TR-409 failure
/// shape: an assertion that reports "passed" while testing something the
/// author never wrote.
pub fn assertion_operator(name: &str) -> Option<&'static AssertionOperator> {
    ASSERTION_OPERATORS.iter().find(|o| o.name == name)
}

// ── Evaluation ──────────────────────────────────────────────────────────────
//
// Values are `serde_json::Value` because that is the shape a response already
// has on both paths — the load runtime's parsed body and the browser tier's
// JSON bridge.
//
// # Two callers, one evaluator
//
// KnockPort collections will run as LOAD TESTS, so this is evaluated in two
// very different places:
//
//   one Send      in wasm, once, latency irrelevant
//   a load run    natively, per-VU, per-request, at high RPS
//
// That is why every predicate is synchronous, allocation-light on the common
// paths, and CANNOT panic: a panic here aborts a VU mid-iteration, and a
// silent `false` is a check that reports failure it did not observe. The
// `unwrap`-free style below is deliberate, not defensive habit.
//
// # Why the regex matcher is INJECTED
//
// `matches` / `notMatches` need a regex, and linking one here would undo
// TR-434 — `regex` was 152 KB of the eager wasm tier and removing it halved
// the artifact. Linking Rust's `regex` would ALSO be unfaithful: KnockPort's
// operator uses JS `RegExp`, which has backreferences and lookaround that
// Rust's engine deliberately does not.
//
// So the capability is passed in, the same way `ExecWasmOptions.transport`
// passes in network access the wasm cannot have. The browser tier supplies
// the host's `RegExp` (bit-identical to today's behaviour); the load runtime
// supplies whatever engine it already links. Neither pays for the other.

use serde_json::Value;

/// Supplies regex matching for `matches` / `notMatches`.
///
/// Absent, those two operators evaluate to `false` and
/// [`AssertionOutcome::unsupported`] is set, so a caller that forgot to wire
/// one gets a NAMED reason rather than a silently failing assertion.
pub trait RegexMatcher {
    /// True when `pattern` matches `haystack`. Must not panic; an invalid
    /// pattern is `false`.
    fn is_match(&self, pattern: &str, haystack: &str) -> bool;
}

impl<F: Fn(&str, &str) -> bool> RegexMatcher for F {
    fn is_match(&self, pattern: &str, haystack: &str) -> bool {
        self(pattern, haystack)
    }
}

/// The result of evaluating one assertion.
///
/// Carries `name` so a LOAD RUN can aggregate outcomes the way k6 aggregates
/// checks — pass/fail counts grouped by assertion, not one row per request.
/// Without the name in the outcome the aggregator would have to correlate by
/// index, which breaks the moment a conditional assertion is skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionOutcome {
    pub name: String,
    pub passed: bool,
    /// Set when the assertion could not be evaluated at all — an unknown
    /// operator, or `matches` with no matcher wired. Distinct from
    /// `passed: false`, which means the predicate ran and said no.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
    /// Why it failed, worded for a human. Present only on a FAILING row — a
    /// passing assertion needs no explanation, and this is emitted per
    /// assertion per request in a load run.
    ///
    /// TR-443: built HERE, not by the caller. The caller does not have the
    /// resolved `actual` value — only this function does — and the app and a
    /// load report must word the same failure the same way. Two formatters
    /// would drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A short, safe rendering of a value for a failure message.
///
/// Long strings are truncated: a failed assertion against a 2 MB body must not
/// put 2 MB into every result row of a load run.
fn preview(value: &Value) -> String {
    match value {
        Value::String(s) if s.chars().count() > 120 => {
            let head: String = s.chars().take(117).collect();
            Value::String(format!("{head}...")).to_string()
        }
        other => other.to_string(),
    }
}

/// Numeric coercion: both sides must be numbers, or numeric strings.
///
/// Strings included because YAML scalars and response headers are text — a
/// `content-length` header compared against `1024` must work.
fn numeric_pair(a: &Value, b: &Value) -> Option<(f64, f64)> {
    fn to_num(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64().filter(|f| !f.is_nan()),
            Value::String(s) if !s.trim().is_empty() => {
                s.trim().parse::<f64>().ok().filter(|f| !f.is_nan())
            }
            _ => None,
        }
    }
    Some((to_num(a)?, to_num(b)?))
}

/// Key-sorted JSON, so two structurally equal objects compare equal
/// regardless of key order.
fn stable_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let pairs: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", Value::String((*k).clone()), stable_json(&map[*k])))
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(stable_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn try_parse_json(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) => serde_json::from_str(s).ok(),
        _ => None,
    }
}

/// KnockPort's `looseEqual`, ported exactly.
///
/// The order of the arms is load-bearing: numeric coercion runs BEFORE the
/// structural comparison, so `200 == "200"` is true while `true == "true"` is
/// false (neither side is numeric, neither is an object).
fn loose_equal(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    if let Some((x, y)) = numeric_pair(a, b) {
        return x == y;
    }
    if a.is_object() || a.is_array() || b.is_object() || b.is_array() {
        let pa = try_parse_json(a);
        let pb = try_parse_json(b);
        return match (pa, pb) {
            (Some(x), Some(y)) => stable_json(&x) == stable_json(&y),
            (Some(x), None) => stable_json(&x) == stable_json(b),
            (None, Some(y)) => stable_json(a) == stable_json(&y),
            (None, None) => stable_json(a) == stable_json(b),
        };
    }
    false
}

/// Length of a string, array or object — `None` for anything else.
fn length_of(v: &Value) -> Option<usize> {
    match v {
        Value::String(s) => Some(s.chars().count()),
        Value::Array(a) => Some(a.len()),
        Value::Object(o) => Some(o.len()),
        _ => None,
    }
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Truthiness, JS-style: `0`, `""`, `null`, `false` are falsy. An empty array
/// or object is TRUTHY in JS, which surprises people — pinned by a test.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `contains`: substring for strings, membership for arrays, key for objects.
fn contains_value(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::String(s) => s.contains(&as_string(needle)),
        Value::Array(items) => items.iter().any(|i| loose_equal(i, needle)),
        Value::Object(map) => map.contains_key(&as_string(needle)),
        _ => false,
    }
}

/// String coercion matching JS `String(x ?? "")` for the shapes that reach
/// here: a JSON string yields its contents, NOT a quoted literal.
fn as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Evaluate one assertion.
///
/// `name` and `target` are DIFFERENT and both are needed. `name` is the
/// assertion's identity — its description, used to aggregate outcomes across a
/// load run — while `target` is what was actually checked (`status`,
/// `json.items.0.name`) and is what a failure message must name. Wording a
/// failure with the description produces "expected target should fail equals
/// 500", which says nothing about what was inspected.
///
/// Never panics, never allocates for the unary paths, and returns a named
/// `unsupported` rather than a bare `false` when it cannot evaluate at all.
pub fn assert_evaluate(
    name: &str,
    target: &str,
    actual: &Value,
    operator: &str,
    expected: &Value,
    matcher: Option<&dyn RegexMatcher>,
) -> AssertionOutcome {
    let Some(op) = assertion_operator(operator) else {
        // Rejected BY NAME. Falling back to `eq` would make an assertion
        // report a verdict on something the author never wrote.
        return AssertionOutcome {
            name: name.to_string(),
            passed: false,
            message: None,
            unsupported: Some(format!(
                "unknown assertion operator '{operator}' — supported: {}",
                ASSERTION_OPERATORS
                    .iter()
                    .map(|o| o.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    };

    let mut unsupported = None;
    let passed = match op.name {
        "eq" => loose_equal(actual, expected),
        "neq" => !loose_equal(actual, expected),
        "gt" => numeric_pair(actual, expected).is_some_and(|(a, b)| a > b),
        "gte" => numeric_pair(actual, expected).is_some_and(|(a, b)| a >= b),
        "lt" => numeric_pair(actual, expected).is_some_and(|(a, b)| a < b),
        "lte" => numeric_pair(actual, expected).is_some_and(|(a, b)| a <= b),
        "in" => match expected {
            Value::Array(items) => items.iter().any(|i| loose_equal(actual, i)),
            _ => false,
        },
        "notIn" => match expected {
            Value::Array(items) => !items.iter().any(|i| loose_equal(actual, i)),
            _ => false,
        },
        "contains" => contains_value(actual, expected),
        "notContains" => !contains_value(actual, expected),
        "length" => match length_of(actual) {
            Some(len) => numeric_pair(&Value::from(len), expected).is_some_and(|(a, b)| a == b),
            None => false,
        },
        "matches" | "notMatches" => match matcher {
            Some(m) => {
                let hit = m.is_match(&as_string(expected), &as_string(actual));
                if op.name == "matches" {
                    hit
                } else {
                    !hit
                }
            }
            None => {
                unsupported = Some(format!(
                    "'{}' needs a regex matcher and none was supplied — the host \
                     engine is injected so this crate does not link one (TR-434); \
                     pass one to assert_evaluate",
                    op.name
                ));
                false
            }
        },
        "startsWith" => as_string(actual).starts_with(&as_string(expected)),
        "endsWith" => as_string(actual).ends_with(&as_string(expected)),
        "between" => match expected {
            Value::Array(range) if range.len() == 2 => {
                match (
                    numeric_pair(actual, &range[0]),
                    numeric_pair(actual, &range[1]),
                ) {
                    (Some((a, lo)), Some((_, hi))) => {
                        // The bounds may be given either way round.
                        a >= lo.min(hi) && a <= lo.max(hi)
                    }
                    _ => false,
                }
            }
            _ => false,
        },
        "isEmpty" => is_empty(actual),
        "isNotEmpty" => !is_empty(actual),
        "isNull" => actual.is_null(),
        // JSON has no `undefined`; an absent target resolves to Null, so the
        // two operators agree here. Both are kept because collections already
        // contain both spellings.
        "isUndefined" => actual.is_null(),
        "isDefined" => !actual.is_null(),
        "isTruthy" => is_truthy(actual),
        "isFalsy" => !is_truthy(actual),
        "isJson" => actual.is_object() || actual.is_array() || try_parse_json(actual).is_some(),
        "isNumber" => numeric_pair(actual, actual).is_some(),
        "isString" => actual.is_string(),
        "isBoolean" => actual.is_boolean(),
        "isArray" => actual.is_array(),
        // Unreachable: `assertion_operator` only returns rows from the table
        // above, and every row is handled. A `_ => false` would silently pass
        // a newly-added operator as "failed" instead of failing to compile.
        other => {
            unsupported = Some(format!(
                "operator '{other}' is in the table but not evaluated"
            ));
            false
        }
    };

    // The wording matches what KnockPort's TypeScript evaluator produced, so
    // migrating does not silently reword every existing failure.
    let message = if passed || unsupported.is_some() {
        None
    } else if op.arity == AssertionArity::Unary {
        Some(format!(
            "expected target {target} {}; actual {}",
            op.summary,
            preview(actual)
        ))
    } else {
        Some(format!(
            "expected target {target} {} {}; actual {}",
            op.summary,
            preview(expected),
            preview(actual)
        ))
    };

    AssertionOutcome {
        name: name.to_string(),
        passed,
        unsupported,
        message,
    }
}

// ── Target resolution ───────────────────────────────────────────────────────
//
// The documented vocabulary, ported from KnockPort's `resolveAssertionTarget`:
//
//   status / statusText / responseTime / size / body   response members
//   response.<member>                                  same, explicit
//   json / json.path.to.field / json.items.0.name      dotted JSON descent
//   header("Name") / headers.Name / headers["Name"]    case-insensitive header
//   cookie("name")                                     response cookie value
//   jsonpath("$.store.bicycle.color")                  bounded JSONPath subset
//
// Anything else is an ERROR NAMING THE TARGET. Never a silent value: a typo'd
// target that resolved to null would make `isEmpty` pass and read as a green
// assertion about a field that does not exist.
//
// Parsed by hand rather than with `regex`. TR-434 removed that dependency —
// it was 152 KB of the eager wasm tier — and these forms are fixed shapes, the
// same argument that retired the 37 catalogue patterns.

/// The response members an assertion can target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionTarget {
    pub status: i64,
    pub status_text: String,
    /// As received. Lookup is case-insensitive, so the case here is preserved
    /// for messages rather than normalised away.
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub response_time: f64,
    pub size: i64,
    pub cookies: Vec<(String, String)>,
}

impl AssertionTarget {
    /// The parsed body, or `None` when it is not JSON.
    ///
    /// Parsed ON DEMAND rather than stored: in a load run this struct is built
    /// per request, and most assertions never touch the body — parsing every
    /// response eagerly would be work done per-VU for nothing.
    fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    fn header(&self, name: &str) -> Result<Value, String> {
        let want = name.trim().to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == want)
            .map(|(_, v)| Value::String(v.clone()))
            .ok_or_else(|| format!("header \"{name}\" was not present on the response"))
    }

    fn cookie(&self, name: &str) -> Result<Value, String> {
        self.cookies
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| Value::String(v.clone()))
            .ok_or_else(|| format!("cookie \"{name}\" was not set by this response"))
    }
}

/// `name("arg")` / `name('arg')` → the argument, if `raw` has exactly that
/// shape for one of `names`.
fn call_arg<'a>(raw: &'a str, names: &[&str]) -> Option<(&'a str, &'a str)> {
    let open = raw.find('(')?;
    if !raw.ends_with(')') {
        return None;
    }
    let head = raw[..open].trim();
    if !names.contains(&head) {
        return None;
    }
    let inner = raw[open + 1..raw.len() - 1].trim();
    let quote = inner.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    if inner.len() < 2 || !inner.ends_with(quote) {
        return None;
    }
    Some((head, &inner[1..inner.len() - 1]))
}

/// One step of the bounded JSONPath subset.
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// Lex `$.a.b[0]['c']` into steps.
///
/// Bounded ON PURPOSE: no filters, wildcards or recursive descent. Those are
/// reported as unsupported rather than guessed at — a wildcard that silently
/// matched the first element would make an assertion pass on the wrong item.
fn lex_json_path(path: &str) -> Result<Vec<Step<'_>>, String> {
    let mut steps = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0;
    if bytes.first() == Some(&b'$') {
        i = 1;
    }
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                if bytes.get(i + 1) == Some(&b'.') {
                    return Err(format!(
                        "path \"{path}\": recursive descent (..) is not supported"
                    ));
                }
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'.' && bytes[j] != b'[' {
                    j += 1;
                }
                if j == start {
                    return Err(format!("path \"{path}\": empty step after '.'"));
                }
                let key = &path[start..j];
                if key == "*" {
                    return Err(format!("path \"{path}\": wildcards are not supported"));
                }
                steps.push(Step::Key(key));
                i = j;
            }
            b'[' => {
                let close = path[i..]
                    .find(']')
                    .ok_or_else(|| format!("path \"{path}\": unclosed '['"))?
                    + i;
                let inner = path[i + 1..close].trim();
                if inner == "*" {
                    return Err(format!("path \"{path}\": wildcards are not supported"));
                }
                let first = inner.chars().next();
                if first == Some('\'') || first == Some('"') {
                    let q = first.unwrap();
                    if inner.len() < 2 || !inner.ends_with(q) {
                        return Err(format!("path \"{path}\": unterminated quoted key"));
                    }
                    steps.push(Step::Key(&inner[1..inner.len() - 1]));
                } else {
                    let idx: usize = inner
                        .parse()
                        .map_err(|_| format!("path \"{path}\": \"{inner}\" is not an index"))?;
                    steps.push(Step::Index(idx));
                }
                i = close + 1;
            }
            _ => return Err(format!("path \"{path}\": unexpected character at {i}")),
        }
    }
    Ok(steps)
}

fn json_path_lookup(root: &Value, path: &str) -> Result<Value, String> {
    let mut current = root;
    for step in lex_json_path(path)? {
        match step {
            Step::Index(idx) => {
                let arr = current
                    .as_array()
                    .ok_or_else(|| format!("path \"{path}\": expected an array at [{idx}]"))?;
                current = arr
                    .get(idx)
                    .ok_or_else(|| format!("path \"{path}\": index {idx} is outside the array"))?;
            }
            Step::Key(key) => {
                // `json.items.0.name` reaches here as a KEY step, because
                // KnockPort's lexer emits a key for every dot-segment. It
                // works there because a JS array is an object whose keys are
                // numeric strings — `"0" in arr` is true. Ported exactly:
                // a numeric key against an array indexes it.
                if let Some(arr) = current.as_array() {
                    if let Ok(idx) = key.parse::<usize>() {
                        current = arr.get(idx).ok_or_else(|| {
                            format!("path \"{path}\": index {idx} is outside the array")
                        })?;
                        continue;
                    }
                }
                let obj = current
                    .as_object()
                    .ok_or_else(|| format!("path \"{path}\": expected an object at \"{key}\""))?;
                current = obj
                    .get(key)
                    .ok_or_else(|| format!("path \"{path}\": key \"{key}\" is absent"))?;
            }
        }
    }
    Ok(current.clone())
}

/// Resolve a target expression against the response.
///
/// `Err` names the target. It is deliberately NOT `Ok(Null)`: a typo'd target
/// resolving to null would make `isEmpty` pass and read as a green assertion
/// about a field that does not exist.
pub fn resolve_assertion_target(target: &str, ctx: &AssertionTarget) -> Result<Value, String> {
    let raw = target.trim();

    if let Some((_, path)) = call_arg(raw, &["jsonpath"]) {
        let json = ctx
            .json()
            .ok_or_else(|| format!("jsonpath target \"{path}\": the response body is not JSON"))?;
        return json_path_lookup(&json, path);
    }
    if let Some((_, name)) = call_arg(raw, &["header", "headers"]) {
        return ctx.header(name);
    }
    if let Some((_, name)) = call_arg(raw, &["cookie"]) {
        return ctx.cookie(name);
    }

    let expr = raw.strip_prefix("response.").unwrap_or(raw);
    match expr {
        "status" => return Ok(Value::from(ctx.status)),
        "statusText" => return Ok(Value::String(ctx.status_text.clone())),
        "responseTime" => return Ok(Value::from(ctx.response_time)),
        "size" => return Ok(Value::from(ctx.size)),
        "body" => return Ok(Value::String(ctx.body.clone())),
        "json" => return Ok(ctx.json().unwrap_or(Value::Null)),
        _ => {}
    }

    if let Some(rest) = expr
        .strip_prefix("json.")
        .map(|r| format!(".{r}"))
        .or_else(|| expr.strip_prefix("json[").map(|r| format!("[{r}")))
    {
        let json = ctx
            .json()
            .ok_or_else(|| format!("target \"{raw}\": the response body is not JSON"))?;
        return json_path_lookup(&json, &rest);
    }

    // `headers.Name` / `headers["Name"]`
    for prefix in ["headers", "header"] {
        if let Some(rest) = expr.strip_prefix(prefix) {
            if let Some(name) = rest.strip_prefix('.') {
                if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return ctx.header(name);
                }
            }
            if rest.starts_with('[') && rest.ends_with(']') {
                let inner = rest[1..rest.len() - 1].trim();
                let q = inner.chars().next();
                if (q == Some('"') || q == Some('\'')) && inner.len() >= 2 {
                    return ctx.header(&inner[1..inner.len() - 1]);
                }
            }
        }
    }

    // A bare word that is not a response member is most likely a header name.
    if !expr.is_empty()
        && expr
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && expr
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return ctx.header(expr);
    }

    Err(format!(
        "unparseable target \"{raw}\" — expected status, statusText, responseTime, size, body, \
         json, json.<path>, header(\"Name\"), cookie(\"name\") or jsonpath(\"$...\")"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_is_exactly_knockports() {
        // This list is transcribed from packages/engine/src/assertions.ts's
        // `AssertionOperator` union. Pinning it here means an operator added
        // on one side and not the other fails a test instead of shipping as
        // a dropdown entry that always fails, or a rule the UI cannot offer.
        let expected = [
            "eq",
            "neq",
            "gt",
            "gte",
            "lt",
            "lte",
            "in",
            "notIn",
            "contains",
            "notContains",
            "length",
            "matches",
            "notMatches",
            "startsWith",
            "endsWith",
            "between",
            "isEmpty",
            "isNotEmpty",
            "isNull",
            "isUndefined",
            "isDefined",
            "isTruthy",
            "isFalsy",
            "isJson",
            "isNumber",
            "isString",
            "isBoolean",
            "isArray",
        ];
        let actual: Vec<&str> = ASSERTION_OPERATORS.iter().map(|o| o.name).collect();
        assert_eq!(actual, expected, "the operator set or its ORDER changed");
        assert_eq!(ASSERTION_OPERATORS.len(), 28);
    }

    #[test]
    fn arity_matches_knockports_table() {
        // A wrong arity is not a cosmetic bug: the editor hides the expected
        // input for a unary operator, so flipping one produces a form that
        // asks for a value the evaluator ignores — or omits one it needs.
        let unary: Vec<&str> = ASSERTION_OPERATORS
            .iter()
            .filter(|o| o.arity == AssertionArity::Unary)
            .map(|o| o.name)
            .collect();
        assert_eq!(
            unary,
            [
                "isEmpty",
                "isNotEmpty",
                "isNull",
                "isUndefined",
                "isDefined",
                "isTruthy",
                "isFalsy",
                "isJson",
                "isNumber",
                "isString",
                "isBoolean",
                "isArray"
            ]
        );
        assert_eq!(unary.len(), 12);
        assert_eq!(ASSERTION_OPERATORS.len() - unary.len(), 16);
    }

    #[test]
    fn names_are_unique_so_lookup_is_unambiguous() {
        let mut seen = std::collections::HashSet::new();
        for o in ASSERTION_OPERATORS {
            assert!(seen.insert(o.name), "duplicate operator {}", o.name);
        }
    }

    #[test]
    fn unknown_operators_are_rejected_by_name_not_defaulted() {
        // The failure this prevents: falling back to `eq` makes an assertion
        // report "passed" while testing something the author never wrote.
        assert!(assertion_operator("eq").is_some());
        assert!(assertion_operator("isArray").is_some());
        for unknown in ["equals", "EQ", "==", "", "notAnOperator", "isarray"] {
            assert!(
                assertion_operator(unknown).is_none(),
                "{unknown} must not resolve"
            );
        }
    }

    #[test]
    fn every_row_serialises_for_the_editor() {
        // The editor consumes this table across the wasm boundary; a row that
        // cannot serialise is a dropdown that silently loses an entry.
        let json = serde_json::to_string(ASSERTION_OPERATORS).expect("table serialises");
        assert!(json.contains(r#""name":"eq""#), "{json}");
        assert!(json.contains(r#""arity":"binary""#));
        assert!(json.contains(r#""arity":"unary""#));
        assert!(json.contains(r#""summary":"is parseable JSON""#));
        // Deserialised as `Value`, not back into `AssertionOperator`: the
        // struct borrows `&'static str`, so it can only be rebuilt from a
        // leaked buffer. What matters to the editor is the SHAPE of each row,
        // and that is what this checks.
        let back: serde_json::Value = serde_json::from_str(&json).expect("table round-trips");
        let rows = back.as_array().expect("an array of rows");
        assert_eq!(rows.len(), ASSERTION_OPERATORS.len());
        for (row, expected) in rows.iter().zip(ASSERTION_OPERATORS) {
            assert_eq!(row["name"], expected.name);
            assert_eq!(row["summary"], expected.summary);
            assert!(row["arity"].is_string());
        }
    }

    #[test]
    fn summaries_are_present_and_lowercase_phrases() {
        // They are used mid-sentence in a failure message ("status equals
        // 200"), so a capitalised or empty summary reads wrong there.
        for o in ASSERTION_OPERATORS {
            assert!(!o.summary.is_empty(), "{} has no summary", o.name);
            let first = o.summary.chars().next().unwrap();
            assert!(
                first.is_lowercase(),
                "{}'s summary should read mid-sentence: {:?}",
                o.name,
                o.summary
            );
        }
    }
    use serde_json::json;

    fn ev(actual: Value, op: &str, expected: Value) -> AssertionOutcome {
        assert_evaluate("t", "t", &actual, op, &expected, None)
    }
    fn ok(actual: Value, op: &str, expected: Value) -> bool {
        let o = ev(actual, op, expected);
        assert!(o.unsupported.is_none(), "{:?}", o.unsupported);
        o.passed
    }

    #[test]
    fn numeric_coercion_crosses_string_and_number_but_not_bool() {
        // Response headers and YAML scalars are TEXT, so `content-length`
        // against 1024 must work. Booleans must NOT coerce: `true == "true"`
        // is false in KnockPort, and quietly making it true would flip
        // assertions that currently fail.
        assert!(ok(json!(200), "eq", json!("200")));
        assert!(ok(json!("200"), "eq", json!(200)));
        assert!(
            ok(json!(" 200 "), "eq", json!(200)),
            "headers carry padding"
        );
        assert!(!ok(json!(true), "eq", json!("true")));
        assert!(ok(json!(true), "eq", json!(true)));
        assert!(
            !ok(json!("abc"), "gt", json!(1)),
            "non-numeric never compares"
        );
        assert!(ok(json!("3"), "gt", json!("2")));
    }

    #[test]
    fn object_equality_ignores_key_order_and_sees_through_json_strings() {
        assert!(ok(json!({"a":1,"b":2}), "eq", json!({"b":2,"a":1})));
        // A response body arrives as a STRING; the expected value is written
        // as YAML structure. Comparing them must succeed.
        assert!(ok(json!(r#"{"a":1}"#), "eq", json!({"a":1})));
        assert!(ok(json!({"a":1}), "eq", json!(r#"{"a":1}"#)));
        assert!(!ok(json!({"a":1}), "eq", json!({"a":2})));
        assert!(ok(json!([1, 2]), "eq", json!([1, 2])));
        assert!(
            !ok(json!([1, 2]), "eq", json!([2, 1])),
            "arrays are ordered"
        );
    }

    #[test]
    fn null_never_equals_anything_including_empty() {
        // `null == ""` being true would make "field is absent" and "field is
        // blank" indistinguishable, which is exactly the bug an assertion is
        // meant to catch.
        assert!(!ok(json!(null), "eq", json!("")));
        assert!(!ok(json!(null), "eq", json!(0)));
        assert!(!ok(json!(null), "eq", json!(false)));
        assert!(ok(json!(null), "isNull", json!(null)));
        assert!(ok(json!(null), "isEmpty", json!(null)));
    }

    #[test]
    fn contains_means_substring_membership_or_key() {
        assert!(ok(json!("hello world"), "contains", json!("lo wo")));
        assert!(
            ok(json!([1, 2, 3]), "contains", json!("2")),
            "loose membership"
        );
        assert!(ok(json!({"a": 1}), "contains", json!("a")), "object = key");
        assert!(!ok(json!({"a": 1}), "contains", json!(1)), "not by value");
        assert!(
            ok(json!(42), "notContains", json!("4")),
            "numbers contain nothing"
        );
    }

    #[test]
    fn length_counts_chars_items_and_keys() {
        assert!(ok(json!("abc"), "length", json!(3)));
        assert!(ok(json!("héllo"), "length", json!(5)), "chars, not bytes");
        assert!(ok(json!([1, 2]), "length", json!("2")));
        assert!(ok(json!({"a":1,"b":2}), "length", json!(2)));
        assert!(
            !ok(json!(123), "length", json!(3)),
            "a number has no length"
        );
    }

    #[test]
    fn between_accepts_bounds_in_either_order_and_is_inclusive() {
        assert!(ok(json!(5), "between", json!([1, 10])));
        assert!(ok(json!(5), "between", json!([10, 1])), "reversed bounds");
        assert!(ok(json!(1), "between", json!([1, 10])), "inclusive low");
        assert!(ok(json!(10), "between", json!([1, 10])), "inclusive high");
        assert!(!ok(json!(11), "between", json!([1, 10])));
        assert!(!ok(json!(5), "between", json!([1])), "needs exactly two");
        assert!(!ok(json!(5), "between", json!(1)), "needs an array");
    }

    #[test]
    fn truthiness_follows_js_including_the_surprising_cases() {
        // An empty array/object is TRUTHY in JS. People expect otherwise, so
        // pin it — silently "fixing" it would diverge from the TypeScript.
        assert!(ok(json!([]), "isTruthy", json!(null)));
        assert!(ok(json!({}), "isTruthy", json!(null)));
        // …while isEmpty says the opposite about the same value. Both are
        // correct; they answer different questions.
        assert!(ok(json!([]), "isEmpty", json!(null)));
        assert!(ok(json!(0), "isFalsy", json!(null)));
        assert!(ok(json!(""), "isFalsy", json!(null)));
        assert!(ok(json!(false), "isFalsy", json!(null)));
    }

    #[test]
    fn type_predicates_do_not_coerce() {
        assert!(ok(json!("5"), "isString", json!(null)));
        // A numeric STRING is "a number" here, because numeric_pair coerces
        // it — headers are text. isString is also true; they are not exclusive.
        assert!(ok(json!("5"), "isNumber", json!(null)));
        assert!(ok(json!(5), "isNumber", json!(null)));
        assert!(!ok(json!(5), "isString", json!(null)));
        assert!(ok(json!(true), "isBoolean", json!(null)));
        assert!(ok(json!([1]), "isArray", json!(null)));
        assert!(!ok(json!({"a":1}), "isArray", json!(null)));
        assert!(ok(json!(r#"{"a":1}"#), "isJson", json!(null)));
        assert!(!ok(json!("not json"), "isJson", json!(null)));
    }

    #[test]
    fn matches_without_a_matcher_is_unsupported_not_a_silent_failure() {
        // TR-434 removed `regex` from this crate; the host engine is injected.
        // A caller that forgets must get a NAMED reason, because a bare
        // `false` reads as "the assertion ran and the body did not match".
        let o = ev(json!("abc"), "matches", json!("^a"));
        assert!(!o.passed);
        let why = o.unsupported.expect("a named reason");
        assert!(why.contains("regex matcher"), "{why}");
        assert!(why.contains("TR-434"), "{why}");
    }

    #[test]
    fn matches_uses_the_injected_matcher_for_both_polarities() {
        let m = |pattern: &str, hay: &str| pattern == "^a" && hay.starts_with('a');
        let hit = assert_evaluate("t", "t", &json!("abc"), "matches", &json!("^a"), Some(&m));
        assert!(hit.passed && hit.unsupported.is_none());
        let miss = assert_evaluate("t", "t", &json!("xyz"), "matches", &json!("^a"), Some(&m));
        assert!(!miss.passed && miss.unsupported.is_none());
        let neg = assert_evaluate(
            "t",
            "t",
            &json!("xyz"),
            "notMatches",
            &json!("^a"),
            Some(&m),
        );
        assert!(neg.passed, "notMatches inverts the same matcher");
    }

    #[test]
    fn an_unknown_operator_is_refused_by_name_and_lists_the_alternatives() {
        let o = ev(json!(1), "equals", json!(1));
        assert!(!o.passed);
        let why = o.unsupported.expect("a named reason");
        assert!(why.contains("unknown assertion operator 'equals'"), "{why}");
        assert!(why.contains("eq"), "it lists what IS supported: {why}");
    }

    #[test]
    fn the_outcome_carries_the_name_so_a_load_run_can_aggregate_it() {
        // KnockPort collections will run as load tests, where outcomes are
        // aggregated like k6 checks — pass/fail counts grouped by assertion
        // name, not one row per request. Correlating by INDEX instead breaks
        // the moment a conditional assertion is skipped.
        let o = assert_evaluate(
            "status is 200",
            "status",
            &json!(200),
            "eq",
            &json!(200),
            None,
        );
        assert_eq!(o.name, "status is 200");
        assert!(o.passed);
        let json = serde_json::to_string(&o).expect("outcome serialises");
        assert!(json.contains(r#""name":"status is 200""#), "{json}");
        assert!(json.contains(r#""passed":true"#));
        // `unsupported` is omitted when absent, so the common path stays small
        // on the wire — this is emitted per assertion per request in a run.
        assert!(!json.contains("unsupported"), "{json}");
    }

    #[test]
    fn every_operator_in_the_table_evaluates_without_panicking() {
        // The evaluator must never panic: a panic aborts a VU mid-iteration.
        // Hostile-ish shapes against every operator, both arities.
        let shapes = [
            json!(null),
            json!(0),
            json!(-1.5),
            json!(""),
            json!("x"),
            json!(true),
            json!([]),
            json!([1, "a"]),
            json!({}),
            json!({"k": null}),
        ];
        for op in ASSERTION_OPERATORS {
            for a in &shapes {
                for b in &shapes {
                    let o = assert_evaluate("t", "t", a, op.name, b, None);
                    if op.name == "matches" || op.name == "notMatches" {
                        assert!(o.unsupported.is_some());
                    } else {
                        assert!(o.unsupported.is_none(), "{} {:?}", op.name, o.unsupported);
                    }
                }
            }
        }
    }
    fn ctx() -> AssertionTarget {
        AssertionTarget {
            status: 200,
            status_text: "OK".into(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("X-Request-Id".into(), "abc-123".into()),
            ],
            body: r#"{"items":[{"name":"first"},{"name":"second"}],"count":2,"ok":true}"#.into(),
            response_time: 12.5,
            size: 64,
            cookies: vec![("session".into(), "s1".into())],
        }
    }

    #[test]
    fn response_members_resolve_with_and_without_the_prefix() {
        let c = ctx();
        for (t, want) in [
            ("status", json!(200)),
            ("response.status", json!(200)),
            ("statusText", json!("OK")),
            ("responseTime", json!(12.5)),
            ("size", json!(64)),
        ] {
            assert_eq!(resolve_assertion_target(t, &c).unwrap(), want, "{t}");
        }
        assert_eq!(resolve_assertion_target("body", &c).unwrap(), json!(c.body));
        // Whitespace around a target is tolerated — YAML scalars carry it.
        assert_eq!(
            resolve_assertion_target("  status  ", &c).unwrap(),
            json!(200)
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive_in_every_form() {
        let c = ctx();
        for t in [
            "header(\"content-type\")",
            "headers('Content-Type')",
            "headers.Content-Type",
            "headers[\"CONTENT-TYPE\"]",
            "Content-Type",
        ] {
            assert_eq!(
                resolve_assertion_target(t, &c).unwrap(),
                json!("application/json"),
                "{t}"
            );
        }
    }

    #[test]
    fn an_absent_header_is_an_error_not_an_empty_value() {
        // The whole point: resolving to null would make `isEmpty` PASS and
        // read as a green assertion about a header that was never sent.
        let c = ctx();
        let e = resolve_assertion_target("header(\"X-Missing\")", &c).unwrap_err();
        assert!(e.contains("X-Missing"), "{e}");
        assert!(e.contains("not present"), "{e}");
    }

    #[test]
    fn json_descent_by_dot_and_index_and_jsonpath() {
        let c = ctx();
        assert_eq!(
            resolve_assertion_target("json.count", &c).unwrap(),
            json!(2)
        );
        assert_eq!(
            resolve_assertion_target("json.items.0.name", &c).unwrap(),
            json!("first")
        );
        assert_eq!(
            resolve_assertion_target("json[\"count\"]", &c).unwrap(),
            json!(2)
        );
        assert_eq!(
            resolve_assertion_target("jsonpath(\"$.items[1].name\")", &c).unwrap(),
            json!("second")
        );
        // A bare `json` yields the whole parsed body.
        assert!(resolve_assertion_target("json", &c).unwrap().is_object());
    }

    #[test]
    fn a_missing_key_or_out_of_range_index_names_the_step() {
        let c = ctx();
        let e = resolve_assertion_target("json.nope", &c).unwrap_err();
        assert!(e.contains("\"nope\" is absent"), "{e}");
        let e = resolve_assertion_target("json.items.9.name", &c).unwrap_err();
        assert!(e.contains("index 9 is outside"), "{e}");
        let e = resolve_assertion_target("json.count.deeper", &c).unwrap_err();
        assert!(e.contains("expected an object"), "{e}");
    }

    #[test]
    fn unsupported_jsonpath_features_are_refused_not_guessed() {
        // A wildcard that silently matched the FIRST element would make an
        // assertion pass on the wrong item — worse than refusing.
        let c = ctx();
        for (path, needle) in [
            ("jsonpath(\"$.items[*].name\")", "wildcard"),
            ("jsonpath(\"$..name\")", "recursive descent"),
        ] {
            let e = resolve_assertion_target(path, &c).unwrap_err();
            assert!(e.contains(needle), "{path}: {e}");
        }
    }

    #[test]
    fn a_non_json_body_is_an_error_naming_the_target() {
        let mut c = ctx();
        c.body = "plain text".into();
        let e = resolve_assertion_target("json.count", &c).unwrap_err();
        assert!(e.contains("not JSON"), "{e}");
        // …but `body` and the members still resolve.
        assert_eq!(
            resolve_assertion_target("body", &c).unwrap(),
            json!("plain text")
        );
        // and a bare `json` is Null rather than an error, matching KnockPort.
        assert_eq!(resolve_assertion_target("json", &c).unwrap(), json!(null));
    }

    #[test]
    fn cookies_resolve_by_exact_name_and_absence_is_an_error() {
        let c = ctx();
        assert_eq!(
            resolve_assertion_target("cookie(\"session\")", &c).unwrap(),
            json!("s1")
        );
        let e = resolve_assertion_target("cookie(\"nope\")", &c).unwrap_err();
        assert!(e.contains("was not set by this response"), "{e}");
    }

    #[test]
    fn an_unparseable_target_lists_the_vocabulary() {
        let c = ctx();
        for bad in ["", "1abc", "json..x", "header(unquoted)", "!!"] {
            let e = resolve_assertion_target(bad, &c).unwrap_err();
            assert!(!e.is_empty(), "{bad}");
        }
        let e = resolve_assertion_target("!!", &c).unwrap_err();
        assert!(e.contains("unparseable target"), "{e}");
        assert!(e.contains("jsonpath"), "it names the alternatives: {e}");
    }

    #[test]
    fn resolution_never_panics_on_hostile_targets() {
        // Targets come from a collection file, which is untrusted input, and
        // in a load run this runs per request per VU.
        let c = ctx();
        for bad in [
            "json[",
            "json[]",
            "json['",
            "jsonpath(",
            "jsonpath(\"",
            "header(\"",
            "json[999999999999999999999]",
            "json.",
            ".",
            "[",
            "]",
            "$$$",
            "json[-1]",
        ] {
            let _ = resolve_assertion_target(bad, &c);
        }
    }

    #[test]
    fn target_and_evaluator_compose_end_to_end() {
        // The pair as a caller uses them: resolve, then evaluate.
        let c = ctx();
        let actual = resolve_assertion_target("json.items.0.name", &c).unwrap();
        let o = assert_evaluate(
            "first item",
            "json.items.0.name",
            &actual,
            "eq",
            &json!("first"),
            None,
        );
        assert!(o.passed && o.unsupported.is_none());

        let status = resolve_assertion_target("status", &c).unwrap();
        assert!(
            assert_evaluate(
                "2xx",
                "status",
                &status,
                "between",
                &json!([200, 299]),
                None
            )
            .passed
        );

        // A header compared numerically — the reason numeric coercion crosses
        // string/number at all.
        let mut c2 = ctx();
        c2.headers.push(("Content-Length".into(), "1024".into()));
        let len = resolve_assertion_target("Content-Length", &c2).unwrap();
        assert!(assert_evaluate("big", "Content-Length", &len, "gt", &json!(1000), None).passed);
    }
    /// TR-442: the vocabulary is also shipped as a STATIC JSON fixture so a
    /// consumer can read it WITHOUT initialising the wasm.
    ///
    /// KnockPort's assertion-expression parser and its editor dropdown both
    /// need the operator names, and both run before — or entirely without —
    /// the wasm tier being live. Reading them through a wasm export made the
    /// parser fail with "no tropel core provider is registered" in every unit
    /// test and in the editor's first render.
    ///
    /// A hand-copied list in TypeScript would drift. This test GENERATES the
    /// fixture from the table and fails when the file on disk disagrees, so
    /// the two cannot diverge without CI saying so.
    #[test]
    fn the_operator_fixture_matches_the_table() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/core-wasm/fixtures/assertion-operators.json"
        );
        let want =
            serde_json::to_string_pretty(ASSERTION_OPERATORS).expect("table serialises") + "\n";
        // Also emitted as a plain ESM module: `packages/core-wasm/src/index.js`
        // deliberately avoids JSON import assertions (the catalog ships the
        // same way as `meta.js`), so a `.js` sibling is what it can actually
        // import.
        let js_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/core-wasm/src/assertion-operators.js"
        );
        let want_js = format!(
            "// GENERATED by tropel-variables' `the_operator_fixture_matches_the_table` test.\n\
             // Do not hand-edit — the test rewrites it and fails when it is stale.\n\
             export default {want};"
        );
        // Read NORMALISED. `.gitattributes` carries `* text=auto`, so a
        // Windows checkout materialises these generated files as CRLF while
        // the generator above emits LF. A raw byte compare therefore called a
        // content-identical fixture STALE on Windows only — and then rewrote
        // it, so the job failed AND left a dirty tree. The comparison is about
        // the vocabulary, not about the checkout's line-ending policy.
        let normalise = |p: &str| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .replace("\r\n", "\n")
        };
        let got_js = normalise(js_path);
        let got = normalise(path);
        if got != want || got_js != want_js {
            std::fs::write(path, &want).expect("fixture is writable");
            std::fs::write(js_path, &want_js).expect("js fixture is writable");
            panic!(
                "packages/core-wasm/fixtures/assertion-operators.json was stale and has been \
                 regenerated — commit it. The vocabulary is consumed by KnockPort's parser and \
                 editor WITHOUT the wasm, so this file is a real interface, not a cache."
            );
        }
    }
    #[test]
    fn a_failing_assertion_carries_a_human_message_and_a_passing_one_does_not() {
        // TR-443: the TypeScript evaluator produced this wording, and the
        // panel renders it. Keeping it byte-compatible means migrating does
        // not silently reword every existing failure.
        let fail = assert_evaluate("status", "status", &json!(200), "eq", &json!(500), None);
        assert!(!fail.passed);
        assert_eq!(
            fail.message.as_deref(),
            Some("expected target status equals 500; actual 200")
        );

        // A unary operator has no expected value to name.
        let unary = assert_evaluate("body", "body", &json!("x"), "isEmpty", &json!(null), None);
        assert!(!unary.passed);
        assert_eq!(
            unary.message.as_deref(),
            Some("expected target body is empty; actual \"x\"")
        );

        // A PASSING row carries none — it is emitted per assertion per
        // request in a load run, and "why did it pass" is not a question.
        let pass = assert_evaluate("status", "status", &json!(200), "eq", &json!(200), None);
        assert!(pass.passed && pass.message.is_none());
        let json = serde_json::to_string(&pass).expect("serialises");
        assert!(!json.contains("message"), "{json}");

        // `unsupported` and `message` do not both appear: an assertion that
        // could not run has no comparison to describe.
        let unknown = assert_evaluate("x", "x", &json!(1), "nope", &json!(1), None);
        assert!(unknown.unsupported.is_some() && unknown.message.is_none());
    }

    #[test]
    fn a_failure_message_truncates_a_huge_actual_value() {
        // A failed assertion against a 2 MB body must not put 2 MB into every
        // result row of a load run.
        let big = json!("y".repeat(5000));
        let out = assert_evaluate("body", "body", &big, "eq", &json!("x"), None);
        let msg = out.message.expect("a failing row has a message");
        assert!(msg.len() < 300, "message was {} bytes", msg.len());
        assert!(msg.contains("..."), "{msg}");
    }
    #[test]
    fn the_failure_message_names_the_target_not_the_assertion_name() {
        // These are different things and conflating them produces a message
        // that says nothing about what was inspected — "expected target
        // should fail equals 500" instead of "expected target status equals
        // 500". `name` exists to aggregate outcomes across a load run;
        // `target` is what was actually checked.
        let out = assert_evaluate(
            "should fail",
            "status",
            &json!(200),
            "eq",
            &json!(500),
            None,
        );
        assert_eq!(out.name, "should fail", "identity is the description");
        assert_eq!(
            out.message.as_deref(),
            Some("expected target status equals 500; actual 200"),
            "the WORDING names the target"
        );
    }
}
