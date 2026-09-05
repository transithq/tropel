//! Cookie-scope rules — the ones that decide whether a `Set-Cookie` is stored
//! at all.
//!
//! TR-457: this exists because the jar accepted `Domain=com`. A server on
//! `evil.com` sending that had the cookie stored with domain `com` and
//! replayed to `bank.com` — verified through the real jar, not theorised:
//!
//! ```text
//! Domain=com         -> bank.com          : Some("sid=leaked")
//! Domain=example.com -> www.example.com   : Some("ok=1")    (the control)
//! ```
//!
//! `publicsuffix` IS in the dependency graph, via `cookie_store` — but
//! reqwest's `Jar::default()` does not enforce it, so nothing rejected the
//! cookie. Every browser does (RFC 6265 §5.3 step 5).
//!
//! The rule lives in this crate rather than in `tropel-http` for two reasons:
//! it is a RULE and this crate is where rules live, and it is wasm-safe, so
//! `core-wasm` can export it whenever KnockPort should read it here instead of
//! keeping its own copy. KnockPort implements the same single-label test today
//! (KP-213) — a duplication worth naming: a one-line predicate is low-risk to
//! duplicate, and exporting it would cost a publish cycle for one boolean, so
//! this is a deliberate trade rather than an oversight.

/// Is this domain a public suffix — a registry under which anyone can
/// register a name?
///
/// A SINGLE-LABEL test, not the full Public Suffix List, and the gap is worth
/// stating rather than burying: it rejects `com`, `net`, `org` and
/// `localhost`, and it does NOT reject `co.uk` or `github.io`. Embedding the
/// PSL means ~250 KB of data that needs updating forever after, in a crate the
/// wasm tier links. The single-label case is the one reachable from any `.com`
/// server, which is the one that matters.
pub fn is_likely_public_suffix(domain: &str) -> bool {
    !domain.trim_start_matches('.').contains('.')
}

/// Should this `Set-Cookie` line be stored at all, given the host it came from?
///
/// RFC 6265 §5.3 step 5: a `Domain` attribute that is a public suffix is
/// REJECTED unless it is identical to the request host — in which case the
/// cookie becomes host-only, which is what keeps `Domain=localhost` working on
/// `localhost`.
///
/// Rejected, never "downgraded to host-only": storing it host-only would keep
/// a cookie the sender asked to scope differently, and it would still go back
/// to the sender on every request.
pub fn set_cookie_is_acceptable(set_cookie: &str, request_host: &str) -> bool {
    let Some(domain) = domain_attribute(set_cookie) else {
        // No Domain attribute at all: host-only, always fine.
        return true;
    };
    let domain = domain.trim().trim_start_matches('.').to_lowercase();
    if domain.is_empty() {
        return true;
    }
    if !is_likely_public_suffix(&domain) {
        return true;
    }
    domain == request_host.trim().to_lowercase()
}

/// The `Domain=` attribute value, if the line carries one.
///
/// Attribute names are case-insensitive per RFC 6265 §5.2, and splitting on
/// `;` is enough here because a Domain value cannot contain one.
fn domain_attribute(set_cookie: &str) -> Option<&str> {
    set_cookie.split(';').skip(1).find_map(|part| {
        let (name, value) = part.split_once('=')?;
        name.trim().eq_ignore_ascii_case("domain").then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_suffix_domain_is_rejected() {
        // The exact leak: evil.com scoping a cookie to every `.com` host.
        assert!(!set_cookie_is_acceptable(
            "sid=leaked; Domain=com; Path=/",
            "evil.com"
        ));
        assert!(!set_cookie_is_acceptable("a=1; Domain=net", "evil.net"));
        // A leading dot is the same attribute (RFC 6265 §5.2.3).
        assert!(!set_cookie_is_acceptable("a=1; Domain=.com", "evil.com"));
    }

    #[test]
    fn identical_to_the_host_survives_so_localhost_keeps_working() {
        // `localhost` is single-label, so the test above catches it. It must
        // survive by the identical-to-host branch or every local dev server
        // stops keeping its session.
        assert!(set_cookie_is_acceptable(
            "sid=dev; Domain=localhost; Path=/",
            "localhost"
        ));
        assert!(set_cookie_is_acceptable("a=1; Domain=com", "com"));
    }

    #[test]
    fn ordinary_scoping_is_untouched() {
        // The control. A fix that also broke parent-domain scoping would be
        // worse than the bug it closes.
        assert!(set_cookie_is_acceptable(
            "sid=ok; Domain=example.com; Path=/",
            "api.example.com"
        ));
        assert!(set_cookie_is_acceptable(
            "sid=ok; Path=/",
            "api.example.com"
        ));
        assert!(set_cookie_is_acceptable("sid=ok", "api.example.com"));
    }

    #[test]
    fn the_attribute_is_found_however_it_is_written() {
        // Case-insensitive, whitespace-tolerant, and never confused by a
        // COOKIE called "domain" — that is the name=value pair, not an
        // attribute, so it is skipped.
        assert!(!set_cookie_is_acceptable("a=1; DOMAIN=com", "evil.com"));
        assert!(!set_cookie_is_acceptable(
            "a=1;   domain  =  com  ",
            "evil.com"
        ));
        assert!(set_cookie_is_acceptable("domain=com; Path=/", "evil.com"));
    }
}
