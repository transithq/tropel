//! Pure OAuth2 flow helpers (RFC 6749 + RFC 7636 PKCE) and JWT decoding.
//!
//! ZERO transport: every builder produces plain data (`AuthorizeRequest`,
//! `TokenRequest`, header/query pairs) and the embedder performs the HTTP
//! round-trip, then feeds the response body back through `parse_token_response`.
//! That keeps this module compilable to `wasm32-unknown-unknown` (the Tropel
//! core tier) and lets native hosts reuse the exact same logic.
//!
//! Not implemented (by design, absent upstream too): automatic token
//! persistence, clock skew policy — the embedder owns the credential store.

use std::fmt::Write as _;

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use rand::{Rng, RngExt};

/// RFC 3986 unreserved characters stay unencoded in query/form values.
const UNRESERVED: AsciiSet = NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("invalid oauth input: {0}")]
    Invalid(String),
    #[error("token response error: {error}{detail}", detail = error_description.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    TokenError {
        error: String,
        error_description: Option<String>,
    },
    #[error("invalid JWT: {0}")]
    InvalidJwt(String),
}

pub type Result<T> = std::result::Result<T, OauthError>;

// ── Clock ────────────────────────────────────────────────────────────────────
// Same pattern as tropel-variables: std's SystemTime panics on
// wasm32-unknown-unknown; web-time reads the host Date.now() there.

#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
fn now_epoch_secs() -> i64 {
    now_epoch_millis().div_euclid(1000)
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
fn now_epoch_millis() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn now_epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// RFC 3339 timestamp (UTC, millisecond precision) from UNIX epoch millis.
fn epoch_millis_to_rfc3339(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        ms
    )
}

// ── PKCE (RFC 7636) ──────────────────────────────────────────────────────────

const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Generate a PKCE code verifier: 43–128 chars from the RFC 7636 charset.
/// Default length is 128 (maximum entropy).
pub fn generate_code_verifier(length: usize) -> String {
    let len = length.clamp(43, 128);
    let mut rng = rand::rng();
    (0..len)
        .map(|_| VERIFIER_CHARS[rng.random_range(0..VERIFIER_CHARS.len())] as char)
        .collect()
}

/// Derive the S256 code challenge: `BASE64URL(SHA256(verifier))`.
pub fn code_challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A generated PKCE pair ready to attach to the authorize + token requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkcePair {
    pub code_verifier: String,
    /// The challenge method; always `S256` (plain is insecure and obsolete).
    pub code_challenge_method: String,
    pub code_challenge: String,
}

impl PkcePair {
    pub fn generate() -> Self {
        let code_verifier = generate_code_verifier(128);
        let code_challenge = code_challenge_s256(&code_verifier);
        Self {
            code_verifier,
            code_challenge_method: "S256".to_string(),
            code_challenge,
        }
    }
}

fn random_urlsafe_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

// ── Authorization request (authorization_code + implicit) ────────────────────

/// Client authentication strategy for the token endpoint (RFC 6749 §2.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientAuthMethod {
    /// HTTP Basic (`Authorization: Basic base64(id:secret)`) — recommended.
    Basic,
    /// `client_id`/`client_secret` in the form body.
    PostBody,
}

/// Parameters for building the browser `authorization_code` (or implicit)
/// authorize URL. All `{{var}}` templates must be resolved by the embedder
/// before calling — this module does no variable interpolation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthorizeParams {
    pub auth_url: String,
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// `code` (authorization_code) or `token` (implicit).
    #[serde(default = "default_response_type")]
    pub response_type: String,
    pub state: Option<String>,
    pub pkce: Option<GeneratedPkce>,
    /// Extra parameters passed through verbatim (e.g. audience, resource).
    #[serde(default)]
    pub extra: Vec<(String, String)>,
}
fn default_response_type() -> String {
    "code".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedPkce {
    pub code_verifier: String,
    /// Empty means `S256` (plain method still accepted for legacy servers).
    #[serde(default)]
    pub code_challenge_method: String,
    /// Optional for the wire surface: the authorize builder re-derives the
    /// challenge from the verifier + method when this is omitted, so the
    /// challenge on the URL always matches the verifier on file.
    #[serde(default)]
    pub code_challenge: String,
}

impl From<PkcePair> for GeneratedPkce {
    fn from(p: PkcePair) -> Self {
        Self {
            code_verifier: p.code_verifier,
            code_challenge_method: p.code_challenge_method,
            code_challenge: p.code_challenge,
        }
    }
}

/// A fully-query-encoded authorize URL plus the state/verifier the embedder
/// must remember for the token exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizeRequest {
    pub url: String,
    pub state: Option<String>,
    /// Present when PKCE was requested — keep for the token exchange.
    pub code_verifier: Option<String>,
}

