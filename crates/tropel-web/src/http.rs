//! The `DriverHttpClient` bridge for the browser slice.
//!
//! TROPEL_WASM_BUILD.md Step 5A: the wasm module imports a synchronous host
//! function from the `env` module (`tropel_host_http`) — the JS host
//! implements it and supplies responses. `Request` and `Response` cross the
//! bridge postcard-encoded over linear memory (packed `(ptr << 32) | len`).
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
    /// Host-provided synchronous HTTP bridge. `req` is a postcard-encoded
    /// `Request`; the return value packs `(ptr << 32) | len` of a
    /// postcard-encoded `Response` in linear memory, or `0` on error.
    fn tropel_host_http(req_ptr: *const u8, req_len: usize) -> u64;
}

// ── native: test seam ──
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native_seam {
    use std::sync::OnceLock;

    use tropel_sdk::types::{Request, Response};
    use tropel_sdk::{Result, TropelError};

    pub type Handler = Box<dyn Fn(&Request) -> Result<Response> + Send + Sync>;

    static HANDLER: OnceLock<Handler> = OnceLock::new();

    /// Install the handler native tests use instead of the wasm host import.
    #[cfg(test)]
    pub fn set_handler(h: Handler) {
        let _ = HANDLER.set(h);
    }

    pub fn bridge(req: &Request) -> Result<Response> {
        match HANDLER.get() {
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
        let bytes = postcard::to_stdvec(req)
            .map_err(|e| tropel_sdk::TropelError::Http(format!("encode request: {e}")))?;
        let packed = unsafe { tropel_host_http(bytes.as_ptr(), bytes.len()) };
        if packed == 0 {
            return Err(tropel_sdk::TropelError::Http("host HTTP bridge returned 0".into()));
        }
        let ptr = (packed >> 32) as usize;
        let len = (packed & 0xFFFF_FFFF) as usize;
        // SAFETY: the host contract is that the returned pointer/len describe
        // a valid postcard-encoded Response in this module's linear memory.
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        postcard::from_bytes(slice)
            .map_err(|e| tropel_sdk::TropelError::Http(format!("decode response: {e}")))
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
