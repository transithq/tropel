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

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// ── locate the wasm artifact ─────────────────────────────────────────────
function findArtifact() {
  const candidates = [
    process.env.TROPEL_WASM_PATH,
    path.join(__dirname, "wasm", "tropel_web.wasm"),
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
const seenBodies = [];
const exec = await createExecWasm({
  wasmBytes,
  wasiImports,
  // node:wasi preview1 needs initialize(instance) after instantiation.
  onInstantiate: (instance) => wasi.initialize(instance),
  transport: (req) => {
    seenUrls.push(req.url);
    // Backlog line 44: the first item carries a JSON body; it must arrive
    // INTACT across the bridge (not the old one-byte junk body).
    seenBodies.push(req.body);
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
        // Backlog line 43: total is the EIGHTH Timings field — its absence
        // previously broke the round-trip with DeserializeUnexpectedEnd.
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
const item = (name, url, test, body) => ({
  name,
  request: {
    url,
    method: "GET",
    headers: {},
    query_params: {},
    body: body ?? null,
    auth: null,
    certificate: null,
    follow_redirects: true,
    timeout: null,
    response_type: "text",
  },
  prerequest: [],
  test: [test],
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
        "pm.execution.setNextRequest('second');",
      // Backlog line 44: a JSON body must survive the tropel_host_http bridge.
      { ok: true }
    ),
    item(
      "second",
      "https://fixture.test/second",
      "pm.test('carried variable', () => pm.expect(pm.variables.get('carried')).to.eql('yes'));" +
        "pm.test('second status', () => pm.expect(pm.response.code).to.eql(200));",
      null // no body — symmetric with the F3 fixture
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

// W1-A wire-P0 regression: a decode failure still emits http_reqs and
// pm.test swallows the assertion into a 0.0 check, so COUNTING checks can
// never catch it. Every check must have PASSED — value === 1 (a 0.0 check
// means the assertion body threw or the wire decoded a failure as pass).
const failedChecks = checks.filter((s) => s.value !== 1);
if (failedChecks.length > 0)
  fail(`all checks must PASS (value === 1), got ${failedChecks.length} failing`);

// W1-A wire-P0 regression: http_req_failed (Rate) must be 0 for every
// request — the fixture transport answers 200. A re-introduced wire P0
// that mis-decodes the failure flag (or drops it) surfaces here even
// though http_reqs/checks still emit.
const reqFailed = first.samples.filter((s) => s.metric === "http_req_failed");
if (reqFailed.length === 0)
  fail(`expected http_req_failed samples, got none (wire P0: failure flag missing)`);
const failedReqs = reqFailed.filter((s) => s.value !== 0);
if (failedReqs.length > 0)
  fail(`all requests must succeed (http_req_failed === 0), got ${failedReqs.length} failed`);

const expectedSeen = [
  "https://fixture.test/first",
  "https://fixture.test/second",
];
for (const url of expectedSeen) {
  if (!seenUrls.includes(url)) fail(`transport must have been asked for ${url}`);
}

// Backlog line 44: the JSON body on the first item must arrive INTACT —
// {"ok":true} rendered from the Body::Json envelope — not a one-byte junk
// string (the old positional postcard reader). The first request per
// iteration is the JSON-body one.
if (seenBodies[0] !== '{"ok":true}')
  fail(`JSON body corrupted across the bridge: got ${JSON.stringify(seenBodies[0])}`);

console.log(`PASS: native-path fixture through wasm32 runtime (${artifact})`);
console.log(`  iterations: ${outcome.iterations.length}, http_reqs: ${reqs.length}, checks: ${checks.length}`);
console.log(`  urls seen: ${seenUrls.join(", ")}`);
