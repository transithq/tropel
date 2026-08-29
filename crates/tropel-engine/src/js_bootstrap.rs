//! Per-VU QuickJS context bootstrap.
//!
//! Moved out of the former `engine.rs` god-file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
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

/// All shim libraries concatenated at COMPILE TIME (concat!) into a single
/// `&'static str`, byte-identical for every VU and every scenario.
///
/// W2 line 182: this list MUST mirror [`ShimBundle::default`] exactly — it
/// previously carried only 5 shims while the default bundle carried 6 (with
/// bru), so the bytecode path short-circuited on `is_default()` and bru.js
/// was compiled into the binary but NEVER evaluated (`typeof bru ===
/// 'undefined'` in every engine VU). Keep the two in lockstep: same shims,
/// same order, same section headers.
const JS_SHIM_BUNDLE: &str = concat!(
    "// ==== shim: deep-equal-shim ====\n",
    include_str!("../../../js/shared/deep-equal.js"),
    "\n",
    "// ==== shim: k6-core-shim ====\n",
    include_str!("../../../js/shared/k6-core.js"),
    "\n",
    "// ==== shim: pm-shim ====\n",
    include_str!("../../../js/scripting-api/pm.js"),
    "\n",
    "// ==== shim: chai-shim ====\n",
    include_str!("../../../js/chai/chai-shim.js"),
    "\n",
    "// ==== shim: lodash-shim ====\n",
    include_str!("../../../js/lodash/lodash-shim.js"),
    "\n",
    "// ==== shim: cryptojs-shim ====\n",
    include_str!("../../../js/cryptojs-shim/cryptojs.js"),
    "\n",
    "// ==== shim: exec-shim ====\n",
    include_str!("../../../js/exec/exec.js"),
    "\n",
    "// ==== shim: bru-shim ====\n",
    include_str!("../../../js/scripting-api/bru.js"),
);

/// One shim library: a name + its source text.
pub struct ShimEntry(pub &'static str, pub std::borrow::Cow<'static, str>);

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
    /// Render the bundle to source text, concatenated with section headers
    /// (byte-identical to the compile-time [`JS_SHIM_BUNDLE`] for the
    /// default bundle — W2 line 182: both lists MUST enumerate the same 6
    /// shims in the same order, or the bytecode path silently drops bru).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for ShimEntry(name, src) in &self.0 {
            out.push_str(&format!("// ==== shim: {name} ====\n"));
            out.push_str(src);
            out.push('\n');
        }
        out
    }

    /// True if this bundle is the embedded default (same entries as
    /// [`ShimBundle::default`]).
    pub fn is_default(&self) -> bool {
        let d = Self::default();
        self.0.len() == d.0.len()
            && self.0.iter().zip(d.0.iter()).all(|(a, b)| {
                a.0 == b.0
                    && match (&a.1, &b.1) {
                        // Both borrowed from the same static: pointer
                        // equality suffices (line 328 optimization).
                        (std::borrow::Cow::Borrowed(x), std::borrow::Cow::Borrowed(y)) => {
                            std::ptr::eq(*x, *y)
                        }
                        // Owned or mixed: fall back to content comparison.
                        _ => a.1 == b.1,
                    }
            })
    }
}

impl Default for ShimBundle {
    fn default() -> Self {
        Self(vec![
            ShimEntry(
                "deep-equal-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/shared/deep-equal.js")),
            ),
            // TR-501: `check`, `group` and the metric constructors are k6's
            // API, not Postman's. They used to live in pm.js and be installed
            // onto globalThis from there, so every non-Postman format had to
            // load the whole 70 KB Postman shim to get `check()`. Split out so
            // format-driven bundles can drop pm.js without breaking k6
            // builtins. Ordered before pm-shim: pm.js no longer installs them.
            ShimEntry(
                "k6-core-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/shared/k6-core.js")),
            ),
            ShimEntry(
                "pm-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/scripting-api/pm.js")),
            ),
            ShimEntry(
                "chai-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/chai/chai-shim.js")),
            ),
            ShimEntry(
                "lodash-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/lodash/lodash-shim.js")),
            ),
            ShimEntry(
                "cryptojs-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/cryptojs-shim/cryptojs.js")),
            ),
            ShimEntry(
                "exec-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/exec/exec.js")),
            ),
            ShimEntry(
                "bru-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/scripting-api/bru.js")),
            ),
        ])
    }
}

/// P-B: source-gated bundle split — only load shims the script actually uses.
/// This eliminates ~77KB/VU for cryptojs and ~43KB/VU for lodash when the
/// script doesn't reference them, saving ~120KB/VU (~1.2GB at 10k VUs).
///
/// Conservative gate: any bare `crypto` token pulls cryptojs; any `_.`
/// method call pulls lodash. The full bundle is the fallback when the scan
/// is uncertain.
impl ShimBundle {
    /// Build a minimal shim bundle that includes only the shims the script
    /// actually references. Always includes the core shims (deep-equal, pm,
    /// chai, exec, bru). Conditionally includes cryptojs and lodash based
    /// on conservative keyword scanning of the script source.
    pub fn from_script(script: &[u8]) -> Self {
        Self::from_script_bytes(script)
    }

