//! Per-VU QuickJS context bootstrap.
//!
//! Moved out of the former `engine.rs` god-file.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// P2 line 286: per-VU QuickJS heap cap (bytes). Configurable via
/// `TROPEL_JS_HEAP_MB` env var (default 10 MB).
pub(crate) fn js_heap_bytes() -> usize {
    std::env::var("TROPEL_JS_HEAP_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(10 * 1024 * 1024)
}

/// P2 line 286: per-eval JS execution deadline (seconds). Configurable via
/// `TROPEL_JS_DEADLINE_SECS` env var (default 10 s).
pub(crate) fn js_deadline_secs() -> Duration {
    std::env::var("TROPEL_JS_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(10))
}
use tropel_sandbox::config::SandboxConfig;
use tropel_sandbox::state::SharedPmState;
use tropel_sdk::error::TropelError;
use tropel_sdk::traits::DriverHttpClient;
use tropel_sdk::Result;

/// Version of the shim bundle, INDEPENDENT of the engine version (P4b).
///
/// The shims (`js/`) are JS-only and can ship as assets without a Tropel
/// release — so a handshake that compares engine version alone can't tell
/// whether two runs used the same `pm.*`/`trp.*` semantics. Bump this on any
/// behavioural change to the bundle. Surfaced in `tropel version`; the
/// engine↔shim comparison itself is the P6 version-handshake work.
pub(crate) const SHIM_BUNDLE_VERSION: &str = "0.1.0";

/// One shim library in the embedded set.
///
/// W2 line 182 used to be a live bug class here: a `const JS_SHIM_BUNDLE`
/// concat and `ShimBundle::default()` were TWO hand-maintained lists of the
/// same shims, and they drifted — the concat carried 5 while the default
/// carried 6, so bru.js was compiled into the binary but NEVER evaluated
/// (`typeof bru === 'undefined'` in every engine VU). This enum is now the
/// single list; every bundle, including the one that gets compiled to
/// bytecode, is derived from it, so the two can no longer disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shim {
    /// `globalThis.__tropelDeepEqual` — the one canonical deep-equality
    /// implementation. A HARD dependency of [`Shim::Pm`], [`Shim::Chai`] and
    /// [`Shim::Lodash`], all three of which call it by name, so it must be
    /// present in, and first in, every bundle that carries any of them.
    DeepEqual,
    /// `check`, `group`, `Counter`, `Gauge`, `Rate`, `Trend` — the k6
    /// builtins a script can call WITHOUT importing anything.
    ///
    /// These used to be installed by pm.js, which forced every non-Postman
    /// format to load the whole 70 KB Postman shim just to get `check()`.
    /// Extracted so a format bundle can drop pm.js without breaking them, so
    /// this variant belongs in EVERY bundle (TR-501).
    K6Core,
    /// `pm`, `postman`, the configured canonical namespace (`trp` by
    /// default), and the k6-style `check` / `group` / `Counter` / `Gauge` /
    /// default) — the Postman surface only. The k6-style `check` / `group` /
    /// `Counter` / `Gauge` / `Rate` / `Trend` globals used to live here too,
    /// which is what coupled every format to pm.js; they are [`Shim::K6Core`]
    /// now.
    Pm,
    /// `chai` / `expect`. Soft-referenced by pm.js at CALL time
    /// (`typeof chai !== 'undefined' && chai.expect`, pm.js:832), which falls
    /// back to pm.js's own `AssertChain`, so dropping chai degrades assertion
    /// fidelity but never throws.
    Chai,
    /// `_` (lodash subset).
    Lodash,
    /// `CryptoJS` — a dispatcher over the `__tropel_native_*` bridges.
    CryptoJs,
    /// `exec` (k6's `exec` module surface) and the bare `test` global.
    Exec,
    /// `bru`, `req`, `res` — the Bruno scripting API.
    Bru,
}

impl Shim {
    /// Canonical evaluation order, and the full embedded set.
    ///
    /// [`Shim::DeepEqual`] MUST stay first: pm, chai and lodash all call
    /// `globalThis.__tropelDeepEqual`. The remaining order is preserved from
    /// the pre-TR-501 bundle so no behaviour moves with this refactor.
    pub const ALL: [Shim; 8] = [
        Shim::DeepEqual,
        Shim::K6Core,
        Shim::Pm,
        Shim::Chai,
        Shim::Lodash,
        Shim::CryptoJs,
        Shim::Exec,
        Shim::Bru,
    ];

    /// The section-header name this shim is rendered under.
    pub fn name(self) -> &'static str {
        match self {
            Shim::DeepEqual => "deep-equal-shim",
            Shim::K6Core => "k6-core-shim",
            Shim::Pm => "pm-shim",
            Shim::Chai => "chai-shim",
            Shim::Lodash => "lodash-shim",
            Shim::CryptoJs => "cryptojs-shim",
            Shim::Exec => "exec-shim",
            Shim::Bru => "bru-shim",
        }
    }

    /// The embedded source text.
    pub fn source(self) -> &'static str {
        match self {
            Shim::DeepEqual => include_str!("../../../js/shared/deep-equal.js"),
            Shim::K6Core => include_str!("../../../js/shared/k6-core.js"),
            Shim::Pm => include_str!("../../../js/scripting-api/pm.js"),
            Shim::Chai => include_str!("../../../js/chai/chai-shim.js"),
            Shim::Lodash => include_str!("../../../js/lodash/lodash-shim.js"),
            Shim::CryptoJs => include_str!("../../../js/cryptojs-shim/cryptojs.js"),
            Shim::Exec => include_str!("../../../js/exec/exec.js"),
            Shim::Bru => include_str!("../../../js/scripting-api/bru.js"),
        }
    }
}

/// One shim library: a name + its source text.
pub struct ShimEntry(pub &'static str, pub Cow<'static, str>);

/// The shim bundle for a JS context (P4b: injectable, defaults to the
/// embedded set).
///
/// - **Native / CLI keeps the embedded default** — reproducibility matters; a
///   load test's semantics must not change because someone dropped a
///   different `pm.js` beside the binary.
/// - **The web client supplies its own** — a `pm.*` fix ships as a JS asset
///   with the web app: no wasm rebuild, no Tropel release.
pub struct ShimBundle(pub Vec<ShimEntry>);

impl ShimBundle {
    /// Build a bundle from an explicit shim list, in the order given.
    pub fn from_shims(shims: &[Shim]) -> Self {
        Self(
            shims
                .iter()
                .map(|s| ShimEntry(s.name(), Cow::Borrowed(s.source())))
                .collect(),
        )
    }

    /// Render the bundle to source text, concatenated with section headers.
    ///
    /// This is the ONLY thing that turns a bundle into JS — the bytecode path
    /// compiles this string too, so there is no second, hand-maintained
    /// concatenation that can drift out of step with the entry list (W2 line
    /// 182: that drift is what left bru.js compiled but never evaluated).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for ShimEntry(name, src) in &self.0 {
            out.push_str(&format!("// ==== shim: {name} ====\n"));
            out.push_str(src);
            out.push('\n');
        }
        out
    }

    /// Stable process-lifetime identity of this bundle, used to key the
    /// compiled-bytecode cache ([`shim_bytecode_for`]).
    ///
    /// Before TR-501 the cache was a single `OnceLock<Option<Vec<u8>>>` keyed
    /// on nothing, so `bootstrap_shims` could only take the bytecode path when
    /// `shim.is_default()` — reusing that static for any other bundle would
    /// have served the wrong bytecode. The effect was a PESSIMISATION: every
    /// gated bundle fell through to per-VU source eval, which cost more heap
    /// than the shims the gating dropped (✅MEAS on master, Apple M2:
    /// default 497,584 B/VU vs http-only-gated 557,824 B/VU).
    ///
    /// Keying: for a `Cow::Borrowed` the source is `&'static str`, so
    /// `(address, length)` is a SOUND identity — a `'static` string is never
    /// freed, so its address cannot be recycled, and two `&'static str` with
    /// the same address and length are the same bytes. (`is_default()` used
    /// exactly this `std::ptr::eq` argument before it was deleted.) For a
    /// `Cow::Owned` the address CAN be recycled after a free, so the content
    /// is hashed instead. The variant is folded in so the two can never
    /// collide.
    ///
    /// This is O(number of shims), not O(bundle bytes) — it runs once per VU
    /// spawn, so hashing ~220 KB of source per VU would have been its own
    /// regression.
    pub(crate) fn key(&self) -> BundleKey {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.0.len().hash(&mut h);
        for ShimEntry(name, src) in &self.0 {
            name.hash(&mut h);
            match src {
                Cow::Borrowed(s) => {
                    0u8.hash(&mut h);
                    (s.as_ptr() as usize).hash(&mut h);
                    s.len().hash(&mut h);
                }
                Cow::Owned(s) => {
                    1u8.hash(&mut h);
                    s.hash(&mut h);
                }
            }
        }
        BundleKey(h.finish())
    }
}

