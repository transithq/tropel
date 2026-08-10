//! JS context bootstrap for the browser slice — mirrors the engine's
//! `create_vu_js_context` (crates/tropel-engine/src/js_bootstrap.rs) but
//! self-contained, because tropel-web cannot depend on tropel-engine (the
//! engine drags reqwest/tokio-net into the wasm graph; P5b gate).

use std::time::Duration;

use tropel_js::JsContext;
use tropel_sandbox::bindings::trp::TrpBridge;
use tropel_sandbox::state::SharedPmState;

/// The embedded shim bundle — the same sources the engine embeds via its
/// `ShimBundle::default()`, in the same order.
const SHIM_SOURCES: [&str; 6] = [
    include_str!("../../../js/scripting-api/pm.js"),
    include_str!("../../../js/chai/chai-shim.js"),
    include_str!("../../../js/lodash/lodash-shim.js"),
    include_str!("../../../js/cryptojs-shim/cryptojs.js"),
    include_str!("../../../js/exec/exec.js"),
    include_str!("../../../js/scripting-api/bru.js"),
];

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
/// Returns `None` when context creation fails (scripts are then skipped, as
/// in the engine). Memory and execution-time limits mirror the engine's.
pub async fn create_web_js_context(pm_state: &SharedPmState) -> Option<JsContext> {
    let mut ctx = JsContext::new(Some(10 * 1024 * 1024), Some(Duration::from_secs(10)))
        .await
        .ok()?;

    for source in SHIM_SOURCES {
        if ctx.bootstrap_library(source).await.is_err() {
            return None;
        }
    }

    if tropel_native::install_all(&mut ctx).await.is_err() {
        return None;
    }

    let bridge = TrpBridge::new(pm_state.clone());
    if bridge.install(&mut ctx).is_err() {
        return None;
    }

    // The sleep burns WALL time; the per-eval JS interrupt deadline must not
    // count it against the JS execution budget (engine parity, backlog line
    // 104). Re-arm the deadline after the blocking sleep.
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
    let _ = ctx.eval(SLEEP_CODE).await;

    Some(ctx)
}
