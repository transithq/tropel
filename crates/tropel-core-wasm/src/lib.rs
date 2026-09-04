//! `tropel-core-wasm` — the eager-loaded core tier for browser embedders
//! (KnockPort). wasm32-unknown-unknown + wasm-bindgen; deliberately NO
//! QuickJS: see `API_CLIENT_WEB_PAYLOAD.md` §2.3 (two-tier wasm). The
//! website/web app only ever talks to HTTP through a relay (CORS), so the
//! heavy `tropel-web` (wasip1 + QuickJS) scenario slice is extension/native
//! territory; this crate covers the pure compute the page always needs —
//! starting with the dynamic-variable catalog.
//!
//! Exports are thin adapters over `tropel-variables` — the catalog itself is
//! NOT duplicated here, so the website, the extension and the native runner
//! cannot drift.

use std::sync::OnceLock;

use tropel_auth::oauth::{
    attach_token, build_authorize_url, build_token_request, decode_jwt, jwt_expires_at,
    parse_token_response, sign_jwt, sign_wsse, AuthorizeParams, JwtAlgorithm, StoredToken,
    TokenPlacement, TokenRequestParams, WsseParams,
};
use tropel_variables::{
    DynamicCatalog, VariableResolver, VariableScope, MAX_VARIABLE_RESOLUTION_PASSES,
    PREDEFINED_VARIABLE_META,
};
use wasm_bindgen::prelude::*;

static CATALOG: OnceLock<DynamicCatalog> = OnceLock::new();

fn catalog() -> &'static DynamicCatalog {
    CATALOG.get_or_init(DynamicCatalog::new)
}

/// Resolve every predefined dynamic variable (`{{$guid}}`, `{{$timestamp}}`,
/// …) in the input. Each occurrence generates a fresh value; unknown `{{$…}}`
/// names survive as literal placeholders (Tropel semantics). Plain `{{var}}`
/// references are untouched — the embedder resolves those against its own
/// environment/collection maps.
///
/// TR-403: returns `Result` — a total-output cap (16 MiB) produces a JS
/// `Error` with the message naming the limit, so the consumer never receives
/// silently truncated data on the wire.
#[wasm_bindgen(js_name = "resolveVariables")]
pub fn resolve_variables(input: &str) -> Result<String, wasm_bindgen::JsValue> {
    catalog()
        .resolve(input)
        .map_err(|msg| wasm_bindgen::JsValue::from_str(&msg))
}

