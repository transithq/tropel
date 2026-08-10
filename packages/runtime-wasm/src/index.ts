// @tropel/runtime-wasm — host for the tropel-web wasm32-wasip1 runtime slice
// (TROPEL_WASM_BUILD.md Step 5A / Shape A).
//
// Loads `tropel_web.wasm`, wires WASI preview1, implements the
// `env.tropel_host_http` import (the DriverHttpClient bridge), and exposes a
// synchronous `run()` that speaks the postcard C ABI (`tropel_alloc` /
// `tropel_run` / `tropel_free`) over linear memory.

import {
  decodeHttpRequest,
  decodeRunOutcome,
  encodeResponse,
  encodeRunRequest,
} from "./postcard.js";
import type { HttpRequest, HttpResponse, RunOutcome, RunRequest } from "./types.js";
import { render as renderBundle, defaultBundle } from "@tropel/shims";

// N1: the default shim bundle the host supplies to the wasm runtime — the
// same sources the native engine embeds (ShimBundle::default() order),
// shipped as an asset via @tropel/shims so a pm.*/trp.* fix needs no wasm
// rebuild or release.
const defaultShimSource: string = renderBundle(defaultBundle);

/** How a response is produced for a request the runtime makes. */
export type Transport = (req: HttpRequest) => HttpResponse;

export interface ExecWasmOptions {
  /** Bytes of the compiled `tropel_web.wasm` artifact. */
  wasmBytes: ArrayBuffer | Uint8Array;
  /** Synchronous transport answering each request (the wasm call is sync). */
  transport: Transport;
  /**
   * WASI preview1 import object (the `wasi_snapshot_preview1` module). In a
   * browser pass `@bjorn3/browser_wasi_shim`'s `wasi.wasiImport`; in Node,
   * `node:wasi`'s `wasi.wasiImport`. When omitted the wrapper attempts a
   * dynamic `node:wasi` import, then falls back to the browser shim.
   */
  wasiImports?: WebAssembly.ModuleImports;
  /**
   * The shim bundle source text to feed the runtime's JS contexts (N1,
   * TROPEL_MODULARIZATION_REVIEW_R2.md): the wasm slice no longer embeds the
   * shims, so the host supplies them via the `tropel_host_shim` import.
   * Defaults to `@tropel/shims`' `render(defaultBundle)` when omitted. A
   * `pm.*`/`trp.*` fix then ships as a JS asset with the app — no wasm
   * rebuild, no release.
   */
  shimSource?: string;
  /** Post-instantiation hook (e.g. `wasi.initialize(instance)`). */
  onInstantiate?: (instance: WebAssembly.Instance) => void;
}

export interface ExecWasm {
  /**
   * Run one scenario pass synchronously. The wasm's `tropel_run` is a
   * blocking call — the transport answers each request inline. Returns the
   * decoded outcome, or `{ iterations: [], error }` on a fatal failure.
   */
  run(req: RunRequest): RunOutcome;
  /**
   * The runtime version compiled into the wasm (tropel-web's
   * `CARGO_PKG_VERSION`, read via the `tropel_version` C ABI export).
   * P6 version handshake: compare against the connected `tropel agent`'s
   * version via [`checkVersionParity`] — a mismatch means unverified-parity
   * results.
   */
  runtimeVersion: string;
}

interface WasmExports {
  memory: WebAssembly.Memory;
  tropel_alloc(len: number): number;
  tropel_free(ptr: number, len: number): void;
  tropel_run(ptr: number, len: number): bigint;
  tropel_version(): bigint;
}

/**
 * P6 version handshake: compare a `tropel agent`'s version against the wasm
 * runtime version. Returns `{ matched, warning }` — callers should surface
 * the warning visibly and mark results unverified-parity when `matched` is
 * false (the API client's load contract, TROPEL_MODULARIZATION_TODO.md P6).
 */
