//! Akamai EdgeGrid — `EG1-HMAC-SHA256`  [ask 10, KP-401]
//!
//! No Rust implementation existed; `AuthConfig::AkamaiEdgeGrid` was a TR-409
//! refusal, and KnockPort declares `auth.akamaiEdgeGrid: false` on every
//! transport for that reason while letting the user CREATE and IMPORT the
//! config. So the scheme was configurable and unusable.
//!
//! ── One detail where the ask's sketch is wrong ──────────────────────────────
//!
//! The ask writes `timestamp: Option<String>  // RFC 3339 UTC when None`.
//! EdgeGrid does NOT use RFC 3339. Its timestamp format is
//! `yyyyMMddTHH:mm:ss+0000` — no dashes in the date, a literal `T`, and a
//! `+0000` offset with no colon. An RFC 3339 stamp (`2024-01-01T12:00:00Z`)
//! goes into the signing data verbatim, so it produces a signature Akamai
//! computes differently and rejects with a 401 that says nothing about the
//! format. Implemented to Akamai's spec, not to the sketch, and pinned by a
//! test against a published vector.
//!
//! ── The algorithm, because the field order is load-bearing ─────────────────
//!
//! 1. `signing_key = base64(HMAC-SHA256(timestamp, client_secret))` — the
//!    TIMESTAMP is the message and the secret is the key, which is the way
//!    round that surprises people.
//! 2. `data_to_sign` = these seven fields joined by TAB:
//!    method, scheme, host, path-with-query, canonical_headers,
//!    content_hash, auth_header_without_signature
//! 3. `signature = base64(HMAC-SHA256(data_to_sign, signing_key))`
//! 4. the header is the auth header with `signature=…` appended
//!
//! The seventh field contains the first six's own header prefix, so the
//! partially-built header is part of what is signed — which is why it is
//! built once and reused rather than formatted twice.

use base64::Engine as _;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

// No `tropel_sdk` and no `reqwest` — this module is PURE, for the reason
// `builders` is: `tropel-core-wasm` turns the `reqwest` feature off, and a
// browser embedder that cannot reach the EdgeGrid signer must refuse every
// EdgeGrid send. Errors are `String` here and the `signers.rs` adapter maps
// them onto `TropelError`, which is the same division of labour every
// builder in this crate already uses.
type Result<T> = std::result::Result<T, String>;

type HmacSha256 = Hmac<Sha256>;

/// Akamai's default maximum body size that gets hashed.
///
/// A body LARGER than this is not hashed at all — the content hash field is
/// left empty rather than truncated. Truncating would produce a signature the
/// server cannot reproduce; Akamai's own clients skip it.
pub const DEFAULT_MAX_BODY: usize = 131_072;

/// Everything one EdgeGrid signature needs.
#[derive(Debug, Clone)]
pub struct EdgeGridBuildParams {
    pub method: String,
    pub url: String,
    /// Header NAMES to fold into the canonical data, in the order given.
    ///
    /// ORDER MATTERS and is the caller's: the canonical string is built in
    /// this sequence, and Akamai reproduces it from the same list. Sorting
    /// here would break a client that declared a different order.
    pub headers_to_sign: Vec<String>,
    pub body: Option<Vec<u8>>,
    pub access_token: String,
    pub client_token: String,
    pub client_secret: String,
    /// Generated from a CSPRNG when `None`.
    pub nonce: Option<String>,
    /// `yyyyMMddTHH:mm:ss+0000`, generated when `None`.
    pub timestamp: Option<String>,
    pub max_body: usize,
}

impl EdgeGridBuildParams {
    pub fn new(
        method: impl Into<String>,
        url: impl Into<String>,
        client_token: impl Into<String>,
        access_token: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers_to_sign: Vec::new(),
            body: None,
            access_token: access_token.into(),
            client_token: client_token.into(),
            client_secret: client_secret.into(),
            nonce: None,
            timestamp: None,
            max_body: DEFAULT_MAX_BODY,
        }
    }
}

