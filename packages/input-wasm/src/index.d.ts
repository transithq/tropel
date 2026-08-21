// @tropel/input-wasm — typings (see index.js).

export interface InitInputWasmOptions {
  /** Explicit URL for tropel_input_wasm_bg.wasm. */
  wasmUrl?: string | URL | Request;
  /** Pre-fetched wasm bytes (node/tests). */
  wasmBytes?: ArrayBuffer | Uint8Array;
}

/** Initialize the input wasm. Resolves `true` when ready, `false` when wasm
 * is unavailable (imports then degrade to the TS fallback). */
export function initInputWasm(options?: InitInputWasmOptions): Promise<boolean>;

/** True once initInputWasm() has resolved successfully. */
export function isInputWasmReady(): boolean;

/** Detect the import format id: `"openapi"` | `"postman"` | `"har"`, or `""`
 * when the bytes are not recognized. */
export function detect(bytes: Uint8Array): string;

/** Auto-detect and parse arbitrary import bytes → Scenario JSON. Throws when
 * nothing matches or the parser rejects the content. */
export function importAny(bytes: Uint8Array): string;

/** Parse bytes as an explicitly-named format → Scenario JSON. Skips
 * detection; throws on an unknown format id or parse failure. */
export function importById(id: string, bytes: Uint8Array): string;