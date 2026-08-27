# The tropel npm package set — TR-401 contract

**This file is the source of truth for the published npm set.** The old
`TROPEL_EXEC_SPLIT.md` §5 designed a single `@tropel/exec-wasm` package; the
tree evolved into four packages and the split doc never landed. Rather than
renaming the packages to an abandoned plan, this contract pins down what
actually ships. If a document disagrees with this file, this file wins.

## The four packages

| Package | Tier | Contains | Loaded by |
|---|---|---|---|
| **`@tropel/core-wasm`** | eager (loads at boot) | variables + auth: `DynamicCatalog` (dynamic vars), `AuthConfig`/OAuth adapters, `resolveVariables` | the web app's boot path |
| **`@tropel/input-wasm`** | lazy (loads on demand) | collection import: Postman/k6/HAR/OpenAPI/… adapters, `parseCollection` | the import flow (drag-drop / paste) |
| **`@tropel/runtime-wasm`** | lazy | the scripting runtime: the JS bundle (shims) + the pm/k6 surface compiled to wasm | load runs |
| **`@tropel/shims`** | boot | the JS scripting bundle (lodash, chai, crypto-js, the k6/pm shims) | injected into the runtime-wasm context |

## What the client consumes

knockport consumes `@tropel/core-wasm` (eager), `@tropel/input-wasm` (lazy)
and `@tropel/shims`; its desktop build links `tropel-runtime` natively
(zero Rust in the client repo — the native path is a spawned `tropel agent`
over a socket, see TR-405).

## Naming history

`@tropel/runtime-wasm` was renamed from `@tropel/exec-wasm`. The dead
`!packages/exec-wasm/*` ignore lines were removed with TR-008. Any code,
doc or CI referencing `@tropel/exec-wasm` is wrong — use
`@tropel/runtime-wasm`.

## Versioning

All four packages share ONE version number with the binary, `tropel-web` and
`tropel-sdk`, stamped by the same CI job (TR-406). On connect, the client
compares the agent's version against the loaded wasm's; a mismatch is a
visible warning and load results are marked unverified-parity.
