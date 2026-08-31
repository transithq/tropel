#!/usr/bin/env bash
# Build @tropel/core-wasm: compile tropel-core-wasm for wasm32-unknown-unknown
# (size-tuned release-wasm profile), run wasm-bindgen, optimize with a modern
# binaryen, copy the artifacts into pkg/, and dry-run the pack so packaging
# problems surface pre-publish.
#
# wasm-pack itself is NOT used: its pinned binaryen rejects rustc's
# reference-types output, and its manifest parser refuses inherited workspace
# table fields. The steps below reproduce its pipeline minus those bugs.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO_ROOT="$(cd ../.. && pwd)"

# ── 1. Compile the crate (cargo handles it from the repo root so the
#       workspace + release-wasm profile are in scope) ────────────────────
# Respect a machine-local target dir (C:/tropel-native-target) when present —
# mirrors `scripts/wasm-size.sh` and keeps the W4 task's "Use C:/tropel-native-target"
# instruction fast on this host. Pass --target-dir explicitly when that dir
# exists so the wasm is found regardless of CARGO_TARGET_DIR env.
if [ -d "C:/tropel-native-target" ]; then
  (cd "$REPO_ROOT" && cargo build -p tropel-core-wasm --target wasm32-unknown-unknown --profile release-wasm --target-dir C:/tropel-native-target)
else
  (cd "$REPO_ROOT" && cargo build -p tropel-core-wasm --target wasm32-unknown-unknown --profile release-wasm)
fi
# Locate the artifact under the default target dir or the machine-local override.
WASM=""
for c in \
  "$REPO_ROOT/target/wasm32-unknown-unknown/release-wasm/tropel_core_wasm.wasm" \
  "C:/tropel-native-target/wasm32-unknown-unknown/release-wasm/tropel_core_wasm.wasm"; do
  if [ -f "$c" ]; then WASM="$c"; break; fi
done
test -n "$WASM" || { echo "error: tropel_core_wasm.wasm not produced (looked in target/ and C:/tropel-native-target release-wasm)" >&2; exit 1; }

# ── 2. wasm-bindgen (web target; the CLI that wasm-pack would install) ────
WASM_BINDGEN_BIN="${WASM_BINDGEN:-wasm-bindgen}"
command -v "$WASM_BINDGEN_BIN" >/dev/null 2>&1 || {
  echo "error: wasm-bindgen not on PATH (cargo install wasm-bindgen-cli)" >&2
  exit 1
}
mkdir -p pkg
"$WASM_BINDGEN_BIN" --target web --out-dir pkg "$WASM"

# ── 3. binaryen -Oz (modern wasm-opt: --enable-* flags required for the
#       bulk-memory rustc emits) ──────────────────────────────────────────
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --strip-debug \
    --enable-bulk-memory --enable-sign-ext \
    --enable-nontrapping-float-to-int --enable-reference-types \
    -o pkg/tropel_core_wasm_bg.wasm pkg/tropel_core_wasm_bg.wasm
else
  echo "warning: wasm-opt not found — shipping unoptimized (npm exec -y -p binaryen -- wasm-opt …)" >&2
fi

# ── 4. Extract catalog metadata from the COMPILED wasm (single source of
#       truth: Rust PREDEFINED_VARIABLE_META) for init-free autocomplete.
#       Written as meta.js (plain ESM default export) so consumers need no
#       import-assertion support ───────────────────────────────────────────
node -e "
const fs = require('fs');
(async () => {
  const g = await import('./pkg/tropel_core_wasm.js');
  await g.default({ module_or_path: fs.readFileSync('./pkg/tropel_core_wasm_bg.wasm') });
  const meta = JSON.parse(g.predefinedVariablesMeta());
  fs.writeFileSync('./pkg/meta.js', 'export default ' + JSON.stringify(meta, null, 2) + ';\n');
})().catch((e) => { console.error(e); process.exit(1); });
"

# ── 5. Sanity: smoke test + size report ───────────────────────────────────
node smoke.mjs
SIZE=$(wc -c < pkg/tropel_core_wasm_bg.wasm)
echo "  wasm size: $((SIZE / 1024)) KiB ($SIZE B)"

