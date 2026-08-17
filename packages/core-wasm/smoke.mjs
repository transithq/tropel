// Smoke test for @tropel/core-wasm (node >= 20). Run: node smoke.mjs
// Verifies: init-free catalog metadata, wasm init, dynamic-variable
// resolution, and the resolver's degradation contract.
import { readFileSync, existsSync } from "node:fs";
import { initCoreWasm, getPredefinedVariablesMeta, isCoreWasmReady, resolveDynamicVariables } from "./src/index.js";

// Metadata comes from pkg/meta.js (build-time extraction from the compiled
// catalog) — it must be available BEFORE and WITHOUT init.
if (!existsSync(new URL("./pkg/meta.js", import.meta.url))) {
  throw new Error("pkg/meta.js missing — run scripts/build.sh");
}
const preMeta = getPredefinedVariablesMeta();
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
const meta = getPredefinedVariablesMeta();
if (meta.length < 30) throw new Error(`metadata too small after init: ${meta.length}`);
if (meta !== preMeta) throw new Error("metadata must be the stable build-time payload");

console.log(`core-wasm smoke OK — catalog: ${meta.length} variables`);
