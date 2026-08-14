//! The `DriverHttpClient` bridge for the browser slice.
//!
//! TROPEL_WASM_BUILD.md Step 5A: the wasm module imports a synchronous host
//! function from the `env` module (`tropel_host_http`) — the JS host
//! implements it and supplies responses. The wire formats mirror the
//! documented split in `wire.rs`: `Request` crosses as JSON text (the SDK's
//! `Body`/`AuthConfig` serde is JSON-oriented and postcard refuses
//! `deserialize_any` — the same reason `Scenario` is carried as
//! `scenario_json`), while `Response` crosses postcard-encoded. Both go over
//! linear memory packed as `(ptr << 32) | len`.
//!
//! On native (tests), there is no host import: a test-injectable handler
//! stands in, so the full run path — runner → client → samples — is
//! exercised without a browser.

use tropel_sdk::traits::DriverHttpClient;
use tropel_sdk::types::{Request, Response};
use tropel_sdk::Result;

/// The browser slice's HTTP client: every request goes through the host.
#[derive(Debug, Default)]
pub struct WebHttpClient;

// ── wasm: the host import ──
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    /// Host-provided synchronous HTTP bridge. `req` is a JSON-encoded
    /// `Request` (its `Body`/`AuthConfig` serde is JSON-oriented and cannot
    /// round-trip postcard — the exact hazard wire.rs documents for
    /// `Scenario`); the return value packs `(ptr << 32) | len` of a
    /// postcard-encoded `Response` in linear memory, or `0` on error.
    fn tropel_host_http(req_ptr: *const u8, req_len: usize) -> u64;
}

// ── native: test seam ──
// `pub` (doc(hidden)) not `pub(crate)`: the F3 differential harness lives in
// `tests/native_vs_wasm.rs` — an INTEGRATION test, which compiles the lib
// WITHOUT `cfg(test)` and cannot see `pub(crate)` items. The seam only exists
// on non-wasm targets, so it never ships in the wasm build; tropel-web is
// `publish = false`, so this is a test-only public surface.
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
pub mod native_seam {
    use std::sync::Mutex;

    use tropel_sdk::types::{Request, Response};
    use tropel_sdk::{Result, TropelError};

    pub type Handler = Box<dyn Fn(&Request) -> Result<Response> + Send + Sync>;

    // A Mutex (replaceable) rather than OnceLock: the F3 differential harness
    // installs its own deterministic handler, and test ordering is not
    // guaranteed — OnceLock's first-wins semantics would let an unrelated
    // test's handler leak into the harness and silently break the diff.
    static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);

    /// Install the handler native tests use instead of the wasm host import.
    /// Replaces any previous handler (last install wins).
    pub fn set_handler(h: Handler) {
        *HANDLER.lock().unwrap() = Some(h);
    }

    pub fn bridge(req: &Request) -> Result<Response> {
        let guard = HANDLER.lock().unwrap();
        match guard.as_ref() {
            Some(h) => h(req),
            None => Err(TropelError::Http(
                "no native HTTP handler installed (set one in tests)".into(),
            )),
        }
    }
}

fn bridge(req: &Request) -> Result<Response> {
    #[cfg(target_arch = "wasm32")]
    {
        // JSON, not postcard: `Body`'s Deserialize routes through
        // serde_json::Value (deserialize_any) and `AuthConfig` is internally
        // tagged — both refuse postcard, so a postcard-encoded `Request` with
        // a JSON/AUTH body failed to decode and the TS host silently sent a
        // one-byte junk body (backlog line 44). Same rationale as
        // `RunRequest.scenario_json` in wire.rs.
        let bytes = serde_json::to_vec(req)
            .map_err(|e| tropel_sdk::TropelError::Http(format!("encode request: {e}")))?;
        let packed = unsafe { tropel_host_http(bytes.as_ptr(), bytes.len()) };
        if packed == 0 {
            return Err(tropel_sdk::TropelError::Http(
                "host HTTP bridge returned 0".into(),
            ));
        }
        let ptr = (packed >> 32) as usize;
        let len = (packed & 0xFFFF_FFFF) as usize;
        // SAFETY: the host contract is that the returned pointer/len describe
        // a valid postcard-encoded Response in this module's linear memory.
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let decoded = postcard::from_bytes(slice)
            .map_err(|e| tropel_sdk::TropelError::Http(format!("decode response: {e}")));
        // The response buffer was allocated by the HOST via a re-entrant call
        // into this module's own `tropel_alloc` — reclamation is ours, and
        // leaking it per request would grow linear memory without bound on a
        // long-running web run (the F3 reviewer flagged this). The pointer is
        // valid until freed; decode is complete, so reclaim now.
        // SAFETY: ptr/len exactly match the buffer the host just allocated
        // via our `tropel_alloc`, and this is the only free.
        unsafe {
            crate::tropel_free(ptr as *mut u8, len);
        }
        decoded
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        native_seam::bridge(req)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DriverHttpClient for WebHttpClient {
    async fn execute(&self, req: &Request) -> Result<Response> {
        bridge(req)
    }
}
