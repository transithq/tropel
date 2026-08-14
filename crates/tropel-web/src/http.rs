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

use std::collections::HashMap;

use tropel_sdk::traits::DriverHttpClient;
#[cfg(target_arch = "wasm32")]
use tropel_sdk::types::Body;
use tropel_sdk::types::{Method, Request, Response, ResponseType};
use tropel_sdk::{Result, TropelError};

/// The browser slice's HTTP client: every request goes through the host.
#[derive(Debug, Default)]
pub struct WebHttpClient;

/// Wire form of `Request` for the `tropel_host_http` bridge.
///
/// Backlog line 44 (P0): the body used to cross the wire as `Option<Body>`
/// in its NATIVE postcard form — `Body::Json` serializes as the raw JSON
/// value's shape, `FormData`/`UrlEncoded`/`Binary`/`GraphQL` as tagged maps —
/// so the TS decoder (which must not know every shape) read a map-count
/// varint as a string length and produced a 1-char garbage body for every
/// non-`Raw` request (a JSON POST went out as one junk byte). The fix
/// mirrors the crate's own documented hazard pattern (wire.rs: why `Scenario`
/// is carried as a JSON string): the body is now a JSON **envelope string**
/// (`{"mode": ..., ...}`) that the host decodes unambiguously. The fields
/// after `body` (auth, certificate, follow_redirects, timeout, response_type)
/// were never read by the v1 host and are dropped from the wire — so the
/// "nothing follows body" claim in postcard.ts is now literally true.
///
/// `pub` (doc(hidden)) not `pub(crate)`: the F3 differential harness
/// (`tests/native_vs_wasm.rs`, an integration test) decodes the wire with
/// [`decode_wire_request`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[doc(hidden)]
pub struct WireRequest {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
    pub body: Option<String>,
}

/// Encode `Option<Body>` as the wire's JSON envelope string. Every variant
/// gets a `mode` tag, so the TS host never has to disambiguate postcard
/// shapes (strings vs maps vs raw JSON values). Binary bytes ride as a JSON
/// byte array (no extra dependency; correct, if not compact, on the bridge).
#[cfg(target_arch = "wasm32")]
fn body_to_wire(body: &Option<Body>) -> Option<String> {
    let b = body.as_ref()?;
    let env = match b {
        Body::Raw(s) => serde_json::json!({ "mode": "raw", "raw": s }),
        Body::Json(v) => serde_json::json!({ "mode": "json", "json": v }),
        Body::FormData(fields) => serde_json::json!({ "mode": "form_data", "fields": fields }),
        Body::UrlEncoded(fields) => serde_json::json!({ "mode": "url_encoded", "fields": fields }),
        Body::Binary(data) => serde_json::json!({ "mode": "binary", "data": data }),
        Body::GraphQL { query, variables } => {
            serde_json::json!({ "mode": "graphql", "query": query, "variables": variables })
        }
    };
    Some(env.to_string())
}

/// Decode a wire `Request` (the F3 harness / any native consumer). The wire
/// carries the request shape the host needs; auth/certificate/timeout/etc.
/// are not part of the v1 wire, so they default (the native seam and the
/// wasm run path are the same code, so the harness only needs url/method/
/// headers/query_params for its deterministic fixture).
///
/// WIRE-OF-RECORD: [`WireRequest`] is the layout the host sees — a future
/// fix that wants auth/timeout/etc. in the browser (backlog line 203, the
/// P2 "decoder drops Request.auth") must extend `WireRequest` AND the TS
/// decoder together, not rely on `Request`'s own serde.
#[doc(hidden)]
pub fn decode_wire_request(bytes: &[u8]) -> Result<Request> {
    let wire: WireRequest = postcard::from_bytes(bytes)
        .map_err(|e| TropelError::Http(format!("decode wire request: {e}")))?;
    Ok(Request {
        url: wire.url,
        method: Method::parse(&wire.method).unwrap_or(Method::GET),
        headers: wire.headers,
        query_params: wire.query_params,
        body: None,
        auth: None,
        certificate: None,
        follow_redirects: true,
        timeout: None,
        response_type: ResponseType::Text,
    })
}

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
        // Backlog line 44 (P0): serialize the WIRE VIEW (body as JSON
        // envelope string), not the native `Request` — the native form put
        // `Body`'s ambiguous postcard shapes on the wire.
        let wire = WireRequest {
            url: req.url.clone(),
            method: req.method.to_string(),
            headers: req.headers.clone(),
            query_params: req.query_params.clone(),
            body: body_to_wire(&req.body),
        };
        let bytes = postcard::to_stdvec(&wire)
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