/// Resolve plain `{{var}}` references against the embedder's variable map.
///
/// ## Why this exists
///
/// `resolveVariables` above deliberately leaves plain `{{var}}` alone, and its
/// doc comment told embedders to "resolve those against its own
/// environment/collection maps". KnockPort did exactly that — and its
/// TypeScript resolver diverged from this one in two ways that reach the wire
/// silently:
///
/// 1. **Grammar.** It matched `[\w.:]+` where this resolver matches `[^{}]+`,
///    so `{{base-url}}`, `{{x-api-key}}` and `{{user-id}}` — the commonest
///    naming convention in imported Postman collections — resolved in a load
///    run and went out as LITERAL TEXT from the app.
/// 2. **Escaping.** It had one raw-substitution mode for every field. This
///    resolver has three, and the runner uses `resolve_json_deep` for bodies
///    and `resolve_url_deep` for URLs. A value containing `"` therefore
///    produced invalid JSON from the app and correct JSON from a run — a bug
///    already fixed here once (backlog line 135) and independently
///    reintroduced there.
///
/// The implementation already existed in `tropel-variables` (whose `lib.rs`
/// does `pub use resolver::*`); only the facade was missing.
///
/// **Cost, measured — not zero.** `lto = "thin"` strips unreachable code, and
/// nothing called `resolve_json`/`resolve_url`/`resolve_deep`, so the linker
/// had removed them from the shipped artifact. Making them reachable costs
/// **+17,515 B raw / +11,949 B gzip** (583,680 → 601,195 B, ~2 % of the tier,
/// still 98,805 B under the 700,000 B budget). Most of it is the
/// `serde_json` deserializer for the variable map, which was also unreachable
/// before. Worth stating plainly because the first write-up of this task
/// claimed "already in the bytes, zero payload" — true of the crate, false of
/// the artifact.
///
/// ## Contract
///
/// - `vars_json` is a FLAT `{"name": "value"}` object. Scope layering
///   (globals < collection < env < folder < request < runtime) stays the
///   embedder's job — that is data-merging over its own scope model, not
///   execution. The map lands in `VariableScope::env`, whose values are
///   returned verbatim with no JSON round-trip.
/// - `mode` selects the escaper, and callers MUST pick per field:
///   `"json"` for a JSON body, `"url"` for a URL, `"plain"` elsewhere.
/// - `deep` runs the multi-pass resolver so `{{host_{{suffix}}}}` and
///   `{{a}}`→`{{b}}` chains resolve, capped at
///   [`maxVariableResolutionPasses`].
/// - An unknown name survives as a literal `{{name}}` — visible, never
///   silently emptied.
/// - An unparseable `vars_json` is a JS `Error`, never a silent passthrough:
///   a template reaching the wire unresolved is the failure this whole export
///   exists to prevent.
#[wasm_bindgen(js_name = "resolveTemplate")]
pub fn resolve_template(
    input: &str,
    vars_json: &str,
    mode: &str,
    deep: bool,
) -> Result<String, wasm_bindgen::JsValue> {
    resolve_template_inner(input, vars_json, mode, deep)
        .map_err(|msg| wasm_bindgen::JsValue::from_str(&msg))
}

/// The whole of `resolveTemplate`, minus the `JsValue` conversion.
///
/// Split out so the semantics are testable in a NATIVE build: `JsValue` cannot
/// be constructed off wasm32, which is why the oauth adapters below are only
/// covered by the JS smoke test. These semantics are the two things that
/// already diverged once in a TypeScript re-implementation, so they get real
/// unit tests rather than a note explaining why they have none.
fn resolve_template_inner(
    input: &str,
    vars_json: &str,
    mode: &str,
    deep: bool,
) -> Result<String, String> {
    let env: std::collections::HashMap<String, String> =
        serde_json::from_str(vars_json).map_err(|e| {
            format!("resolveTemplate: vars must be a flat JSON object of string values ({e})")
        })?;

    let scope = VariableScope {
        env,
        ..Default::default()
    };
    let resolver = VariableResolver::new();

    Ok(match (mode, deep) {
        ("plain", false) => resolver.resolve(input, &scope),
        ("plain", true) => resolver.resolve_deep(input, &scope, MAX_VARIABLE_RESOLUTION_PASSES),
        ("json", false) => resolver.resolve_json(input, &scope),
        ("json", true) => resolver.resolve_json_deep(input, &scope, MAX_VARIABLE_RESOLUTION_PASSES),
        ("url", false) => resolver.resolve_url(input, &scope),
        ("url", true) => resolver.resolve_url_deep(input, &scope, MAX_VARIABLE_RESOLUTION_PASSES),
        // Named refusal rather than a silent fallback to "plain": picking the
        // wrong escaper is exactly how a quote-bearing value corrupts a JSON
        // body, so a typo in the mode must fail loudly.
        (other, _) => {
            return Err(format!(
                "resolveTemplate: unknown mode {other:?} — expected \"plain\", \"json\" or \"url\""
            ))
        }
    })
}

