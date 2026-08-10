# @tropel/exec-wasm

Browser/Node host for the **tropel** load-testing engine compiled to
`wasm32-wasip1` — the same runtime that powers tropel-native, running inside
WebAssembly.

The package wires WASI preview1, implements the `env.tropel_host_http` HTTP
bridge, and speaks the postcard C ABI (`tropel_alloc` / `tropel_run` /
`tropel_free`) over the module's linear memory — so a scenario collection
(Postman, k6, OpenAPI, HAR) runs **inside the wasm runtime**, in-process, with
the host answering each HTTP request synchronously.

## Install

```bash
npm install @tropel/exec-wasm
```

Requires Node ≥ 20 (built-in WASI preview1) or `@bjorn3/browser_wasi_shim`
for browsers.

## Usage

```js
import { createExecWasm } from "@tropel/exec-wasm";

const exec = await createExecWasm({
  wasmBytes: await fetch("/tropel_web.wasm").then((r) => r.arrayBuffer()),
  // Answer every request the runtime makes (synchronous — the wasm call is
  // blocking). Wire this to fetch/XHR/your transport.
  transport: (req) => ({
    url: req.url,
    statusCode: 200,
    statusText: "OK",
    headers: { "content-type": "application/json" },
    body: new TextEncoder().encode('{"ok":true}'),
    responseTimeMs: 5,
    timings: {
      blockedMs: 2, dnsMs: 2, connectingMs: 5, tlsHandshakingMs: 0,
      sendingMs: 0, waitingMs: 5, receivingMs: 5,
    },
    size: 12,
  }),
});

const outcome = exec.run({
  scenarioJson: JSON.stringify(scenario), // Postman-format collection
  vuId: 1,
  scenarioName: "checkout",
  iterations: 2,
  envVars: {},
  expectedStatuses: ["200"],
});
```

`outcome` is the full run result: per-iteration samples (`http_reqs`,
`http_req_duration`, `checks`, …) with tags, timestamps, and script-failure
counts — the same shape tropel-native produces, so results are comparable
across engines.

## Browser

In a browser, pass the WASI shim's import object:

```js
import { WASI } from "@bjorn3/browser_wasi_shim";

const wasi = new WASI([], [], [
  // stdin/stdout/stderr fds
]);
const exec = await createExecWasm({
  wasmBytes,
  wasiImports: wasi.wasiImport,
  onInstantiate: (instance) => wasi.initialize(instance),
  transport,
});
```

## Build & test

```bash
npm run build   # compiles dist/, copies wasm/tropel_web.wasm, dry-runs npm pack
npm test        # smoke.mjs — drives the F3 fixture through node:wasi
```

The packaged artifact is built by Tropel's CI (wasm job) and gate-checked with
`TROPEL_REQUIRE_WASM=1`, so a missing or stale `wasm/` never ships.

## License

Apache-2.0
