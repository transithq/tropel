// @tropel/core-wasm — typings (see index.js).

export interface PredefinedVariableMeta {
  name: string;
  description: string;
}

export interface InitCoreWasmOptions {
  /** Explicit URL for tropel_core_wasm_bg.wasm. */
  wasmUrl?: string | URL | Request;
  /** Pre-fetched wasm bytes (node/tests). */
  wasmBytes?: ArrayBuffer | Uint8Array;
}

/**
 * Initialize the core wasm. Resolves `true` when ready, `false` when wasm is
 * unavailable (the resolver then degrades to a no-op passthrough).
 */
export function initCoreWasm(options?: InitCoreWasmOptions): Promise<boolean>;

/** True once initCoreWasm() has resolved successfully. */
export function isCoreWasmReady(): boolean;

/**
 * Resolve every predefined dynamic variable (`{{$guid}}`, `{{$timestamp}}`, …)
 * — fresh value per occurrence, Tropel semantics. Plain `{{var}}` refs are
 * untouched. Returns the input unchanged if wasm is not ready.
 */
export function resolveDynamicVariables(template: string): string;

/** Catalog metadata `[{"name":"$guid","description":…}]` for editor UIs. */
export function getPredefinedVariablesMeta(): PredefinedVariableMeta[];

/** Just the `$`-prefixed catalog names (autocomplete lists). */
export function getPredefinedVariableNames(): string[];