/// `resolveTemplate`, plus WHY resolution stopped — as a JSON string:
/// `{"value": "...", "hitCap": bool, "unresolved": ["name", …]}`.
///
/// The distinction matters and only the resolver's loop can make it. A chain
/// that never settles (`a` → `b` → `a`) and an unknown name BOTH leave a
/// literal `{{…}}` in the output, but they deserve opposite treatment: the
/// first is a config error worth failing loudly with the chain named, the
/// second must stay a visible literal and send.
///
/// - `hitCap` — the pass budget ran out while the text was still CHANGING.
///   That is a cycle; an unknown name stabilizes on the first pass.
/// - `unresolved` — placeholder names still present, first-occurrence order,
///   read with the SAME regex that drives resolution, so an embedder cannot
///   disagree about what counts as a placeholder.
///
/// Exists so an embedder keeps its loud, chain-naming error WITHOUT growing a
/// second cycle detector — which is how the grammar diverged the first time.
#[wasm_bindgen(js_name = "resolveTemplateDetailed")]
pub fn resolve_template_detailed(
    input: &str,
    vars_json: &str,
    mode: &str,
) -> Result<String, wasm_bindgen::JsValue> {
    resolve_template_detailed_inner(input, vars_json, mode)
        .map_err(|msg| wasm_bindgen::JsValue::from_str(&msg))
}

fn resolve_template_detailed_inner(
    input: &str,
    vars_json: &str,
    mode: &str,
) -> Result<String, String> {
    let env: std::collections::HashMap<String, String> =
        serde_json::from_str(vars_json).map_err(|e| {
            format!(
                "resolveTemplateDetailed: vars must be a flat JSON object of string values ({e})"
            )
        })?;
    let scope = VariableScope {
        env,
        ..Default::default()
    };
    let outcome = VariableResolver::new()
        .resolve_reporting(input, &scope, MAX_VARIABLE_RESOLUTION_PASSES, mode)
        .map_err(|e| format!("resolveTemplateDetailed: {e}"))?;
    Ok(serde_json::json!({
        "value": outcome.value,
        "hitCap": outcome.hit_cap,
        "unresolved": outcome.unresolved,
    })
    .to_string())
}

/// The `{{a}}`→`{{b}}` chain cap the multi-pass resolver enforces (Postman
/// documents 20). Exported so an embedder reports the SAME ceiling instead of
/// inventing a second one.
#[wasm_bindgen(js_name = "maxVariableResolutionPasses")]
pub fn max_variable_resolution_passes() -> usize {
    MAX_VARIABLE_RESOLUTION_PASSES
}

/// Catalog metadata as a JSON string: `[{"name":"$guid","description":…},…]`.
/// Feed the names into editor autocomplete; the descriptions into tooltips.
#[wasm_bindgen(js_name = "predefinedVariablesMeta")]
pub fn predefined_variables_meta() -> String {
    let entries = PREDEFINED_VARIABLE_META.iter().map(|m| {
        format!(
            "{{\"name\":\"{}\",\"description\":\"{}\"}}",
            m.name, m.description
        )
    });
    format!("[{}]", entries.collect::<Vec<_>>().join(","))
}

// ── OAuth2 flows (tropel-auth::oauth, pure — the embedder sends the requests) ─────

fn err(e: impl std::fmt::Display) -> JsValue {
    // ASCII-only: the web-target glue truncates strings at their UTF-16 code
    // unit count when encoding into wasm memory, so any multi-byte UTF-8
    // character would corrupt the message at the JS boundary.
    JsValue::from_str(
        &e.to_string()
            .chars()
            .map(|c| if c.is_ascii() { c } else { '?' })
            .collect::<String>(),
    )
}

