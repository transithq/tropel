// Smoke test for @tropel/input-wasm — runs against the REAL compiled wasm
// (pkg/tropel_input_wasm_bg.wasm), mirroring packages/core-wasm/smoke.mjs.
import { readFileSync } from "node:fs";
import { initInputWasm, detect, importAny, importById } from "./src/index.js";

const text = (s) => new TextEncoder().encode(s);

await initInputWasm({ wasmBytes: readFileSync("./pkg/tropel_input_wasm_bg.wasm") });

const postman = text(JSON.stringify({
  info: {
    name: "Smoke Collection",
    schema: "https://schema.getpostman.com/json/collection/v2.1.0/collection.json",
  },
  item: [{ name: "GET Users", request: { method: "GET", url: { raw: "https://api.example.com/users" } } }],
}));

const openapi = text(JSON.stringify({
  openapi: "3.0.3",
  info: { title: "Pets", version: "1.0.0" },
  paths: { "/pets": { get: { summary: "List pets", responses: { "200": { description: "ok" } } } } },
}));

const har = text(JSON.stringify({
  log: {
    version: "1.2",
    entries: [{
      request: { method: "GET", url: "https://example.com/", headers: [], queryString: [] },
      response: { status: 200, statusText: "OK" },
    }],
  },
}));

let failures = 0;
const check = (cond, label) => {
  if (!cond) { console.error(`FAIL: ${label}`); failures++; }
  else console.log(`ok: ${label}`);
};

check(detect(postman) === "postman", "detect postman");
check(detect(openapi) === "openapi", "detect openapi");
check(detect(har) === "har", "detect har");
check(detect(text("hello")) === "", "detect unknown");

const s1 = JSON.parse(importAny(postman));
check(s1.info.name === "Smoke Collection" && s1.items.length === 1, "importAny postman");

const s2 = JSON.parse(importAny(openapi));
check(s2.info.name === "Pets" && s2.items.length === 1, "importAny openapi");

const s3 = JSON.parse(importAny(har));
check(s3.items.length === 1, "importAny har");

const s4 = JSON.parse(importById("openapi", openapi));
check(s4.info.name === "Pets", "importById openapi");

let threw = false;
try { importById("bogus", openapi); } catch { threw = true; }
check(threw, "importById unknown format throws");

if (failures > 0) {
  console.error(`smoke: ${failures} failure(s)`);
  process.exit(1);
}
console.log("smoke: all checks passed");