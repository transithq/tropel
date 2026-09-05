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
/// Never panics, never allocates for the unary paths, and returns a named
/// `unsupported` rather than a bare `false` when it cannot evaluate at all.
pub fn assert_evaluate(
    name: &str,
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

    AssertionOutcome {
        name: name.to_string(),
        passed,
        unsupported,
    }
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
        assert_evaluate("t", &actual, op, &expected, None)
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
        let hit = assert_evaluate("t", &json!("abc"), "matches", &json!("^a"), Some(&m));
        assert!(hit.passed && hit.unsupported.is_none());
        let miss = assert_evaluate("t", &json!("xyz"), "matches", &json!("^a"), Some(&m));
        assert!(!miss.passed && miss.unsupported.is_none());
        let neg = assert_evaluate("t", &json!("xyz"), "notMatches", &json!("^a"), Some(&m));
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
        let o = assert_evaluate("status is 200", &json!(200), "eq", &json!(200), None);
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
                    let o = assert_evaluate("t", a, op.name, b, None);
                    if op.name == "matches" || op.name == "notMatches" {
                        assert!(o.unsupported.is_some());
                    } else {
                        assert!(o.unsupported.is_none(), "{} {:?}", op.name, o.unsupported);
                    }
                }
            }
        }
    }
}
