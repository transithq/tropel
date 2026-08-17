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
//
// Until init resolves (or in environments without WebAssembly) the resolver
// degrades to a no-op passthrough — `{{$…}}` survive literal and the
// embedder's own {{var}} map still resolves. The embedder may keep a small
// TS fallback for exactly that race.

let wasmInstance = null;
let glue = null;
let metaCache = null;

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
 * ready (never throws).
 */
export function resolveDynamicVariables(template) {
  return glue !== null ? glue.resolveVariables(template) : template;
}

/** Catalog metadata `[{"name":"$guid","description":…}]` for editor UIs. */
export function getPredefinedVariablesMeta() {
  if (glue === null) return [];
  metaCache ??= JSON.parse(glue.predefinedVariablesMeta());
  return metaCache;
}

/** Just the `$`-prefixed catalog names (autocomplete lists). */
export function getPredefinedVariableNames() {
  return getPredefinedVariablesMeta().map((m) => m.name);
}
