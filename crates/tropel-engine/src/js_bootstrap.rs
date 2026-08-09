//! Per-VU QuickJS context bootstrap.
//!
//! Moved out of the former `engine.rs` god-file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tropel_http::client::HttpClient;
use tropel_sandbox::config::SandboxConfig;
use tropel_sandbox::state::SharedPmState;
use tropel_sdk::error::TropelError;
use tropel_sdk::Result;

/// All shim libraries concatenated at COMPILE TIME (concat!) into a single
/// `&'static str`, byte-identical for every VU and every scenario.
const JS_SHIM_BUNDLE: &str = concat!(
    "// ==== shim: pm-api ====\n",
    include_str!("../../../js/pm-api/pm.js"),
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
    /// default bundle).
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
            && self
                .0
                .iter()
                .zip(d.0.iter())
                .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
    }
}

impl Default for ShimBundle {
    fn default() -> Self {
        Self(vec![
            ShimEntry(
                "pm-api",
                std::borrow::Cow::Borrowed(include_str!("../../../js/pm-api/pm.js")),
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
        ])
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
/// (pm-api, chai, lodash, crypto, exec), install the native modules and PM
/// bridge functions, and wire a blocking `sleep(seconds)` helper.
///
/// Returns `None` if context creation fails — context-creation failures log
/// a warning, but a shim bootstrap failure is logged at ERROR level (the VU
/// still runs, just without scripts).
pub(crate) async fn create_vu_js_context(
    vu_id: u32,
    pm_state: &SharedPmState,
    http_client: &Arc<HttpClient>,
    shim: &ShimBundle,
    config: &SandboxConfig,
) -> Option<tropel_js::JsContext> {
    let mut ctx = match tropel_js::JsContext::new(
        Some(10 * 1024 * 1024),
        Some(Duration::from_secs(10)),
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

    let bridge = tropel_sandbox::bindings::pm::PmBridge::new(pm_state.clone(), http_client.clone());
    if let Err(e) = bridge.install(&mut ctx) {
        tracing::warn!("VU {}: Failed to install PM bridge functions: {}", vu_id, e);
    }

    // The sleep burns WALL time; the per-eval JS interrupt deadline must not
    // count it against the JS execution budget, or a stock k6 pacing idiom
    // like `sleep(Math.random()*10)` is interrupted on resume (backlog line
    // 104). Re-arm the deadline after the blocking sleep, like the WS loop
    // does per step.
    let (deadline, max_exec) = ctx.interrupt_deadline_handle();
    ctx.with_ctx(|rq_ctx| {
        let globals = rq_ctx.globals();
        let deadline_sleep = deadline.clone();
        let _ = globals.set(
            "__tropel_native_sleep",
            rquickjs::function::Func::from(move |ms: f64| {
                if ms > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(ms / 1000.0));
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
    use tropel_http::client::HttpClient;
    use tropel_sandbox::state::new_pm_state;

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
        let client = Arc::new(
            HttpClient::new(&HttpConfig::default()).expect("http client should construct"),
        );
        let config = SandboxConfig {
            namespace: "acme".into(),
            aliases: vec!["product".into(), "wire".into()],
        };
        let mut ctx = create_vu_js_context(7, &pm_state, &client, &ShimBundle::default(), &config)
            .await
            .expect("context must be created");

        let check = ctx
            .eval(
                "typeof acme === 'object' && typeof product === 'object' \
                 && product === acme && wire === acme && typeof pm === 'object' \
                 && typeof trp === 'undefined' && typeof tropel === 'undefined'",
            )
            .await
            .expect("probe should eval");
        assert_eq!(
            check, "true",
            "custom namespace/aliases must be installed via the preamble; default trp absent — got: {check}"
        );
    }
}
