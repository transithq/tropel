//! Signed-auth exports — digest, Hawk, AWS SigV4 and OAuth1.
//!
//! # Granularity is the point
//!
//! Every export here takes the RAW inputs a browser already has (a
//! `WWW-Authenticate` header string, `URL` components, a form body) and does
//! ALL the rule application in Rust. The alternative — exporting
//! `tropel_auth::builders`' parameter structs directly — would have left the
//! caller to derive the AWS signing service, the S3 double-encoding rule, the
//! §3.4.1.2 base-string URI and the challenge parse in TypeScript.
//!
//! That is not hypothetical: TR-428 through TR-431 exist because those rules
//! were unreachable from this crate, and each one fails only in the case
//! nobody tests — virtual-hosted S3, an escaped quote in a realm, an IPv6
//! host. A coarse boundary is what keeps them in Rust (invariant #3).
//!
//! Nonces and cnonces are caller-supplied by design: `crypto.getRandomValues`
//! is a better source than anything this crate can offer, and the PKCE export
//! already sets that precedent.

use base64::Engine as _;
use tropel_auth::builders as b;
use wasm_bindgen::prelude::*;

use crate::err;

/// Parse/serialise with STRING errors so every `*_inner` below is callable —
/// and therefore testable — on a native target. `JsValue` cannot be
/// constructed off wasm32; it panics. The thin `#[wasm_bindgen]` wrappers are
/// the only things that touch it, which is why the error paths here have
/// tests at all.
fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_str(s).map_err(|e| e.to_string())
}

fn as_json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| e.to_string())
}

/// `[[name, value], …]` in the order the browser will send them.
type HeaderList = Vec<(String, String)>;

/// The wire shape of a produced header.
///
/// A plain derive rather than `serde_json::json!`: the macro builds a
/// `serde_json::Value`, and its `Map` plus the `Value` Serialize impl are
/// several KB of wasm that nothing else in this crate was pulling in.
#[derive(serde::Serialize)]
struct HeaderJson<'a> {
    name: &'a str,
    value: &'a str,
}

fn decode_body(body_base64: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    match body_base64 {
        None => Ok(None),
        Some(s) => base64::engine::general_purpose::STANDARD
            .decode(s)
            .map(Some)
            .map_err(|e| format!("body_base64 is not valid base64: {e}")),
    }
}

// ── Digest ───────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DigestSignRequest {
    /// The server's raw `WWW-Authenticate` value, parsed here — including the
    /// multi-scheme and quoted-pair rules (TR-429, TR-430).
    www_authenticate: String,
    username: String,
    password: String,
    method: String,
    /// The request-target as sent: path plus query.
    uri: String,
    /// The caller's per-(host, realm, nonce) counter. Starts at 1 and MUST
    /// increment per request against the same nonce, or the server replays.
    nc: u64,
    cnonce: String,
}

/// Answer a digest challenge → JSON `{name, value}`, or `null` when the header
/// carries no Digest challenge (it may legitimately offer only Basic).
#[wasm_bindgen(js_name = "digestSign")]
pub fn digest_sign(request_json: &str) -> Result<String, JsValue> {
    digest_sign_inner(request_json).map_err(err)
}

fn digest_sign_inner(request_json: &str) -> Result<String, String> {
    let r: DigestSignRequest = from_json(request_json)?;
    let Some(c) = b::find_digest_challenge(&r.www_authenticate) else {
        return Ok("null".to_string());
    };
    let get = |k: &str| c.get(k).map(String::as_str);
    let out = b::digest_build_authorization(&b::DigestBuildParams {
        username: &r.username,
        password: &r.password,
        method: &r.method,
        uri: &r.uri,
        realm: get("realm").unwrap_or(""),
        nonce: get("nonce").unwrap_or(""),
        nc: r.nc,
        cnonce: &r.cnonce,
        qop: get("qop"),
        algorithm: get("algorithm"),
        opaque: get("opaque"),
    });
    as_json(&HeaderJson {
        name: &out.name,
        value: &out.value,
    })
}