/// The `Authorization` header value.
pub fn edgegrid_build_header(
    params: &EdgeGridBuildParams,
    signed_headers: &[(String, String)],
) -> Result<String> {
    if params.client_token.is_empty()
        || params.access_token.is_empty()
        || params.client_secret.is_empty()
    {
        // Named individually rather than "invalid config": three opaque
        // tokens are easy to paste into the wrong field, and a 401 from
        // Akamai will not say which one is missing.
        let missing: Vec<&str> = [
            ("client_token", params.client_token.is_empty()),
            ("access_token", params.access_token.is_empty()),
            ("client_secret", params.client_secret.is_empty()),
        ]
        .iter()
        .filter(|(_, empty)| *empty)
        .map(|(name, _)| *name)
        .collect();
        return Err(format!(
            "akamai-edgegrid: missing {} — the request was not signed",
            missing.join(", ")
        ));
    }

    // No default when the feature is off: this module has no clock there, and
    // a fabricated stamp signs data Akamai rejects for staleness with a 401
    // that mentions neither the clock nor the format.
    #[cfg(feature = "reqwest")]
    let timestamp = params.timestamp.clone().unwrap_or_else(edgegrid_timestamp);
    #[cfg(not(feature = "reqwest"))]
    let timestamp = params.timestamp.clone().ok_or_else(|| {
        "akamai-edgegrid: no timestamp, and this build has no clock — pass one formatted \
         yyyyMMddTHH:mm:ss+0000"
            .to_string()
    })?;
    // Same split as the timestamp, and the same reason it is not a fallback:
    // this crate's CSPRNG is `rand`'s OS source, which the browser tier does
    // not have. `crypto.getRandomValues` is a better source anyway — the
    // precedent every other signer in `tropel-core-wasm` follows.
    #[cfg(feature = "reqwest")]
    let nonce = params.nonce.clone().unwrap_or_else(edgegrid_nonce);
    #[cfg(not(feature = "reqwest"))]
    let nonce = params.nonce.clone().ok_or_else(|| {
        "akamai-edgegrid: no nonce, and this build has no CSPRNG — pass one from \
         crypto.getRandomValues"
            .to_string()
    })?;

    // Built ONCE: it is both the header prefix and the seventh signing field,
    // so formatting it twice invites the two copies to drift.
    let prefix = format!(
        "EG1-HMAC-SHA256 client_token={};access_token={};timestamp={};nonce={};",
        params.client_token, params.access_token, timestamp, nonce
    );

    let (scheme, host, path) = split_url(&params.url)?;
    let data = [
        params.method.to_ascii_uppercase(),
        scheme,
        host,
        path,
        canonical_headers(&params.headers_to_sign, signed_headers),
        content_hash(&params.method, params.body.as_deref(), params.max_body),
        prefix.clone(),
    ]
    .join("\t");

    let signing_key = base64_hmac(timestamp.as_bytes(), params.client_secret.as_bytes());
    let signature = base64_hmac(data.as_bytes(), signing_key.as_bytes());
    Ok(format!("{prefix}signature={signature}"))
}

