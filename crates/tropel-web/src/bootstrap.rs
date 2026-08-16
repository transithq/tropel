//! JS context bootstrap for the browser slice — mirrors the engine's
//! `create_vu_js_context` (crates/tropel-engine/src/js_bootstrap.rs) but
//! self-contained, because tropel-web cannot depend on tropel-engine (the
//! engine drags reqwest/tokio-net into the wasm graph; P5b gate).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tropel_js::JsContext;
use tropel_sandbox::bindings::trp::TrpBridge;
use tropel_sandbox::config::SandboxConfig;
use tropel_sandbox::state::SharedPmState;

/// The embedded shim bundle — the same sources the engine embeds via its
/// `ShimBundle::default()`, in the same order. The NATIVE default (N1: the
/// wasm32 build takes the bundle from the host instead, so the artifact no
/// longer carries ~150–250 KB of uncompressed JS and a `pm.*` fix ships as a
/// JS asset with the web app — no wasm rebuild, no release).
#[cfg(not(target_arch = "wasm32"))]
const SHIM_SOURCES: [&str; 6] = [
    include_str!("../../../js/scripting-api/pm.js"),
    include_str!("../../../js/chai/chai-shim.js"),
    include_str!("../../../js/lodash/lodash-shim.js"),
    include_str!("../../../js/cryptojs-shim/cryptojs.js"),
    include_str!("../../../js/exec/exec.js"),
    include_str!("../../../js/scripting-api/bru.js"),
];

// ── wasm: the shim host import (N1, TROPEL_MODULARIZATION_REVIEW_R2.md) ──
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    /// Host-provided shim bundle source text. Returns packed `(ptr << 32) |
    /// len` of a UTF-8 bundle string in linear memory (the host allocated it
    /// via this module's own `tropel_alloc`), or `0` when the host has no
    /// bundle (scripts then run shim-less).
    fn tropel_host_shim() -> u64;
}

/// The shim bundle source text to bootstrap, in the engine's order.
///
/// N1: on wasm32 the bundle comes from the host (`@tropel/shims`
/// `render(defaultBundle)` or the web app's own) so a shim fix is a JS asset
/// shipped with the app — no wasm rebuild. On native the embedded
/// [`SHIM_SOURCES`] remain the default. Returns `None` only when the host
/// supplied no bundle (wasm32): the context then runs shim-less and scripted
/// items fail loudly per item.
fn shim_bundle() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        // SAFETY: host contract — the return value is packed (ptr << 32) |
        // len of a UTF-8 bundle buffer in linear memory allocated via this
        // module's own tropel_alloc, or 0.
        let packed = unsafe { tropel_host_shim() };
        if packed == 0 {
            return None;
        }
        let ptr = (packed >> 32) as usize;
        let len = (packed & 0xFFFF_FFFF) as usize;
        // SAFETY: host contract — ptr/len describe a valid UTF-8 bundle
        // buffer in this module's linear memory.
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let text = String::from_utf8_lossy(slice).into_owned();
        // The host allocated it re-entrantly via our tropel_alloc — reclaim
        // (same contract as http.rs's bridge; leaking it would grow linear
        // memory on every context creation).
        // SAFETY: ptr/len exactly match the host's tropel_alloc buffer, and
        // the decode is complete.
        unsafe {
            crate::tropel_free(ptr as *mut u8, len);
        }
        Some(text)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(SHIM_SOURCES.join("\n"))
    }
}

/// `sleep(seconds)` wrapper installed in the VU context (engine parity).
const SLEEP_CODE: &str = r#"
if (typeof sleep === 'undefined') {
  function sleep(seconds) {
    if (typeof __tropel_native_sleep === 'function') {
      __tropel_native_sleep(seconds * 1000);
    }
  }
}
"#;

