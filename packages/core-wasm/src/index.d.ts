// @tropel/core-wasm — typings (see index.js).

export interface PredefinedVariableMeta {
  name: string;
  description: string;
}

export interface InitCoreWasmOptions {
  /** Explicit URL for tropel_core_wasm_bg.wasm. */
  wasmUrl?: string | URL | Request;
  /** Pre-fetched wasm bytes (node/tests). */
  wasmBytes?: ArrayBuffer | Uint8Array;
}

/**
 * Initialize the core wasm. Resolves `true` when ready, `false` when wasm is
 * unavailable (the resolver then degrades to a no-op passthrough).
 */
export function initCoreWasm(options?: InitCoreWasmOptions): Promise<boolean>;

/** True once initCoreWasm() has resolved successfully. */
export function isCoreWasmReady(): boolean;

/**
 * Resolve every predefined dynamic variable (`{{$guid}}`, `{{$timestamp}}`, …)
 * — fresh value per occurrence, Tropel semantics. Plain `{{var}}` refs are
 * untouched. Returns the input unchanged if wasm is not ready.
 */
export function resolveDynamicVariables(template: string): string;

/**
 * Catalog metadata `[{"name":"$guid","description":…}]` for editor UIs.
 * Async: `pkg/meta.js` is generated at package build time and is loaded lazily
 * so the package can be imported (without throwing) before the build runs.
 */
export function getPredefinedVariablesMeta(): Promise<PredefinedVariableMeta[]>;

/** Just the `$`-prefixed catalog names (autocomplete lists). Async — see
 * {@link getPredefinedVariablesMeta}. */
export function getPredefinedVariableNames(): Promise<string[]>;

// ── OAuth2 flows (RFC 6749 + RFC 7636 PKCE, pure — the embedder sends) ──────

export interface PkcePair {
  codeVerifier: string;
  codeChallengeMethod: "S256";
  codeChallenge: string;
}

export interface AuthorizeParams {
  auth_url: string;
  client_id: string;
  redirect_uri: string;
  scopes?: string[];
  /** `"code"` (default) or `"token"` (implicit). */
  response_type?: string;
  state?: string;
  /** Attach PKCE: `{code_verifier, code_challenge_method?}` — or omit, and
   * pass nothing; use generatePkcePair() first. */
  pkce?: { code_verifier: string; code_challenge_method?: string };
  /** Extra query parameters passed through verbatim. */
  extra?: [string, string][];
}

export interface AuthorizeRequest {
  url: string;
  state: string | null;
  code_verifier: string | null;
}

export type OAuth2GrantType =
  | "authorization_code"
  | "client_credentials"
  | "password"
  | "refresh_token";

export interface TokenRequestParams {
  grant_type: OAuth2GrantType;
  token_url: string;
  client_id?: string;
  client_secret?: string;
  auth_method?: "basic" | "post_body";
  code?: string;
  redirect_uri?: string;
  code_verifier?: string;
  username?: string;
  password?: string;
  refresh_token?: string;
  scopes?: string[];
}

export interface TokenRequest {
  url: string;
  /** `application/x-www-form-urlencoded` body. */
  body: string;
  /** `Basic base64(id:secret)` when the secret travels in the header. */
  basic_auth_header: string | null;
  content_type: string;
}

export interface TokenResponse {
  access_token: string;
  token_type: string | null;
  expires_in: number | null;
  refresh_token: string | null;
  scope: string | null;
  id_token: string | null;
}

export interface StoredToken {
  access_token: string;
  token_type: string;
  refresh_token: string | null;
  /** Absolute UNIX seconds; null = no expiry advertised. */
  expires_at: number | null;
  scope: string | null;
}

export interface TokenAttachment {
  kind: "header" | "query";
  key: string;
  value: string;
}

export interface DecodedJwt {
  header: Record<string, unknown>;
  payload: Record<string, unknown>;
  signature: string;
}

export interface OauthError {
  message: string;
}

/** Generate a PKCE pair (128-char verifier + S256 challenge). Throws when
 * the core wasm is not ready. */
export function generatePkcePair(): PkcePair;

/** Build the authorize URL the user's browser is sent to. Throws on invalid
 * input or when the core wasm is not ready. */
export function oauth2BuildAuthorizeUrl(params: AuthorizeParams): AuthorizeRequest;

/** Build the token-endpoint POST. */
export function oauth2BuildTokenRequest(params: TokenRequestParams): TokenRequest;

/** Parse a token-endpoint response body. Throws on RFC 6749 §5.2 error
 * payloads and malformed JSON. */
export function oauth2ParseTokenResponse(body: string): TokenResponse;

/** Fold a parsed response into a stored token with absolute `expires_at`. */
export function oauth2StoreToken(parsedResponse: TokenResponse): StoredToken;

/** Is a stored token expired? Pure JS (host clock), init-free; tokens
 * without `expires_at` never expire. Default skew: 60s. */
export function oauth2IsTokenExpired(token: StoredToken, skewSecs?: number): boolean;

/** Position a token on a request as a header/query pair. */
export function oauth2AttachToken(
  token: string,
  tokenType: string | null,
  placement: "header" | "query",
  headerPrefix?: string | null,
  queryKey?: string | null,
): TokenAttachment;

/** Decode a compact JWT without verifying the signature. Throws on
 * malformed tokens. */
export function oauth2DecodeJwt(token: string): DecodedJwt;

/** The JWT `exp` claim (UNIX seconds), or null when absent. */
export function oauth2JwtExpiresAt(token: string): number | null;

/** Sign a compact JWT with an HMAC-SHA2 algorithm →
 * `header.payload.signature` (base64url, no padding). `header` may be null
 * (defaults to `{"alg","typ":"JWT"}`); `alg` is always forced to `algorithm`.
 * Throws on non-object payloads/headers. */
export function oauth2SignJwt(
  payload: Record<string, unknown>,
  header: Record<string, unknown> | null,
  algorithm: "HS256" | "HS384" | "HS512",
  secret: string,
): string;

/** Build a WSSE UsernameToken security header set
 * (`PasswordDigest = BASE64(SHA1(nonce + created + password))`). Empty
 * nonce/created are generated. */
export function wsseSign(params: {
  username: string;
  password: string;
  nonce?: string;
  created?: string;
}): { authorization: string; nonce: string; created: string };