# P5b-style budget gate (API_CLIENT_WEB_PAYLOAD.md §2.3 + TR-404): the eager
# core tier (variables + auth) is hard-gated at 700 KB post-`wasm-opt -Oz
# --strip-debug`. This gate IS the 700 KB CI assertion (TR-404) — `wasm-size.sh`
# covers the `tropel-web` slice; THIS script covers the eager `core-wasm` tier.
# The number is generated into `README.md`
# rather than hand-typed; `scripts/build.sh` rewrites the README's Size line.
# Re-measure with `twiggy top` after any payload change.
BUDGET=700000
if [ "$SIZE" -ge "$BUDGET" ]; then
  echo "FAIL: core wasm over budget ($((BUDGET / 1024)) KiB, $SIZE B >= $BUDGET)" >&2
  exit 1
fi
# Generate the size into README.md (TR-404: generated, not typed).
HEADROOM=$((BUDGET - SIZE))
if command -v node >/dev/null 2>&1; then
  node -e "
    const fs=require('fs');
    const path='README.md';
    let s=fs.readFileSync(path,'utf8');
    const size=Number(process.argv[1]);
    const headroom=Number(process.argv[2]);
    const kb=(size/1024).toFixed(1);
    const line='**'+size.toLocaleString('en-US')+' B** raw after \`wasm-opt -Oz --strip-debug\` (≈140 KB brotli, **'+headroom.toLocaleString('en-US')+' B headroom** under the **700 KB** gate). ✅MEAS';
    // Replace the generated block between the HTML comment and the next heading or EOF
    // The README has a comment line then the size line; replace the first occurrence of '**... B** raw after'
    // TOLERANCE — do not rewrite for sub-1% drift.
    //
    // The wasm byte count is NOT reproducible across host platforms: the same
    // source built on macOS/aarch64 and on CI's Linux/x86_64 differs by ~1 KB
    // (0.18% observed). Because CI's gate is
    //   bash build.sh && git diff --exit-code -- README.md
    // an unconditional rewrite made the gate unsatisfiable off-CI: a developer
    // ran the script, it wrote their host's number, CI rebuilt, got its own
    // number, and failed as \"stale\". It also dirtied the tree on every local
    // release run, tripping release.sh's dirty-tree preflight.
    //
    // The invariant TR-404 actually protects is (a) the payload stays under
    // 700 KB — asserted unconditionally above, and platform-independent at this
    // margin — and (b) the number is generated, not hand-typed. Both survive a
    // tolerance. A real payload change is orders of magnitude bigger than 1%.
    const prevMatch=s.match(/\*\*([\d,]+) B\*\* raw after \`wasm-opt/);
    const prev=prevMatch?Number(prevMatch[1].replace(/,/g,'')):0;
    const drift=prev?Math.abs(size-prev)/prev:1;
    if(prev && drift<0.01) {
      console.log('  README.md Size line kept at '+prev+' B (this host measured '
        +size+' B, '+(drift*100).toFixed(2)+'% drift — under the 1% rewrite threshold)');
    } else if(s.includes('raw after \`wasm-opt')) {
      s=s.replace(/\*\*[\d,]+ B\*\* raw after \`wasm-opt[^\\n]*✅MEAS[^\\n]*/, line);
      // Also replace legacy '457 KB' if still present
      s=s.replace(/457 KB raw after.*?glue\./s, line+' — see CI (\`scripts\/wasm-size.sh\` + \`packages\/core-wasm\/scripts\/build.sh\` budget check). Measured with \`twiggy top\`: the dominant costs are the \`regex\` engine code + \`unicode-perl\` tables (the full default Unicode property tables were cut — the catalog patterns are ASCII-only), the wasm-bindgen custom section, and chrono\/uuid\/rand glue. Re-measure with \`twiggy top\` after any payload change and re-tighten the gate.');
      fs.writeFileSync(path,s);
      console.log('  README.md Size line regenerated: '+size+' B ('+headroom+' B headroom)');
    }
  " "$SIZE" "$HEADROOM" || echo "warning: README.md size regeneration failed" >&2
fi

# ── 5. Dry-run the publish artifact ───────────────────────────────────────
npm pack --dry-run
