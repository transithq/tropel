// Smoke test for @tropel/shims: assert the built bundle's default order and
// byte-identity against the REPO's js/ sources (the single source of truth),
// that render() emits the engine's section separators, and that the k6
// family ships. Mirrors the parity contract in
// crates/tropel-engine/src/js_bootstrap.rs (ShimBundle).
//
// Run: node smoke.mjs   (needs dist/ built; fail-hard, no SKIP — the sources
// always exist in a checkout, so a missing bundle is a real failure).
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(__dirname, "..", "..");

// ── the engine's ShimBundle::default() order + repo paths ────────────────
// SINGLE AUTHORITY: crates/tropel-engine/src/js_bootstrap.rs
// (ShimBundle::default()). If that file gains or reorders an entry, update
// this list AND scripts/render-bundle.mjs's DEFAULT_ORDER together.
//
// LIST COMPLETENESS is checked separately, against the Rust source of truth —
// see assertListMatchesRust() below. Content parity alone is what let
// `k6-core-shim` be added to `Shim::ALL` and to 35 include_str! sites while
// packages/shims/ kept shipping the 7-shim bundle: every published
// @tropel/runtime-wasm embedder lost `check`/`group`/`Counter`/`Gauge`/
// `Rate`/`Trend`, because those had just moved out of pm.js into k6-core.js.
// Three hand-maintained copies of one list is the bug; the check has to read
// the original.
/**
 * Assert this file's DEFAULT_ORDER matches `Shim::ALL` in the engine.
 *
 * The Rust enum is the single source of truth for which shims a default
 * bundle contains (js_bootstrap.rs: `Shim::ALL` + `Shim::name()`). Parsing it
 * is uglier than duplicating the list, and that is exactly the point — a
 * duplicate silently diverges, a parse fails loudly.
 */
function assertListMatchesRust(order) {
  const src = readFileSync(
    new URL("../../crates/tropel-engine/src/js_bootstrap.rs", import.meta.url),
    "utf8",
  );

  // `pub const ALL: [Shim; N] = [ Shim::DeepEqual, Shim::K6Core, … ];`
  const allBlock = src.match(/pub const ALL: \[Shim; \d+\] = \[([\s\S]*?)\];/);
  if (!allBlock) throw new Error("could not find `Shim::ALL` in js_bootstrap.rs");
  const variants = [...allBlock[1].matchAll(/Shim::(\w+)/g)].map((m) => m[1]);

  // `Shim::DeepEqual => "deep-equal-shim",`
  const names = new Map(
    [...src.matchAll(/Shim::(\w+)\s*=>\s*"([a-z0-9-]+)"/g)].map((m) => [m[1], m[2]]),
  );

  const expected = variants.map((v) => {
    const n = names.get(v);
    if (!n) throw new Error(`Shim::${v} is in ALL but has no name() arm`);
    return n;
  });
  const actual = order.map(([name]) => name);

  const missing = expected.filter((n) => !actual.includes(n));
  const extra = actual.filter((n) => !expected.includes(n));
  if (missing.length || extra.length) {
    throw new Error(
      `shim bundle list has drifted from Shim::ALL\n` +
        `  missing here: ${missing.join(", ") || "(none)"}\n` +
        `  not in Rust : ${extra.join(", ") || "(none)"}\n` +
        `  Rust order  : ${expected.join(", ")}\n` +
        `  this file   : ${actual.join(", ")}`,
    );
  }
  if (expected.join() !== actual.join()) {
    throw new Error(
      `shim ORDER differs from Shim::ALL (load order is load-bearing: ` +
        `k6-core must precede pm)\n  Rust: ${expected.join(", ")}\n  here: ${actual.join(", ")}`,
    );
  }
  console.log(`  list matches Shim::ALL (${expected.length} shims, same order)`);
}

const DEFAULT_ORDER = [
  ["deep-equal-shim", "js/shared/deep-equal.js"],
  ["k6-core-shim", "js/shared/k6-core.js"],
  ["pm-shim", "js/scripting-api/pm.js"],
  ["chai-shim", "js/chai/chai-shim.js"],
  ["lodash-shim", "js/lodash/lodash-shim.js"],
  ["cryptojs-shim", "js/cryptojs-shim/cryptojs.js"],
  ["exec-shim", "js/exec/exec.js"],
  ["bru-shim", "js/scripting-api/bru.js"],
];
const K6_ORDER = [
  ["k6-shim", "js/k6-shim/k6-shim.js"],
  ["jslib-shim", "js/k6-shim/jslib-shim.js"],
  ["open-data-shim", "js/k6-shim/open-data-shim.js"],
  ["sleep-shim", "js/k6-shim/sleep-shim.js"],
];

const bundlePath = path.join(__dirname, "dist", "bundle.js");
if (!existsSync(bundlePath)) {
  console.error("FAIL: dist/bundle.js not built — run npm run build");
  process.exit(1);
}
const shimDir = path.join(__dirname, "shim");
if (!existsSync(shimDir)) {
  console.error("FAIL: shim/ not populated — run npm run build");
  process.exit(1);
}