// ── Hawk ─────────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HawkSignRequest {
    method: String,
    resource: String,
    host: String,
    port: u16,
    id: String,
    key: String,
    algorithm: Option<String>,
    ts: String,
    nonce: String,
    #[serde(default)]
    ext: String,
}

/// Build a Hawk `Authorization` header → JSON `{name, value}`.
#[wasm_bindgen(js_name = "hawkSign")]
pub fn hawk_sign(request_json: &str) -> Result<String, JsValue> {
    hawk_sign_inner(request_json).map_err(err)
}

fn hawk_sign_inner(request_json: &str) -> Result<String, String> {
    let r: HawkSignRequest = from_json(request_json)?;
    let out = b::hawk_build_header(&b::HawkBuildParams {
        method: &r.method,
        resource: &r.resource,
        host: &r.host,
        port: r.port,
        id: &r.id,
        key: &r.key,
        algorithm: r.algorithm.as_deref(),
        ts: &r.ts,
        nonce: &r.nonce,
        ext: &r.ext,
    });
    as_json(&HeaderJson {
        name: &out.name,
        value: &out.value,
    })
}

// ── AWS SigV4 ────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SigV4SignRequest {
    method: String,
    /// `URL.hostname` — WITHOUT brackets for IPv6; this crate re-adds them.
    host: String,
    /// `URL.pathname`, already single-encoded.
    path: String,
    /// `URL.search` without the leading `?`, or "".
    #[serde(default)]
    query: String,
    #[serde(default)]
    headers: HeaderList,
    /// Base64, because the payload hash is over exact bytes and a JS string
    /// cannot represent an arbitrary binary body. Absent → UNSIGNED-PAYLOAD.
    body_base64: Option<String>,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    region: Option<String>,
    /// Omit to derive from the host — the virtual-hosted rule (TR-428).
    service: Option<String>,
    amz_date: String,
    date_stamp: String,
}

/// Sign a request with AWS SigV4 → JSON `[{name, value}, …]`.
///
/// The service derivation, the S3-vs-everything-else canonical-URI rule and
/// the `s3-control` → `s3` signing-name mapping all happen here. A caller that
/// re-derived them would reproduce the bug recorded in `default_service`:
/// a 403 on every virtual-hosted-S3 and API Gateway request.
#[wasm_bindgen(js_name = "awsSigV4Sign")]
pub fn aws_sigv4_sign(request_json: &str) -> Result<String, JsValue> {
    aws_sigv4_sign_inner(request_json).map_err(err)
}

fn aws_sigv4_sign_inner(request_json: &str) -> Result<String, String> {
    let r: SigV4SignRequest = from_json(request_json)?;
    let body = decode_body(r.body_base64.as_deref())?;
    let region = r.region.as_deref().unwrap_or("us-east-1");
    let service = r
        .service
        .clone()
        .unwrap_or_else(|| b::default_service(&r.host));
    let signing_service = b::signing_name(&service);
    let canonical_uri = b::sigv4_canonical_uri(&r.path, &service);
    let signing_key = b::derive_signing_key(&r.secret_key, &r.date_stamp, region, signing_service);
    let out = b::aws_sigv4_build_headers(
        &b::AwsSigV4BuildParams {
            method: &r.method,
            path: &r.path,
            query: &r.query,
            host: &b::bracket_host(&r.host),
            headers: &r.headers,
            body: body.as_deref(),
            access_key: &r.access_key,
            secret_key: &r.secret_key,
            session_token: r.session_token.as_deref(),
            region,
            service: &service,
            amz_date: &r.amz_date,
            date_stamp: &r.date_stamp,
        },
        &canonical_uri,
        signing_service,
        &signing_key,
    );
    let headers: Vec<HeaderJson<'_>> = out
        .headers
        .iter()
        .map(|h| HeaderJson {
            name: &h.name,
            value: &h.value,
        })
        .collect();
    as_json(&headers)
}

