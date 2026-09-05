//! Header-name folding — the ONE rule for "do these two names mean the same
//! header".
//!
//! TR-455: this exists because there were two answers. tropel's sandbox
//! bridge compared with `eq_ignore_ascii_case`; KnockPort's `scripting-core.ts`
//! compares with JavaScript's `String.prototype.toLowerCase()`, which folds
//! the whole of Unicode. They disagree, and not only in theory:
//!
//! ```text
//! "X-\u{212A}EY".to_lowercase()          == "x-key"   (JS, and now here)
//! "X-\u{212A}EY".eq_ignore_ascii_case(..) != "x-key"  (the old Rust rule)
//! ```
//!
//! U+212A is the Kelvin sign, and it lowercases to an ASCII `k`. So a script
//! reading that header FOUND it in the app and did not find it in a load run —
//! the same script, two answers, and no error on either side to say so. That
//! is the D4 failure ("can two implementations disagree invisibly?") in its
//! purest form.
//!
//! The two are reconciled toward Unicode rather than toward ASCII. RFC 9110
//! defines a field name as an ASCII `token`, so ASCII folding is defensible
//! for HTTP alone — but the lookup KEY comes from a user's script, not from
//! the wire, and the host that runs those scripts folds Unicode. Narrowing the
//! host instead would mean a name that resolves in every other JavaScript
//! runtime silently stops resolving here.
//!
//! NOT used for signing. AWS SigV4, Hawk and OAuth1 canonicalise header names
//! with ASCII lowercasing because their specifications say so; a "more
//! correct" fold there would change the signature and produce a 403.

/// Fold a header name for comparison, matching JavaScript's
/// `String.prototype.toLowerCase()`.
///
/// Rust's `str::to_lowercase` and ECMAScript's `toLowerCase` both implement
/// the Unicode Default Case Conversion with SpecialCasing (including the
/// Final_Sigma context), which is what makes the two hosts agree.
pub fn fold_header_name(name: &str) -> String {
    name.to_lowercase()
}

/// Do two header names refer to the same header?
///
/// The ASCII fast path is not an optimisation detail worth hiding: real header
/// names are ASCII tokens, so it is what runs for essentially every lookup,
/// and it allocates nothing. The Unicode path only runs when a name actually
/// contains non-ASCII — where the old rule was wrong.
pub fn header_name_eq(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    fold_header_name(a) == fold_header_name(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ascii_fast_path_agrees_with_the_unicode_path() {
        // If these two ever disagreed, the fast path would be a silent
        // behaviour change that only shows up for some inputs.
        for (a, b) in [
            ("Content-Type", "content-type"),
            ("X-API-KEY", "x-api-key"),
            ("Accept", "accept"),
            ("Set-Cookie", "SET-COOKIE"),
            ("a", "b"),
            ("x-one", "x-two"),
        ] {
            assert_eq!(
                header_name_eq(a, b),
                fold_header_name(a) == fold_header_name(b),
                "fast path disagrees with the Unicode path for {a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn the_kelvin_sign_matches_an_ascii_k_as_it_does_in_javascript() {
        // The concrete divergence TR-455 closes. `"X-\u{212A}EY".toLowerCase()`
        // is `"x-key"` in every JavaScript runtime, so a script that looked up
        // this name found the header in KnockPort and did NOT find it in a
        // load run.
        assert!(header_name_eq("X-\u{212A}EY", "x-key"));
        // And the old rule really did say otherwise — pinned so the fix
        // cannot be reverted to ASCII without this failing.
        assert!(!"X-\u{212A}EY".eq_ignore_ascii_case("x-key"));
    }

    #[test]
    fn other_shapes_where_unicode_folding_is_what_javascript_does() {
        // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE lowercases to TWO
        // scalars (i + U+0307) in both Rust and JavaScript.
        assert_eq!(fold_header_name("\u{0130}"), "i\u{0307}");
        // U+1E9E LATIN CAPITAL LETTER SHARP S lowercases to ß, not to "ss".
        assert_eq!(fold_header_name("\u{1E9E}"), "\u{00DF}");
        // Angstrom sign folds onto the ordinary å.
        assert!(header_name_eq("X-\u{212B}", "x-\u{00E5}"));
    }

    #[test]
    fn a_non_ascii_name_still_does_not_match_a_different_header() {
        // Folding wider must not make MORE things equal than JavaScript does.
        assert!(!header_name_eq("X-\u{212A}EY", "x-value"));
        assert!(!header_name_eq("\u{0130}d", "id"));
    }
}
