// @tropel/core-wasm — facade over the tropel core tier (browser embedders).
//
// The core tier (crates/tropel-core-wasm) carries the pure compute a page
// always needs — starting with the Postman dynamic-variable catalog — with
// NO QuickJS (API_CLIENT_WEB_PAYLOAD.md §2.3 two-tier split: the heavy
// tropel-web wasip1 slice stays extension/native territory).
//
// Usage (KnockPort):
//   await initCoreWasm();            // app boot — fire and forget
//   const out = resolveDynamicVariables("id={{$guid}}");  // sync, wasm-backed
//   const names = getPredefinedVariableNames();           // sync, no init needed
//
// The name/description metadata is extracted from the compiled catalog at
// package build time (pkg/meta.js, single source of truth: the Rust
// PREDEFINED_VARIABLE_META table), so autocomplete lists work before the
// wasm fetch finishes — and without import-assertion syntax, which has
// uneven browser support. Until init resolves (or in environments without
// WebAssembly) the resolver degrades to a no-op passthrough — `{{$…}}`
// survive literal and the embedder's own {{var}} map still resolves.

// The name/description metadata for the predefined dynamic variables is
// generated at package build time into `pkg/meta.js`. It is loaded lazily
// (dynamic import) so the package can be imported in environments where the
// wasm has not yet been built — CI runs `node -e "import('@tropel/core-wasm')"`
// from a clean clone without a Rust toolchain, and a static top-level import
// here would break module resolution with ENOENT before any consumer code
// runs. The first call memoizes the result; subsequent calls are sync.
let catalogMetaPromise = null;
async function loadCatalogMeta() {
  if (!catalogMetaPromise) {
    catalogMetaPromise = import("../pkg/meta.js").then((m) => m.default ?? m);
  }
  return catalogMetaPromise;
}

let wasmInstance = null;
let glue = null;

/**
 * Initialize the core wasm. Resolves `true` when ready.
 * Options:
 *   - `wasmUrl`:   explicit URL/path for tropel_core_wasm_bg.wasm
 *                  (default: resolved relative to this module)
 *   - `wasmBytes`: ArrayBuffer/Uint8Array with the wasm (node/tests)
 */
export async function initCoreWasm(options = {}) {
  if (wasmInstance) return true;
  try {
    const g = await import("../pkg/tropel_core_wasm.js");
    let source = options.wasmBytes;
    if (source === undefined) {
      const url = options.wasmUrl ?? new URL("../pkg/tropel_core_wasm_bg.wasm", import.meta.url);
      source = await (await fetch(url)).arrayBuffer();
    }
    wasmInstance = await g.default({ module_or_path: source });
    glue = g;
    return true;
  } catch (err) {
    console.warn("[tropel-core] core wasm unavailable — {{$dynamic}} resolution disabled:", err);
    return false;
  }
}

/** True once initCoreWasm() has resolved successfully. */
export function isCoreWasmReady() {
  return wasmInstance !== null;
}

/**
 * Resolve every predefined dynamic variable (`{{$guid}}`, `{{$timestamp}}`,
 * …) in the input — fresh value per occurrence, Tropel semantics. Plain
 * `{{var}}` refs are untouched. Returns the input unchanged if wasm is not
 * ready.
 *
 * TR-403: THROWS on a total-output overflow (the wasm tier caps expansion at
 * 16 MiB and returns a real `Error` naming the limit) — a consumer must
 * catch it, never send a silently-truncated body. The old "never throws"
 * claim was wrong: the uncapped path could grow a 460 kB input into 200 MB.
 */
export function resolveDynamicVariables(template) {
  if (glue === null) {
    return template;
  }
  try {
    return glue.resolveVariables(template);
  } catch (err) {
    throw new Error(
      `resolveDynamicVariables failed (dynamic variable expansion exceeded the 16 MiB output cap)`,
      { cause: err },
    );
  }
}

/**
 * Catalog metadata `[{"name":"$guid","description":…}]` for editor UIs.
 * Async and init-free: extracted from the compiled catalog at package build
 * time (single source of truth — the Rust PREDEFINED_VARIABLE_META table in
 * crates/tropel-core-wasm). Lazy because pkg/meta.js is build-generated and
 * may be absent in dev / CI-from-a-clean-clone.
 */
export async function getPredefinedVariablesMeta() {
  return await loadCatalogMeta();
}

/** Just the `$`-prefixed catalog names (autocomplete lists). */
export async function getPredefinedVariableNames() {
  const meta = await loadCatalogMeta();
  return meta.map((m) => m.name);
}

// ── OAuth2 flows (RFC 6749 + RFC 7636 PKCE, pure — the embedder sends) ──────
// All functions throw when the wasm is not ready; embedders should await
// initCoreWasm() and degrade gracefully (e.g. disable the token buttons).

function requireGlue(name) {
  if (glue === null) throw new Error(`[tropel-core] ${name} requires the core wasm (initCoreWasm)`);
  return glue;
}

/**
 * Generate a PKCE pair: `{codeVerifier, codeChallengeMethod:"S256",
 * codeChallenge}`. The verifier is 128 chars from the RFC 7636 charset,
 * drawn from `crypto.getRandomValues`; the S256 challenge is computed by the
 * wasm (SHA-256 + base64url) so it matches the token-request builder byte
 * for byte.
 */
