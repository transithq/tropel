// Smoke test for @tropel/core-wasm (node >= 20). Run: node smoke.mjs
// Verifies: wasm init, dynamic-variable resolution, catalog metadata,
// plain {{var}} passthrough, and fresh-per-occurrence semantics.
import { readFileSync } from "node:fs";
import { initCoreWasm, getPredefinedVariablesMeta, isCoreWasmReady, resolveDynamicVariables } from "./src/index.js";

const bytes = readFileSync(new URL("./pkg/tropel_core_wasm_bg.wasm", import.meta.url));

// Before init: no-op degradation.
const pre = resolveDynamicVariables("id={{$guid}}");
if (!pre.includes("{{$guid}}")) throw new Error("pre-init must not resolve");
if (isCoreWasmReady()) throw new Error("must not be ready before init");
if (getPredefinedVariablesMeta().length !== 0) throw new Error("pre-init metadata must be empty");

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

// Metadata covers the catalog.
const meta = getPredefinedVariablesMeta();
if (meta.length < 30) throw new Error(`metadata too small: ${meta.length}`);
for (const m of meta) {
  if (!m.name.startsWith("$") || !m.description) throw new Error(`bad meta entry: ${JSON.stringify(m)}`);
}

console.log(`core-wasm smoke OK — catalog: ${meta.length} variables`);