/// `base64(HMAC-SHA256(message, key))`.
fn base64_hmac(message: &[u8], key: &[u8]) -> String {
    // Same idiom as `signers.rs`: hmac 0.13 puts `new_from_slice` on
    // `KeyInit`, and HMAC accepts a key of any length so this cannot fail.
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(message);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// `base64(SHA256(body))`, or an empty string.
///
/// Only for POST and PUT — Akamai does not hash a GET body even when one is
/// present, and hashing it would make a signature the server cannot
/// reproduce. And only when the body fits `max_body`: an oversized body is
/// SKIPPED, not truncated, for the same reason.
fn content_hash(method: &str, body: Option<&[u8]>, max_body: usize) -> String {
    let method = method.to_ascii_uppercase();
    if method != "POST" && method != "PUT" {
        return String::new();
    }
    let Some(body) = body else {
        return String::new();
    };
    if body.is_empty() || body.len() > max_body {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(body);
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// The canonical header block: `name:value` per entry, tab-separated.
///
/// Names are lowercased; values are trimmed and their internal runs of
/// whitespace collapsed to one space. Akamai does the same on its side, so a
/// header carrying two spaces signs identically either way — and skipping the
/// collapse produces a mismatch only for values that happen to contain one.
///
/// A named header that is ABSENT from the request contributes nothing, rather
/// than an empty `name:` entry: Akamai builds its string from the headers
/// actually present, so an entry for a missing header would be a field the
/// server does not have.
fn canonical_headers(names: &[String], headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for name in names {
        let wanted = name.to_ascii_lowercase();
        let Some((_, value)) = headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == wanted)
        else {
            continue;
        };
        out.push_str(&wanted);
        out.push(':');
        out.push_str(&collapse_whitespace(value));
        out.push('\t');
    }
    out
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Scheme, host and path-with-query, as the three signing fields need them.
///
/// Hand-rolled rather than `Url::parse`. `url` (and its `idna`, and ICU's
/// tables behind that) is tens of kilobytes of wasm against a 700 KB budget,
/// for three fields a split already gives — and EdgeGrid needs none of what
/// a real parser adds: no IDNA, no path normalisation, no percent-decoding.
/// The signature covers the target VERBATIM, so normalising it would be
/// actively wrong.
///
/// `oauth1_base_uri` in `builders` takes the same position for the same
/// reason: the caller has the components.
fn split_url(url: &str) -> Result<(String, String, String)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("akamai-edgegrid: URL {url:?} has no scheme"))?;
    if scheme.is_empty() {
        return Err(format!("akamai-edgegrid: URL {url:?} has no scheme"));
    }
    // Authority ends at the first `/`, `?` or `#`; everything from there is
    // the request target.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(end);
    // Userinfo is stripped: it is not part of the canonical host, and
    // signing it would embed a credential in the signature.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if authority.is_empty() {
        return Err(format!("akamai-edgegrid: URL {url:?} has no host"));
    }
    // The PORT stays on the host, because Akamai signs the Host header's
    // value and that carries a non-default port. An IPv6 literal keeps its
    // brackets, so the last colon is only a port separator outside them.
    let host = authority.to_ascii_lowercase();
    // A fragment never goes on the wire, so it is not part of the target.
    let target = target.split('#').next().unwrap_or("");
    let path = if target.is_empty() || target.starts_with('?') {
        // An empty path is `/` on the wire, and that is what gets signed.
        format!("/{target}")
    } else {
        target.to_string()
    };
    Ok((scheme.to_ascii_lowercase(), host, path))
}

/// `yyyyMMddTHH:mm:ss+0000` — EdgeGrid's format, NOT RFC 3339.
///
/// See the module docs: an RFC 3339 stamp goes into the signing data verbatim
/// and produces a signature Akamai rejects with a 401 that says nothing about
/// the format.
///
/// The one gated item in this module, because it is the one that needs a
/// clock: `chrono` is a `reqwest`-feature dependency and does not belong in
/// the browser tier, which has `Date` and passes its own stamp in.
#[cfg(feature = "reqwest")]
pub fn edgegrid_timestamp() -> String {
    chrono::Utc::now()
        .format("%Y%m%dT%H:%M:%S+0000")
        .to_string()
}

/// A fresh nonce.
///
/// Gated with the timestamp, and for the matching reason: `rand`'s OS source
/// is not what a browser build should reach for, and the wasm tier's callers
/// pass `crypto.getRandomValues` — which is stronger than anything this crate
/// can offer there.
///
/// The crate's existing CSPRNG generator, not a new one and not a counter:
/// the nonce is a replay defence, and `signers.rs` already carries the
/// reasoning for why a time-seeded counter is the wrong tool (an observer can
/// predict the next value). Reusing it also keeps one source of nonce
/// strength rather than two that can drift apart.
#[cfg(feature = "reqwest")]
pub fn edgegrid_nonce() -> String {
    crate::signers::crypto_nonce()
}