/// Build the authorization URL the user's browser is sent to.
pub fn build_authorize_url(params: &AuthorizeParams) -> Result<AuthorizeRequest> {
    if params.auth_url.is_empty() {
        return Err(OauthError::Invalid("auth_url is required".into()));
    }
    let response_type = if params.response_type.is_empty() {
        "code".to_string()
    } else {
        params.response_type.clone()
    };
    if response_type != "code" && response_type != "token" {
        return Err(OauthError::Invalid(format!(
            "unsupported response_type: {response_type}"
        )));
    }
    let state = params
        .state
        .clone()
        .unwrap_or_else(|| random_urlsafe_token(24));
    let pkce = match &params.pkce {
        Some(p) => {
            if p.code_verifier.len() < 43 || p.code_verifier.len() > 128 {
                return Err(OauthError::Invalid(
                    "code_verifier must be 43-128 characters (RFC 7636)".into(),
                ));
            }
            let method = if p.code_challenge_method.is_empty() {
                "S256".to_string()
            } else {
                p.code_challenge_method.clone()
            };
            if method != "S256" && method != "plain" {
                return Err(OauthError::Invalid(format!(
                    "unsupported code_challenge_method: {method}"
                )));
            }
            let challenge = if method == "S256" {
                code_challenge_s256(&p.code_verifier)
            } else {
                p.code_verifier.clone()
            };
            Some((method, challenge))
        }
        None => None,
    };

    let mut q: Vec<(String, String)> = vec![
        ("response_type".into(), response_type.clone()),
        ("client_id".into(), params.client_id.clone()),
        ("state".into(), state.clone()),
    ];
    if response_type == "code" && !params.redirect_uri.is_empty() {
        q.push(("redirect_uri".into(), params.redirect_uri.clone()));
    }
    if !params.scopes.is_empty() {
        q.push(("scope".into(), params.scopes.join(" ")));
    }
    if let Some((method, challenge)) = &pkce {
        q.push(("code_challenge_method".into(), method.clone()));
        q.push(("code_challenge".into(), challenge.clone()));
    }
    q.extend(params.extra.iter().cloned());

    let sep = if params.auth_url.contains('?') {
        '&'
    } else {
        '?'
    };
    let query = q
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, &UNRESERVED),
                utf8_percent_encode(v, &UNRESERVED)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    Ok(AuthorizeRequest {
        url: format!("{}{}{}", params.auth_url, sep, query),
        state: Some(state),
        code_verifier: params.pkce.as_ref().map(|p| p.code_verifier.clone()),
    })
}

// ── Token requests ───────────────────────────────────────────────────────────

/// Grant types the builders support.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantType {
    #[default]
    AuthorizationCode,
    ClientCredentials,
    Password,
    RefreshToken,
    /// RFC 8628 device authorization grant. Two token-endpoint requests: the
    /// initial one (`grant_type=device_code`) returns `device_code` +
    /// `user_code` + `verification_uri`; the POLL request uses the URN grant
    /// type (`urn:ietf:params:oauth:grant-type:device_code`) with the
    /// `device_code` field (see `TokenRequestParams.device_code`).
    DeviceCode,
}

impl GrantType {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantType::AuthorizationCode => "authorization_code",
            GrantType::ClientCredentials => "client_credentials",
            GrantType::Password => "password",
            GrantType::RefreshToken => "refresh_token",
            GrantType::DeviceCode => "device_code",
        }
    }
}

/// Parameters for a token-endpoint request. Fields outside the chosen grant
/// are ignored; `code_verifier` is required for authorization_code + PKCE.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenRequestParams {
    pub grant_type: GrantType,
    pub token_url: String,
    /// Empty for anonymous/public clients (client_credentials may also
    /// carry the id only inside the Basic auth header).
    #[serde(default)]
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_method: Option<ClientAuthMethod>,
    // authorization_code
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_verifier: Option<String>,
    // password
    pub username: Option<String>,
    pub password: Option<String>,
    // refresh_token
    pub refresh_token: Option<String>,
    // device_code (RFC 8628): when set, the token request is the POLL phase
    // (grant_type=urn:ietf:params:oauth:grant-type:device_code + device_code).
    pub device_code: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// A sendable token-endpoint request: URL, form body, and (optional) Basic
/// auth header. The embedder performs the POST.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenRequest {
    pub url: String,
    /// `application/x-www-form-urlencoded` body.
    pub body: String,
    /// `Authorization: Basic …` when client_secret is sent via Basic auth
    /// (empty otherwise — secret travels in the body then).
    pub basic_auth_header: Option<String>,
    pub content_type: String,
}

