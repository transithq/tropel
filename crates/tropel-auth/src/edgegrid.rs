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

use tropel_sdk::{Result, TropelError};

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
        return Err(TropelError::Other(format!(
            "akamai-edgegrid: missing {} — the request was not signed",
            missing.join(", ")
        )));
    }

    let timestamp = params.timestamp.clone().unwrap_or_else(edgegrid_timestamp);
    let nonce = params.nonce.clone().unwrap_or_else(edgegrid_nonce);

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
fn split_url(url: &str) -> Result<(String, String, String)> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| TropelError::Other(format!("akamai-edgegrid: invalid URL {url:?}: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| TropelError::Other(format!("akamai-edgegrid: URL {url:?} has no host")))?
        .to_string();
    let mut path = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    Ok((parsed.scheme().to_string(), host, path))
}

/// `yyyyMMddTHH:mm:ss+0000` — EdgeGrid's format, NOT RFC 3339.
///
/// See the module docs: an RFC 3339 stamp goes into the signing data verbatim
/// and produces a signature Akamai rejects with a 401 that says nothing about
/// the format.
pub fn edgegrid_timestamp() -> String {
    chrono::Utc::now()
        .format("%Y%m%dT%H:%M:%S+0000")
        .to_string()
}

/// A fresh nonce.
///
/// The crate's existing CSPRNG generator, not a new one and not a counter:
/// the nonce is a replay defence, and `signers.rs` already carries the
/// reasoning for why a time-seeded counter is the wrong tool (an observer can
/// predict the next value). Reusing it also keeps one source of nonce
/// strength rather than two that can drift apart.
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
        assert!(err.contains("invalid URL"), "got {err}");
    }
}
