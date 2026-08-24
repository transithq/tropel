// Smoke test for @tropel/core-wasm (node >= 20). Run: node smoke.mjs
// Verifies: init-free catalog metadata, wasm init, dynamic-variable
// resolution, the resolver's degradation contract, and the OAuth2 flow
// surface (RFC 6749 builders + RFC 7636 PKCE + JWT decode).
import { readFileSync, existsSync } from "node:fs";
import {
  initCoreWasm,
  getPredefinedVariablesMeta,
  isCoreWasmReady,
  resolveDynamicVariables,
  generatePkcePair,
  oauth2BuildAuthorizeUrl,
  oauth2BuildTokenRequest,
  oauth2ParseTokenResponse,
  oauth2StoreToken,
  oauth2IsTokenExpired,
  oauth2AttachToken,
  oauth2DecodeJwt,
  oauth2JwtExpiresAt,
  oauth2SignJwt,
  wsseSign,
} from "./src/index.js";

// Metadata comes from pkg/meta.js (build-time extraction from the compiled
// catalog) — it must be available BEFORE and WITHOUT init. Async because
// pkg/meta.js is loaded lazily (so a clean-clone `node -e "import('./src/index.js')"`
// does not throw ENOENT before the wasm has been built).
if (!existsSync(new URL("./pkg/meta.js", import.meta.url))) {
  throw new Error("pkg/meta.js missing — run scripts/build.sh");
}
const preMeta = await getPredefinedVariablesMeta();
if (preMeta.length < 30) throw new Error(`metadata too small: ${preMeta.length}`);
for (const m of preMeta) {
  if (!m.name.startsWith("$") || !m.description) throw new Error(`bad meta entry: ${JSON.stringify(m)}`);
}

const bytes = readFileSync(new URL("./pkg/tropel_core_wasm_bg.wasm", import.meta.url));

// Before init: no-op degradation.
const pre = resolveDynamicVariables("id={{$guid}}");
if (!pre.includes("{{$guid}}")) throw new Error("pre-init must not resolve");
if (isCoreWasmReady()) throw new Error("must not be ready before init");

const ok = await initCoreWasm({ wasmBytes: bytes });
if (!ok) throw new Error("init failed");
if (!isCoreWasmReady()) throw new Error("must be ready after init");

// Idempotent second init.
if (!(await initCoreWasm({ wasmBytes: bytes }))) throw new Error("second init failed");

// $guid resolves to a uuid; plain {{vars}} survive untouched.
const out = resolveDynamicVariables("id={{$guid}} host={{host}}");
if (out.includes("{{$")) throw new Error(`unresolved dynamic: ${out}`);
if (!/^id=[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12} host=\{\{host\}\}$/.test(out)) {
  throw new Error(`bad shape: ${out}`);
}

// Fresh value per occurrence.
const [a, b] = resolveDynamicVariables("{{$guid}}|{{$guid}}").split("|");
if (a === b || a.length !== 36 || b.length !== 36) throw new Error("guid not fresh per occurrence");

// Plain variables are NOT the resolver's business.
if (resolveDynamicVariables("{{baseUrl}}/x") !== "{{baseUrl}}/x") {
  throw new Error("plain vars must survive");
}

// Timestamp shape.
const ts = Number(resolveDynamicVariables("{{$timestamp}}"));
if (!(ts > 1_700_000_000)) throw new Error(`bad timestamp: ${ts}`);

// Metadata still served after init (same build-time payload).
const meta = await getPredefinedVariablesMeta();
if (meta.length < 30) throw new Error(`metadata too small after init: ${meta.length}`);
if (meta !== preMeta) throw new Error("metadata must be the stable build-time payload");

// ── OAuth2 flows (wasm-backed builders) — wasm-backed only, no degradation;
//    requireGlue() throws until initCoreWasm() has run. ──────────────────────

// PKCE: RFC 7636 charset + Appendix B vector.
const pkce = generatePkcePair();
if (pkce.codeVerifier.length !== 128) throw new Error(`verifier length ${pkce.codeVerifier.length}`);
if (!/^[A-Za-z0-9\-._~]+$/.test(pkce.codeVerifier)) throw new Error("verifier charset");
{
  const glue = await import("./pkg/tropel_core_wasm.js");
  const rfcVerifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
  const pair = JSON.parse(glue.oauth2GeneratePkcePair(rfcVerifier));
  if (pair.code_challenge !== "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM") {
    throw new Error(`PKCE vector mismatch: ${pair.code_challenge}`);
  }
}

// Authorization-code authorize URL.
const auth = oauth2BuildAuthorizeUrl({
  auth_url: "https://auth.example.com/authorize",
  client_id: "my-client",
  redirect_uri: "https://app.example.com/cb",
  scopes: ["read", "write"],
  pkce: { code_verifier: pkce.codeVerifier, code_challenge_method: "S256" },
});
if (!auth.url.startsWith("https://auth.example.com/authorize?response_type=code")) {
  throw new Error(`authorize url: ${auth.url}`);
}
for (const piece of ["client_id=my-client", "scope=read%20write", "state=", "code_challenge_method=S256"]) {
  if (!auth.url.includes(piece)) throw new Error(`authorize url missing ${piece}: ${auth.url}`);
}
if (auth.code_verifier !== pkce.codeVerifier) throw new Error("verifier not echoed");
if (!auth.state) throw new Error("state must be generated");