/// Build the token-endpoint POST for any supported grant.
pub fn build_token_request(params: &TokenRequestParams) -> Result<TokenRequest> {
    use base64::engine::general_purpose::STANDARD;
    if params.token_url.is_empty() {
        return Err(OauthError::Invalid("token_url is required".into()));
    }

    let mut form: Vec<(String, String)> =
        vec![("grant_type".into(), params.grant_type.as_str().into())];
    match params.grant_type {
        GrantType::AuthorizationCode => {
            let code = params
                .code
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| {
                    OauthError::Invalid("authorization_code grant requires code".into())
                })?;
            form.push(("code".into(), code.to_string()));
            if let Some(uri) = params.redirect_uri.as_deref().filter(|u| !u.is_empty()) {
                form.push(("redirect_uri".into(), uri.to_string()));
            }
            if let Some(verifier) = params.code_verifier.as_deref().filter(|v| !v.is_empty()) {
                form.push(("code_verifier".into(), verifier.to_string()));
            }
        }
        GrantType::ClientCredentials => {}
        GrantType::Password => {
            for (name, value) in [
                ("username", &params.username),
                ("password", &params.password),
            ] {
                match value.as_deref().filter(|v| !v.is_empty()) {
                    Some(v) => form.push((name.into(), v.to_string())),
                    None => {
                        return Err(OauthError::Invalid(format!(
                            "password grant requires {name}"
                        )))
                    }
                }
            }
        }
        GrantType::RefreshToken => {
            let rt = params
                .refresh_token
                .as_deref()
                .filter(|r| !r.is_empty())
                .ok_or_else(|| {
                    OauthError::Invalid("refresh_token grant requires refresh_token".into())
                })?;
            form.push(("refresh_token".into(), rt.to_string()));
        }
        GrantType::DeviceCode => {
            // RFC 8628 §3.4: the POLL request uses the URN grant type and
            // carries `device_code`; the initial request uses
            // `grant_type=device_code` with client_id + scope.
            if let Some(dc) = params.device_code.as_deref().filter(|d| !d.is_empty()) {
                form[0] = (
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                );
                form.push(("device_code".into(), dc.to_string()));
            }
        }
    }
    if !params.scopes.is_empty() {
        form.push(("scope".into(), params.scopes.join(" ")));
    }

    let auth_method = params.auth_method.unwrap_or(ClientAuthMethod::Basic);
    let basic = |id: &str, secret: &str| {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{id}:{secret}").as_bytes())
        )
    };
    let mut basic_auth_header = None;
    match (params.client_secret.as_deref(), auth_method) {
        (Some(secret), ClientAuthMethod::Basic) if !params.client_id.is_empty() => {
            form.push(("client_id".into(), params.client_id.clone()));
            basic_auth_header = Some(basic(&params.client_id, secret));
        }
        (Some(secret), ClientAuthMethod::PostBody) => {
            form.push(("client_id".into(), params.client_id.clone()));
            form.push(("client_secret".into(), secret.to_string()));
        }
        (Some(secret), ClientAuthMethod::Basic) => {
            // P1 line 149: when client_id is empty but client_secret is
            // provided, still include the secret. The old code silently
            // dropped it, causing 401 invalid_client with no diagnostic.
            form.push(("client_id".into(), params.client_id.clone()));
            basic_auth_header = Some(basic(&params.client_id, secret));
        }
        (None, _) => {
            if !params.client_id.is_empty() {
                form.push(("client_id".into(), params.client_id.clone()));
            }
        }
    }

    let body = form
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, &UNRESERVED),
                utf8_percent_encode(v, &UNRESERVED)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    Ok(TokenRequest {
        url: params.token_url.clone(),
        body,
        basic_auth_header,
        content_type: "application/x-www-form-urlencoded".into(),
    })
}

// ── Token response ───────────────────────────────────────────────────────────

/// Parsed RFC 6749 §5.1 token response (+ `id_token` for OIDC). Unknown
/// fields are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    /// Seconds until expiry as reported by the server.
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Parse a token-endpoint response body. Error payloads (§5.2) map to
/// `OauthError::TokenError`.
pub fn parse_token_response(body: &str) -> Result<TokenResponse> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| OauthError::Invalid(format!("token response is not JSON: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(OauthError::TokenError {
            error: err.into(),
            error_description: v
                .get("error_description")
                .and_then(|d| d.as_str())
                .map(str::to_string),
        });
    }
    let access_token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| OauthError::TokenError {
            error: "invalid_response".into(),
            error_description: Some("missing access_token".into()),
        })?;
    Ok(TokenResponse {
        access_token: access_token.into(),
        token_type: v
            .get("token_type")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        expires_in: v.get("expires_in").and_then(|t| t.as_i64()),
        refresh_token: v
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        scope: v.get("scope").and_then(|t| t.as_str()).map(str::to_string),
        id_token: v
            .get("id_token")
            .and_then(|t| t.as_str())
            .map(str::to_string),
    })
}

/// RFC 8628 §3.2 device authorization response — the server's answer to the
/// INITIAL `grant_type=device_code` request. The user visits
/// `verification_uri` and enters `user_code`; the client then POLLS the token
/// endpoint with the `device_code` value (see `build_token_request`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Seconds until the device_code expires.
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Recommended poll interval in seconds.
    #[serde(default)]
    pub interval: Option<i64>,
}

/// RFC 8628 §3.5 device-code POLL result. The server answers the token
/// endpoint with `authorization_pending` / `slow_down` while the user hasn't
/// authorized, and a normal token response once they have.
#[derive(Debug, Clone)]
pub enum DeviceCodePoll {
    /// Keep polling (optionally at a slower interval).
    Pending { interval_seconds: i64 },
    /// Keep polling, but slow down (RFC 8628 §3.5: interval +5 s).
    SlowDown { interval_seconds: i64 },
    /// The user authorized — the token response.
    Authorized(TokenResponse),
    /// The user denied or the code expired.
    Denied {
        error: String,
        description: Option<String>,
    },
}

/// Parse the INITIAL device-authorization response (RFC 8628 §3.2).
pub fn parse_device_code_response(body: &str) -> Result<DeviceCodeResponse> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| OauthError::Invalid(format!("device response is not JSON: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(OauthError::TokenError {
            error: err.into(),
            error_description: v
                .get("error_description")
                .and_then(|d| d.as_str())
                .map(str::to_string),
        });
    }
    let get = |k: &str| -> Result<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| OauthError::TokenError {
                error: "invalid_response".into(),
                error_description: Some(format!("missing {k} in device authorization response")),
            })
    };
    Ok(DeviceCodeResponse {
        device_code: get("device_code")?,
        user_code: get("user_code")?,
        verification_uri: get("verification_uri")?,
        verification_uri_complete: v
            .get("verification_uri_complete")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        expires_in: v.get("expires_in").and_then(|x| x.as_i64()),
        interval: v.get("interval").and_then(|x| x.as_i64()),
    })
}