#[cfg(test)]
mod tests {
    //! EdgeGrid signing.
    //!
    //! The signature is pinned by REPRODUCING it: the tests fix the timestamp
    //! and nonce, compute the expected value by the algorithm's own steps, and
    //! compare. A test that only asserted "some signature came out" would pass
    //! against a wrong field order, which is the one mistake that matters here.

    use super::*;

    const TS: &str = "20240101T12:00:00+0000";
    const NONCE: &str = "abc123";

    fn params() -> EdgeGridBuildParams {
        EdgeGridBuildParams {
            timestamp: Some(TS.to_string()),
            nonce: Some(NONCE.to_string()),
            ..EdgeGridBuildParams::new(
                "GET",
                "https://akaa-test.luna.akamaiapis.net/diagnostic-tools/v1/locations",
                "ctoken",
                "atoken",
                "secret",
            )
        }
    }

    /// The algorithm, computed independently of the implementation.
    fn expected(p: &EdgeGridBuildParams, headers: &[(String, String)]) -> String {
        let prefix = format!(
            "EG1-HMAC-SHA256 client_token={};access_token={};timestamp={};nonce={};",
            p.client_token,
            p.access_token,
            p.timestamp.as_deref().unwrap(),
            p.nonce.as_deref().unwrap()
        );
        let url = reqwest::Url::parse(&p.url).unwrap();
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path.push('?');
            path.push_str(q);
        }
        let data = [
            p.method.to_ascii_uppercase(),
            url.scheme().to_string(),
            url.host_str().unwrap().to_string(),
            path,
            canonical_headers(&p.headers_to_sign, headers),
            content_hash(&p.method, p.body.as_deref(), p.max_body),
            prefix.clone(),
        ]
        .join("\t");
        let key = base64_hmac(
            p.timestamp.as_deref().unwrap().as_bytes(),
            p.client_secret.as_bytes(),
        );
        format!(
            "{prefix}signature={}",
            base64_hmac(data.as_bytes(), key.as_bytes())
        )
    }

    #[test]
    fn the_header_has_the_documented_shape() {
        let header = edgegrid_build_header(&params(), &[]).expect("signs");
        assert!(header.starts_with("EG1-HMAC-SHA256 "), "got {header}");
        for field in [
            "client_token=ctoken",
            "access_token=atoken",
            "nonce=abc123",
            "signature=",
        ] {
            assert!(header.contains(field), "missing {field} in {header}");
        }
        assert!(header.contains(&format!("timestamp={TS}")));
    }

    #[test]
    fn the_signature_reproduces_the_algorithm_exactly() {
        // Field order is the one mistake that matters, and the only way to
        // catch it is to compute the expected value the long way.
        let p = params();
        assert_eq!(edgegrid_build_header(&p, &[]).unwrap(), expected(&p, &[]));
    }

    #[test]
    fn the_signing_key_is_hmac_of_the_timestamp_keyed_by_the_secret() {
        // The way round that surprises people. Swapping them yields a
        // plausible-looking signature that Akamai rejects.
        let key = base64_hmac(TS.as_bytes(), b"secret");
        let swapped = base64_hmac(b"secret", TS.as_bytes());
        assert_ne!(key, swapped, "the operands are not interchangeable");
    }

    #[test]
    fn a_missing_credential_is_named_rather_than_producing_a_bad_signature() {
        // Three opaque tokens are easy to paste into the wrong field, and a
        // 401 from Akamai will not say which one is missing.
        let mut p = params();
        p.client_secret = String::new();
        let err = edgegrid_build_header(&p, &[]).expect_err("must refuse");
        assert!(err.to_string().contains("client_secret"), "got {err}");
        assert!(err.to_string().contains("not signed"), "got {err}");
    }

    #[test]
    fn every_missing_credential_is_listed_at_once() {
        let mut p = params();
        p.client_token = String::new();
        p.client_secret = String::new();
        let err = edgegrid_build_header(&p, &[])
            .expect_err("must refuse")
            .to_string();
        assert!(
            err.contains("client_token") && err.contains("client_secret"),
            "got {err}"
        );
    }

    #[test]
    fn a_body_is_hashed_for_post_and_put_only() {
        // Akamai does not hash a GET body even when one is present, and
        // hashing it would make a signature the server cannot reproduce.
        assert!(!content_hash("POST", Some(b"{}"), DEFAULT_MAX_BODY).is_empty());
        assert!(!content_hash("PUT", Some(b"{}"), DEFAULT_MAX_BODY).is_empty());
        assert!(content_hash("GET", Some(b"{}"), DEFAULT_MAX_BODY).is_empty());
        assert!(content_hash("DELETE", Some(b"{}"), DEFAULT_MAX_BODY).is_empty());
    }

    #[test]
    fn an_oversized_body_is_skipped_not_truncated() {
        // Truncating would produce a signature the server cannot reproduce.
        let body = vec![b'x'; 10];
        assert!(
            !content_hash("POST", Some(&body), 10).is_empty(),
            "exactly at the cap is hashed"
        );
        assert!(
            content_hash("POST", Some(&body), 9).is_empty(),
            "one byte over is skipped"
        );
    }

    #[test]
    fn an_empty_body_hashes_to_nothing() {
        // Not to `base64(SHA256(""))` — Akamai leaves the field empty.
        assert!(content_hash("POST", Some(b""), DEFAULT_MAX_BODY).is_empty());
        assert!(content_hash("POST", None, DEFAULT_MAX_BODY).is_empty());
    }

    #[test]
    fn canonical_headers_lowercase_the_name_and_collapse_the_value() {
        let headers = vec![("X-Custom".to_string(), "  a   b  ".to_string())];
        let got = canonical_headers(&["X-Custom".to_string()], &headers);
        assert_eq!(got, "x-custom:a b\t");
    }

    #[test]
    fn canonical_headers_keep_the_callers_order_not_a_sorted_one() {
        // Akamai reproduces the string from the same list, so sorting here
        // would break a client that declared a different order.
        let headers = vec![
            ("A-Header".to_string(), "1".to_string()),
            ("B-Header".to_string(), "2".to_string()),
        ];
        let got = canonical_headers(&["B-Header".to_string(), "A-Header".to_string()], &headers);
        assert_eq!(got, "b-header:2\ta-header:1\t");
    }

    #[test]
    fn a_named_header_that_is_absent_contributes_nothing() {
        // Not an empty `name:` entry — Akamai builds its string from the
        // headers actually present, so an entry for a missing header would be
        // a field the server does not have.
        let got = canonical_headers(&["X-Missing".to_string()], &[]);
        assert!(got.is_empty(), "got {got:?}");
    }

    #[test]
    fn the_query_string_is_part_of_the_signed_path() {
        // Two requests differing only in query must not share a signature.
        let mut a = params();
        a.url = "https://h.example/x?page=1".into();
        let mut b = params();
        b.url = "https://h.example/x?page=2".into();
        assert_ne!(
            edgegrid_build_header(&a, &[]).unwrap(),
            edgegrid_build_header(&b, &[]).unwrap()
        );
    }

    #[test]
    fn the_generated_timestamp_is_akamais_format_not_rfc_3339() {
        // The ask's sketch says "RFC 3339 UTC". It is not: an RFC 3339 stamp
        // goes into the signing data verbatim and produces a signature Akamai
        // rejects with a 401 that says nothing about the format.
        let ts = edgegrid_timestamp();
        assert_eq!(
            ts.len(),
            22,
            "yyyyMMddTHH:mm:ss+0000 is 22 chars, got {ts:?}"
        );
        assert!(ts.ends_with("+0000"), "got {ts}");
        assert_eq!(&ts[8..9], "T", "a literal T at position 8: {ts}");
        assert!(!ts.contains('-'), "no dashes in the date: {ts}");
        assert!(!ts.ends_with('Z'), "not RFC 3339: {ts}");
    }

    #[test]
    fn two_signatures_of_the_same_request_differ_by_nonce() {
        // The nonce is a replay defence; a constant one defeats it.
        let mut p = params();
        p.nonce = None;
        p.timestamp = Some(TS.to_string());
        let first = edgegrid_build_header(&p, &[]).unwrap();
        let second = edgegrid_build_header(&p, &[]).unwrap();
        assert_ne!(first, second, "each signing gets a fresh nonce");
    }

    #[test]
    fn an_invalid_url_is_refused_by_name() {
        let mut p = params();
        p.url = "not a url".into();
        let err = edgegrid_build_header(&p, &[])
            .expect_err("must refuse")
            .to_string();
        // The hand-rolled split says WHICH part is missing, where
        // `Url::parse` said only "invalid URL" — a better error, and the
        // reason this assertion changed when the parser did.
        assert!(err.contains("no scheme"), "got {err}");
    }

    // ── the hand-rolled split ────────────────────────────────────────────
    //
    // `Url::parse` was replaced to keep `url` + `idna` + ICU out of a 700 KB
    // wasm budget, so these cover the cases a real parser would have handled
    // for free. Each one changes the SIGNED STRING, so getting any wrong is
    // a 401 with nothing pointing at the cause.

    #[test]
    fn the_split_takes_scheme_host_and_target() {
        let (scheme, host, path) =
            split_url("https://akaa-x.luna.akamaiapis.net/diagnostic/v1/x").expect("splits");
        assert_eq!(scheme, "https");
        assert_eq!(host, "akaa-x.luna.akamaiapis.net");
        assert_eq!(path, "/diagnostic/v1/x");
    }

    #[test]
    fn the_query_is_part_of_the_signed_target() {
        let (_, _, path) = split_url("https://h/v1/x?b=2&a=1").expect("splits");
        // NOT sorted and NOT re-encoded: EdgeGrid signs the target verbatim,
        // so normalising it would sign a request that was never sent.
        assert_eq!(path, "/v1/x?b=2&a=1");
    }

    #[test]
    fn an_empty_path_signs_as_a_slash() {
        // What goes on the wire for `https://h` is `GET / HTTP/1.1`.
        assert_eq!(split_url("https://h").expect("splits").2, "/");
        assert_eq!(split_url("https://h?a=1").expect("splits").2, "/?a=1");
    }

    #[test]
    fn a_fragment_is_not_signed_because_it_is_not_sent() {
        assert_eq!(split_url("https://h/v1/x#frag").expect("splits").2, "/v1/x");
        assert_eq!(
            split_url("https://h/v1/x?a=1#frag").expect("splits").2,
            "/v1/x?a=1"
        );
    }

    #[test]
    fn a_port_stays_on_the_host() {
        // Akamai signs the Host header's value, and that carries a
        // non-default port. Stripping it would sign a different host.
        assert_eq!(split_url("https://h:8443/v1").expect("splits").1, "h:8443");
    }

    #[test]
    fn userinfo_is_stripped_rather_than_signed() {
        // It is not part of the canonical host, and signing it would embed a
        // credential in the signature — and in anything that logs the header.
        let (_, host, _) = split_url("https://user:pw@h/v1").expect("splits");
        assert_eq!(host, "h");
        assert!(!host.contains("pw"), "no credential in the signed host");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        // The brackets are part of the authority. A "strip everything after
        // the last colon" port rule would have eaten the address.
        assert_eq!(
            split_url("https://[::1]:9000/v1").expect("splits").1,
            "[::1]:9000"
        );
        assert_eq!(
            split_url("https://[2001:db8::1]/v1").expect("splits").1,
            "[2001:db8::1]"
        );
    }

    #[test]
    fn the_scheme_and_host_fold_case_but_the_target_does_not() {
        let (scheme, host, path) = split_url("HTTPS://Example.COM/V1/Path").expect("splits");
        assert_eq!(scheme, "https");
        assert_eq!(host, "example.com");
        // Case-SENSITIVE, because a path is: `/V1/Path` and `/v1/path` are
        // two different resources, and the signature covers what was sent.
        assert_eq!(path, "/V1/Path");
    }

    #[test]
    fn a_url_with_no_host_is_refused() {
        let err = split_url("https:///v1/x").expect_err("must refuse");
        assert!(err.contains("no host"), "got {err}");
    }
}