export function checkVersionParity(
  agentVersion: string,
  wasmVersion: string
): { matched: boolean; warning: string | null } {
  if (agentVersion === wasmVersion) {
    return { matched: true, warning: null };
  }
  return {
    matched: false,
    warning:
      `tropel agent ${agentVersion} != wasm runtime ${wasmVersion} — ` +
      `mixed-version deployment; results marked unverified-parity`,
  };
}

// Minimal WASI surface this host needs. Both providers (Node's built-in
// `node:wasi` and the optional @bjorn3/browser_wasi_shim) are resolved via
// string-cast dynamic imports and typed through these interfaces, so the
// package typechecks WITHOUT @types/node or the shim installed.
interface NodeWasi {
  WASI: new (opts: {
    version: "preview1";
    args: string[];
    env: Record<string, string>;
    preopens: Record<string, string>;
  }) => {
    wasiImport: WebAssembly.ModuleImports;
    initialize(instance: WebAssembly.Instance): void;
  };
}

// The minimal @bjorn3/browser_wasi_shim surface this host uses.
interface BrowserWasiShim {
  WASI: new (
    args: string[],
    env: string[][],
    fds: unknown[]
  ) => {
    wasiImport: WebAssembly.ModuleImports;
    initialize(instance: WebAssembly.Instance): void;
  };
  File: new (data: Uint8Array) => unknown;
  OpenFile: new (file: unknown) => unknown;
  ConsoleStdout: {
    lineBuffered(cb: (msg: string) => void): unknown;
  };
}

/**
 * Build WASI preview1 imports, preferring an explicit override, then the
 * caller's environment (Node's `node:wasi`, else `@bjorn3/browser_wasi_shim`).
 */
async function resolveWasiImports(override?: WebAssembly.ModuleImports): Promise<{
  imports: WebAssembly.ModuleImports;
  onInstantiate?: (instance: WebAssembly.Instance) => void;
}> {
  if (override) return { imports: override };

  try {
    // Node ≥ 20 ships preview1 WASI in `node:wasi`.
    const { WASI } = (await import("node:wasi" as string)) as NodeWasi;
    const wasi = new WASI({ version: "preview1", args: [], env: {}, preopens: {} });
    return {
      imports: wasi.wasiImport,
      onInstantiate: (instance) => wasi.initialize(instance),
    };
  } catch {
    try {
      const mod = (await import("@bjorn3/browser_wasi_shim" as string)) as BrowserWasiShim;
      const wasi = new mod.WASI([], [], [
        new mod.OpenFile(new mod.File(new Uint8Array())),
        mod.ConsoleStdout.lineBuffered((m) => console.log(m)),
        mod.ConsoleStdout.lineBuffered((m) => console.warn(m)),
      ]);
      return {
        imports: wasi.wasiImport,
        onInstantiate: (instance) => wasi.initialize(instance),
      };
    } catch {
      throw new Error(
        "@tropel/runtime-wasm: no WASI provider — pass wasiImports, or install " +
          "@bjorn3/browser_wasi_shim (browser) / run on Node ≥ 20"
      );
    }
  }
}