/// Parse a device-code POLL response (RFC 8628 §3.5). The token endpoint
/// either returns `error=authorization_pending`/`slow_down` (keep polling) or
/// a normal token response.
pub fn parse_device_code_poll(body: &str) -> Result<DeviceCodePoll> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| OauthError::Invalid(format!("poll response is not JSON: {e}")))?;
    let interval = v.get("interval").and_then(|x| x.as_i64()).unwrap_or(5);
    match v.get("error").and_then(|e| e.as_str()) {
        Some("authorization_pending") => Ok(DeviceCodePoll::Pending {
            interval_seconds: interval,
        }),
        Some("slow_down") => Ok(DeviceCodePoll::SlowDown {
            interval_seconds: interval + 5,
        }),
        Some(err) => Ok(DeviceCodePoll::Denied {
            error: err.into(),
            description: v
                .get("error_description")
                .and_then(|d| d.as_str())
                .map(str::to_string),
        }),
        None => parse_token_response(body).map(DeviceCodePoll::Authorized),
    }
}

/// Stored token + its computed absolute expiry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub token_type: String,
    pub refresh_token: Option<String>,
    /// Absolute UNIX seconds; `None` means no expiry advertised.
    pub expires_at: Option<i64>,
    pub scope: Option<String>,
}

impl StoredToken {
    pub fn from_response(response: &TokenResponse) -> Self {
        Self {
            access_token: response.access_token.clone(),
            token_type: response
                .token_type
                .clone()
                .unwrap_or_else(|| "Bearer".into()),
            refresh_token: response.refresh_token.clone(),
            expires_at: response.expires_in.map(|s| now_epoch_secs() + s),
            scope: response.scope.clone(),
        }
    }

    /// True when the token has expired (or expires within `skew_secs`).
    /// Tokens without an expiry are never considered expired.
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        match self.expires_at {
            Some(at) => now_epoch_secs() + skew_secs >= at,
            None => false,
        }
    }
}

// ── Placement — attaching a token to a request ───────────────────────────────

/// Where the access token goes (Postman/Bruno parity knobs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenPlacement {
    /// `Authorization: <prefix> <token>` (prefix defaults to the token_type).
    Header,
    /// `?<query_key>=<token>`.
    Query,
}

/// A token positioned on a request as a concrete header/query pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenAttachment {
    pub kind: TokenPlacement,
    pub key: String,
    pub value: String,
}

/// Attach a bearer-style token: header with the resolved prefix
/// (`Bearer` when unspecified) or a query parameter named `query_key`
/// (defaults to `access_token`).
pub fn attach_token(
    token: &str,
    token_type: Option<&str>,
    placement: TokenPlacement,
    header_prefix: Option<&str>,
    query_key: Option<&str>,
) -> TokenAttachment {
    match placement {
        TokenPlacement::Header => {
            let prefix = header_prefix
                .filter(|p| !p.is_empty())
                .or(token_type)
                .filter(|p| !p.is_empty())
                .unwrap_or("Bearer");
            TokenAttachment {
                kind: TokenPlacement::Header,
                key: "Authorization".into(),
                value: format!("{prefix} {token}"),
            }
        }
        TokenPlacement::Query => TokenAttachment {
            kind: TokenPlacement::Query,
            key: query_key
                .filter(|k| !k.is_empty())
                .unwrap_or("access_token")
                .into(),
            value: token.into(),
        },
    }
}

// ── JWT decode (no signature verification) ───────────────────────────────────

/// Decoded JWT — header/payload claims as raw JSON values. Signature
/// verification is intentionally out of scope: API clients display tokens,
/// they do not trust them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedJwt {
    pub header: serde_json::Value,
    pub payload: serde_json::Value,
    pub signature: String,
}

/// Split and base64url-decode a compact JWS. Fails fast on malformed parts.
pub fn decode_jwt(token: &str) -> Result<DecodedJwt> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(OauthError::InvalidJwt(
            "expected three dot-separated parts".into(),
        ));
    }
    let decode = |part: &str, what: &str| -> Result<serde_json::Value> {
        let bytes = URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|e| OauthError::InvalidJwt(format!("{what} is not base64url: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| OauthError::InvalidJwt(format!("{what} is not JSON: {e}")))
    };
    Ok(DecodedJwt {
        header: decode(parts[0], "header")?,
        payload: decode(parts[1], "payload")?,
        signature: parts[2].into(),
    })
}

/// The JWT `exp` claim (UNIX seconds) when present, else `None`.
pub fn jwt_expires_at(token: &str) -> Result<Option<i64>> {
    let jwt = decode_jwt(token)?;
    Ok(jwt.payload.get("exp").and_then(|e| e.as_i64()))
}

// ── JWT signing (HS256/HS384/HS512) ──────────────────────────────────────────
// Symmetric signing for the API-client use case: developers composing signed
// JWTs to send, not verifying untrusted tokens. The JSON header/payload are
// serialized via serde_json (stable key order, compact separators) before the
// HMAC, so what the user sees decoded is exactly what was signed.

type HmacSha256 = Hmac<Sha256>;
type HmacSha384 = Hmac<sha2::Sha384>;
type HmacSha512 = Hmac<sha2::Sha512>;

/// HMAC-SHA2 algorithms for [`sign_jwt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JwtAlgorithm {
    #[serde(rename = "HS256")]
    Hs256,
    #[serde(rename = "HS384")]
    Hs384,
    #[serde(rename = "HS512")]
    Hs512,
}