    /// Build a minimal shim bundle by scanning a file path for keywords.
    /// Reads the file once (OS page cache makes this cheap after the first
    /// VU) and delegates to [`Self::from_script_bytes`].
    pub fn from_script_path(path: &std::path::Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_script_bytes(&bytes),
            Err(_) => Self::default(),
        }
    }

    fn from_script_bytes(script: &[u8]) -> Self {
        // Convert to str for scanning; lossy is fine — we're looking for
        // ASCII keywords, not parsing UTF-8.
        let src = String::from_utf8_lossy(script);
        let needs_crypto =
            src.contains("CryptoJS") || src.contains("crypto.") || src.contains("crypto ");
        let needs_lodash = src.contains("_.") || src.contains("lodash");

        let mut entries = vec![
            ShimEntry(
                "deep-equal-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/shared/deep-equal.js")),
            ),
            // Unconditional: `check`/`group`/metrics are k6 builtins a script
            // may call without importing anything, so gating them on content
            // would turn a working script into a ReferenceError (TR-501).
            ShimEntry(
                "k6-core-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/shared/k6-core.js")),
            ),
            ShimEntry(
                "pm-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/scripting-api/pm.js")),
            ),
            ShimEntry(
                "chai-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/chai/chai-shim.js")),
            ),
        ];
        if needs_lodash {
            entries.push(ShimEntry(
                "lodash-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/lodash/lodash-shim.js")),
            ));
        }
        if needs_crypto {
            entries.push(ShimEntry(
                "cryptojs-shim",
                std::borrow::Cow::Borrowed(include_str!("../../../js/cryptojs-shim/cryptojs.js")),
            ));
        }
        entries.push(ShimEntry(
            "exec-shim",
            std::borrow::Cow::Borrowed(include_str!("../../../js/exec/exec.js")),
        ));
        entries.push(ShimEntry(
            "bru-shim",
            std::borrow::Cow::Borrowed(include_str!("../../../js/scripting-api/bru.js")),
        ));
        Self(entries)
    }
}

/// Process-wide cache of the compiled shim bundle bytecode.
///
/// Compiled ONCE by the first VU (qjsc-style: `JS_Eval` COMPILE_ONLY +
/// `JS_WriteObject`), then every subsequent VU loads the byte blob and runs
/// it instead of re-parsing + re-compiling the shim source. QuickJS bytecode
/// is tied to the build (version + feature flags), not to a particular
/// context, so one compilation is valid for all VU contexts in this process.
///
/// NOTE: this cache is keyed to the single [`JS_SHIM_BUNDLE`] constant. If a
/// second (different) shim bundle is ever added, this needs a per-bundle key
/// (e.g. `OnceLock<HashMap<&'static str, Option<Vec<u8>>>>`) — reusing this
/// static for a different bundle would silently serve the wrong bytecode.
static SHIM_BYTECODE: OnceLock<Option<Vec<u8>>> = OnceLock::new();

/// True once bytecode compilation failed — every VU then falls back to the
/// per-VU source eval path instead of retrying the compile each time.
static SHIM_BYTECODE_FAILED: AtomicBool = AtomicBool::new(false);

/// True once the cached bytecode failed to RUN in a VU. A run failure is
/// deterministic (same bytecode + same bundle for every VU), so after the
/// first failure all subsequent VUs short-circuit straight to the source eval
/// fallback instead of re-attempting the failing bytecode per VU.
static SHIM_BYTECODE_RUN_FAILED: AtomicBool = AtomicBool::new(false);

