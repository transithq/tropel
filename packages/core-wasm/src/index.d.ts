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

/** Escaper for {@link resolveTemplate}. Pick per FIELD, not per request. */
export type ResolveMode = "plain" | "json" | "url";

/**
 * Resolve plain `{{var}}` references against a flat variable map.
 *
 * `mode` selects the escaper and must match the field: `"json"` for a JSON
 * body (quotes/newlines escaped, a bare `"k": {{frag}}` left raw), `"url"` for
 * a URL (raw, Postman-compatible), `"plain"` elsewhere. An unknown mode
 * throws rather than falling back.
 *
 * `deep` (default true) resolves `{{a}}`→`{{b}}` chains and nested names like
 * `{{host_{{suffix}}}}`, capped at {@link maxVariableResolutionPasses}.
 *
 * Unknown names survive as literal `{{name}}`. **Throws when the tier is not
 * ready** — it never degrades to a passthrough.
 */
export function resolveTemplate(
  template: string,
  vars: Record<string, string>,
  mode: ResolveMode,
  deep?: boolean,
): string;

/** What {@link resolveTemplateDetailed} reports. */
export interface ResolveOutcome {
  value: string;
  /**
   * The pass budget ran out while the text was still CHANGING — a cycle, or a
   * chain deeper than the cap. An unknown name never sets this: it stabilizes
   * on the first pass.
   */
  hitCap: boolean;
  /** Placeholder names still present, first-occurrence order, deduplicated. */
  unresolved: string[];
}

/**
 * {@link resolveTemplate}, plus why resolution stopped. Use it to tell a
 * never-settling chain (fail loudly, name the chain) from an unknown name
 * (stays a visible literal, still sends) — the two are indistinguishable in
 * the output string alone.
 */
export function resolveTemplateDetailed(
  template: string,
  vars: Record<string, string>,
  mode: ResolveMode,
): ResolveOutcome;

/** The `{{a}}`→`{{b}}` chain cap (Postman documents 20). */
export function maxVariableResolutionPasses(): number;

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

/** One header the signer says must reach the wire. */
export interface SignedHeader {
  name: string;
  value: string;
}

/** Answer a digest challenge (RFC 7616). `wwwAuthenticate` is the server's RAW
 *  header value; `nc` is the caller's per-(host, realm, nonce) counter starting
 *  at 1; `cnonce` is caller-generated. Returns `null` when the header carries
 *  no Digest challenge. */
export function digestSign(params: {
  wwwAuthenticate: string;
  username: string;
  password: string;
  method: string;
  uri: string;
  nc: number;
  cnonce: string;
}): SignedHeader | null;

/** Build a Hawk `Authorization` header. */
export function hawkSign(params: {
  method: string;
  resource: string;
  host: string;
  port: number;
  id: string;
  key: string;
  algorithm?: string;
  ts: string;
  nonce: string;
  ext?: string;
}): SignedHeader;

/** Sign a request with AWS SigV4. `host` is `URL.hostname` (IPv6 brackets are
 *  re-added by the Rust); omit `service` to derive it from the host.
 *  `bodyBase64` throws if undecodable rather than signing an empty body. */
export function awsSigV4Sign(params: {
  method: string;
  host: string;
  path: string;
  query?: string;
  headers?: readonly (readonly [string, string])[];
  bodyBase64?: string;
  accessKey: string;
  secretKey: string;
  sessionToken?: string;
  region?: string;
  service?: string;
  amzDate: string;
  dateStamp: string;
}): SignedHeader[];

/** Sign a request with OAuth 1.0a (RFC 5849). Throws when `signatureMethod`
 *  is not one of `oauth1SignatureMethods()` — never a silent downgrade. */
export function oauth1Sign(params: {
  method: string;
  scheme: string;
  host: string;
  port?: number;
  path: string;
  queryParams?: readonly (readonly [string, string])[];
  formBody?: string;
  consumerKey: string;
  consumerSecret: string;
  token?: string;
  tokenSecret?: string;
  signatureMethod: string;
  nonce: string;
  timestamp: string;
}): SignedHeader;

/** The signature methods `oauth1Sign` accepts. */
export function oauth1SignatureMethods(): string[];