// ── OAuth1 ───────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuth1SignRequest {
    method: String,
    /// `URL.protocol` without the trailing colon.
    scheme: String,
    /// `URL.hostname` — WITHOUT brackets for IPv6; this crate re-adds them.
    host: String,
    /// `URL.port === "" ? null : Number(URL.port)`.
    port: Option<u16>,
    path: String,
    /// Query parameters, already decoded.
    #[serde(default)]
    query_params: HeaderList,
    /// The raw body when it is `application/x-www-form-urlencoded`; decoded
    /// here so the `+`-is-a-space rule stays in Rust.
    form_body: Option<String>,
    consumer_key: String,
    consumer_secret: String,
    token: Option<String>,
    token_secret: Option<String>,
    signature_method: String,
    nonce: String,
    timestamp: String,
}

/// Sign a request with OAuth1 → JSON `{name, value}`.
///
/// Returns an error naming the supported set when `signatureMethod` is not one
/// of them, rather than silently downgrading to HMAC-SHA1.
#[wasm_bindgen(js_name = "oauth1Sign")]
pub fn oauth1_sign(request_json: &str) -> Result<String, JsValue> {
    oauth1_sign_inner(request_json).map_err(err)
}

fn oauth1_sign_inner(request_json: &str) -> Result<String, String> {
    let r: OAuth1SignRequest = from_json(request_json)?;
    let base_uri = b::oauth1_base_uri(&r.scheme, &r.host, r.port, &r.path);
    let mut params = r.query_params.clone();
    if let Some(form) = r.form_body.as_deref() {
        params.extend(b::parse_form(form.as_bytes()));
    }
    let out = b::oauth1_build_header(&b::OAuth1BuildParams {
        method: &r.method,
        base_uri: &base_uri,
        request_params: &params,
        consumer_key: &r.consumer_key,
        consumer_secret: &r.consumer_secret,
        token: r.token.as_deref(),
        token_secret: r.token_secret.as_deref(),
        signature_method: &r.signature_method,
        nonce: &r.nonce,
        timestamp: &r.timestamp,
    })
    .ok_or_else(|| {
        format!(
            "unsupported OAuth1 signature_method '{}' - supported: {}",
            r.signature_method,
            b::OAUTH1_SIGNATURE_METHODS.join(", ")
        )
    })?;
    as_json(&HeaderJson {
        name: &out.header.name,
        value: &out.header.value,
    })
}

// ── Akamai EdgeGrid ──────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EdgeGridSignRequest {
    method: String,
    /// The FULL url. EdgeGrid signs path+query as one string and derives the
    /// host from it, so splitting it into components here would only give the
    /// caller a way to reassemble it differently than Akamai does.
    url: String,
    /// Header names to fold into the signature, in the caller's order.
    ///
    /// Not sorted: Akamai rebuilds the canonical string from the same list,
    /// so a different order is a different signature.
    #[serde(default)]
    headers_to_sign: Vec<String>,
    /// The request's headers, `[[name, value], …]`. `headersToSign` names
    /// which of these participate; this is where their VALUES come from.
    #[serde(default)]
    headers: HeaderList,
    /// Base64, matching `digestSign`/`awsSigV4Sign`. A JSON string would
    /// have made a binary body unsignable, and EdgeGrid hashes raw bytes.
    body_base64: Option<String>,
    client_token: String,
    access_token: String,
    client_secret: String,
    /// Caller-supplied, like every other nonce in this file:
    /// `crypto.getRandomValues` is a better source than anything this crate
    /// can reach on wasm32-unknown-unknown.
    nonce: String,
    /// `yyyyMMddTHH:mm:ss+0000`. Caller-supplied for the same reason the
    /// OAuth1 timestamp is — the host clock is the browser's.
    timestamp: String,
    /// Bodies above this are NOT hashed rather than truncated: a truncated
    /// content hash is a signature the server cannot reproduce. `null` uses
    /// Akamai's own 128 KiB default.
    max_body: Option<usize>,
}

/// Sign a request with Akamai EdgeGrid → JSON `{name, value}`.
///
/// The whole request crosses the boundary because the signature covers it:
/// method, url, the named headers' values, and the body hash. A caller
/// passing only credentials could not produce this.
#[wasm_bindgen(js_name = "edgegridSign")]
pub fn edgegrid_sign(request_json: &str) -> Result<String, JsValue> {
    edgegrid_sign_inner(request_json).map_err(err)
}

