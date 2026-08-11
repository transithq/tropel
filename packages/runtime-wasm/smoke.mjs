// Smoke test for @tropel/runtime-wasm: load the REAL tropel_web.wasm artifact,
// run the same fixture the Rust F3 harness uses (2 items wired with
// setNextRequest, a carried variable, status + header assertions), and assert
// the outcome decodes with the expected trace. Mirrors
// crates/tropel-web/tests/native_vs_wasm.rs, but through the npm wrapper.
//
// Run: node smoke.mjs   (needs dist/ built + the wasm artifact; skip-safe)
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { decodeHttpRequest, Writer } from "./dist/postcard.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// ── locate the wasm artifact ─────────────────────────────────────────────
function findArtifact() {
  const candidates = [
    process.env.TROPEL_WASM_PATH,
    path.join(__dirname, "wasm", "tropel_web.wasm"),
    path.join(__dirname, "..", "..", "target", "wasm32-wasip1", "release-wasm", "tropel_web.wasm"),
    "C:/tropel-native-target/wasm32-wasip1/release-wasm/tropel_web.wasm",
    path.join(__dirname, "..", "..", "target", "wasm32-wasip1", "release", "tropel_web.wasm"),
    "C:/tropel-native-target/wasm32-wasip1/release/tropel_web.wasm",
  ].filter(Boolean);
  return candidates.find((c) => existsSync(c));
}

const artifact = findArtifact();
if (!artifact) {
  // Same gate as the Rust F3 harness: CI sets TROPEL_REQUIRE_WASM=1 so a
  // missing runtime FAILS the step instead of passing vacuously.
  if (process.env.TROPEL_REQUIRE_WASM) {
    console.error("FAIL: tropel_web.wasm not built — run scripts/build.sh or the wasm job");
    process.exit(1);
  }
  console.log("SKIP: tropel_web.wasm not built — run scripts/build.sh or the wasm job");
  process.exit(0);
}

// ── build the wrapper ────────────────────────────────────────────────────
const { createExecWasm, checkVersionParity } = await import("./dist/index.js");

const wasmBytes = readFileSync(artifact);
let wasiImports;
let wasi;
try {
  // Node ≥ 20 built-in preview1 WASI — no npm dependency needed for the smoke.
  const { WASI } = await import("node:wasi");
  wasi = new WASI({ version: "preview1", args: [], env: {}, preopens: {} });
  wasiImports = wasi.wasiImport;
} catch {
  console.log("SKIP: no WASI provider (need Node ≥ 20 or browser_wasi_shim)");
  process.exit(0);
}

// ── fixture transport (byte-identical semantics to F3's fixture_response) ─
const seenUrls = [];
const exec = await createExecWasm({
  wasmBytes,
  wasiImports,
  // node:wasi preview1 needs initialize(instance) after instantiation.
  onInstantiate: (instance) => wasi.initialize(instance),
  transport: (req) => {
    seenUrls.push(req.url);
    // Line-44 round trip: the FIRST item carries a JSON body through the
    // scenario — the wasm encodes it via the real body_to_wire envelope and
    // this transport decodes it via the real decodeHttpRequest. Assert the
    // body survived (it used to arrive as a 1-char junk body).
    if (req.url.includes("/first")) {
      if (req.body !== '{"a":1}' || req.bodyMode !== "json")
        fail(`json body must round-trip the wire, got body=${JSON.stringify(req.body)} mode=${req.bodyMode}`);
    }
    return {
      url: req.url,
      statusCode: 200,
      statusText: "OK",
      headers: { "content-type": "application/json" },
      body: new TextEncoder().encode('{"ok":true}'),
      responseTimeMs: 5,
      timings: {
        blockedMs: 2,
        dnsMs: 2,
        connectingMs: 5,
        tlsHandshakingMs: 0,
        sendingMs: 0,
        waitingMs: 5,
        receivingMs: 5,
        totalMs: 5,
      },
      // One cookie exercises the Option-heavy Cookie wire layout (name/value
      // plain strings, domain/secure/httpOnly as Options) round-trip.
      cookies: [{ name: "sid", value: "abc123", domain: "fixture.test", secure: true }],
      size: 12,
    };
  },
});

// ── the F3 fixture scenario ──────────────────────────────────────────────
const item = (name, url, test) => ({
  name,
  request: {
    url,
    method: "GET",
    headers: {},
    query_params: {},
    // Line-44 round trip: the first item carries a real JSON body so the
    // wasm's body_to_wire envelope flows through the actual decoder.
    body: name === "first" ? { a: 1 } : null,
    auth: null,
    certificate: null,
    follow_redirects: true,
    timeout: null,
    response_type: "text",
  },
  prerequest: null,
  test,
  assertions: [],
  items: [],
});

const scenario = {
  info: { name: "f3-diff", description: null, schema: null },
  items: [
    item(
      "first",
      "https://fixture.test/first",
      "pm.variables.set('carried', 'yes');" +
        "pm.test('status is 200', () => pm.expect(pm.response.code).to.eql(200));" +
        "pm.test('header content-type', () => pm.expect(pm.response.headers.get('content-type')).to.eql('application/json'));" +
        "pm.execution.setNextRequest('second');"
    ),
    item(
      "second",
      "https://fixture.test/second",
      "pm.test('carried variable', () => pm.expect(pm.variables.get('carried')).to.eql('yes'));" +
        "pm.test('second status', () => pm.expect(pm.response.code).to.eql(200));"
    ),
  ],
  variables: {},
  auth: null,
};