impl Default for ShimBundle {
    fn default() -> Self {
        Self::from_shims(&Shim::ALL)
    }
}

/// TR-501: the shims an input FORMAT can reach at all.
///
/// The declarative engine path (`run_scenario_vus`) runs exactly one kind of
/// JS: the `prerequest` / `test` scripts carried by `ScenarioItem`s, via
/// `ScenarioRunner::run_script`. Nothing else in that path evaluates JS, so a
/// shim no script of that format can name is pure per-VU heap.
///
/// `None` means "this table does not know that format" and the caller MUST
/// fall back to the full [`ShimBundle::default`]. Formats are opt-in: an
/// unknown or newly registered adapter id can never silently lose a shim —
/// a missing shim is a `ReferenceError` in a customer's script, which is far
/// worse than the memory an unnecessary one costs.
///
/// What is deliberately NOT excluded, and why:
///
/// - **`Shim::Pm` is in every row.** The obvious next win (70,197 B of
///   source, the largest single shim) is dropping it from the four
///   script-free formats, and it was measured — see `TR-501` in
///   `tropel_plan/tasks/W5-structural-ceilings.md`. It is NOT taken here:
///   pm.js also installs the configured canonical namespace from
///   `__tropel_sandbox_config` (the `SandboxConfig` preamble in
///   [`create_vu_js_context`] is written on the assumption that pm.js
///   consumes it), and it is the only definer of `check` / `group` /
///   `Counter` / `Gauge` / `Rate` / `Trend` (pm.js:1625). Proving no script
///   exists is an adapter-local argument; proving nothing else wants the
///   namespace is not.
/// - **`k6` keeps chai, lodash and cryptojs.** k6's own `check` and the
///   metric constructors come from pm.js, not from `js/k6-shim/`, so the
///   "a k6 run does not need pm.js" intuition is backwards for this bundle.
/// - **`bru` keeps pm.** Bruno's own adapter test fixture carries `pm.*`
///   scripts (`tropel-input-bru/src/lib.rs:619`) — Bruno collections
///   migrated from Postman really do use them.
fn format_shims(format: &str) -> Option<&'static [Shim]> {
    use Shim::*;
    Some(match format {
        // Postman scripts are arbitrary JS: `pm.expect(...)` (chai-style),
        // `_.map`, `CryptoJS.MD5` are all documented Postman sandbox
        // globals. Only Bruno's `bru`/`req`/`res` is unreachable — a
        // Postman collection has no syntax that produces it.
        "postman" => &[DeepEqual, K6Core, Pm, Chai, Lodash, CryptoJs, Exec],
        // Bruno scripts reach the same library surface plus `bru`.
        "bru" => &[DeepEqual, K6Core, Pm, Chai, Lodash, CryptoJs, Exec, Bru],
        // The k6 InputAdapter fallback (used when the k6 Driver is not
        // registered) wraps the transpiled script as one item's `test`.
        // Arbitrary JS again — minus Bruno's API.
        // `k6` is deliberately NOT narrowed.
        //
        // A k6 script is arbitrary user JS. Unlike the collection formats,
        // there is no structural guarantee about what it references — it can
        // reach `pm.*`, `trp.*`, chai, lodash, CryptoJS, or anything the
        // Driver installs. Narrowing it caused
        // `cookie_jar_set_reaches_the_wire_and_reads_back_server_cookies` to
        // fire ZERO of its four requests: the script failed at load and the
        // run silently did nothing, which is exactly the failure mode this
        // whole table has to avoid.
        //
        // Real k6 runs take the k6 Driver, which has its own bundle and does
        // not consult this table at all; this row only covers the adapter
        // fallback. Narrowing it buys little and risks a silent no-op, so it
        // returns None and gets the full default bundle.
        "k6" => return None,
        // These four adapters construct every `ScenarioItem` with
        // `prerequest: vec![]` and `test: vec![]`, at every construction
        // site — har/lib.rs:358, openapi/lib.rs:551, http/lib.rs:272,
        // insomnia/lib.rs:261+339 — so they cannot emit a script, so no
        // script can reference the user-facing assertion/utility libraries.
        // `assertion_libraries_are_unreachable_for_script_free_formats`
        // re-derives that from the parsed Scenario, so an adapter that
        // starts emitting scripts breaks the test instead of the customer.
        //
        // `Pm` stays here too, for the same reason as the `k6` row: pm.js
        // installs the canonical `trp` namespace, not only `pm`/`postman`.
        // The further -49% (280,480 -> 142,976 B/VU) that dropping it would
        // buy is real and measured, but it needs `trp` extracted first —
        // exactly the way `check`/`group` were extracted into K6Core.
        "har" | "openapi" | "http" | "insomnia" => &[DeepEqual, K6Core, Pm, Exec],
        _ => return None,
    })
}

/// P-B + TR-501: only materialise shims a run can actually use.
///
/// Two independent, individually-safe layers:
///
/// 1. **Format** ([`format_shims`]) — what the input format can name at all.
/// 2. **Content** — a conservative keyword scan of the input file for the two
///    optional libraries. The scan reads the WHOLE input file, not just the
///    script text, so it is deliberately over-inclusive: a Postman collection
///    whose URL happens to contain `crypto.` pulls cryptojs it does not need,
///    which costs memory. The reverse — scanning only the extracted scripts
///    and missing one — costs a `ReferenceError`.
impl ShimBundle {
    /// Build a bundle for a known input format, gated further by a keyword
    /// scan of the input. An unrecognised `format` yields the full default.
    pub fn for_format(format: &str, input: &[u8]) -> Self {
        let Some(allowed) = format_shims(format) else {
            tracing::debug!(
                "TR-501: no shim table for input format '{format}' — using the full default bundle"
            );
            return Self::default();
        };
        // Convert to str for scanning; lossy is fine — we're looking for
        // ASCII keywords, not parsing UTF-8.
        let src = String::from_utf8_lossy(input);
        let needs_crypto =
            src.contains("CryptoJS") || src.contains("crypto.") || src.contains("crypto ");
        let needs_lodash = src.contains("_.") || src.contains("lodash");

        let kept: Vec<Shim> = allowed
            .iter()
            .copied()
            .filter(|s| match s {
                Shim::Lodash => needs_lodash,
                Shim::CryptoJs => needs_crypto,
                _ => true,
            })
            .collect();
        Self::from_shims(&kept)
    }

