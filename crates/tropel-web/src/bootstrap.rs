//! JS context bootstrap for the browser slice — mirrors the engine's
//! `create_vu_js_context` (crates/tropel-engine/src/js_bootstrap.rs) but
//! self-contained, because tropel-web cannot depend on tropel-engine (the
//! engine drags reqwest/tokio-net into the wasm graph; P5b gate).

use std::time::Duration;

use tropel_js::JsContext;
use tropel_sandbox::bindings::trp::TrpBridge;
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
/// Returns `None` when context creation fails (scripts are then skipped, as
/// in the engine). Memory and execution-time limits mirror the engine's.
pub async fn create_web_js_context(pm_state: &SharedPmState) -> Option<JsContext> {
    let mut ctx = JsContext::new(Some(10 * 1024 * 1024), Some(Duration::from_secs(10)))
        .await
        .ok()?;

    // N1: the bundle is one source text — the host import on wasm32 (the
    // browser build supplies its own), the embedded set on native. A `None`
    // (wasm32 + host supplied no bundle) runs shim-less: scripted items
    // fail loudly per item (ReferenceError surfaced in script_failures),
    // the same contract as the engine's shim-eval failure path.
    if let Some(bundle) = shim_bundle() {
        if ctx.bootstrap_library(&bundle).await.is_err() {
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
