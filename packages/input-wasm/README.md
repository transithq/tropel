# @tropel/input-wasm

Tropel collection-import slice for browser embedders (KnockPort).

A **lazy** sibling of `@tropel/core-wasm`: the eager core tier stays small
(variables + auth, hard 700 KB budget gate), while the bulky collection
parsers live here and are fetched only when the import UI opens
(see `API_CLIENT_WEB_PAYLOAD.md` §2.3 two-tier split).

## Formats

- **OpenAPI 3.x / Swagger 2.0** — JSON or YAML, intra-document `$ref`
  resolution, server-variable defaulting, path-parameter substitution,
  security → auth.
- **Postman Collection v2.1 / v2.0** — nested folders, per-request
  headers/query/body/auth.
- **HAR** — replayable request list (static assets filtered).

## API

```js
import { initInputWasm, detect, importAny, importById } from "@tropel/input-wasm";

await initInputWasm();               // when the import modal opens
detect(bytes);                       // "openapi" | "postman" | "har" | ""
importAny(bytes);                    // dispatch by content detection → Scenario JSON
importById("openapi", bytes);        // explicit format → Scenario JSON
```

Output is the protocol-agnostic `tropel-sdk` `Scenario` shape
(`info` / `items` / `variables` / `auth`). Embedders map it to their own
collection model in TypeScript (KnockPort `packages/format`).

## Build

```sh
npm run build   # cargo build release-wasm + wasm-bindgen + wasm-opt + smoke
```

Requires `wasm-bindgen` on PATH and a Rust toolchain with the
`wasm32-unknown-unknown` target installed.