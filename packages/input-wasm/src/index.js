// @tropel/input-wasm — facade over the tropel collection-import slice
// (browser embedders).
//
// The input tier (crates/tropel-input-wasm) carries the bulky collection
// parsers — OpenAPI 3.x / Swagger 2.0, Postman v2.x, HAR — compiled to
// wasm32-unknown-unknown. It is a LAZY sibling of the eager core tier
// (packages/core-wasm): embedders fetch it only when the import UI opens,
// keeping the eagerly-loaded core small (API_CLIENT_WEB_PAYLOAD.md §2.3).
//
// Usage (KnockPort):
//   await initInputWasm();                      // when the import modal opens
//   const format = detect(bytes);               // "openapi" | "postman" | "har" | ""
//   const scenarioJson = importAny(bytes);      // dispatch by content detection
//   const scenarioJson = importById("openapi", bytes);  // explicit format
//
// Scenario JSON is the protocol-agnostic `tropel-sdk` shape
// (`info` / `items` / `variables` / `auth`); the embedder maps it to its own
// collection model in TypeScript (KnockPort packages/format).

let wasmInstance = null;
let glue = null;

/**
 * Initialize the input wasm. Resolves `true` when ready.
 * Options:
 *   - `wasmUrl`:   explicit URL/path for tropel_input_wasm_bg.wasm
 *                  (default: resolved relative to this module)
 *   - `wasmBytes`: ArrayBuffer/Uint8Array with the wasm (node/tests)
 */
export async function initInputWasm(options = {}) {
  if (wasmInstance) return true;
  try {
    const g = await import("../pkg/tropel_input_wasm.js");
    let source = options.wasmBytes;
    if (source === undefined) {
      const url = options.wasmUrl ?? new URL("../pkg/tropel_input_wasm_bg.wasm", import.meta.url);
      source = await (await fetch(url)).arrayBuffer();
    }
    wasmInstance = await g.default({ module_or_path: source });
    glue = g;
    return true;
  } catch (err) {
    console.warn("[tropel-input] input wasm unavailable — collection import disabled:", err);
    return false;
  }
}

/** True once initInputWasm() has resolved successfully. */
export function isInputWasmReady() {
  return wasmInstance !== null;
}

/** Detect the import format: "openapi" | "postman" | "har", or "" when the
 * bytes are not recognized. Requires the wasm (init first). */
export function detect(bytes) {
  return glue !== null ? glue.detect(bytes) : "";
}

/** Auto-detect and parse arbitrary import bytes → Scenario JSON. Throws when
 * nothing matches or the parser rejects the content. Requires the wasm. */
export function importAny(bytes) {
  return requireGlue("importAny").importAny(bytes);
}

/** Parse bytes as an explicitly-named format ("openapi"|"postman"|"har") →
 * Scenario JSON. Skips detection. Requires the wasm. */
export function importById(id, bytes) {
  return requireGlue("importById").importById(id, bytes);
}

function requireGlue(name) {
  if (glue === null) throw new Error(`[tropel-input] ${name} requires the input wasm (initInputWasm)`);
  return glue;
}