export function generatePkcePair() {
  const g = requireGlue("generatePkcePair");
  const CHARS =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
  const buf = new Uint8Array(128);
  crypto.getRandomValues(buf);
  const codeVerifier = Array.from(buf, (byte) => CHARS[byte % CHARS.length]).join("");
  const pairJson = g.oauth2GeneratePkcePair(codeVerifier);
  const pair = JSON.parse(pairJson);
  return {
    codeVerifier,
    codeChallengeMethod: pair.code_challenge_method,
    codeChallenge: pair.code_challenge,
  };
}

/**
 * Build the OAuth2 authorize URL.
 * @param {object} params `{auth_url, client_id, redirect_uri, scopes[],
 * response_type?, state?, pkce?{code_verifier, code_challenge_method},
 * extra?}` — all `{{var}}` templates must be resolved beforehand.
 * @returns {{url:string, state:string|null, code_verifier:string|null}}
 */
export function oauth2BuildAuthorizeUrl(params) {
  return JSON.parse(requireGlue("oauth2BuildAuthorizeUrl").oauth2BuildAuthorizeUrl(JSON.stringify(params)));
}

/**
 * Build the token-endpoint POST.
 * @param {object} params `{grant_type:"authorization_code"|"client_credentials"|
 * "password"|"refresh_token", token_url, client_id, client_secret?,
 * auth_method?:"basic"|"post_body", code?, redirect_uri?, code_verifier?,
 * username?, password?, refresh_token?, scopes[]}`
 * @returns {{url:string, body:string, basic_auth_header:string|null, content_type:string}}
 */
export function oauth2BuildTokenRequest(params) {
  return JSON.parse(requireGlue("oauth2BuildTokenRequest").oauth2BuildTokenRequest(JSON.stringify(params)));
}

/**
 * Parse a token-endpoint response body (RFC 6749 §5.1). Throws on error
 * payloads (§5.2) and non-JSON bodies.
 * @returns {{access_token:string, token_type:string|null, expires_in:number|null,
 * refresh_token:string|null, scope:string|null, id_token:string|null}}
 */
export function oauth2ParseTokenResponse(body) {
  return JSON.parse(requireGlue("oauth2ParseTokenResponse").oauth2ParseTokenResponse(body));
}

/**
 * Fold a parsed token response into a stored token with an absolute
 * `expires_at` (host clock). Input: the object from oauth2ParseTokenResponse.
 * @returns {{access_token:string, token_type:string, refresh_token:string|null,
 * expires_at:number|null, scope:string|null}}
 */
export function oauth2StoreToken(parsedResponse) {
  return JSON.parse(requireGlue("oauth2StoreToken").oauth2StoreToken(JSON.stringify(parsedResponse)));
}

/**
 * Is a stored token expired? Pure JS (host clock), init-free. Tokens without
 * `expires_at` are never expired. `skewSecs` defaults to 60.
 */
export function oauth2IsTokenExpired(token, skewSecs = 60) {
  return typeof token.expires_at === "number"
    ? Math.floor(Date.now() / 1000) + skewSecs >= token.expires_at
    : false;
}

/**
 * Position a token on a request.
 * @param {string} token
 * @param {string|null} tokenType e.g. "Bearer"
 * @param {"header"|"query"} placement
 * @param {string|null} headerPrefix overrides the token type prefix
 * @param {string|null} queryKey overrides the "access_token" query name
 * @returns {{kind:"header"|"query", key:string, value:string}}
 */
export function oauth2AttachToken(token, tokenType, placement, headerPrefix = null, queryKey = null) {
  return JSON.parse(
    requireGlue("oauth2AttachToken").oauth2AttachToken(
      token,
      tokenType ?? "",
      placement,
      headerPrefix ?? "",
      queryKey ?? "",
    ),
  );
}

/**
 * Decode a compact JWT (no signature verification — clients display tokens,
 * they don't trust them) → `{header, payload, signature}`. Throws on
 * malformed tokens.
 */
export function oauth2DecodeJwt(token) {
  return JSON.parse(requireGlue("oauth2DecodeJwt").oauth2DecodeJwt(token));
}

/** The JWT `exp` claim (UNIX seconds), or null when absent/malformed. */
export function oauth2JwtExpiresAt(token) {
  const secs = Number(requireGlue("oauth2JwtExpiresAt").oauth2JwtExpiresAt(token));
  return secs < 0 ? null : secs;
}

/**
 * Sign a compact JWT with an HMAC-SHA2 algorithm.
 * @param {object} payload the claims — a plain JSON object.
 * @param {object|null} header optional JOSE header fields; `alg` is forced to
 * `algorithm`, `typ` defaults to "JWT". Pass null for the standard header.
 * @param {"HS256"|"HS384"|"HS512"} algorithm
 * @param {string} secret the HMAC key (shared secret).
 * @returns {string} the compact `header.payload.signature` token.
 */
export function oauth2SignJwt(payload, header, algorithm, secret) {
  return requireGlue("oauth2SignJwt").oauth2SignJwt(
    header === null || header === undefined ? "" : JSON.stringify(header),
    JSON.stringify(payload),
    algorithm,
    secret,
  );
}

/**
 * Build a WSSE UsernameToken security header set (SOAP, SHA-1 digest profile).
 * @param {object} params `{username, password, nonce?, created?}` — empty
 * nonce/created are generated (random nonce + host-clock RFC 3339 timestamp).
 * @returns {{authorization:string, nonce:string, created:string}} attach
 * `authorization` as the `Authorization` header value.
 */
export function wsseSign(params) {
  return JSON.parse(requireGlue("wsseSign").wsseSign(JSON.stringify(params)));
}