const { defaultBundle, k6Bundle, render } = await import("./dist/bundle.js");

// Completeness FIRST: if the list itself has drifted from the engine, every
// content comparison below is comparing the wrong set and will still pass.
assertListMatchesRust(DEFAULT_ORDER);

function fail(msg) {
  console.error(`FAIL: ${msg}`);
  process.exit(1);
}

// ── default bundle: order + byte-identity with the repo sources ──────────
if (defaultBundle.length !== DEFAULT_ORDER.length)
  fail(
    `default bundle has ${defaultBundle.length} entries, expected ${DEFAULT_ORDER.length}`
  );
for (let i = 0; i < DEFAULT_ORDER.length; i++) {
  const [name, rel] = DEFAULT_ORDER[i];
  const entry = defaultBundle[i];
  if (entry.name !== name)
    fail(`default entry ${i} is '${entry.name}', expected '${name}'`);
  const repoSource = readFileSync(path.join(repo, rel), "utf8");
  if (entry.source !== repoSource)
    fail(`'${name}' source differs from js/${rel} (stale copy?)`);
}

// ── k6 family ships ──────────────────────────────────────────────────────
if (k6Bundle.length !== K6_ORDER.length)
  fail(`k6 bundle has ${k6Bundle.length} entries, expected ${K6_ORDER.length}`);
for (const [name, rel] of K6_ORDER) {
  const entry = k6Bundle.find((e) => e.name === name);
  if (!entry) fail(`k6 shim '${name}' missing`);
  const repoSource = readFileSync(path.join(repo, rel), "utf8");
  if (entry.source !== repoSource) fail(`'${name}' source differs from ${rel}`);
}

// ── shared-scope name collisions (k6 family) ─────────────────────────────
// The k6 shims are concatenated into ONE shared scope
// (K6_NATIVE_SHIM_BUNDLE / render()), so a duplicate top-level function
// silently shadows the earlier definition — last file wins regardless of
// which file is conceptually authoritative. Regression guard for the
// base64ToBytes collision: open-data-shim's helper is private-named
// (openDataBase64ToBytes) so the k6-shim's Uint8Array version — whose
// .buffer is consumed by http()/file callers — must be the ONLY definition.
const joinedK6 = k6Bundle.map((e) => e.source).join("\n");
const b64Defs = [...joinedK6.matchAll(/function\s+base64ToBytes\s*\(/g)];
if (b64Defs.length !== 1)
  fail(
    `k6 bundle defines base64ToBytes ${b64Defs.length} times (expected exactly 1 — open-data-shim must keep its private openDataBase64ToBytes name)`
  );
// Scope the body check to the single definition (bounded by the NEXT top-level
// `function` or the bundle end) so a later unrelated `.subarray(0, o)` can
// never false-pass a plain-Array regression.
const defStart = b64Defs[0].index;
const nextFn = joinedK6.indexOf("function ", defStart + 1);
const defEnd = nextFn === -1 ? joinedK6.length : nextFn;
if (defEnd < 0 || !joinedK6.slice(defStart, defEnd).includes("return out.subarray(0, o);"))
  fail("base64ToBytes must be the k6-shim Uint8Array version (returns .subarray(0, o))");
if (!joinedK6.includes("openDataBase64ToBytes"))
  fail("open-data-shim's private openDataBase64ToBytes helper is missing");

// ── shim/ copies must match their js/ twins (tarball completeness) ───────
// The shipped shim/ dir is FLAT — build.sh copies each repo source to its
// bare basename (js/chai/chai-shim.js -> shim/chai-shim.js) — so the twin
// filename is simply path.basename of the repo-relative path.
for (const [, repoRel] of [...DEFAULT_ORDER, ...K6_ORDER]) {
  const shipped = readFileSync(path.join(__dirname, "shim", path.basename(repoRel)), "utf8");
  const twin = readFileSync(path.join(repo, repoRel), "utf8");
  if (shipped !== twin) fail(`shim/${path.basename(repoRel)} is stale vs ${repoRel}`);
}

// ── render(): engine separators, byte-exact ──────────────────────────────
const rendered = render();
for (const [name, rel] of DEFAULT_ORDER) {
  const sep = `// ==== shim: ${name} ====\n`;
  if (!rendered.includes(sep))
    fail(`render() missing the '${name}' section header`);
  const src = readFileSync(path.join(repo, rel), "utf8");
  if (!rendered.includes(sep + src)) fail(`render() corrupted '${name}'`);
}

console.log("PASS: @tropel/shims bundle matches the engine's ShimBundle");
console.log(
  `  default: ${defaultBundle.map((e) => e.name).join(", ")}`
);
console.log(
  `  k6: ${k6Bundle.map((e) => e.name).join(", ")}`
);
console.log(`  total source bytes: ${[...defaultBundle, ...k6Bundle].reduce((n, e) => n + e.source.length, 0)}`);
