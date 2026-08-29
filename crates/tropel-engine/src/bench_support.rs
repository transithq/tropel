//! Helpers the benchmark crate needs to measure the REAL VU path.
//!
//! These exist so `tropel-bench` measures production code rather than an
//! approximation of it. The per-VU memory gate previously measured a bare
//! `JsContext` — no shims — against a budget whose entire subject is shim
//! loading, so it could not fail for the thing it guarded (TR-501). It also
//! measured RSS, which is unavailable on macOS in that harness and silently
//! skipped there.
//!
//! Not `#[cfg(test)]`: `CONVENTIONS.md` requires a test to exercise the
//! production code path, and several defects survived because a test asserted
//! a `#[cfg(test)]` twin of the real function. A benchmark gate has the same
//! obligation.

use crate::js_bootstrap::{create_vu_js_context, ShimBundle};
use crate::vu_loop::DriverHttpClientImpl;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tropel_core::config::HttpConfig;
use tropel_http::client::{HttpClient, VuCookieClient};
use tropel_sandbox::config::SandboxConfig;
use tropel_sandbox::state::new_pm_state;
use tropel_sdk::traits::DriverHttpClient;

/// QuickJS heap bytes held by one fully-bootstrapped VU context — the number
/// the per-VU memory budget is written against.
///
/// Uses QuickJS's own accounting (`JS_ComputeMemoryUsage`) rather than a
/// process RSS delta: RSS is polluted by allocator arenas and by whatever else
/// the process is doing, and is not available on every platform, all of which
/// invite a budget that quietly measures nothing.
///
/// Returns `None` only if the context cannot be built at all, which the caller
/// must treat as a failure rather than a pass.
pub async fn vu_context_heap_bytes() -> Option<u64> {
    heap_bytes_for(&ShimBundle::default()).await
}

/// QuickJS heap bytes for a VU whose shim bundle was gated from `script`.
///
/// TR-501 claims gating saves ~120 KB/VU for an http-only script. Measure it
/// rather than assert it — see `js_bootstrap`'s gating tests.
pub async fn vu_context_heap_bytes_for_script(script: &[u8]) -> Option<u64> {
    heap_bytes_for(&ShimBundle::from_script(script)).await
}

async fn heap_bytes_for(shim: &ShimBundle) -> Option<u64> {
    let pm_state = new_pm_state();
    let client: Arc<dyn DriverHttpClient> = Arc::new(DriverHttpClientImpl {
        client: VuCookieClient::new(HttpClient::new(&HttpConfig::default()).ok()?),
    });
    let ctx = create_vu_js_context(
        1,
        &pm_state,
        &client,
        shim,
        &SandboxConfig::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    Some(ctx.quickjs_heap_bytes())
}