/// Create a JS context wired exactly like an engine VU context: shim bundle,
/// native modules, the `trp`/`pm`/`bru` binding bridge, and the blocking
/// `sleep` helper with the interrupt deadline re-armed after it.
///
/// W2 line 181: previously diverged from the engine's `create_vu_js_context`
/// on four points, all now aligned: (1) `sleep` slices and polls the
/// force-stop flag instead of one uninterruptible chunk; (2) the context is
/// built with `new_with_force_stop` over a CALLER-held flag — the old
/// `JsContext::new` wired a private `AtomicBool` nobody could flip, so
/// `exec.scenario.executor()` / `vusActive` / every force-stop check was
/// dead; (3) a non-default `SandboxConfig` preamble is evaluated before the
/// shim bundle, so a custom canonical name/aliases actually install; (4)
/// every failure is LOGGED (warn/error) instead of a silent `.ok()?`.
///
/// Returns `None` when context creation fails (scripts are then skipped, as
/// in the engine). Memory and execution-time limits mirror the engine's.
pub async fn create_web_js_context(
    pm_state: &SharedPmState,
    config: &SandboxConfig,
    force_stop: Arc<AtomicBool>,
) -> Option<JsContext> {
    let mut ctx = match JsContext::new_with_force_stop(
        Some(10 * 1024 * 1024),
        Some(Duration::from_secs(10)),
        force_stop.clone(),
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(
                "tropel-web: failed to create JS context: {e} (scripts will be skipped)"
            );
            return None;
        }
    };

    // W2 line 181 (engine parity): a NON-default sandbox config (custom
    // canonical name / aliases) must be installed as `__tropel_sandbox_config`
    // BEFORE the shim bundle evals, so pm.js's install tail exposes the
    // configured names. The default config is skipped — pm.js's own fallback
    // (`tropel` + `wire`) is byte-identical.
    if config != &SandboxConfig::default() {
        if let Err(e) = ctx.eval(&config.render_js_preamble()).await {
            // Loud: the embedder asked for a specific canonical name and
            // silently getting `tropel.*` would make every `trp.*` script
            // throw ReferenceError at runtime.
            tracing::warn!(
                "tropel-web: failed to set sandbox config preamble: {e} — failing the JS context"
            );
            return None;
        }
    }

    // N1: the bundle is one source text — the host import on wasm32 (the
    // browser build supplies its own), the embedded set on native. A `None`
    // (wasm32 + host supplied no bundle) runs shim-less: scripted items
    // fail loudly per item (ReferenceError surfaced in script_failures),
    // the same contract as the engine's shim-eval failure path.
    if let Some(bundle) = shim_bundle() {
        if let Err(e) = ctx.bootstrap_library(&bundle).await {
            // W2 line 181: loud, like the engine's shim-bootstrap error path
            // — a silent skip left every script throwing `pm is not defined`.
            tracing::error!("tropel-web: JS shim bootstrap FAILED: {e} — scripts will be skipped");
            return None;
        }
    }

    if let Err(e) = tropel_native::install_all(&mut ctx).await {
        tracing::warn!("tropel-web: failed to install native modules: {e}");
    }

    let bridge = TrpBridge::new(pm_state.clone());
    if let Err(e) = bridge.install(&mut ctx) {
        tracing::warn!("tropel-web: failed to install PM bridge functions: {e}");
    }

    // The sleep burns WALL time; the per-eval JS interrupt deadline must not
    // count it against the JS execution budget (engine parity, backlog line
    // 104). W2 line 181: the sleep is sliced and polls the force-stop flag
    // (engine parity) — on force-stop the JS deadline is zeroed so the eval
    // interrupts the moment control returns to JS.
    let (deadline, max_exec) = ctx.interrupt_deadline_handle();
    let force_stop_sleep = force_stop.clone();
    ctx.with_ctx(|rq_ctx| {
        let globals = rq_ctx.globals();
        let deadline_sleep = deadline.clone();
        let _ = globals.set(
            "__tropel_native_sleep",
            rquickjs::function::Func::from(move |ms: f64| {
                if ms > 0.0 {
                    let step = Duration::from_millis(10);
                    let mut remaining = Duration::from_secs_f64(ms / 1000.0);
                    while remaining > Duration::ZERO {
                        if force_stop_sleep.load(Ordering::Acquire) {
                            deadline_sleep.store(0, Ordering::Relaxed);
                            return;
                        }
                        let slice = remaining.min(step);
                        std::thread::sleep(slice);
                        remaining -= slice;
                    }
                }
                tropel_js::rearm_deadline(&deadline_sleep, max_exec);
            }),
        );
    });
    let _ = ctx.eval(SLEEP_CODE).await;

    Some(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tropel_sandbox::state::new_pm_state;

    /// W2 line 181: the web context must honor a NON-default SandboxConfig
    /// (engine parity) — custom namespace + aliases installed via the
    /// preamble BEFORE the shim bundle, default `trp` absent.
    #[tokio::test]
    async fn create_web_js_context_honors_custom_sandbox_config() {
        let pm_state = new_pm_state();
        let config = SandboxConfig {
            namespace: "acme".into(),
            aliases: vec!["product".into(), "wire".into()],
        };
        let mut ctx = create_web_js_context(&pm_state, &config, Arc::new(AtomicBool::new(false)))
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

    /// W2 line 181: the force-stop flag must actually interrupt a busy-loop
    /// eval. The old `JsContext::new` wired a private flag nobody could flip,
    /// so the force-stop checks (exec.scenario.executor / vusActive) were
    /// dead in the web slice.
    #[tokio::test]
    async fn create_web_js_context_force_stop_interrupts_busy_loop() {
        let pm_state = new_pm_state();
        let force_stop = Arc::new(AtomicBool::new(false));
        let mut ctx =
            create_web_js_context(&pm_state, &SandboxConfig::default(), force_stop.clone())
                .await
                .expect("context must be created");

        force_stop.store(true, Ordering::Release);
        // A busy loop would run the full 10s deadline if the flag were ignored.
        let err = ctx.eval("while (true) {}").await.err();
        assert!(
            err.is_some(),
            "force-stop flag must interrupt a busy-loop eval, got Ok"
        );
    }
}