impl JwtAlgorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            JwtAlgorithm::Hs256 => "HS256",
            JwtAlgorithm::Hs384 => "HS384",
            JwtAlgorithm::Hs512 => "HS512",
        }
    }
}

fn hmac_sign(alg: JwtAlgorithm, key: &[u8], data: &[u8]) -> Vec<u8> {
    match alg {
        JwtAlgorithm::Hs256 => {
            let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key)
                .expect("HMAC-SHA256 accepts any key length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        JwtAlgorithm::Hs384 => {
            let mut mac = <HmacSha384 as KeyInit>::new_from_slice(key)
                .expect("HMAC-SHA384 accepts any key length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        JwtAlgorithm::Hs512 => {
            let mut mac = <HmacSha512 as KeyInit>::new_from_slice(key)
                .expect("HMAC-SHA512 accepts any key length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
    }
}

/// Sign a compact JWT with an HMAC-SHA2 algorithm.
///
/// `header`/`payload` are JSON object values. `header` is normalized: the
/// `alg` entry is replaced with the algorithm used (a missing header defaults
/// to `{"alg","typ":"JWT"}`), `typ` is preserved when set and filled in
/// otherwise. Returns the compact `header.payload.signature` string
/// (base64url, no padding).
pub fn sign_jwt(
    header: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    algorithm: JwtAlgorithm,
    secret: &str,
) -> Result<String> {
    if !payload.is_object() {
        return Err(OauthError::InvalidJwt(
            "payload must be a JSON object".into(),
        ));
    }
    let mut hdr = match header {
        Some(v) => {
            if !v.is_object() {
                return Err(OauthError::InvalidJwt(
                    "header must be a JSON object".into(),
                ));
            }
            v.clone()
        }
        None => serde_json::json!({}),
    };
    let obj = hdr.as_object_mut().expect("checked object above");
    obj.insert(
        "alg".into(),
        serde_json::Value::String(algorithm.as_str().into()),
    );
    obj.entry("typ")
        .or_insert_with(|| serde_json::Value::String("JWT".into()));
    let payload_str =
        serde_json::to_string(payload).map_err(|e| OauthError::InvalidJwt(e.to_string()))?;
    let header_str =
        serde_json::to_string(&hdr).map_err(|e| OauthError::InvalidJwt(e.to_string()))?;
    let h64 = URL_SAFE_NO_PAD.encode(header_str.as_bytes());
    let p64 = URL_SAFE_NO_PAD.encode(payload_str.as_bytes());
    let signing_input = format!("{h64}.{p64}");
    let sig = hmac_sign(algorithm, secret.as_bytes(), signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

// ── WSSE UsernameToken (OASIS SOAP profile, SHA-1 digest) ────────────────────

/// Inputs for a WSSE UsernameToken profile signature (Postman/Insomnia parity).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WsseParams {
    pub username: String,
    pub password: String,
    /// Fixed nonce; generated (base64 of 16 random bytes) when empty.
    #[serde(default)]
    pub nonce: String,
    /// RFC 3339 timestamp; generated from the host clock when empty.
    #[serde(default)]
    pub created: String,
}

/// A concrete WSSE security header set: the `Authorization` header value plus
/// the nonce/timestamp that were used (the server validates freshness).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsseHeader {
    /// `UsernameToken Username="…" PasswordDigest="…" Nonce="…" Created="…"`.
    pub authorization: String,
    pub nonce: String,
    pub created: String,
}

/// Build a WSSE UsernameToken signature:
/// `PasswordDigest = BASE64(SHA1(nonce + created + password))`.
pub fn sign_wsse(params: &WsseParams) -> Result<WsseHeader> {
    if params.username.is_empty() {
        return Err(OauthError::Invalid("wsse requires username".into()));
    }
    let nonce = if params.nonce.is_empty() {
        random_urlsafe_token(16)
    } else {
        params.nonce.clone()
    };
    let created = if params.created.is_empty() {
        let ms = now_epoch_millis();
        epoch_millis_to_rfc3339(ms)
    } else {
        params.created.clone()
    };
    let digest_input = format!("{nonce}{created}{}", params.password);
    let digest = STANDARD.encode(sha1::Sha1::digest(digest_input.as_bytes()));
    let mut value = String::from("UsernameToken ");
    let _ = write!(&mut value, "Username=\"{}\", ", params.username);
    let _ = write!(&mut value, "PasswordDigest=\"{digest}\", ");
    let _ = write!(&mut value, "Nonce=\"{nonce}\", ");
    let _ = write!(&mut value, "Created=\"{created}\"");
    Ok(WsseHeader {
        authorization: value,
        nonce,
        created,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_params() -> AuthorizeParams {
        AuthorizeParams {
            auth_url: "https://auth.example.com/authorize".into(),
            client_id: "my-client".into(),
            redirect_uri: "https://app.example.com/cb".into(),
            scopes: vec!["read".into(), "write".into()],
            ..Default::default()
        }
    }

    #[test]
    fn pkce_verifier_conforms_to_rfc7636() {
        for _ in 0..32 {
            let v = generate_code_verifier(128);
            assert_eq!(v.len(), 128);
            assert!(v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
            assert_eq!(code_challenge_s256(&v).len(), 43);
        }
        // RFC 7636 Appendix B test vector
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_s256(v),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_carries_everything() {
        let mut p = auth_params();
        p.state = Some("st-1".into());
        p.pkce = Some(PkcePair::generate().into());
        let req = build_authorize_url(&p).unwrap();
        assert!(req
            .url
            .starts_with("https://auth.example.com/authorize?response_type=code"));
        assert!(req.url.contains("client_id=my-client"));
        assert!(req
            .url
            .contains("redirect_uri=https%3A%2F%2Fapp.example.com%2Fcb"));
        assert!(req.url.contains("scope=read+write") || req.url.contains("scope=read%20write"));
        assert!(req.url.contains("state=st-1"));
        assert!(req.url.contains("code_challenge_method=S256"));
        assert!(req.code_verifier.is_some());
        assert_eq!(req.state.as_deref(), Some("st-1"));
    }

    #[test]
    fn authorize_url_generates_state_when_absent() {
        let req = build_authorize_url(&auth_params()).unwrap();
        assert!(req.url.contains("state="));
        assert!(req.state.is_some());
    }

    #[test]
    fn implicit_flow_omits_redirect_and_pkce() {
        let mut p = auth_params();
        p.response_type = "token".into();
        p.redirect_uri = "".into();
        let req = build_authorize_url(&p).unwrap();
        assert!(req.url.contains("response_type=token"));
        assert!(!req.url.contains("redirect_uri="));
        assert!(req.code_verifier.is_none());
    }

    #[test]
    fn basic_auth_header_encodes_credentials() {
        let req = build_token_request(&TokenRequestParams {
            grant_type: GrantType::AuthorizationCode,
            token_url: "https://auth.example.com/token".into(),
            client_id: "id".into(),
            client_secret: Some("s3cret".into()),
            code: Some("abc".into()),
            redirect_uri: Some("https://app.example.com/cb".into()),
            code_verifier: Some("v".repeat(43)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            req.basic_auth_header.as_deref(),
            Some("Basic aWQ6czNjcmV0") // base64("id:s3cret")
        );
        assert!(req.body.contains("grant_type=authorization_code"));
        assert!(req.body.contains("code=abc"));
        assert!(req.body.contains("code_verifier=vvv"));
        assert!(!req.body.contains("client_secret"));
    }

    #[test]
    fn device_code_grant_two_phase_flow() {
        // TR-409: RFC 8628 device grant. The initial request uses
        // `grant_type=device_code` + client_id + scope; the POLL request uses
        // the URN grant type + the device_code value.
        let initial = build_token_request(&TokenRequestParams {
            grant_type: GrantType::DeviceCode,
            token_url: "https://idp/token".into(),
            client_id: "cli".into(),
            scopes: vec!["read".into(), "write".into()],
            ..Default::default()
        })
        .unwrap();
        assert!(initial.body.contains("grant_type=device_code"));
        assert!(initial.body.contains("client_id=cli"));
        assert!(initial.body.contains("scope=read%20write"));

        // The poll request carries the URN grant type and the device_code.
        let poll = build_token_request(&TokenRequestParams {
            grant_type: GrantType::DeviceCode,
            token_url: "https://idp/token".into(),
            client_id: "cli".into(),
            device_code: Some("GmRhmhcxhwAzkoEqiMEg_DnyEysNkuNhszIySk9eS".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(
            poll.body
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
            "poll must use the RFC 8628 URN grant type: {}",
            poll.body
        );
        assert!(poll
            .body
            .contains("device_code=GmRhmhcxhwAzkoEqiMEg_DnyEysNkuNhszIySk9eS"));
    }

    #[test]
    fn device_code_response_and_poll_parsing() {
        // TR-409: RFC 8628 §3.2 initial response + §3.5 poll results.
        let initial = parse_device_code_response(
            r#"{"device_code":"GmRhmhcxhwAzkoEqiMEg_DnyEysNkuNhszIySk9eS",
                "user_code":"WDJB-MJHT",
                "verification_uri":"https://example.com/device",
                "verification_uri_complete":"https://example.com/device?user_code=WDJB-MJHT",
                "expires_in":1800,"interval":5}"#,
        )
        .unwrap();
        assert_eq!(
            initial.device_code,
            "GmRhmhcxhwAzkoEqiMEg_DnyEysNkuNhszIySk9eS"
        );
        assert_eq!(initial.user_code, "WDJB-MJHT");
        assert_eq!(initial.verification_uri, "https://example.com/device");
        assert_eq!(initial.expires_in, Some(1800));
        assert_eq!(initial.interval, Some(5));

        // Poll: pending → keep polling; slow_down → +5 s interval.
        let pending = parse_device_code_poll(r#"{"error":"authorization_pending"}"#).unwrap();
        match pending {
            DeviceCodePoll::Pending { interval_seconds } => assert_eq!(interval_seconds, 5),
            other => panic!("expected Pending, got {other:?}"),
        }
        let slow = parse_device_code_poll(r#"{"error":"slow_down"}"#).unwrap();
        match slow {
            DeviceCodePoll::SlowDown { interval_seconds } => assert_eq!(interval_seconds, 10),
            other => panic!("expected SlowDown, got {other:?}"),
        }

        // Poll: the user authorized → the token response.
        let authorized =
            parse_device_code_poll(r#"{"access_token":"tok","token_type":"Bearer"}"#).unwrap();
        match authorized {
            DeviceCodePoll::Authorized(t) => assert_eq!(t.access_token, "tok"),
            other => panic!("expected Authorized, got {other:?}"),
        }

        // Poll: denied.
        let denied = parse_device_code_poll(r#"{"error":"access_denied"}"#).unwrap();
        match denied {
            DeviceCodePoll::Denied { error, .. } => assert_eq!(error, "access_denied"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn post_body_method_puts_secret_in_form() {
        let req = build_token_request(&TokenRequestParams {
            grant_type: GrantType::RefreshToken,
            token_url: "https://auth.example.com/token".into(),
            client_id: "id".into(),
            client_secret: Some("s3cret".into()),
            auth_method: Some(ClientAuthMethod::PostBody),
            refresh_token: Some("rt".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(req.basic_auth_header.is_none());
        assert!(req.body.contains("client_secret=s3cret"));
        assert!(req.body.contains("refresh_token=rt"));
    }

    #[test]
    fn password_and_client_credentials_grants() {
        let req = build_token_request(&TokenRequestParams {
            grant_type: GrantType::Password,
            token_url: "https://t".into(),
            username: Some("u".into()),
            password: Some("p&x".into()),
            client_id: "c".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(req.body.contains("grant_type=password"));
        assert!(req.body.contains("username=u"));
        assert!(req.body.contains("password=p%26x"));

        let req = build_token_request(&TokenRequestParams {
            grant_type: GrantType::ClientCredentials,
            token_url: "https://t".into(),
            client_id: "c".into(),
            client_secret: Some("s".into()),
            scopes: vec!["api".into()],
            ..Default::default()
        })
        .unwrap();
        assert!(req.body.contains("grant_type=client_credentials"));
        assert!(req.body.contains("scope=api"));
    }

    #[test]
    fn grants_validate_required_fields() {
        assert!(build_token_request(&TokenRequestParams {
            grant_type: GrantType::AuthorizationCode,
            token_url: "https://t".into(),
            ..Default::default()
        })
        .is_err());
        assert!(build_token_request(&TokenRequestParams {
            grant_type: GrantType::RefreshToken,
            token_url: "https://t".into(),
            ..Default::default()
        })
        .is_err());
        assert!(build_token_request(&TokenRequestParams {
            grant_type: GrantType::Password,
            token_url: "".into(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn token_response_round_trip() {
        let body = r#"{"access_token":"at","token_type":"Bearer","expires_in":3600,
            "refresh_token":"rt","scope":"read","id_token":"a.b.c","extra":1}"#;
        let tr = parse_token_response(body).unwrap();
        assert_eq!(tr.access_token, "at");
        assert_eq!(tr.expires_in, Some(3600));
        assert_eq!(tr.refresh_token.as_deref(), Some("rt"));
        assert_eq!(tr.id_token.as_deref(), Some("a.b.c"));

        let stored = StoredToken::from_response(&tr);
        assert!(!stored.is_expired(0));
        assert!(StoredToken {
            expires_at: Some(now_epoch_secs() - 1),
            ..Default::default()
        }
        .is_expired(0));
        assert!(!StoredToken::default().is_expired(3600)); // no expiry
    }

    #[test]
    fn token_error_payload_maps_to_error() {
        let err =
            parse_token_response(r#"{"error":"invalid_grant","error_description":"code expired"}"#)
                .unwrap_err();
        assert!(matches!(err, OauthError::TokenError { .. }));
        assert!(format!("{err}").contains("code expired"));
    }

    #[test]
    fn attach_token_header_and_query() {
        let h = attach_token("tok", Some("Bearer"), TokenPlacement::Header, None, None);
        assert_eq!(
            (h.kind, h.key.as_str(), h.value.as_str()),
            (TokenPlacement::Header, "Authorization", "Bearer tok")
        );
        let h2 = attach_token(
            "tok",
            Some("Bearer"),
            TokenPlacement::Header,
            Some("JWT"),
            None,
        );
        assert_eq!(h2.value, "JWT tok");
        let q = attach_token("tok", None, TokenPlacement::Query, None, Some("tok"));
        assert_eq!(
            (q.kind, q.key.as_str(), q.value.as_str()),
            (TokenPlacement::Query, "tok", "tok")
        );
    }

    #[test]
    fn decode_jwt_reads_claims() {
        // header {"alg":"HS256","typ":"JWT"} · payload {"sub":"u1","exp":9999999999}
        let token =
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MSIsImV4cCI6OTk5OTk5OTk5OX0.c2ln";
        let jwt = decode_jwt(token).unwrap();
        assert_eq!(jwt.header["alg"], "HS256");
        assert_eq!(jwt.payload["sub"], "u1");
        assert_eq!(jwt_expires_at(token).unwrap(), Some(9999999999));
        assert!(decode_jwt("not.a").is_err());
    }

    #[test]
    fn sign_jwt_hs256_round_trip_and_verify() {
        let payload = serde_json::json!({
            "iss": "joe",
            "exp": 1_300_819_380_i64,
            "http://example.com/is_root": true
        });
        let token = sign_jwt(None, &payload, JwtAlgorithm::Hs256, "secret-key").unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        // Header normalized to alg + typ.
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "HS256");
        assert_eq!(header["typ"], "JWT");
        // Payload decodes back to the claims.
        let decoded: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(decoded["iss"], "joe");
        assert_eq!(decoded["exp"], 1_300_819_380_i64);
        // Signature recomputes with the HMAC over the signing input.
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(b"secret-key").unwrap();
        mac.update(signing_input.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        assert_eq!(parts[2], expected);
        // decode_jwt accepts what sign_jwt produced.
        assert_eq!(decode_jwt(&token).unwrap().payload["iss"], "joe");
        assert!(sign_jwt(None, &serde_json::json!([]), JwtAlgorithm::Hs256, "k").is_err());
        assert!(sign_jwt(
            Some(&serde_json::json!([])),
            &serde_json::json!({}),
            JwtAlgorithm::Hs256,
            "k",
        )
        .is_err());
    }

    #[test]
    fn sign_jwt_algorithms_differ_and_preserve_custom_header() {
        let payload = serde_json::json!({"sub": "u1"});
        let t256 = sign_jwt(None, &payload, JwtAlgorithm::Hs256, "k").unwrap();
        let t384 = sign_jwt(None, &payload, JwtAlgorithm::Hs384, "k").unwrap();
        let t512 = sign_jwt(None, &payload, JwtAlgorithm::Hs512, "k").unwrap();
        let mut sigs: Vec<&str> = [t256.as_str(), t384.as_str(), t512.as_str()]
            .iter()
            .map(|t| t.rsplit_once('.').unwrap().1)
            .collect();
        sigs.sort_unstable();
        sigs.dedup();
        assert_eq!(
            sigs.len(),
            3,
            "HS256/384/512 must produce distinct signatures"
        );
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(t384.rsplit_once('.').unwrap().1)
                .unwrap()
                .len(),
            48
        );
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(t512.rsplit_once('.').unwrap().1)
                .unwrap()
                .len(),
            64
        );
        // Custom header fields (kid/cty) survive; alg is forced to the real one.
        let custom = serde_json::json!({"alg": "none", "kid": "key-1", "cty": "JWT"});
        let t = sign_jwt(Some(&custom), &payload, JwtAlgorithm::Hs512, "k").unwrap();
        let header: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(t.split('.').next().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(header["alg"], "HS512");
        assert_eq!(header["kid"], "key-1");
        assert_eq!(header["cty"], "JWT");
        // Signature algorithm string round-trips.
        let alg: JwtAlgorithm = serde_json::from_str("\"HS384\"").unwrap();
        assert_eq!(alg, JwtAlgorithm::Hs384);
        assert_eq!(
            serde_json::to_string(&JwtAlgorithm::Hs256).unwrap(),
            "\"HS256\""
        );
    }

    #[test]
    fn sign_wsse_matches_known_digest() {
        let hdr = sign_wsse(&WsseParams {
            username: "user".into(),
            password: "passwd".into(),
            nonce: "abc".into(),
            created: "2024-01-01T00:00:00.000Z".into(),
        })
        .unwrap();
        // BASE64(SHA1("abc" + "2024-01-01T00:00:00.000Z" + "passwd"))
        assert!(hdr
            .authorization
            .contains("PasswordDigest=\"KagALHpGxQBG3g5ylp5cW1N9xtc=\""));
        assert_eq!(
            hdr.authorization,
            "UsernameToken Username=\"user\", PasswordDigest=\"KagALHpGxQBG3g5ylp5cW1N9xtc=\", \
             Nonce=\"abc\", Created=\"2024-01-01T00:00:00.000Z\""
        );
        assert_eq!(hdr.nonce, "abc");
        assert_eq!(hdr.created, "2024-01-01T00:00:00.000Z");
        // Generated nonce/created: non-empty, RFC 3339-shaped, digest stable
        // for the echoed values.
        let gen = sign_wsse(&WsseParams {
            username: "user".into(),
            password: "passwd".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(!gen.nonce.is_empty());
        assert!(gen.created.ends_with("Z") && gen.created.len() == 24);
        let again = sign_wsse(&WsseParams {
            username: "user".into(),
            password: "passwd".into(),
            nonce: gen.nonce.clone(),
            created: gen.created.clone(),
        })
        .unwrap();
        assert_eq!(again.authorization, gen.authorization);
        assert!(sign_wsse(&WsseParams::default()).is_err());
    }

    #[test]
    fn rfc3339_and_http_date_formatters() {
        // 1970-01-01T00:00:00Z + 86_400_501 ms → 1970-01-02T00:00:00.501Z
        assert_eq!(
            epoch_millis_to_rfc3339(86_400_501),
            "1970-01-02T00:00:00.501Z"
        );
        assert_eq!(epoch_millis_to_rfc3339(0), "1970-01-01T00:00:00.000Z");
        // Known leap-year boundary: 2024-03-01T00:00:00Z = 1709251200 s.
        assert_eq!(
            epoch_millis_to_rfc3339(1_709_251_200_000),
            "2024-03-01T00:00:00.000Z"
        );
    }
}