    /// [`Self::for_format`] against a file on disk. Reads the file once per
    /// scenario (the bundle is then shared by every VU via `Arc`); an
    /// unreadable file falls back to the full default bundle, since a bundle
    /// cannot be narrowed on evidence that could not be read.
    pub fn for_format_path(format: &str, path: &std::path::Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self::for_format(format, &bytes),
            Err(e) => {
                tracing::debug!(
                    "TR-501: could not read '{}' for shim gating ({e}) — using the full default bundle",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Content-only gating, with no format knowledge: the full shim set
    /// minus lodash/cryptojs when the input never names them. This is what
    /// `for_format` degrades to for an unknown format, and what the
    /// measurement harness uses as the "gated" comparison point.
    pub fn from_script(script: &[u8]) -> Self {
        let src = String::from_utf8_lossy(script);
        let needs_crypto =
            src.contains("CryptoJS") || src.contains("crypto.") || src.contains("crypto ");
        let needs_lodash = src.contains("_.") || src.contains("lodash");
        let kept: Vec<Shim> = Shim::ALL
            .iter()
            .copied()
            .filter(|s| match s {
                Shim::Lodash => needs_lodash,
                Shim::CryptoJs => needs_crypto,
                _ => true,
            })
            .collect();
        Self::from_shims(&kept)
    }
}

/// Identity of a shim bundle — see [`ShimBundle::key`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BundleKey(u64);

/// One cache slot: the compiled bytecode for ONE bundle, plus the two sticky
/// failure flags that were previously process-global `AtomicBool`s. Making
/// them per-bundle matters: a run failure on bundle A used to disable the
/// bytecode path for every other bundle in the process.
struct ShimBytecodeSlot {
    key: BundleKey,
    /// `None` once compilation failed for THIS bundle — sticky, so a VU does
    /// not retry a compile that is deterministically broken.
    bytecode: Option<Arc<Vec<u8>>>,
    /// Set once this bundle's bytecode failed to RUN in some context. A run
    /// failure is deterministic (same blob, same bundle, every VU), so after
    /// the first one every VU short-circuits to the source-eval fallback.
    run_failed: bool,
}

/// How many DISTINCT shim bundles keep a compiled-bytecode slot.
///
/// Sizing: one bundle is built per scenario (`run_scenario_vus`), and a run
/// resolves a single input format, so the live set is one per scenario — 1 in
/// the common case, a handful for a multi-scenario config. The format table
/// can produce at most 7 formats x 4 content-gate shapes = 28 distinct
/// bundles, but only a process that ran every format and every gate
/// combination (i.e. the test suite) reaches that. 16 covers every realistic
/// run with room to spare and bounds the cache at 16 x ~200 KB ~= 3 MB
/// PROCESS-wide — not per VU, which is the number this task exists to reduce.
///
/// Past the cap, further bundles fall back to per-VU source eval and warn
/// once. That is correct, just slower — and it is exactly what the pre-TR-501
/// code did for EVERY non-default bundle.
const SHIM_BYTECODE_CACHE_CAP: usize = 16;

/// Process-wide cache of compiled shim-bundle bytecode, keyed by bundle.
///
/// Each distinct bundle is compiled ONCE (qjsc-style: `JS_Eval` with
/// COMPILE_ONLY, then `JS_WriteObject`), and every VU using that bundle loads
/// the blob and runs it instead of re-parsing + re-compiling the source.
/// QuickJS bytecode is tied to the build (version + feature flags), not to a
/// particular context, so one compilation is valid for every VU context in
/// the process.
///
/// TR-501: this replaces a single `OnceLock<Option<Vec<u8>>>` keyed on
/// nothing. Its own comment noted that reusing it for a different bundle
/// "would silently serve the wrong bytecode", so `bootstrap_shims` took the
/// bytecode path ONLY when `shim.is_default()` — which made shim gating a
/// net loss, because every gated bundle then paid per-VU source eval.
static SHIM_BYTECODE_CACHE: Mutex<Vec<ShimBytecodeSlot>> = Mutex::new(Vec::new());

/// Warn once, not once per VU, when the cache cap is reached.
static SHIM_BYTECODE_CACHE_FULL_LOGGED: AtomicBool = AtomicBool::new(false);

/// Fetch this bundle's compiled bytecode, compiling it once if this is the
/// first VU to ask for it. `None` means "use the source-eval fallback".
///
/// The lock is held across `compile_global_bytecode` (which is synchronous —
/// no await point inside the critical section), so concurrently spawning VUs
/// that want the SAME bundle block until the first finishes compiling rather
/// than each compiling their own copy. That is the same serialisation
/// `OnceLock::get_or_init` provided, now per bundle instead of per process.
fn shim_bytecode_for(
    ctx: &mut tropel_js::JsContext,
    bundle: &ShimBundle,
    key: BundleKey,
) -> Option<Arc<Vec<u8>>> {
    // A panic in another VU's compile must not wedge every remaining VU into
    // the slow path forever; recover the guard and carry on.
    let mut cache = SHIM_BYTECODE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(slot) = cache.iter().find(|s| s.key == key) {
        if slot.run_failed {
            return None;
        }
        return slot.bytecode.clone();
    }

    if cache.len() >= SHIM_BYTECODE_CACHE_CAP {
        if !SHIM_BYTECODE_CACHE_FULL_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "Shim bytecode cache is full ({SHIM_BYTECODE_CACHE_CAP} distinct bundles); \
                 further bundles fall back to per-VU source eval"
            );
        }
        return None;
    }

    let rendered = bundle.render();
    let compiled = match ctx.compile_global_bytecode(&rendered) {
        Ok(bc) => {
            tracing::info!(
                "Compiled shim bundle [{}] to bytecode once ({} B from {} B of source) — reusing across VUs",
                bundle
                    .0
                    .iter()
                    .map(|e| e.0)
                    .collect::<Vec<_>>()
                    .join("+"),
                bc.len(),
                rendered.len()
            );
            Some(Arc::new(bc))
        }
        Err(e) => {
            tracing::warn!(
                "Shim bytecode compilation failed ({e}); falling back to per-VU source eval"
            );
            None
        }
    };
    cache.push(ShimBytecodeSlot {
        key,
        bytecode: compiled.clone(),
        run_failed: false,
    });
    compiled
}

/// Mark a bundle's bytecode as unrunnable, so subsequent VUs go straight to
/// source eval instead of re-attempting a deterministically failing blob.
fn mark_shim_bytecode_run_failed(key: BundleKey) {
    let mut cache = SHIM_BYTECODE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(slot) = cache.iter_mut().find(|s| s.key == key) {
        slot.run_failed = true;
    }
}

/// Snapshot of the live bytecode cache: one `(key, blob)` per distinct
/// bundle, `None` where compilation failed. Reads the real production cache
/// — the tests use it to assert that DISTINCT bundles get DISTINCT bytecode,
/// which is the property the old single `OnceLock` could not provide.
#[cfg(test)]
pub(crate) fn shim_bytecode_cache_snapshot() -> Vec<(BundleKey, Option<Arc<Vec<u8>>>)> {
    let cache = SHIM_BYTECODE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.iter().map(|s| (s.key, s.bytecode.clone())).collect()
}

/// Create a JS context for one VU, bootstrap the shim libraries `shim`
/// carries, install the native modules and PM bridge functions, and wire a
/// `sleep(seconds)` helper.
///
/// TR-501: `shim` is no longer always the full embedded set — the caller
/// builds it from the input format (`ShimBundle::for_format`), so which
/// libraries a VU gets depends on what that format's scripts can name. Do not
/// assume `pm`/`chai`/`_`/`CryptoJS`/`bru` are all present in a context built
/// here; check the bundle.
///
/// Returns `None` if context creation fails — context-creation failures log
/// a warning, but a shim bootstrap failure is logged at ERROR level (the VU
/// still runs, just without scripts).
pub(crate) async fn create_vu_js_context(
    vu_id: u32,
    pm_state: &SharedPmState,
    http_client: &Arc<dyn DriverHttpClient>,
    shim: &ShimBundle,
    config: &SandboxConfig,
    force_stop: Arc<AtomicBool>,
) -> Option<tropel_js::JsContext> {
    let mut ctx = match tropel_js::JsContext::new_with_force_stop(
        Some(js_heap_bytes()),
        Some(js_deadline_secs()),
        force_stop.clone(),
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(
                "VU {}: Failed to create JS context: {} (scripts will be skipped)",
                vu_id,
                e
            );
            return None;
        }
    };

    // P4b: a NON-default sandbox config (custom canonical name / aliases)
    // must be installed as `__tropel_sandbox_config` BEFORE the shim bundle
    // evals, so pm.js's install tail exposes the configured names. The
    // default config is skipped — pm.js's own fallback (`tropel` + `wire`)
    // is byte-identical, and skipping keeps the default path untouched.
    if config != &SandboxConfig::default() {
        if let Err(e) = ctx.eval(&config.render_js_preamble()).await {
            // Loud, like the shim-bootstrap failure: the embedder asked for a
            // specific canonical name and silently getting `tropel.*` would
            // make every `trp.*` script throw ReferenceError at runtime.
            tracing::warn!(
                "VU {}: Failed to set sandbox config preamble: {} — failing the VU context",
                vu_id,
                e
            );
            return None;
        }
    }

    if let Err(e) = bootstrap_shims(&mut ctx, vu_id, shim).await {
        // Backlog line 238: a shim-eval failure must be LOUD — warn-only left
        // every script throwing `ReferenceError: pm is not defined`. Fail the
        // VU's JS context (scripts are skipped) and log at error level so the
        // run can't silently degrade into broken scripts.
        tracing::error!(
            "VU {}: JS shim bootstrap FAILED: {} — scripts will be skipped",
            vu_id,
            e
        );
        return None;
    }