// Token request: authorization_code + PKCE, Basic client auth.
const tokenReq = oauth2BuildTokenRequest({
  grant_type: "authorization_code",
  token_url: "https://auth.example.com/token",
  client_id: "my-client",
  client_secret: "s3cret",
  code: "auth-code-123",
  redirect_uri: "https://app.example.com/cb",
  code_verifier: pkce.codeVerifier,
});
if (tokenReq.url !== "https://auth.example.com/token") throw new Error("token url");
if (!tokenReq.body.includes("grant_type=authorization_code")) throw new Error("token body grant");
if (!tokenReq.body.includes("code_verifier=")) throw new Error("token body verifier");
if (!tokenReq.basic_auth_header?.startsWith("Basic ")) throw new Error("basic auth header");
if (tokenReq.basic_auth_header !== "Basic " + Buffer.from("my-client:s3cret").toString("base64")) {
  throw new Error(`bad basic header: ${tokenReq.basic_auth_header}`);
}
if (tokenReq.body.includes("client_secret")) throw new Error("secret must not be in body under basic auth");

// Token response parse → store → expiry.
const parsed = oauth2ParseTokenResponse(
  JSON.stringify({ access_token: "at", token_type: "Bearer", expires_in: 3600, refresh_token: "rt", scope: "read" }),
);
if (parsed.access_token !== "at" || parsed.expires_in !== 3600) throw new Error("token parse");
const stored = oauth2StoreToken(parsed);
if (stored.token_type !== "Bearer" || typeof stored.expires_at !== "number") throw new Error("token store");
if (oauth2IsTokenExpired(stored)) throw new Error("fresh token must not be expired");
const expired = { ...stored, expires_at: Math.floor(Date.now() / 1000) - 1 };
if (!oauth2IsTokenExpired(expired, 30)) throw new Error("past expiry must register");
if (oauth2IsTokenExpired({ ...stored, expires_at: null })) throw new Error("no-expiry token must never expire");

// Error payloads (§5.2) throw.
try {
  oauth2ParseTokenResponse('{"error":"invalid_grant","error_description":"code expired"}');
  throw new Error("error payload must throw");
} catch (e) {
  if (!String(e.message ?? e).includes("code expired")) throw e;
}

// Attachment: header prefix + query placement.
const att = oauth2AttachToken("tok", "Bearer", "header", null, null);
if (att.kind !== "header" || att.key !== "Authorization" || att.value !== "Bearer tok") {
  throw new Error(`attach header: ${JSON.stringify(att)}`);
}
const attQ = oauth2AttachToken("tok", null, "query", null, "access_token");
if (attQ.kind !== "query" || attQ.key !== "access_token" || attQ.value !== "tok") {
  throw new Error(`attach query: ${JSON.stringify(attQ)}`);
}

// JWT decode (display-only; no verification).
const jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MSIsImV4cCI6OTk5OTk5OTk5OX0.c2ln";
const decoded = oauth2DecodeJwt(jwt);
if (decoded.payload.sub !== "u1") throw new Error("jwt payload");
if (oauth2JwtExpiresAt(jwt) !== 9999999999) throw new Error("jwt exp");
try {
  oauth2DecodeJwt("not-a-jwt");
  throw new Error("malformed jwt must throw");
} catch (e) {
  if (!String(e.message ?? e).includes("three")) throw e;
}

// JWT signing — round-trip: sign with HS256, decode back, verify the
// signature against a recomputed HMAC (crypto.subtle on node/web).
const claim = { sub: "u1", iss: "knockport", exp: 9999999999 };
const signed = oauth2SignJwt(claim, null, "HS256", "secret-key");
const resplit = oauth2DecodeJwt(signed);
if (resplit.header.alg !== "HS256") throw new Error(`signed jwt header alg: ${resplit.header.alg}`);
if (resplit.header.typ !== "JWT") throw new Error("signed jwt header typ");
if (resplit.payload.sub !== "u1" || resplit.payload.exp !== 9999999999) {
  throw new Error("signed jwt payload round-trip");
}
{
  const sigPart = signed.split(".")[2];
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode("secret-key"),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  const data = signed.split(".").slice(0, 2).join(".");
  const sig = new Uint8Array(Buffer.from(sigPart, "base64url"));
  const ok = await crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode(data));
  if (!ok) throw new Error("signed jwt signature does not verify");
}
// Distinct algorithms produce distinct signatures; custom header preserved.
const s384 = oauth2SignJwt(claim, null, "HS384", "secret-key");
const s512 = oauth2SignJwt(claim, null, "HS512", "secret-key");
if (signed.split(".")[2] === s384.split(".")[2] || signed.split(".")[2] === s512.split(".")[2]) {
  throw new Error("HS256/384/512 must differ");
}
const customHeader = oauth2DecodeJwt(oauth2SignJwt(claim, { kid: "k1" }, "HS512", "s"));
if (customHeader.header.alg !== "HS512" || customHeader.header.kid !== "k1") {
  throw new Error("custom jose header lost");
}
try {
  oauth2SignJwt([1, 2], null, "HS256", "s");
  throw new Error("non-object payload must throw");
} catch (e) {
  if (!String(e.message ?? e).includes("payload")) throw e;
}

// WSSE UsernameToken — known digest vector + generated nonce/created.
const wsse = wsseSign({ username: "user", password: "passwd", nonce: "abc", created: "2024-01-01T00:00:00.000Z" });
if (wsse.authorization !==
  "UsernameToken Username=\"user\", PasswordDigest=\"KagALHpGxQBG3g5ylp5cW1N9xtc=\", Nonce=\"abc\", Created=\"2024-01-01T00:00:00.000Z\"") {
  throw new Error(`wsse header: ${wsse.authorization}`);
}
const wsseGen = wsseSign({ username: "user", password: "passwd" });
if (!wsseGen.nonce || !wsseGen.created.endsWith("Z")) throw new Error("wsse generation");

console.log(`core-wasm smoke OK — catalog: ${meta.length} variables · oauth2 flows verified`);