// ── P6 version handshake surface ──────────────────────────────────────────
// The wasm exposes its runtime version; the smoke pins it against the
// package version so lockstep drift fails the gate.
const pkgVersion = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")).version;
if (typeof exec.runtimeVersion !== "string" || exec.runtimeVersion.length === 0) {
  console.error("FAIL: runtimeVersion missing from the wasm (tropel_version export)");
  process.exit(1);
}
console.log(`  runtime version: ${exec.runtimeVersion}, package version: ${pkgVersion}`);

const match = checkVersionParity(pkgVersion, exec.runtimeVersion);
if (!match.matched) {
  console.error(`FAIL: lockstep — ${match.warning}`);
  process.exit(1);
}

const outcome = exec.run({
  scenarioJson: JSON.stringify(scenario),
  vuId: 1,
  scenarioName: "f3-diff",
  iterations: 2,
  envVars: {},
  expectedStatuses: ["200"],
});

// ── assertions (mirror the Rust harness) ─────────────────────────────────
function fail(msg) {
  console.error(`FAIL: ${msg}`);
  console.error(JSON.stringify(outcome, null, 2));
  process.exit(1);
}

if (outcome.error) fail(`run returned a fatal error: ${outcome.error}`);
if (outcome.iterations.length !== 2) fail(`expected 2 iterations, got ${outcome.iterations.length}`);

const first = outcome.iterations[0];
const reqs = first.samples.filter((s) => s.metric === "http_reqs");
if (reqs.length !== 2)
  fail(`expected 2 http_reqs in the first iteration (setNextRequest trace), got ${reqs.length}`);
if (!reqs.some((s) => Object.values(s.tags).includes("https://fixture.test/second")))
  fail("the jump target URL must appear in the trace");
if (first.scriptFailures !== 0)
  fail(`scripts must pass on the first iteration, got ${first.scriptFailures} failures`);
if (outcome.iterations[1].scriptFailures !== 0) fail("scripts must pass on the second iteration");

// The variable carried across the jump must have worked — a "carried
// variable" check sample exists when the assertion ran.
const checks = first.samples.filter((s) => s.metric === "checks");
if (checks.length < 2) fail(`expected 2 checks samples, got ${checks.length}`);

const expectedSeen = [
  "https://fixture.test/first",
  "https://fixture.test/second",
];
for (const url of expectedSeen) {
  if (!seenUrls.includes(url)) fail(`transport must have been asked for ${url}`);
}

// ── line-44 regression: the body envelope decodes unambiguously ──────────
// The wire body is a JSON envelope (http.rs body_to_wire): every non-Raw
// variant used to be misread as a 1-char garbage string. Build the wire by
// hand (Writer mirrors postcard) and assert each mode reconstructs exactly.
function wireRequestWithBody(envelope) {
  const w = new Writer();
  w.str("https://fixture.test/post");
  w.str("POST");
  w.strMap({ "content-type": "application/json" });
  w.strMap({});
  if (envelope === null) w.optionNone();
  else {
    w.optionSome();
    w.str(envelope);
  }
  return decodeHttpRequest(w.toUint8Array());
}
const jsonReq = wireRequestWithBody(JSON.stringify({ mode: "json", json: { a: 1 } }));
if (jsonReq.body !== '{"a":1}')
  fail(`json body must decode to the JSON text, got ${JSON.stringify(jsonReq.body)}`);
if (jsonReq.bodyMode !== "json") fail(`json bodyMode must be 'json', got ${jsonReq.bodyMode}`);
const rawReq = wireRequestWithBody(JSON.stringify({ mode: "raw", raw: "hello" }));
if (rawReq.body !== "hello") fail(`raw body must decode to the text, got ${JSON.stringify(rawReq.body)}`);
const urlReq = wireRequestWithBody(JSON.stringify({ mode: "url_encoded", fields: { a: "1", b: "two words" } }));
if (urlReq.body !== "a=1&b=two+words")
  fail(`url_encoded body must decode to a query string, got ${JSON.stringify(urlReq.body)}`);
const binReq = wireRequestWithBody(JSON.stringify({ mode: "binary", data: [1, 2, 3] }));
if (!binReq.bodyBytes || binReq.bodyBytes.length !== 3 || binReq.bodyBytes[2] !== 3)
  fail("binary body must decode to raw bytes");
const noneReq = wireRequestWithBody(null);
if (noneReq.bodyDecoded !== false || noneReq.body !== null)
  fail("absent body must decode as null, not garbage");
console.log("ok: body envelope decodes raw/json/url_encoded/binary/none unambiguously");

console.log(`PASS: native-path fixture through wasm32 runtime (${artifact})`);
console.log(`  iterations: ${outcome.iterations.length}, http_reqs: ${reqs.length}, checks: ${checks.length}`);
console.log(`  urls seen: ${seenUrls.join(", ")}`);