    if let Err(e) = tropel_native::install_all(&mut ctx).await {
        tracing::warn!("VU {}: Failed to install native modules: {}", vu_id, e);
    }

    let bridge = tropel_sandbox::bindings::trp::TrpBridge::with_http_client(
        pm_state.clone(),
        http_client.clone(),
    );
    if let Err(e) = bridge.install(&mut ctx) {
        tracing::warn!("VU {}: Failed to install PM bridge functions: {}", vu_id, e);
    }

    // The sleep burns WALL time; the per-eval JS interrupt deadline must not
    // count it against the JS execution budget, or a stock k6 pacing idiom
    // like `sleep(Math.random()*10)` is interrupted on resume (backlog line
    // 104). Re-arm the deadline after the blocking sleep, like the WS loop
    // does per step.
    let (deadline, max_exec) = ctx.interrupt_deadline_handle();
    let force_stop_sleep = force_stop.clone();
    ctx.with_ctx(|rq_ctx| {
        let globals = rq_ctx.globals();
        let deadline_sleep = deadline.clone();
        // MUST be a SYNC host fn. `JsContext` builds a plain `rquickjs::Runtime`
        // (tropel-js/src/context.rs), which has NO spawner — `Opaque::spawner()`
        // is `.expect("tried to use async function in non async runtime")`. An
        // `Async` host fn calls `ctx.spawn` on first invocation and panics
        // there; rquickjs's ffi layer catches the panic, stashes it in the
        // runtime's `Opaque`, and throws into JS, so the VU sees an opaque
        // "Async script rejected" — and the stashed payload then `resume_unwind`s
        // on whichever VU next raises an exception, which on a shared runtime is
        // a DIFFERENT VU. A previous revision registered this as `Async` and the
        // guard below only checked `typeof sleep`, so 1130 tests passed while
        // `sleep()` was dead on every declarative format.
        //
        // Absolute deadline, not `remaining -= slice`: OS overshoot compounds
        // in the subtractive form (TR-502). Mirrors the k6 driver's copy.
        let _ = globals.set(
            "__tropel_native_sleep",
            rquickjs::function::Func::from(move |ms: f64| {
                if ms > 0.0 {
                    let total = Duration::from_secs_f64(ms / 1000.0);
                    let deadline_inner = std::time::Instant::now() + total;
                    let step = Duration::from_millis(10);
                    loop {
                        if force_stop_sleep.load(Ordering::Acquire) {
                            deadline_sleep.store(0, Ordering::Relaxed);
                            return;
                        }
                        let now = std::time::Instant::now();
                        if now >= deadline_inner {
                            break;
                        }
                        let remaining = deadline_inner - now;
                        std::thread::sleep(remaining.min(step));
                    }
                }
                tropel_js::rearm_deadline(&deadline_sleep, max_exec);
            }),
        );
    });

    // EXPLICIT globalThis assignment, not a declaration. The previous form —
    // `if (typeof sleep === 'undefined') { async function sleep(…) {…} }` —
    // was a no-op: a function declaration inside a block is BLOCK-SCOPED in
    // ES2015+, and Annex B's sloppy-mode hoisting does not apply to async
    // functions, so QuickJS (correctly) never put it on the global object.
    // The wrapper evaluated, went out of scope, and `sleep` stayed undefined
    // on the whole declarative path — a stock k6 pacing idiom
    // (`http.get(u); sleep(1);` in a collection script) threw ReferenceError.
    // The k6 Driver path was unaffected only because its own bundle carries
    // js/k6-shim/sleep-shim.js.
    let sleep_code = [
        "if (typeof globalThis.sleep === 'undefined') {",
        "  globalThis.sleep = async function sleep(seconds) {",
        "    if (typeof __tropel_native_sleep === 'function') {",
        "      await __tropel_native_sleep(seconds * 1000);",
        "    }",
        "  };",
        "}",
    ]
    .join("\n");
    let _ = ctx.eval(&sleep_code).await;

    Some(ctx)
}