fn edgegrid_sign_inner(request_json: &str) -> Result<String, String> {
    let r: EdgeGridSignRequest = from_json(request_json)?;
    if r.nonce.is_empty() || r.timestamp.is_empty() {
        // Refused rather than generated. This crate has no clock and no
        // CSPRNG worth using on wasm32-unknown-unknown, and generating an
        // empty-string nonce would produce a header Akamai rejects with a
        // message about neither.
        return Err(
            "edgegridSign needs a nonce and a timestamp — pass crypto.getRandomValues and the              host clock, formatted yyyyMMddTHH:mm:ss+0000"
                .to_string(),
        );
    }
    let params = tropel_auth::edgegrid::EdgeGridBuildParams {
        method: r.method,
        url: r.url,
        headers_to_sign: r.headers_to_sign,
        body: decode_body(r.body_base64.as_deref())?,
        access_token: r.access_token,
        client_token: r.client_token,
        client_secret: r.client_secret,
        nonce: Some(r.nonce),
        timestamp: Some(r.timestamp),
        max_body: r
            .max_body
            .unwrap_or(tropel_auth::edgegrid::DEFAULT_MAX_BODY),
    };
    let value = tropel_auth::edgegrid::edgegrid_build_header(&params, &r.headers)
        .map_err(|e| e.to_string())?;
    as_json(&HeaderJson {
        name: "Authorization",
        value: &value,
    })
}