export async function createExecWasm(options: ExecWasmOptions): Promise<ExecWasm> {
  // NOTE: resolveWasiImports returns the provider's auto-init hook only when
  // IT constructed the WASI (no override). The caller's own onInstantiate
  // (e.g. `wasi.initialize(instance)` for node:wasi) must be run separately —
  // both hooks fire, provider first.
  const resolved = await resolveWasiImports(options.wasiImports);
  const wasiImports = resolved.imports;

  // The host function must reach the instance it belongs to; the instance
  // only exists after instantiation. Captured in a mutable ref — host
  // functions are only ever called during tropel_run, i.e. post-instantiate.
  let instanceRef: WebAssembly.Instance | null = null;

  // N1: the wasm slice imports tropel_host_shim for its shim bundle. The
  // default bundle is @tropel/shims' render(defaultBundle) — the same
  // sources the native engine embeds (ShimBundle::default()); callers can
  // override with their own bundle (e.g. a newer pm.js fix shipped as an
  // asset). Bytes are copied into linear memory via the module's own
  // tropel_alloc, packed (ptr << 32) | len; 0 = no bundle (shim-less run).
  const hostShim = (): bigint => {
    const src = options.shimSource ?? defaultShimSource;
    if (!src) return 0n;
    const instance = instanceRef!;
    const exports = instance.exports as unknown as WasmExports;
    const bytes = new TextEncoder().encode(src);
    const outPtr = exports.tropel_alloc(bytes.length);
    new Uint8Array(exports.memory.buffer).set(bytes, outPtr);
    return (BigInt(outPtr) << 32n) | BigInt(bytes.length);
  };

  const hostHttp = (reqPtr: number, reqLen: number): bigint => {
    const instance = instanceRef!;
    const exports = instance.exports as unknown as WasmExports;
    const mem = () => new Uint8Array(exports.memory.buffer);

    // Copy the postcard Request out of linear memory.
    const reqBytes = mem().slice(reqPtr, reqPtr + reqLen);
    const req: HttpRequest = decodeHttpRequest(reqBytes);

    const resp: HttpResponse = options.transport(req);
    const respBytes = encodeResponse(resp);

    // Allocate the reply via the module's own tropel_alloc (re-entrant call,
    // the same pattern the Rust harness and the JS host use) and write it.
    const outPtr = exports.tropel_alloc(respBytes.length);
    mem().set(respBytes, outPtr);
    return (BigInt(outPtr) << 32n) | BigInt(respBytes.length);
  };

  const imports: WebAssembly.Imports = {
    wasi_snapshot_preview1: wasiImports,
    env: { tropel_host_http: hostHttp, tropel_host_shim: hostShim },
  };

  const { instance } = await WebAssembly.instantiate(
    // TS 5.9's BufferSource is `ArrayBufferView<ArrayBuffer> | ArrayBuffer`, and
    // `wasmBytes: ArrayBuffer | Uint8Array` narrows to `Uint8Array<ArrayBufferLike>`
    // (SharedArrayBuffer allowed) — which fails overload 1 and falls through to
    // the `Module` overload, typing the result as `Instance`. The cast forces
    // the buffer-source overload (`WebAssemblyInstantiatedSource`).
    (options.wasmBytes instanceof Uint8Array
      ? options.wasmBytes
      : new Uint8Array(options.wasmBytes)) as BufferSource,
    imports
  );
  instanceRef = instance;
  resolved.onInstantiate?.(instance);
  options.onInstantiate?.(instance);

  const exports = instance.exports as unknown as WasmExports;

  // Read the runtime version from the wasm (P6 handshake surface).
  const mem0 = () => new Uint8Array(exports.memory.buffer);
  const verPacked = exports.tropel_version();
  const verPtr = Number(verPacked >> 32n);
  const verLen = Number(verPacked & 0xffff_ffffn);
  const runtimeVersion = new TextDecoder().decode(
    mem0().slice(verPtr, verPtr + verLen)
  );

  return {
    runtimeVersion,
    run(req: RunRequest): RunOutcome {
      const mem = () => new Uint8Array(exports.memory.buffer);

      // Encode + allocate + write the request.
      const reqBytes = encodeRunRequest(req);
      const reqPtr = exports.tropel_alloc(reqBytes.length);
      mem().set(reqBytes, reqPtr);

      const packed = exports.tropel_run(reqPtr, reqBytes.length);
      // The HOST owns the request buffer: tropel_run borrows it via
      // slice::from_raw_parts and never frees it (lib.rs) — reclaim it here.
      exports.tropel_free(reqPtr, reqBytes.length);

      if (packed === 0n) {
        return { iterations: [], error: "tropel_run returned 0 (fatal internal failure)" };
      }
      const outPtr = Number(packed >> 32n);
      const outLen = Number(packed & 0xffff_ffffn);
      const outBytes = mem().slice(outPtr, outPtr + outLen);
      exports.tropel_free(outPtr, outLen);

      return decodeRunOutcome(outBytes);
    },
  };
}