/// Bootstrap the shim libraries in `shim` into `ctx`.
///
/// Preferred path for EVERY bundle, not just the default one: fetch this
/// bundle's bytecode from the keyed process-wide cache (compiled once by the
/// first VU that asked for this bundle) and run it in this context — no
/// per-VU parse/compile. Fallback: evaluate the rendered source directly.
///
/// TR-501: `bootstrap_shims` previously took the bytecode path only when
/// `shim.is_default()`, because the cache was a single unkeyed `OnceLock`.
/// That made shim gating a net loss — a gated bundle carries less source but
/// paid a full per-VU parse+compile for it, and measured 557,824 B/VU against
/// the default bundle's 497,584 B/VU (✅MEAS, release, Apple M2). With the
/// cache keyed by [`ShimBundle::key`], every distinct bundle compiles once
/// and gating is finally a saving.
///
/// Returns `Err` ONLY when the shim bundle could not be evaluated by ANY
/// path (bytecode compile failed + source eval failed, or bytecode run
/// failed + source eval failed) — a true `pm is not defined` condition that
/// the caller must surface loudly.
async fn bootstrap_shims(
    ctx: &mut tropel_js::JsContext,
    vu_id: u32,
    shim: &ShimBundle,
) -> Result<()> {
    let key = shim.key();

    if let Some(bytecode) = shim_bytecode_for(ctx, shim, key) {
        match ctx.run_global_bytecode(&bytecode).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                mark_shim_bytecode_run_failed(key);
                tracing::warn!(
                    "VU {vu_id}: Failed to run JS shim bytecode: {e} \
                     (disabling the bytecode path for this bundle; falling back to source eval)"
                );
                let rendered = shim.render();
                return ctx.bootstrap_library(&rendered).await.map_err(|e2| {
                    TropelError::Js(format!(
                        "VU {vu_id}: shim source eval failed after bytecode run error: {e2}"
                    ))
                });
            }
        }
    }

    let rendered = shim.render();
    ctx.bootstrap_library(&rendered)
        .await
        .map_err(|e| TropelError::Js(format!("VU {vu_id}: shim source eval failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tropel_core::config::HttpConfig;
    use tropel_http::client::{HttpClient, VuCookieClient};
    use tropel_sandbox::state::new_pm_state;
    use tropel_sdk::traits::DriverHttpClient;

    /// Replaces `shim_lists_stay_in_lockstep_with_bru`, which pinned the
    /// design this commit removes.
    ///
    /// That test guarded a real defect (W2 line 182: a `JS_SHIM_BUNDLE`
    /// concat carried 5 shims while `ShimBundle::default()` carried 6, so
    /// bru.js was compiled into the binary but NEVER evaluated) by asserting
    /// the two hand-maintained lists agreed. There is now ONE list —
    /// [`Shim::ALL`] — and the bytecode path compiles `render()` like every
    /// other path, so "the two lists drifted" is no longer expressible. What
    /// still needs guarding is that `render()` really emits every entry, in
    /// order: a `render()` that silently dropped an entry would reintroduce
    /// exactly the `typeof bru === 'undefined'` symptom through a different
    /// door.
    #[test]
    fn render_emits_every_shim_in_the_default_bundle() {
        let d = ShimBundle::default();
        assert_eq!(
            d.0.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![
                "deep-equal-shim",
                "k6-core-shim",
                "pm-shim",
                "chai-shim",
                "lodash-shim",
                "cryptojs-shim",
                "exec-shim",
                "bru-shim"
            ],
            "the default bundle is Shim::ALL, in canonical order"
        );

        let rendered = d.render();
        let mut cursor = 0usize;
        for ShimEntry(name, src) in &d.0 {
            let header = format!("// ==== shim: {name} ====\n");
            let at = rendered[cursor..].find(&header).map(|i| i + cursor);
            let at = at.unwrap_or_else(|| panic!("render() dropped the {name} section header"));
            cursor = at + header.len();
            assert!(
                rendered[cursor..].starts_with(src.as_ref()),
                "render() dropped or reordered the {name} source"
            );
            cursor += src.len();
        }

        // The specific byte that went missing last time.
        let bru_src = Shim::Bru.source();
        assert!(
            rendered.contains(bru_src),
            "render() must emit bru.js — the W2 line-182 symptom was `typeof bru === 'undefined'`"
        );
    }

    /// TR-501: a Postman collection has no syntax that can produce Bruno's
    /// `bru` / `req` / `res`, so a Postman run must not materialise bru.js.
    /// It CAN produce chai-style `pm.expect`, `_` and `CryptoJS`, so those
    /// must survive when the collection names them.
    ///
    /// Fails on pre-fix code: `ShimBundle::from_script` (the only selector
    /// that existed) appended `bru-shim` unconditionally, for every input.
    #[test]
    fn postman_bundle_excludes_bru_and_keeps_the_assertion_libraries() {
        let collection = br#"{"info":{"schema":"getpostman.com/collection"},
            "item":[{"event":[{"listen":"test","script":{"exec":[
              "pm.expect(_.map([1],String)).to.eql(['1']);",
              "pm.environment.set('h', CryptoJS.MD5('x').toString());"
            ]}}]}]}"#;
        let names = shim_names(&ShimBundle::for_format("postman", collection));

        assert!(
            !names.contains(&"bru-shim"),
            "a Postman run must not materialise bru.js — got {names:?}"
        );
        for required in [
            "deep-equal-shim",
            "k6-core-shim",
            "pm-shim",
            "chai-shim",
            "exec-shim",
        ] {
            assert!(
                names.contains(&required),
                "a Postman script can reach {required} — got {names:?}"
            );
        }
        assert!(
            names.contains(&"lodash-shim") && names.contains(&"cryptojs-shim"),
            "this collection names both `_.` and `CryptoJS` — got {names:?}"
        );
    }

    /// TR-501: the same collection WITHOUT the two optional libraries drops
    /// them. This is the layer that existed before (content gating); the
    /// assertion here is that the format layer did not disable it.
    #[test]
    fn postman_bundle_drops_unreferenced_optional_libraries() {
        let collection = br#"{"info":{"schema":"getpostman.com/collection"},
            "item":[{"event":[{"listen":"test","script":{"exec":[
              "pm.test('ok', () => pm.response.to.have.status(200));"
            ]}}]}]}"#;
        let names = shim_names(&ShimBundle::for_format("postman", collection));
        assert!(
            !names.contains(&"lodash-shim") && !names.contains(&"cryptojs-shim"),
            "nothing in this collection names `_` or `CryptoJS` — got {names:?}"
        );
        assert!(
            names.contains(&"pm-shim"),
            "pm.js is not optional for Postman — got {names:?}"
        );
    }

    // `k6_bundle_excludes_bru_but_keeps_pm` was removed: it asserted the k6 row NARROWS.
    // That row now returns the full default bundle on purpose — see
    // `format_shims`. A k6 script is arbitrary JS, and narrowing it made a
    // real test fire zero of its four requests.

    /// TR-501: har / openapi / http / insomnia adapters cannot emit a
    /// `prerequest` or `test` script (see
    /// `assertion_libraries_are_unreachable_for_script_free_formats`), so no
    /// script of those formats can name chai, lodash, CryptoJS or bru.
    ///
    /// Fails on pre-fix code: the only selector was a keyword scan of the
    /// file bytes, which always kept chai and bru and — for a HAR whose
    /// recorded URLs contain `crypto.` or `_.` — kept those too.
    #[test]
    fn script_free_formats_exclude_the_user_script_libraries() {
        // Deliberately seeded with the exact tokens the content scan looks
        // for: a recorded URL can contain anything. The FORMAT is what makes
        // them unreachable, and the format layer must win.
        let recorded = br#"{"log":{"entries":[{"request":{"url":"https://api.example.com/crypto.json?f=_.x&q=lodash"}}]}}"#;
        for format in ["har", "openapi", "http", "insomnia"] {
            let names = shim_names(&ShimBundle::for_format(format, recorded));
            for excluded in ["chai-shim", "lodash-shim", "cryptojs-shim", "bru-shim"] {
                assert!(
                    !names.contains(&excluded),
                    "'{format}' emits no scripts, so nothing can name {excluded} — got {names:?}"
                );
            }
            assert_eq!(
                names,
                vec!["deep-equal-shim", "k6-core-shim", "pm-shim", "exec-shim"],
                "'{format}' bundle"
            );
        }
    }

    /// TR-501: the table is opt-in. An adapter id it does not know — a
    /// third-party input extension, a `subprocess:<cmd>` id, anything added
    /// after this table was written — must get the FULL bundle. A missing
    /// shim is a `ReferenceError` in a customer's script; an unnecessary one
    /// is only memory.
    #[test]
    fn unknown_format_falls_back_to_the_full_bundle() {
        for unknown in ["", "graphql", "subprocess:./gen.sh", "POSTMAN", "postman2"] {
            let names = shim_names(&ShimBundle::for_format(unknown, b"{}"));
            assert_eq!(
                names,
                shim_names(&ShimBundle::default()),
                "unknown format '{unknown}' must get the full default bundle"
            );
        }
    }

    /// TR-501: the load-bearing premise of the `har` / `openapi` / `http` /
    /// `insomnia` row in [`format_shims`] is that those adapters cannot
    /// produce a script. This re-derives it from the REAL adapters and the
    /// REAL parsed `Scenario`, not from reading their source — so an adapter
    /// that starts emitting scripts fails this test instead of shipping a
    /// `ReferenceError` to a customer whose script now has nothing to run
    /// against.
    ///
    /// If this fails: add the libraries that format's scripts can reach back
    /// into `format_shims`, in the same commit.
    #[test]
    fn assertion_libraries_are_unreachable_for_script_free_formats() {
        use tropel_sdk::traits::InputAdapter;

        fn count_scripts(items: &[tropel_sdk::scenario::ScenarioItem]) -> usize {
            items
                .iter()
                .map(|i| i.prerequest.len() + i.test.len() + count_scripts(&i.items))
                .sum()
        }

        let cases: Vec<(&str, Box<dyn InputAdapter>, &[u8])> = vec![
            (
                "har",
                Box::new(tropel_input_har::HarInputAdapter),
                br#"{"log":{"version":"1.2","creator":{"name":"t","version":"1"},"entries":[
                    {"startedDateTime":"2020-01-01T00:00:00Z","time":1,
                     "request":{"method":"GET","url":"https://example.com/a","httpVersion":"HTTP/1.1","headers":[],"queryString":[],"cookies":[],"headersSize":-1,"bodySize":-1},
                     "response":{"status":200,"statusText":"OK","httpVersion":"HTTP/1.1","headers":[],"cookies":[],"content":{"size":0,"mimeType":"text/plain"},"redirectURL":"","headersSize":-1,"bodySize":0},
                     "cache":{},"timings":{"send":0,"wait":1,"receive":0}}]}}"#,
            ),
            (
                "openapi",
                Box::new(tropel_input_openapi::OpenApiInputAdapter),
                br#"{"openapi":"3.0.0","info":{"title":"t","version":"1"},
                    "servers":[{"url":"https://example.com"}],
                    "paths":{"/a":{"get":{"responses":{"200":{"description":"ok"}}}}}}"#,
            ),
            (
                "http",
                Box::new(tropel_input_http::HttpFileAdapter),
                b"GET https://example.com/a\nAccept: application/json\n",
            ),
            (
                "insomnia",
                Box::new(tropel_input_insomnia::InsomniaInputAdapter),
                br#"{"_type":"export","__export_format":4,"resources":[
                    {"_id":"req_1","_type":"request","parentId":"wrk_1","name":"a","method":"GET","url":"https://example.com/a"},
                    {"_id":"wrk_1","_type":"workspace","name":"w"}]}"#,
            ),
        ];

        for (format, adapter, bytes) in cases {
            assert_eq!(
                adapter.id(),
                format,
                "the format_shims key must be the adapter's own id"
            );
            let scenario = adapter
                .parse(bytes)
                .unwrap_or_else(|e| panic!("{format} fixture must parse: {e}"));
            assert!(
                !scenario.items.is_empty(),
                "{format} fixture must produce at least one item, or it proves nothing"
            );
            assert_eq!(
                count_scripts(&scenario.items),
                0,
                "the '{format}' row of format_shims drops chai/lodash/cryptojs/bru on the \
                 grounds that this adapter cannot emit a script. It just did. Put the \
                 libraries its scripts can reach back into format_shims."
            );
        }
    }

    /// Collect the shim section names of a bundle, in order.
    fn shim_names(bundle: &ShimBundle) -> Vec<&'static str> {
        bundle.0.iter().map(|e| e.0).collect()
    }

    /// F1: `HttpClient` itself does not implement `DriverHttpClient` — the
    /// engine wraps it in `DriverHttpClientImpl` (vu_loop.rs). Reuse it here
    /// so the test builds the same trait object the VU loop passes.
    use crate::vu_loop::DriverHttpClientImpl;

    /// P4b: the engine bootstrap must honor a NON-default SandboxConfig.
    /// The VU loop always passes the default (so the config branch would be
    /// provably inert without this test) — an embedder passing a custom
    /// namespace + aliases must get those names installed, and the default
    /// `trp` canonical must be absent (a namespace distinct from the default
    /// proves the config drives the name). This runs through the SAME path
    /// as production: preamble eval before bootstrap_shims, then the
    /// (default) ShimBundle — the bytecode cache path is exercised since
    /// this test runs after other VU contexts compiled it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_vu_js_context_honors_custom_sandbox_config() {
        let pm_state = new_pm_state();
        let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
            client: VuCookieClient::new(
                HttpClient::new(&HttpConfig::default()).expect("http client should construct"),
            ),
        });
        let config = SandboxConfig {
            namespace: "acme".into(),
            aliases: vec!["product".into(), "wire".into()],
        };
        let mut ctx = create_vu_js_context(
            7,
            &pm_state,
            &client,
            &ShimBundle::default(),
            &config,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("context must be created");

        let check = ctx
            .eval(
                "typeof acme === 'object' && typeof product === 'object' \
                 && product === acme && wire === acme && typeof pm === 'object' \
                 && typeof bru === 'object' && typeof trp === 'undefined' \
                 && typeof tropel === 'undefined'",
            )
            .await
            .expect("probe should eval");
        assert_eq!(
            check, "true",
            "custom namespace/aliases must be installed via the preamble; default trp absent; bru must be evaluated by the real bundle path — got: {check}"
        );
    }

    /// TR-503: isolation — one script's globals must not be reachable from
    /// another's. Each VU owns a separate QuickJS Runtime, so a global set
    /// in one must be undefined in the other. This is the 34 leaking globals
    /// guard: if a shim leaks, this fails.
    /// `sleep` must be a GLOBAL function on the declarative path.
    ///
    /// The wrapper `create_vu_js_context` appends used to be a block-scoped
    /// `async function` inside an `if` — which ES2015 block-scopes and Annex B
    /// does not rescue for async functions — so it never reached globalThis
    /// and every collection script calling `sleep(1)` threw ReferenceError.
    /// TROPEL_MASTER_TODO.md:405 flagged the adjacent comment as asserting
    /// the opposite of reality; this pins the reality.
    ///
    /// Fails on the pre-fix code: `typeof sleep` evaluated to "undefined".
    #[tokio::test]
    async fn sleep_is_a_global_function_on_the_declarative_path() {
        let pm_state = new_pm_state();
        let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
            client: VuCookieClient::new(
                HttpClient::new(&HttpConfig::default()).expect("http client"),
            ),
        });
        let mut ctx = create_vu_js_context(
            1,
            &pm_state,
            &client,
            &ShimBundle::default(),
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("VU context");
        let ty = ctx.eval("typeof sleep").await.expect("eval");
        assert_eq!(
            ty, "function",
            "sleep must be installed on globalThis for the declarative path — \
             a block-scoped declaration silently leaves it undefined"
        );

        // `typeof` alone is not evidence: it passed for the whole period in
        // which `sleep` was backed by an `Async` host fn on a runtime with no
        // spawner, so the first CALL panicked with "tried to use async function
        // in non async runtime". Call it, and assert it actually waits.
        let elapsed = ctx
            .eval_async(
                "(async () => { const t = Date.now(); await sleep(0.05); return Date.now() - t; })()",
            )
            .await
            .expect("sleep must be callable, not merely defined");
        let ms: f64 = elapsed.trim().parse().unwrap_or(-1.0);
        assert!(
            ms >= 40.0,
            "await sleep(0.05) must block ~50ms; got {elapsed:?} — the host fn \
             is registered but not actually sleeping"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_vu_globals_are_isolated() {
        let pm_state = new_pm_state();
        let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
            client: VuCookieClient::new(
                HttpClient::new(&HttpConfig::default()).expect("http client should construct"),
            ),
        });
        let mut ctx1 = create_vu_js_context(
            1,
            &pm_state,
            &client,
            &ShimBundle::default(),
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("ctx1");
        let mut ctx2 = create_vu_js_context(
            2,
            &pm_state,
            &client,
            &ShimBundle::default(),
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("ctx2");

        // Set a global in ctx1
        let _ = ctx1.eval("var leak_test = 42; leak_test").await;
        // Must be undefined in ctx2
        let check = ctx2
            .eval("typeof leak_test === 'undefined'")
            .await
            .expect("probe");
        assert_eq!(
            check, "true",
            "per-VU globals must be isolated — leak_test leaked to ctx2: {check}"
        );
        // Also check that built-in shims are present in both but not shared
        let c1 = ctx1.eval("typeof pm === 'object'").await.expect("c1");
        let c2 = ctx2.eval("typeof pm === 'object'").await.expect("c2");
        assert_eq!(c1, "true");
        assert_eq!(c2, "true");
    }

    /// TR-503: the per-VU heap number printed in `README.md` and
    /// `tropel_plan/CONVENTIONS.md` must track the code.
    ///
    /// This is the gate that the 57 KB "shared Runtime" claim needed and did
    /// not have. That figure sat in the README, the budget table, the W5
    /// verification footer and the W6 release gate for as long as it took to
    /// read `context.rs` — the `SHARED_RT` it cited shared nothing. Nothing
    /// compared the documented number against a running context, so nothing
    /// objected.
    ///
    /// A wide band on purpose: this catches an order-of-magnitude divergence
    /// (57 KB vs ~486 KB is 9x) and tolerates allocator and platform variance.
    /// It is a drift alarm, not a precision budget — `perf-regression` owns
    /// the budget.
    ///
    /// If this fails, re-run `measure_per_vu_quickjs_heap` and update BOTH
    /// documents. Do not widen the band to make it pass.
    #[tokio::test]
    async fn documented_per_vu_heap_matches_reality() {
        const DOCUMENTED_BYTES: u64 = 497_584;
        const TOLERANCE: f64 = 0.25;

        let pm_state = new_pm_state();
        let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
            client: VuCookieClient::new(
                HttpClient::new(&HttpConfig::default()).expect("http client should construct"),
            ),
        });
        let ctx = create_vu_js_context(
            1,
            &pm_state,
            &client,
            &ShimBundle::default(),
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("full VU context");

        let actual = ctx.quickjs_heap_bytes();
        let low = (DOCUMENTED_BYTES as f64 * (1.0 - TOLERANCE)) as u64;
        let high = (DOCUMENTED_BYTES as f64 * (1.0 + TOLERANCE)) as u64;
        assert!(
            (low..=high).contains(&actual),
            "per-VU QuickJS heap is {actual} B but README/CONVENTIONS document \
             {DOCUMENTED_BYTES} B (band {low}..={high}). Re-run \
             `cargo test -p tropel-engine --release measure_per_vu_quickjs_heap \
             -- --nocapture --ignored` and update both documents."
        );
    }

    /// TR-503 / TR-501: print the ACTUAL per-VU QuickJS heap so the README
    /// number is derived, not asserted. Run with:
    /// `cargo test -p tropel-engine --release measure_per_vu_quickjs_heap -- --nocapture --ignored`
    #[tokio::test]
    #[ignore = "measurement, not an assertion — run explicitly with --nocapture"]
    async fn measure_per_vu_quickjs_heap() {
        let pm_state = new_pm_state();
        let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
            client: VuCookieClient::new(
                HttpClient::new(&HttpConfig::default()).expect("http client should construct"),
            ),
        });
        let bare = tropel_js::JsContext::new(None, None)
            .await
            .expect("bare context");
        println!(
            "bare JsContext (no shims)      = {} B",
            bare.quickjs_heap_bytes()
        );

        let full = create_vu_js_context(
            1,
            &pm_state,
            &client,
            &ShimBundle::default(),
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("full VU context");
        println!(
            "full VU context (all shims)    = {} B",
            full.quickjs_heap_bytes()
        );

        let gated = create_vu_js_context(
            2,
            &pm_state,
            &client,
            &ShimBundle::from_script(
                b"import http from 'k6/http'; export default () => http.get('http://x');",
            ),
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("gated VU context");
        println!(
            "http-only gated VU context     = {} B",
            gated.quickjs_heap_bytes()
        );
    }

    /// TR-501: the exclusions must not break a real Postman script.
    ///
    /// Exercises the three surfaces the Postman row deliberately keeps —
    /// chai-style `pm.expect(...).to.eql(...)`, lodash `_.map`, and
    /// `CryptoJS.MD5` — through the production path
    /// (`create_vu_js_context` → `bootstrap_shims` → the keyed bytecode
    /// cache) with the bundle `ShimBundle::for_format("postman", …)`
    /// actually selects for this collection.
    ///
    /// This is the test that fails if someone "optimises" chai, lodash or
    /// cryptojs out of the Postman row: each assertion below becomes a
    /// `ReferenceError`, which is exactly the customer-visible symptom.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postman_script_runs_under_the_postman_bundle() {
        let collection = br#"{"info":{"schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item":[{"name":"a","event":[{"listen":"test","script":{"exec":[
              "pm.expect(_.map([1,2],String)).to.eql(['1','2']);",
              "pm.environment.set('h', CryptoJS.MD5('abc').toString());"
            ]}}]}]}"#;
        let bundle = ShimBundle::for_format("postman", collection);
        assert!(
            !shim_names(&bundle).contains(&"bru-shim"),
            "precondition: this run is on the NARROWED Postman bundle"
        );

        let mut ctx = new_vu_ctx(21, &bundle).await;

        // chai-style deep equality through pm.expect — throws on failure.
        let eql = ctx
            .eval("(() => { try { pm.expect(_.map([1,2],String)).to.eql(['1','2']); return 'ok'; } catch (e) { return 'threw: ' + e; } })()")
            .await
            .expect("probe should eval");
        assert_eq!(eql, "ok", "pm.expect(...).to.eql(...) over _.map failed");

        // CryptoJS.MD5 must produce the published vector for "abc".
        let md5 = ctx
            .eval("CryptoJS.MD5('abc').toString()")
            .await
            .expect("probe should eval");
        assert_eq!(
            md5, "900150983cd24fb0d6963f7d28e17f72",
            "CryptoJS.MD5('abc') must match the published vector"
        );

        // …and the negative half: bru really is absent, so the exclusion is
        // doing something rather than being silently ignored.
        let no_bru = ctx
            .eval("typeof bru === 'undefined'")
            .await
            .expect("probe should eval");
        assert_eq!(no_bru, "true", "bru.js must not be materialised");
    }

    /// TR-501: the exclusions must not break a real k6 script either.
    ///
    /// `check` is the idiom every k6 script uses, and it comes from pm.js
    /// (pm.js:1625) — which is why the k6 row keeps pm. `crypto` here is
    /// `CryptoJS`.
    ///
    /// NOT asserted, deliberately: `sleep` and `http`. Both are `undefined`
    /// in this path on master under the FULL default bundle as well — the
    /// `sleep` wrapper `create_vu_js_context` appended used to be a
    /// block-scoped `async function` that never reached the global object
    /// (fixed: it is now an explicit `globalThis.sleep = …` assignment), and
    /// `http.*` comes
    /// from the k6 DRIVER's own bundle in `tropel-input-k6`, which never goes
    /// through `ShimBundle`. Neither is something this change removed; see
    /// `narrowing_removes_only_the_excluded_globals`, which pins that
    /// difference directly instead of asserting a value nothing produces.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn k6_script_runs_under_the_k6_bundle() {
        // Repointed from "k6" to "postman": the k6 row now returns the full
        // bundle on purpose (a k6 script is arbitrary JS), so it is no longer
        // a narrowed bundle and cannot serve as this test's subject.
        // Mentions CryptoJS and _. so content gating keeps them — otherwise
        // this probes content narrowing, not format narrowing.
        let script = b"// CryptoJS _.map\nexport default function () {\n  check(1, {'one': v => v === 1});\n}";
        let bundle = ShimBundle::for_format("postman", script);
        assert!(
            !shim_names(&bundle).contains(&"bru-shim"),
            "precondition: this run is on a NARROWED bundle"
        );

        let mut ctx = new_vu_ctx(22, &bundle).await;

        let checked = ctx
            .eval("typeof check === 'function' && check(1, {'one': v => v === 1})")
            .await
            .expect("probe should eval");
        assert_eq!(
            checked, "true",
            "k6's `check` is installed by pm.js — dropping pm from the k6 row breaks it"
        );

        let metrics = ctx
            .eval("['Counter','Gauge','Rate','Trend','group'].every(n => typeof globalThis[n] === 'function')")
            .await
            .expect("probe should eval");
        assert_eq!(
            metrics, "true",
            "k6's metric constructors and `group` also come from pm.js"
        );

        let sha = ctx
            .eval("CryptoJS.SHA256('abc').toString()")
            .await
            .expect("probe should eval");
        assert_eq!(
            sha, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "CryptoJS.SHA256('abc') must match the published vector"
        );

        let no_bru = ctx
            .eval("typeof bru === 'undefined'")
            .await
            .expect("probe should eval");
        assert_eq!(no_bru, "true", "bru.js must not be materialised");
    }

    /// TR-501: narrowing a bundle must remove EXACTLY the globals of the
    /// shims it drops, and nothing else.
    ///
    /// The per-format tests above assert the bundle's contents; this asserts
    /// the consequence in the VU context, differentially against the full
    /// default bundle. That is the shape that catches collateral damage — a
    /// shim quietly depending on another one, an ordering assumption, a
    /// `var x = x || {}` that only worked because something earlier in the
    /// bundle had already run. Comparing against `ShimBundle::default()`
    /// rather than against a hardcoded list also means it keeps working when
    /// a new shim is added: both legs move together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn narrowing_removes_only_the_excluded_globals() {
        // Every global any shim in the bundle installs, plus the two the k6
        // surface is expected to want.
        const PROBED: &[&str] = &[
            "__tropelDeepEqual",
            "pm",
            "postman",
            "trp",
            "check",
            "group",
            "Counter",
            "Gauge",
            "Rate",
            "Trend",
            "chai",
            "expect",
            "_",
            "CryptoJS",
            "exec",
            "test",
            "bru",
            "req",
            "res",
            "sleep",
            "http",
        ];
        let probe = format!(
            "JSON.stringify({:?}.filter(n => typeof globalThis[n] !== 'undefined'))",
            PROBED
        );

        async fn defined_globals(vu: u32, bundle: &ShimBundle, probe: &str) -> Vec<String> {
            let mut ctx = new_vu_ctx(vu, bundle).await;
            let json = ctx.eval(probe).await.expect("probe should eval");
            serde_json::from_str(&json).expect("probe returns a JSON array")
        }

        let full = defined_globals(41, &ShimBundle::default(), &probe).await;
        assert!(
            full.contains(&"bru".to_string()) && full.contains(&"_".to_string()),
            "precondition: the default bundle really does install bru and lodash — got {full:?}"
        );

        // (format, input, globals the narrowing is ALLOWED to remove)
        let cases: &[(&str, &[u8], &[&str])] = &[
            (
                "postman",
                br#"{"info":{"schema":"getpostman.com/collection"},"exec":"pm.expect(_.map([1],String)); CryptoJS.MD5('x')"}"#,
                &["bru", "req", "res"],
            ),
            // "k6" intentionally absent: that row returns the full bundle,
            // so it removes nothing and this table asserts removal.
            (
                "postman",
                b"export default () => check(1, {}); // _.map CryptoJS",
                &["bru", "req", "res"],
            ),
            (
                "har",
                br#"{"log":{"entries":[]}}"#,
                &["bru", "req", "res", "chai", "expect", "_", "CryptoJS"],
            ),
        ];

        for (format, input, allowed_missing) in cases {
            let bundle = ShimBundle::for_format(format, input);
            let narrowed = defined_globals(42, &bundle, &probe).await;

            let missing: Vec<&String> = full.iter().filter(|g| !narrowed.contains(g)).collect();
            let unexpected: Vec<&&String> = missing
                .iter()
                .filter(|g| !allowed_missing.contains(&g.as_str()))
                .collect();
            assert!(
                unexpected.is_empty(),
                "'{format}' narrowing removed globals it was not allowed to: {unexpected:?} \
                 (bundle {:?}; full had {full:?}, narrowed has {narrowed:?})",
                shim_names(&bundle)
            );

            let extra: Vec<&String> = narrowed.iter().filter(|g| !full.contains(g)).collect();
            assert!(
                extra.is_empty(),
                "'{format}' narrowing INVENTED globals the default bundle does not have: {extra:?}"
            );

            // The exclusion must actually bite, or the test proves nothing.
            assert!(
                !missing.is_empty(),
                "'{format}' bundle {:?} removed nothing at all — the format table is inert",
                shim_names(&bundle)
            );
        }
    }

    /// TR-501, the core of the fix: the bytecode cache must hold a SEPARATE,
    /// DIFFERENT blob per distinct bundle.
    ///
    /// **Fails on pre-fix code.** The cache was one
    /// `static SHIM_BYTECODE: OnceLock<Option<Vec<u8>>>` keyed on nothing,
    /// and its own comment said reusing it for a second bundle "would
    /// silently serve the wrong bytecode" — so `bootstrap_shims` guarded the
    /// whole path behind `if shim.is_default()`. On that code a narrowed
    /// bundle never reaches the cache at all: only ONE entry appears, and
    /// every gated VU pays a full source parse+compile. That is why gating
    /// measured 557,824 B/VU against the default bundle's 497,584 B/VU.
    ///
    /// Asserted here: two distinct bundles produce two cache entries with
    /// two distinct non-empty blobs, AND each context ends up with exactly
    /// the globals of the bundle it asked for (so the right blob reached the
    /// right context — a cache that keyed correctly but served crosswise
    /// would pass the count assertion alone).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bytecode_cache_serves_distinct_bytecode_per_bundle() {
        let full = ShimBundle::default();
        // Narrower on purpose: no chai, no lodash, no cryptojs, no bru.
        let narrow = ShimBundle::for_format("har", b"{}");
        assert_ne!(
            full.key(),
            narrow.key(),
            "precondition: the two bundles must have distinct identities"
        );

        let before: Vec<BundleKey> = shim_bytecode_cache_snapshot()
            .into_iter()
            .map(|(k, _)| k)
            .collect();

        let mut ctx_full = new_vu_ctx(31, &full).await;
        let mut ctx_narrow = new_vu_ctx(32, &narrow).await;

        let after = shim_bytecode_cache_snapshot();
        let full_slot = after
            .iter()
            .find(|(k, _)| *k == full.key())
            .expect("the default bundle must be in the bytecode cache");
        let narrow_slot = after.iter().find(|(k, _)| *k == narrow.key()).expect(
            "the NARROWED bundle must be in the bytecode cache — on the pre-fix single \
             OnceLock it never got there, which is what made gating cost more than it saved",
        );

        let full_bc = full_slot.1.as_ref().expect("default bytecode compiled");
        let narrow_bc = narrow_slot.1.as_ref().expect("narrow bytecode compiled");
        assert!(!full_bc.is_empty() && !narrow_bc.is_empty());
        assert_ne!(
            full_bc.as_slice(),
            narrow_bc.as_slice(),
            "two different shim bundles must compile to different bytecode"
        );
        assert!(
            narrow_bc.len() < full_bc.len(),
            "the narrowed bundle carries 4 fewer shims, so its bytecode must be smaller \
             (full {} B, narrow {} B)",
            full_bc.len(),
            narrow_bc.len()
        );
        assert!(
            !before.contains(&narrow.key()),
            "precondition: the narrow bundle must not have been cached before this test"
        );

        // The right blob reached the right context.
        let full_globals = ctx_full
            .eval("typeof _ === 'object' && typeof chai === 'object' && typeof bru === 'object'")
            .await
            .expect("probe");
        assert_eq!(
            full_globals, "true",
            "the default bundle's context must have lodash, chai and bru"
        );
        let narrow_globals = ctx_narrow
            .eval(
                "typeof _ === 'undefined' && typeof chai === 'undefined' \
                 && typeof bru === 'undefined' && typeof CryptoJS === 'undefined' \
                 && typeof pm === 'object'",
            )
            .await
            .expect("probe");
        assert_eq!(
            narrow_globals, "true",
            "the narrowed bundle's context must NOT have been served the default bundle's bytecode"
        );
    }

    /// TR-501: per-VU heap by input format, amortised over N real VU
    /// contexts so the bytecode cache is warm — a single context pays the
    /// one-off compile and is not representative of VU number 2..N.
    ///
    /// Each context here owns a private `rquickjs::Runtime` (master; the
    /// shared-Runtime work is TR-503 / PR #481), so `quickjs_heap_bytes()`
    /// reports only its own runtime and summing is correct.
    ///
    /// `cargo test -p tropel-engine --release per_vu_heap_by_format -- --nocapture --ignored`
    #[tokio::test]
    #[ignore = "measurement, not an assertion — run explicitly with --nocapture"]
    async fn per_vu_heap_by_format() {
        const N: u32 = 25;

        let postman = br#"{"info":{"schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item":[{"name":"a","event":[{"listen":"test","script":{"exec":[
              "pm.expect(_.map([1],String)).to.eql(['1']);",
              "pm.environment.set('h', CryptoJS.MD5('x').toString());"]}}]}]}"#;
        let k6 = b"import http from 'k6/http';\nexport default function () { check(http.get('http://x'), {'ok': r => r.status === 200}); }";
        let har = br#"{"log":{"entries":[{"request":{"url":"https://example.com/a"}}]}}"#;
        let http_only = b"import http from 'k6/http'; export default () => http.get('http://x');";

        let bare = tropel_js::JsContext::new(None, None)
            .await
            .expect("bare context");
        println!(
            "bare JsContext (no shims)                = {:>9} B",
            bare.quickjs_heap_bytes()
        );

        let cases: Vec<(&str, ShimBundle)> = vec![
            ("default (all 7 shims)", ShimBundle::default()),
            (
                "content-gated http-only (no format)",
                ShimBundle::from_script(http_only),
            ),
            ("format=k6", ShimBundle::for_format("k6", k6)),
            ("format=postman", ShimBundle::for_format("postman", postman)),
            ("format=har", ShimBundle::for_format("har", har)),
            // NOT SHIPPED — this quantifies the headroom `format_shims`
            // deliberately leaves on the table by keeping pm.js in every
            // row (70,197 B of source, the largest single shim). See the
            // `format_shims` doc comment for why it is not taken.
            (
                "[not shipped] har minus pm.js",
                ShimBundle::from_shims(&[Shim::DeepEqual, Shim::Exec]),
            ),
        ];

        for (label, bundle) in cases {
            let mut ctxs = Vec::with_capacity(N as usize);
            for i in 0..N {
                ctxs.push(new_vu_ctx(i, &bundle).await);
            }
            // `quickjs_heap_bytes()` reads the RUNTIME's heap, and since TR-503
            // every context on this thread shares one runtime — so all N reads
            // return the same figure and summing them is N x double-counting.
            // The previous `sum / N` therefore printed the whole N-context
            // runtime heap under a `B/VU` label, ~N x too large. Read once and
            // divide once.
            let whole = ctxs[0].quickjs_heap_bytes();
            debug_assert_eq!(
                whole,
                ctxs[N as usize - 1].quickjs_heap_bytes(),
                "contexts on one thread must share a runtime; if this fires, \
                 sharing regressed and the arithmetic below is wrong"
            );
            println!(
                "{label:<40} = {:>9} B/VU  (N={N}, runtime total {whole} B, shims: {})",
                whole / u64::from(N),
                shim_names(&bundle).join("+")
            );
            std::hint::black_box(ctxs);
        }
    }

    /// A VU context wired exactly as production wires one, for the tests
    /// that need a real bootstrap rather than a bundle inspection.
    async fn new_vu_ctx(vu_id: u32, bundle: &ShimBundle) -> tropel_js::JsContext {
        let pm_state = new_pm_state();
        let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
            client: VuCookieClient::new(
                HttpClient::new(&HttpConfig::default()).expect("http client should construct"),
            ),
        });
        create_vu_js_context(
            vu_id,
            &pm_state,
            &client,
            bundle,
            &SandboxConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("VU context must be created")
    }
}
