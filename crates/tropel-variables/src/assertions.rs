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
}