fn parse_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, JsValue> {
    serde_json::from_str(s).map_err(err)
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, JsValue> {
    serde_json::to_string(v).map_err(err)
}

/// Compute the S256 PKCE challenge for a host-side verifier → JSON
/// `{code_verifier, code_challenge_method:"S256", code_challenge}`. The
/// verifier is generated by the embedder (`crypto.getRandomValues`, 43–128
/// chars of the RFC 7636 charset); the challenge is computed here so it
/// matches the token-request builder byte for byte. Validates length.
#[wasm_bindgen(js_name = "oauth2GeneratePkcePair")]
pub fn oauth2_generate_pkce_pair(verifier: &str) -> Result<String, JsValue> {
    if verifier.len() < 43 || verifier.len() > 128 {
        return Err(err("code_verifier must be 43-128 characters (RFC 7636)"));
    }
    let pair = tropel_auth::oauth::PkcePair {
        code_verifier: verifier.to_string(),
        code_challenge_method: "S256".to_string(),
        code_challenge: tropel_auth::oauth::code_challenge_s256(verifier),
    };
    to_json(&pair)
}

/// Build the OAuth2 authorize URL (authorization_code / implicit) from JSON
/// `AuthorizeParams` `{auth_url, client_id, redirect_uri, scopes[],
/// response_type?, state?, pkce?{code_verifier,code_challenge_method},
/// extra?[[k,v]]}` → JSON `{url, state, code_verifier?}`. Throws on invalid
/// input.
#[wasm_bindgen(js_name = "oauth2BuildAuthorizeUrl")]
pub fn oauth2_build_authorize_url(params_json: &str) -> Result<String, JsValue> {
    let params: AuthorizeParams = parse_json(params_json)?;
    let req = build_authorize_url(&params).map_err(err)?;
    to_json(&req)
}

/// Build the OAuth2 token-endpoint POST from JSON `TokenRequestParams`
/// `{grant_type: "authorization_code"|"client_credentials"|"password"|
/// "refresh_token", token_url, client_id, client_secret?, auth_method?:
/// "basic"|"post_body", code?, redirect_uri?, code_verifier?, username?,
/// password?, refresh_token?, scopes[]}` → JSON `{url, body,
/// basic_auth_header?, content_type}`.
#[wasm_bindgen(js_name = "oauth2BuildTokenRequest")]
pub fn oauth2_build_token_request(params_json: &str) -> Result<String, JsValue> {
    let params: TokenRequestParams = parse_json(params_json)?;
    let req = build_token_request(&params).map_err(err)?;
    to_json(&req)
}

/// Parse a token-endpoint response body → JSON `{access_token, token_type?,
/// expires_in?, refresh_token?, scope?, id_token?}`. Throws on `error`
/// payloads (RFC 6749 §5.2) and malformed JSON.
#[wasm_bindgen(js_name = "oauth2ParseTokenResponse")]
pub fn oauth2_parse_token_response(body: &str) -> Result<String, JsValue> {
    let tr = parse_token_response(body).map_err(err)?;
    to_json(&tr)
}

/// Fold a parsed token response into a stored token with an absolute
/// `expires_at` (host clock at call time). Input: parsed-response JSON.
/// Output: JSON `{access_token, token_type, refresh_token?, expires_at?,
/// scope?}`.
#[wasm_bindgen(js_name = "oauth2StoreToken")]
pub fn oauth2_store_token(parsed_json: &str) -> Result<String, JsValue> {
    let tr: tropel_auth::oauth::TokenResponse = parse_json(parsed_json)?;
    to_json(&StoredToken::from_response(&tr))
}

/// Is a stored token expired? Input: stored-token JSON + skew seconds
/// (e.g. 60). Tokens without `expires_at` are never expired.
#[wasm_bindgen(js_name = "oauth2IsTokenExpired")]
pub fn oauth2_is_token_expired(token_json: &str, skew_secs: i64) -> Result<bool, JsValue> {
    let token: StoredToken = parse_json(token_json)?;
    Ok(token.is_expired(skew_secs))
}

/// Position a token on a request → JSON `{kind:"header"|"query", key, value}`
/// from JSON `TokenAttachment`. `placement` is `"header"` or `"query"`;
/// `header_prefix`/`query_key` may be empty strings for defaults
/// (`Bearer` / `access_token`).
#[wasm_bindgen(js_name = "oauth2AttachToken")]
pub fn oauth2_attach_token(
    token: &str,
    token_type: &str,
    placement: &str,
    header_prefix: &str,
    query_key: &str,
) -> Result<String, JsValue> {
    let placement = match placement {
        "header" => TokenPlacement::Header,
        "query" => TokenPlacement::Query,
        other => return Err(err(format!("unknown placement: {other}"))),
    };
    let att = attach_token(
        token,
        if token_type.is_empty() {
            None
        } else {
            Some(token_type)
        },
        placement,
        if header_prefix.is_empty() {
            None
        } else {
            Some(header_prefix)
        },
        if query_key.is_empty() {
            None
        } else {
            Some(query_key)
        },
    );
    to_json(&att)
}

/// Decode a compact JWT (no signature verification — clients display tokens,
/// they don't trust them) → JSON `{header, payload, signature}`.
#[wasm_bindgen(js_name = "oauth2DecodeJwt")]
pub fn oauth2_decode_jwt(token: &str) -> Result<String, JsValue> {
    let jwt = decode_jwt(token).map_err(err)?;
    to_json(&jwt)
}

/// The JWT `exp` claim (UNIX seconds) or `-1` when absent. Throws on a
/// malformed token.
#[wasm_bindgen(js_name = "oauth2JwtExpiresAt")]
pub fn oauth2_jwt_expires_at(token: &str) -> Result<i64, JsValue> {
    Ok(jwt_expires_at(token).map_err(err)?.unwrap_or(-1))
}

/// Sign a compact JWT with an HMAC-SHA2 algorithm and return the compact
/// `header.payload.signature` string. `header_json` may be `""` (defaults to
/// `{"alg","typ":"JWT"}`) — when present it must be a JSON object; `alg` is
/// always replaced with the algorithm actually used, `typ` filled in when
/// missing. `payload_json` must be a JSON object. `algorithm` is `HS256`,
/// `HS384` or `HS512`.
#[wasm_bindgen(js_name = "oauth2SignJwt")]
pub fn oauth2_sign_jwt(
    header_json: &str,
    payload_json: &str,
    algorithm: &str,
    secret: &str,
) -> Result<String, JsValue> {
    let header: Option<serde_json::Value> = if header_json.is_empty() {
        None
    } else {
        Some(parse_json(header_json)?)
    };
    let payload: serde_json::Value = parse_json(payload_json)?;
    let algorithm = match algorithm {
        "HS256" => JwtAlgorithm::Hs256,
        "HS384" => JwtAlgorithm::Hs384,
        "HS512" => JwtAlgorithm::Hs512,
        other => return Err(err(format!("unknown algorithm: {other}"))),
    };
    sign_jwt(header.as_ref(), &payload, algorithm, secret).map_err(err)
}

// ── WSSE UsernameToken (SOAP, SHA-1 digest profile) ──────────────────────────

/// Build a WSSE UsernameToken security header set → JSON
/// `{authorization, nonce, created}` from JSON `WsseParams`
/// `{username, password, nonce?, created?}`. Empty nonce/created are
/// generated (base64 random nonce + host-clock RFC 3339 timestamp); the
/// embedder attaches `authorization` as the `Authorization` header.
#[wasm_bindgen(js_name = "wsseSign")]
pub fn wsse_sign(params_json: &str) -> Result<String, JsValue> {
    let params: WsseParams = parse_json(params_json)?;
    to_json(&sign_wsse(&params).map_err(err)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_guid() {
        let out = resolve_variables("id={{$guid}}").unwrap();
        assert!(out.starts_with("id="));
        let guid = out.strip_prefix("id=").unwrap();
        assert_eq!(guid.len(), 36);
        assert!(!out.contains("{{$guid}}"));
    }

    #[test]
    fn plain_vars_survive() {
        let out = resolve_variables("{{baseUrl}}/x").unwrap();
        assert_eq!(out, "{{baseUrl}}/x");
    }

    #[test]
    fn meta_is_well_formed_json() {
        let json = predefined_variables_meta();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.len() >= 30, "metadata covers the catalog");
        assert_eq!(parsed[0]["name"], "$guid");
        for entry in &parsed {
            assert!(entry["name"].as_str().unwrap().starts_with('$'));
            assert!(!entry["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn meta_names_all_resolve() {
        let json = predefined_variables_meta();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        for entry in parsed {
            let name = entry["name"].as_str().unwrap();
            // Parameterized entries carry an argument in their description
            // example (`{{$randomString:16}}`); resolve the bare form.
            let resolved = catalog().resolve(&format!("{{{{{name}}}}}")).unwrap();
            assert!(
                !resolved.contains(&format!("{{{{{name}}}}}")),
                "{name} must resolve"
            );
        }
    }

    // The oauth adapter layer is exercised end-to-end against the REAL wasm
    // by packages/core-wasm/smoke.mjs — JsValue cannot be constructed in
    // native test builds, so it is covered here only via tropel-oauth's own
    // unit tests (the adapters are serde pass-throughs).

    // ── resolveTemplate ──────────────────────────────────────────────────
    // These pin the two behaviours a TypeScript re-implementation already got
    // wrong once. Both were silent: the wrong bytes reached the wire and
    // nothing failed.

    fn resolve(input: &str, vars: &str, mode: &str) -> String {
        resolve_template_inner(input, vars, mode, true).expect("resolves")
    }

    #[test]
    fn hyphenated_names_resolve() {
        // The divergence: a `[\w.:]+` grammar cannot match a hyphen, so
        // `{{base-url}}` went out as literal text while a load run resolved
        // it. Hyphens are the commonest convention in imported collections.
        let vars = r#"{"base-url":"https://api.test","x-api-key":"k","user-id":"7"}"#;
        assert_eq!(
            resolve("{{base-url}}/v1", vars, "plain"),
            "https://api.test/v1"
        );
        assert_eq!(resolve("{{x-api-key}}", vars, "plain"), "k");
        assert_eq!(resolve("{{user-id}}", vars, "plain"), "7");
    }

    #[test]
    fn json_mode_escapes_quotes_and_control_chars() {
        // The divergence: one raw-substitution mode for every field, so a
        // value containing a quote produced INVALID JSON from the app and
        // valid JSON from a run.
        let vars = r#"{"greeting":"He said \"hi\"","multi":"a\nb"}"#;
        let out = resolve(r#"{"msg":"{{greeting}}"}"#, vars, "json");
        assert_eq!(out, r#"{"msg":"He said \"hi\""}"#);
        serde_json::from_str::<serde_json::Value>(&out).expect("stays parseable");

        let nl = resolve(r#"{"m":"{{multi}}"}"#, vars, "json");
        serde_json::from_str::<serde_json::Value>(&nl).expect("newline stays parseable");
    }

    #[test]
    fn json_mode_leaves_a_bare_fragment_raw() {
        // Quote-parity: a placeholder INSIDE a string literal is escaped, one
        // standing alone as a value is not — that is what lets a variable
        // carry a JSON object into a body.
        let vars = r#"{"filter":"{\"a\":1}"}"#;
        let out = resolve(r#"{"filter": {{filter}}}"#, vars, "json");
        assert_eq!(out, r#"{"filter": {"a":1}}"#);
        serde_json::from_str::<serde_json::Value>(&out).expect("fragment stays parseable");
    }

    #[test]
    fn url_mode_inserts_raw() {
        // Postman does no percent-encoding here, so a structural URL inside a
        // value survives resolution.
        let vars = r#"{"endpoint":"https://api.test/a b?x=1&y=2"}"#;
        assert_eq!(
            resolve("{{endpoint}}", vars, "url"),
            "https://api.test/a b?x=1&y=2"
        );
    }

    #[test]
    fn unknown_names_stay_literal() {
        // Visible, never silently emptied.
        assert_eq!(resolve("{{nope}}", "{}", "plain"), "{{nope}}");
    }

    #[test]
    fn deep_resolves_chains_and_nested_names() {
        let vars = r#"{"a":"{{b}}","b":"done","suffix":"dev","host_dev":"h"}"#;
        assert_eq!(resolve("{{a}}", vars, "plain"), "done");
        assert_eq!(resolve("{{host_{{suffix}}}}", vars, "plain"), "h");
        // Shallow does NOT chase the chain — the two modes stay distinct.
        assert_eq!(
            resolve_template_inner("{{a}}", vars, "plain", false).unwrap(),
            "{{b}}"
        );
    }

    #[test]
    fn a_cycle_terminates_at_the_pass_cap() {
        // Never a hang: the cap is the ceiling and it is the SAME number the
        // embedder reads from maxVariableResolutionPasses().
        let vars = r#"{"a":"{{b}}","b":"{{a}}"}"#;
        let out = resolve("{{a}}", vars, "plain");
        assert!(
            out.contains("{{"),
            "an unresolvable cycle stays visible: {out}"
        );
        assert_eq!(
            max_variable_resolution_passes(),
            MAX_VARIABLE_RESOLUTION_PASSES
        );
    }

    #[test]
    fn detailed_separates_a_cycle_from_an_unknown_name() {
        // Both leave a literal {{…}} in the output; only the loop can tell
        // them apart, and an embedder needs the difference to decide between
        // "fail loudly naming the chain" and "send it, the name is visible".
        let cyc = resolve_template_detailed_inner("{{a}}", r#"{"a":"{{b}}","b":"{{a}}"}"#, "plain")
            .unwrap();
        let cyc: serde_json::Value = serde_json::from_str(&cyc).unwrap();
        assert_eq!(cyc["hitCap"], true, "a cycle exhausts the budget: {cyc}");
        assert!(!cyc["unresolved"].as_array().unwrap().is_empty());

        let unk = resolve_template_detailed_inner("{{nope}}", "{}", "plain").unwrap();
        let unk: serde_json::Value = serde_json::from_str(&unk).unwrap();
        assert_eq!(
            unk["hitCap"], false,
            "an unknown name settles at once: {unk}"
        );
        assert_eq!(unk["unresolved"], serde_json::json!(["nope"]));
        assert_eq!(unk["value"], "{{nope}}");

        // A chain that DOES settle reports neither.
        let ok = resolve_template_detailed_inner("{{a}}", r#"{"a":"{{b}}","b":"done"}"#, "plain")
            .unwrap();
        let ok: serde_json::Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(ok["value"], "done");
        assert_eq!(ok["hitCap"], false);
        assert_eq!(ok["unresolved"], serde_json::json!([]));
    }

    #[test]
    fn detailed_reports_a_chain_deeper_than_the_cap() {
        // v0 → v1 → … → v21: deeper than the 20-pass budget, so it is
        // reported the same way a cycle is — the embedder's ceiling and this
        // one are the same number by construction.
        let mut vars = serde_json::Map::new();
        for i in 0..=MAX_VARIABLE_RESOLUTION_PASSES {
            vars.insert(
                format!("v{i}"),
                serde_json::json!(format!("{{{{v{}}}}}", i + 1)),
            );
        }
        vars.insert(
            format!("v{}", MAX_VARIABLE_RESOLUTION_PASSES + 1),
            serde_json::json!("end"),
        );
        let out = resolve_template_detailed_inner(
            "{{v0}}",
            &serde_json::Value::Object(vars).to_string(),
            "plain",
        )
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["hitCap"], true, "{out}");
    }

    #[test]
    fn bad_input_fails_loudly() {
        // A typo'd mode must not fall back to "plain" — that is precisely how
        // a quote-bearing value would corrupt a JSON body.
        let err = resolve_template_inner("x", "{}", "jsonn", true).unwrap_err();
        assert!(err.contains("unknown mode"), "{err}");
        // And a malformed map is an error, never a silent passthrough.
        let err = resolve_template_inner("x", "[]", "plain", true).unwrap_err();
        assert!(err.contains("flat JSON object"), "{err}");
    }
}
