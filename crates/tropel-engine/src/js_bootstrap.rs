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
/// 'undefined'` in every engine VU). Keep the two in lockstep: same 6 shims,
/// same order, same section headers.
const JS_SHIM_BUNDLE: &str = concat!(
    "// ==== shim: deep-equal-shim ====\n",
    include_str!("../../../js/shared/deep-equal.js"),
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
        let _ = globals.set(
            "__tropel_native_sleep",
            rquickjs::function::Func::from(move |ms: f64| {
                if ms > 0.0 {
                    // Interruptible sleep: poll the force-stop flag in small
                    // slices. On force-stop, zero the JS interrupt deadline so
                    // the eval is interrupted the moment control returns to JS
                    // (the flag-aware handler unwinds it) — backlog: gracefulStop
                    // force-stop was advisory only.
                    // P2 line 174: use absolute deadline to avoid sleep
                    // inflation from OS overshoot. The old code subtracted
                    // the requested slice, not the actual elapsed time, so
                    // OS overshoot compounds: ~+1-2% Linux, ~+10-20% macOS,
                    // ~+56% Windows (15.6ms granularity).
                    let total = Duration::from_secs_f64(ms / 1000.0);
                    let deadline_sleep_inner = std::time::Instant::now() + total;
                    let step = Duration::from_millis(10);
                    loop {
                        if force_stop_sleep.load(Ordering::Acquire) {
                            deadline_sleep.store(0, Ordering::Relaxed);
                            return;
                        }
                        let now = std::time::Instant::now();
                        if now >= deadline_sleep_inner {
                            break;
                        }
                        let remaining = deadline_sleep_inner - now;
                        std::thread::sleep(remaining.min(step));
                    }
                }
                tropel_js::rearm_deadline(&deadline_sleep, max_exec);
            }),
        );
    });

    let sleep_code = [
        "if (typeof sleep === 'undefined') {",
        "  function sleep(seconds) {",
        "    if (typeof __tropel_native_sleep === 'function') {",
        "      __tropel_native_sleep(seconds * 1000);",
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
    // A non-default (injected) bundle skips the bytecode cache entirely — the
    // process-wide cache is keyed to the single JS_SHIM_BUNDLE and must not
    // serve a different bundle's bytecode (see SHIM_BYTECODE note).
    if !shim.is_default() {
        let src = shim.render();
        return ctx.bootstrap_library(&src).await.map_err(|e| {
            TropelError::Js(format!("VU {vu_id}: injected shim bundle eval failed: {e}"))
        });
    }

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
        Ok(())
    } else {
        ctx.bootstrap_library(JS_SHIM_BUNDLE)
            .await
            .map_err(|e| TropelError::Js(format!("VU {vu_id}: shim source eval failed: {e}")))
    }
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
    fn shim_lists_stay_in_lockstep_with_bru() {
        let d = ShimBundle::default();
        assert_eq!(
            d.0.len(),
            7,
            "ShimBundle::default() must enumerate 7 shims (deep-equal/pm/chai/lodash/crypto/exec/bru)"
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
}