/// Create a JS context for one VU, bootstrap the bundled shim libraries
/// (scripting-api, chai, lodash, crypto, exec), install the native modules and
/// PM bridge functions, and wire a blocking `sleep(seconds)` helper.
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
        // TR-502 proper fix: sleep is now async (Promise-based) so the VU's
        // thread yields and other VUs on the same worker can progress. The old
        // sync std::thread::sleep parked the OS thread. With rquickjs
        // full-async, host functions can be async and the job queue is pumped
        // via finish_promise/pump_promise_queue.
        let _ = globals.set(
            "__tropel_native_sleep",
            rquickjs::function::Func::from(rquickjs::function::Async(
                move |_ctx: rquickjs::Ctx<'_>, ms: f64| {
                    let deadline_sleep = deadline_sleep.clone();
                    let force_stop_sleep = force_stop_sleep.clone();
                    async move {
                        if ms <= 0.0 {
                            tropel_js::rearm_deadline(&deadline_sleep, max_exec);
                            return Ok::<(), rquickjs::Error>(());
                        }
                        let total = Duration::from_secs_f64(ms / 1000.0);
                        let deadline_inner = std::time::Instant::now() + total;
                        let step = Duration::from_millis(10);
                        loop {
                            if force_stop_sleep.load(Ordering::Acquire) {
                                deadline_sleep.store(0, Ordering::Relaxed);
                                return Err(rquickjs::Error::Exception);
                            }
                            let now = std::time::Instant::now();
                            if now >= deadline_inner {
                                break;
                            }
                            let remaining = deadline_inner - now;
                            tokio::time::sleep(remaining.min(step)).await;
                        }
                        tropel_js::rearm_deadline(&deadline_sleep, max_exec);
                        Ok::<(), rquickjs::Error>(())
                    }
                },
            )),
        );
    });

    let sleep_code = [
        "if (typeof sleep === 'undefined') {",
        "  async function sleep(seconds) {",
        "    if (typeof __tropel_native_sleep === 'function') {",
        "      await __tropel_native_sleep(seconds * 1000);",
        "    }",
        "  }",
        "}",
    ]
    .join("\n");
    let _ = ctx.eval(&sleep_code).await;

    Some(ctx)
}