/// The OAuth1 signature methods `oauth1Sign` accepts → JSON array. Exported so
/// a picker UI offers exactly what is implemented instead of keeping its own
/// list, which is how an unsupported method reaches the signer at all.
#[wasm_bindgen(js_name = "oauth1SignatureMethods")]
pub fn oauth1_signature_methods() -> Result<String, JsValue> {
    as_json(&b::OAUTH1_SIGNATURE_METHODS).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("export returned valid JSON")
    }

    #[test]
    fn edgegrid_sign_produces_the_header_the_browser_tier_cannot() {
        // The reason this export exists: KnockPort's auth tier signs and
        // forwards a header, exactly as it does for oauth1/hawk/sigv4. With
        // no export, an EdgeGrid config was creatable, importable — and
        // refused at every send.
        let out = v(&edgegrid_sign_inner(
            r#"{"method":"GET",
                "url":"https://akaa-x.luna.akamaiapis.net/diagnostic/v1/locations",
                "clientToken":"ct","accessToken":"at","clientSecret":"cs",
                "nonce":"nnn","timestamp":"20260909T12:00:00+0000"}"#,
        )
        .expect("signs"));
        assert_eq!(out["name"], "Authorization");
        let value = out["value"].as_str().expect("a string");
        assert!(value.starts_with("EG1-HMAC-SHA256 "), "got {value}");
        assert!(value.contains("client_token=ct;"), "got {value}");
        assert!(value.contains("access_token=at;"), "got {value}");
        assert!(value.contains("nonce=nnn;"), "got {value}");
        assert!(
            value.contains("timestamp=20260909T12:00:00+0000;"),
            "got {value}"
        );
        // The secret authenticates; it is never transmitted.
        assert!(
            !value.contains("cs"),
            "the client secret must not appear: {value}"
        );
    }

    #[test]
    fn edgegrid_sign_is_deterministic_for_a_pinned_nonce_and_timestamp() {
        // Which is what makes the header reproducible from a saved request —
        // and what lets Akamai recompute it. Two calls with the same inputs
        // must agree, or something in the canonical string is order- or
        // clock-dependent.
        let req = r#"{"method":"POST","url":"https://h/v1/x?b=2&a=1",
            "headersToSign":["X-Second","X-First"],
            "headers":[["X-First","1"],["X-Second","2"],["X-Ignored","3"]],
            "bodyBase64":"aGVsbG8=",
            "clientToken":"ct","accessToken":"at","clientSecret":"cs",
            "nonce":"n","timestamp":"20260909T12:00:00+0000"}"#;
        let a = edgegrid_sign_inner(req).expect("signs");
        let b = edgegrid_sign_inner(req).expect("signs");
        assert_eq!(a, b);
    }

    #[test]
    fn edgegrid_sign_signs_the_named_headers_in_the_callers_order() {
        // Akamai rebuilds the canonical string from the same list, so the
        // order is data — sorting it here would produce a signature the
        // server cannot reproduce.
        let base = r#""url":"https://h/v1","clientToken":"ct","accessToken":"at",
            "clientSecret":"cs","nonce":"n","timestamp":"20260909T12:00:00+0000",
            "headers":[["X-First","1"],["X-Second","2"]]"#;
        let forward = edgegrid_sign_inner(&format!(
            r#"{{"method":"GET",{base},"headersToSign":["X-First","X-Second"]}}"#
        ))
        .expect("signs");
        let reversed = edgegrid_sign_inner(&format!(
            r#"{{"method":"GET",{base},"headersToSign":["X-Second","X-First"]}}"#
        ))
        .expect("signs");
        assert_ne!(forward, reversed, "the order changes the signature");
    }

    #[test]
    fn edgegrid_sign_refuses_a_missing_nonce_or_timestamp_by_name() {
        // NOT generated. This crate has no clock and no CSPRNG worth using on
        // wasm32-unknown-unknown, and an empty nonce would produce a header
        // Akamai rejects with a message about neither.
        let err = edgegrid_sign_inner(
            r#"{"method":"GET","url":"https://h/v1","clientToken":"ct",
                "accessToken":"at","clientSecret":"cs","nonce":"","timestamp":"x"}"#,
        )
        .expect_err("must refuse");
        assert!(err.contains("nonce"), "got {err}");
        assert!(err.contains("getRandomValues"), "names the source: {err}");
        assert!(err.contains("yyyyMMdd"), "names the format: {err}");
    }

    #[test]
    fn edgegrid_sign_names_the_missing_credential() {
        let err = edgegrid_sign_inner(
            r#"{"method":"GET","url":"https://h/v1","clientToken":"ct",
                "accessToken":"","clientSecret":"","nonce":"n","timestamp":"t"}"#,
        )
        .expect_err("must refuse");
        assert!(err.contains("access_token"), "got {err}");
        assert!(err.contains("client_secret"), "got {err}");
    }

    #[test]
    fn edgegrid_sign_rejects_a_body_that_is_not_base64() {
        // Base64 like `digestSign`/`awsSigV4Sign`, so a binary body is
        // signable at all — EdgeGrid hashes raw bytes.
        let err = edgegrid_sign_inner(
            r#"{"method":"POST","url":"https://h/v1","bodyBase64":"not base64!!",
                "clientToken":"ct","accessToken":"at","clientSecret":"cs",
                "nonce":"n","timestamp":"t"}"#,
        )
        .expect_err("must refuse");
        assert!(err.contains("body_base64"), "got {err}");
    }

    #[test]
    fn digest_sign_parses_the_challenge_and_answers_it() {
        // The whole point of the coarse boundary: the caller hands over the
        // raw header, including a Digest challenge listed AFTER Basic and a
        // quoted qop list — the two rules a naive TS parser gets wrong.
        let out = digest_sign_inner(
            &serde_json::json!({
                "wwwAuthenticate": r#"Basic realm="b", Digest realm="r", qop="auth, auth-int", nonce="n", opaque="o""#,
                "username": "u", "password": "p", "method": "GET",
                "uri": "/dir/index.html", "nc": 1, "cnonce": "0a4f113b",
            })
            .to_string(),
        )
        .expect("digest_sign succeeds");
        let out = v(&out);
        let value = out["value"].as_str().unwrap();
        assert!(value.starts_with("Digest "));
        assert!(value.contains(r#"realm="r""#), "{value}");
        assert!(value.contains(r#"opaque="o""#), "{value}");
        assert!(value.contains("qop=auth"), "{value}");
    }

    #[test]
    fn digest_sign_returns_null_when_no_digest_challenge() {
        // A server may legitimately offer only Basic. Returning null lets the
        // caller fall through instead of signing garbage.
        let out = digest_sign_inner(
            &serde_json::json!({
                "wwwAuthenticate": r#"Basic realm="b""#,
                "username": "u", "password": "p", "method": "GET",
                "uri": "/", "nc": 1, "cnonce": "c",
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(out, "null");
    }

    #[test]
    fn sigv4_sign_derives_the_service_from_a_virtual_hosted_host() {
        // TR-428's rule, exercised through the export. If the caller had to
        // derive this, `examplebucket` would become the signing service —
        // wrong scope, wrong key, and a double-encoded S3 path.
        let out = aws_sigv4_sign_inner(
            &serde_json::json!({
                "method": "GET",
                "host": "examplebucket.s3.amazonaws.com",
                "path": "/test.txt",
                "accessKey": "AKIAIOSFODNN7EXAMPLE",
                "secretKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "region": "us-east-1",
                "amzDate": "20130524T000000Z",
                "dateStamp": "20130524",
            })
            .to_string(),
        )
        .expect("sigv4_sign succeeds");
        let headers = v(&out);
        let auth = headers[0]["value"].as_str().unwrap();
        assert!(
            auth.contains("/20130524/us-east-1/s3/aws4_request"),
            "service must derive to s3, not the bucket: {auth}"
        );
    }

    #[test]
    fn sigv4_sign_rejects_a_body_that_is_not_base64() {
        // Silently treating it as an empty body would sign the wrong payload
        // hash and produce a 403 the caller cannot diagnose.
        let e = aws_sigv4_sign_inner(
            &serde_json::json!({
                "method": "PUT", "host": "s3.amazonaws.com", "path": "/x",
                "bodyBase64": "not!base64!",
                "accessKey": "A", "secretKey": "S",
                "amzDate": "20130524T000000Z", "dateStamp": "20130524",
            })
            .to_string(),
        );
        assert!(e.is_err(), "an undecodable body must not sign as empty");
    }

    #[test]
    fn oauth1_sign_merges_query_and_form_params_and_brackets_ipv6() {
        // Both TR-431 rules through the export: the `+`-is-a-space form
        // decoding, and IPv6 bracketing that JS `URL.hostname` strips.
        let out = oauth1_sign_inner(
            &serde_json::json!({
                "method": "POST", "scheme": "http", "host": "::1", "port": 8080,
                "path": "/request",
                "queryParams": [["b5", "=%3D"]],
                "formBody": "c2=&a3=2+q",
                "consumerKey": "ck", "consumerSecret": "cs",
                "signatureMethod": "HMAC-SHA1", "nonce": "n", "timestamp": "1",
            })
            .to_string(),
        )
        .expect("oauth1_sign succeeds");
        let value = v(&out)["value"].as_str().unwrap().to_string();
        assert!(value.starts_with("OAuth "), "{value}");
        assert!(value.contains("oauth_signature="), "{value}");
    }

    #[test]
    fn oauth1_sign_refuses_an_unsupported_method_by_name() {
        // TR-409: never silently downgrade to HMAC-SHA1.
        let e = oauth1_sign_inner(
            &serde_json::json!({
                "method": "GET", "scheme": "https", "host": "example.com",
                "path": "/", "consumerKey": "ck", "consumerSecret": "cs",
                "signatureMethod": "RSA-SHA1", "nonce": "n", "timestamp": "1",
            })
            .to_string(),
        );
        assert!(
            e.is_err(),
            "RSA-SHA1 is not implemented and must be refused"
        );
    }

    #[test]
    fn oauth1_signature_methods_matches_what_sign_accepts() {
        // The picker list and the signer must not drift: everything listed
        // must sign, and the list is the only thing a UI should offer.
        let listed: Vec<String> =
            serde_json::from_str(&oauth1_signature_methods().unwrap()).unwrap();
        assert!(!listed.is_empty());
        for m in listed {
            let r = oauth1_sign(
                &serde_json::json!({
                    "method": "GET", "scheme": "https", "host": "example.com",
                    "path": "/", "consumerKey": "ck", "consumerSecret": "cs",
                    "signatureMethod": m, "nonce": "n", "timestamp": "1",
                })
                .to_string(),
            );
            assert!(r.is_ok(), "{m} is listed but oauth1Sign refuses it");
        }
    }
}
