# @tropel/shims

The **tropel JS shim bundle** — the scripting-API libraries the tropel load
testing runtime embeds into every virtual-user JS context:

| name | source |
|---|---|
| `pm-shim` | Postman-style scripting API (`pm.*`) |
| `bru-shim` | Bruno-style scripting API (`bru.*`) |
| `chai-shim` | Chai-style assertions (`pm.expect(...).to.eql(...)`) |
| `lodash-shim` | `_` utilities |
| `cryptojs-shim` | CryptoJS crypto helpers |
| `exec-shim` | `exec` / child-process bridge |
| `k6-shim` | k6-style API (`http.get`, `check`, …) |
| `open-data-shim` | k6 `open()` file loader |
| `sleep-shim` | k6 `sleep()` |

The **default bundle** is byte-identical to the engine's `ShimBundle::default()`
(`crates/tropel-engine/src/js_bootstrap.rs`): `pm` → `chai` → `lodash` →
`cryptojs` → `exec` → `bru`, concatenated with
`// ==== shim: {name} ====` separators — so a script behaves the same whether
it runs in tropel-native or through this bundle.

## Install

```bash
npm install @tropel/shims
```

## Usage

```js
import { defaultBundle, render } from "@tropel/shims";

// Eval the full default bundle into a JS context, exactly as the engine does:
const bundleSource = render(); // pm + chai + lodash + cryptojs + exec + bru
eval(bundleSource);

// Or pick individual shims:
const pm = defaultBundle.find((s) => s.name === "pm-shim").source;
const k6 = (await import("@tropel/shims")).k6Bundle;
```

`defaultBundle` and `k6Bundle` are arrays of `{ name, source }` — embedders
can reorder, subset, or extend before `render()`.

## Build & test

```bash
npm run build   # refreshes shim/ from ../../js/, renders dist/, dry-runs npm pack
npm test        # smoke.mjs — asserts bundle parity with the repo js/ sources
```

The packaged bundle is built by Tropel's CI and gate-checked for
byte-identity against the monorepo's `js/` directory, the single source of
truth.

## License

Apache-2.0