/// Bootstrap the shared shim libraries into `ctx`.
///
/// Preferred path: load the process-wide compiled [`JS_SHIM_BUNDLE`]
/// bytecode (compiled once by the first VU) and run it in this context —
/// no per-VU parse/compile. Fallback: if bytecode compilation failed or the
/// load/run errored, evaluate the source directly (the pre-bytecode path).
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
    // TR-501: respect the ShimBundle gating — an http-only script must not pay
    // for chai/lodash/cryptojs/pm.js it never uses. The old code ignored `shim`
    // (param named `_shim`) and always loaded the full 7-shim bundle, so the
    // per-VU heap stayed at 835k. Now: default bundle uses the bytecode cache
    // (fast), minimal bundles evaluate only their gated source (memory win).
    if shim.is_default() {
        let bytecode = SHIM_BYTECODE.get_or_init(|| {
            if SHIM_BYTECODE_FAILED.load(Ordering::Relaxed) {
                return None;
            }
            match ctx.compile_global_bytecode(JS_SHIM_BUNDLE) {
                Ok(bc) => {
                    tracing::info!(
                        "Compiled JS shim bundle to bytecode once ({} bytes) — reusing across VUs",
                        bc.len()
                    );
                    Some(bc)
                }
                Err(e) => {
                    SHIM_BYTECODE_FAILED.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "Shim bytecode compilation failed ({}); falling back to per-VU source eval",
                        e
                    );
                    None
                }
            }
        });

        if let (Some(bc), false) = (bytecode, SHIM_BYTECODE_RUN_FAILED.load(Ordering::Relaxed)) {
            if let Err(e) = ctx.run_global_bytecode(bc).await {
                SHIM_BYTECODE_RUN_FAILED.store(true, Ordering::Relaxed);
                tracing::warn!(
                    "VU {}: Failed to run JS shim bytecode: {} (disabling bytecode path; falling back to source eval)",
                    vu_id,
                    e
                );
                return ctx.bootstrap_library(JS_SHIM_BUNDLE).await.map_err(|e2| {
                    TropelError::Js(format!(
                        "VU {vu_id}: shim source eval failed after bytecode run error: {e2}"
                    ))
                });
            }
            return Ok(());
        }
        return ctx
            .bootstrap_library(JS_SHIM_BUNDLE)
            .await
            .map_err(|e| TropelError::Js(format!("VU {vu_id}: shim source eval failed: {e}")));
    }

    // Minimal bundle — evaluate only the gated shims, no bytecode cache.
    // The saving is ~77KB (cryptojs) + ~43KB (lodash) + pm.js when unused,
    // ~120KB/VU (~1.2 GB at 10k VUs). Bytecode for minimal bundles could be
    // cached per-variant, but the source eval is cheap for small bundles and
    // the memory win dominates.
    let rendered = shim.render();
    ctx.bootstrap_library(&rendered)
        .await
        .map_err(|e| TropelError::Js(format!("VU {vu_id}: minimal shim source eval failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tropel_core::config::HttpConfig;
    use tropel_http::client::{HttpClient, VuCookieClient};
    use tropel_sandbox::state::new_pm_state;
    use tropel_sdk::traits::DriverHttpClient;

    /// W2 line 182: the TWO shim lists must stay in lockstep — JS_SHIM_BUNDLE
    /// (the compile-time concat used by the bytecode path) used to carry 5
    /// shims while ShimBundle::default() carried 6 (with bru), so bru.js was
    /// compiled into the binary but NEVER evaluated (typeof bru ===
    /// 'undefined' in every engine VU). Guard both invariants: the default
    /// bundle must enumerate bru, and the bytecode source must embed the
    /// same bru.js text.
    #[test]
    /// TR-501: `check`, `group` and the metric constructors must NOT be
    /// installed by pm.js.
    ///
    /// They are k6's API, not Postman's. While pm.js installed them, "drop
    /// pm.js for non-Postman formats" broke `check()`, which made it look as
    /// though every format genuinely needed the 70 KB Postman shim. It did
    /// not — it needed these six symbols.
    ///
    /// **Fails on pre-fix code**: pm.js contained `globalThis.check = check;`
    /// and the five siblings, so the first assertion tripped.
    #[test]
    fn pm_shim_does_not_install_k6_builtins() {
        let d = ShimBundle::default();
        let pm = d
            .0
            .iter()
            .find(|e| e.0 == "pm-shim")
            .expect("pm-shim present")
            .1
            .as_ref();
        for sym in ["check", "group", "Counter", "Gauge", "Rate", "Trend"] {
            assert!(
                !pm.contains(&format!("globalThis.{sym} = {sym};")),
                "pm.js must not install the k6 builtin `{sym}` — it belongs to \
                 k6-core so non-Postman formats can drop pm.js (TR-501)"
            );
        }

        let core = d
            .0
            .iter()
            .find(|e| e.0 == "k6-core-shim")
            .expect("k6-core-shim present")
            .1
            .as_ref();
        for sym in ["check", "group", "Counter", "Gauge", "Rate", "Trend"] {
            assert!(
                core.contains(&format!("globalThis.{sym} = {sym};")),
                "k6-core must install `{sym}` — it is what makes dropping pm.js safe"
            );
        }
    }

    /// k6-core has to load BEFORE pm.js. pm.js no longer defines these
    /// globals, so a bundle that ordered them the other way would still work
    /// today by accident and break the moment anything in pm referenced them.
    #[test]
    fn k6_core_loads_before_pm() {
        let names: Vec<&str> = ShimBundle::default().0.iter().map(|e| e.0).collect();
        let core = names.iter().position(|n| *n == "k6-core-shim");
        let pm = names.iter().position(|n| *n == "pm-shim");
        assert!(
            core < pm,
            "k6-core-shim must precede pm-shim, got {names:?}"
        );
    }

    fn shim_lists_stay_in_lockstep_with_bru() {
        let d = ShimBundle::default();
        assert_eq!(
            d.0.len(),
            8,
            "ShimBundle::default() must enumerate 8 shims (deep-equal/k6-core/pm/chai/lodash/crypto/exec/bru)"
        );
        let bru_entry = d.0.iter().find(|e| e.0 == "bru-shim");
        assert!(
            bru_entry.is_some(),
            "ShimBundle::default() must include the bru-shim entry"
        );
        let bru_src = bru_entry.expect("bru entry").1.as_ref();
        assert!(
            JS_SHIM_BUNDLE.contains(bru_src),
            "JS_SHIM_BUNDLE must embed the same bru.js source as ShimBundle::default()"
        );
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
            "shim order must match between JS_SHIM_BUNDLE and ShimBundle::default()"
        );
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

    /// TR-501: shim gating must not cost more than it saves.
    ///
    /// The claim is *"an http-only k6 script pays nothing for chai, lodash,
    /// cryptojs or `pm.js`"* and *"http-only saves ~120 KB/VU"*. Measured, it
    /// is the other way round: a gated bundle is LARGER than the default.
    ///
    /// The cause is in `bootstrap_shims`: the shared, compile-once bytecode
    /// path is taken only when `shim.is_default()`. Any gated bundle falls
    /// through to per-VU **source eval**, and materialising the parser and
    /// source text costs more than the two shims gating drops.
    ///
    /// So this asserts the honest direction and will FAIL the day gating is
    /// made to pay off — at which point the number in TR-501 gets updated
    /// from a real measurement instead of an aspiration. Asserting the claim
    /// as written would pin a pessimisation as correct.
    #[tokio::test]
    async fn shim_gating_currently_costs_more_than_it_saves() {
        let http_only = b"import http from 'k6/http'; export default () => http.get('http://x');";

        let default_heap = crate::bench_support::vu_context_heap_bytes()
            .await
            .expect("default VU context");
        let gated_heap = crate::bench_support::vu_context_heap_bytes_for_script(http_only)
            .await
            .expect("gated VU context");

        assert!(
            gated_heap > default_heap,
            "shim gating is expected to be a PESSIMISATION today (gated {gated_heap} B vs \
             default {default_heap} B). If this now passes, gating has been fixed — most \
             likely by routing gated bundles through the bytecode cache in bootstrap_shims. \
             Invert this test, re-measure, and update TR-501 with the real saving."
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
}
