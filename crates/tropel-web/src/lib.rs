//! Browser slice of Tropel (TROPEL_WASM_BUILD.md Step 5A).
//!
//! A `wasm32-wasip1` cdylib exposing a narrow C ABI over linear memory:
//! `tropel_alloc` / `tropel_free` manage host-visible buffers, and
//! `tropel_run` takes a postcard-encoded [`RunRequest`] and returns a packed
//! pointer to a postcard-encoded [`RunOutcome`]. The JS host (supplied by
//! `@tropel/exec-wasm`, P6) hides this ABI behind a TypeScript wrapper and
//! implements the `tropel_host_http` import (see [`http`]).

pub mod bootstrap;
pub mod http;
pub mod wire;

use std::sync::Arc;

use tropel_runtime::{flatten_execution_items, ScenarioRunner};
use tropel_sdk::scenario::Scenario;
use tropel_sdk::traits::DriverHttpClient;

pub use wire::{RunOutcome, RunRequest};

// ── C ABI ────────────────────────────────────────────────────────────────

/// Allocate a `len`-byte buffer visible to the host (leaked; free with
/// [`tropel_free`]). The host writes the postcard-encoded [`RunRequest`]
/// here, or reads the encoded [`RunOutcome`] from here.
#[no_mangle]
pub extern "C" fn tropel_alloc(len: usize) -> *mut u8 {
    let mut buf = vec![0u8; len].into_boxed_slice();
    let ptr = buf.as_mut_ptr();
    // Leak the allocation to the host; tropel_free reclaims it.
    std::mem::forget(buf);
    ptr
}

/// Free a buffer previously returned by [`tropel_alloc`].
///
/// # Safety
///
/// `ptr`/`len` must exactly match a buffer previously returned by
/// [`tropel_alloc`] that has not already been freed.
#[no_mangle]
pub unsafe extern "C" fn tropel_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // SAFETY: ptr/len were produced by tropel_alloc.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}

/// Run one web scenario pass.
///
/// `ptr`/`len` point at a postcard-encoded [`RunRequest`] in a buffer
/// previously allocated with [`tropel_alloc`]. The return value packs
/// `(out_ptr << 32) | out_len` of a postcard-encoded [`RunOutcome`] in a
/// fresh [`tropel_alloc`] buffer — the host must [`tropel_free`] it. Returns
/// `0` on a fatal internal failure (the outcome itself carries the error
/// string for ordinary failures).
///
/// # Safety
///
/// `ptr`/`len` must describe a valid, initialized postcard-encoded
/// [`RunRequest`] in a buffer previously returned by [`tropel_alloc`].
#[no_mangle]
pub unsafe extern "C" fn tropel_run(ptr: *const u8, len: usize) -> u64 {
    let outcome = if ptr.is_null() {
        RunOutcome::failed("tropel_run: null request pointer")
    } else {
        // SAFETY: ptr/len describe a valid postcard RunRequest buffer.
        let input = unsafe { std::slice::from_raw_parts(ptr, len) };
        match postcard::from_bytes::<RunRequest>(input) {
            Ok(req) => run_request_sync(req),
            Err(e) => RunOutcome::failed(format!("tropel_run: bad RunRequest postcard: {e}")),
        }
    };

    let bytes = match postcard::to_stdvec(&outcome) {
        Ok(b) => b,
        Err(e) => {
            // Cannot even encode the outcome; encode a minimal error instead.
            return encode_fatal(&format!("tropel_run: outcome encode failed: {e}"));
        }
    };

    let out_ptr = tropel_alloc(bytes.len());
    // SAFETY: out_ptr points at a leaked len-byte buffer from tropel_alloc.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, bytes.len());
    }
    ((out_ptr as u64) << 32) | (bytes.len() as u64)
}

// ── run implementation ───────────────────────────────────────────────────

/// Drive the async run from the synchronous C ABI entry on a current-thread
/// runtime (the wasm-safe tokio feature set; TROPEL_WASM_BUILD.md Step 3).
fn run_request_sync(req: RunRequest) -> RunOutcome {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return RunOutcome::failed(format!("tropel_run: runtime build failed: {e}")),
    };
    rt.block_on(run_request(req))
}

/// Execute the request: bootstrap the JS context, build the scenario runner,
/// and walk `iterations` iterations.
pub async fn run_request(req: RunRequest) -> RunOutcome {
    let scenario = match serde_json::from_str::<Scenario>(&req.scenario_json) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            return RunOutcome::failed(format!(
                "tropel_run: scenario JSON deserialize failed: {e}"
            ));
        }
    };
    let flattened = Arc::new(flatten_execution_items(&scenario.items));
    let names: Arc<Vec<String>> = Arc::new(flattened.iter().map(|i| i.name.clone()).collect());

    let client: Arc<dyn DriverHttpClient> = Arc::new(http::WebHttpClient);

    let mut runner = ScenarioRunner::new(
        scenario,
        flattened,
        names,
        client,
        req.vu_id,
        req.scenario_name.clone(),
    )
    .with_expected_statuses(req.parsed_expected_statuses());

    let pm_state = runner.state_handle();
    if let Some(ctx) = bootstrap::create_web_js_context(&pm_state).await {
        runner = runner.with_js_context(Box::new(ctx));
    }

    let mut iterations = Vec::with_capacity(req.iterations as usize);
    for i in 0..req.iterations {
        iterations.push(runner.run_iteration(i, None, &req.env_vars).await);
    }

    RunOutcome {
        iterations,
        error: None,
    }
}

/// Encode a minimal error outcome (used when the real outcome cannot be
/// encoded). Returns the same packed `(ptr << 32) | len` form.
fn encode_fatal(msg: &str) -> u64 {
    let bytes = match postcard::to_stdvec(&RunOutcome::failed(msg)) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    let out_ptr = tropel_alloc(bytes.len());
    // SAFETY: out_ptr points at a leaked len-byte buffer from tropel_alloc.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, bytes.len());
    }
    ((out_ptr as u64) << 32) | (bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::OnceCell;
    use std::collections::HashMap;
    use std::time::Duration;

    use tropel_sdk::scenario::{Scenario, ScenarioInfo, ScenarioItem};
    use tropel_sdk::types::{Body, Cookie, Method, Response, Timings};

    fn sample_response(req: &tropel_sdk::types::Request) -> tropel_sdk::Result<Response> {
        Ok(Response {
            url: req.url.clone(),
            status_code: 200,
            status_text: "OK".into(),
            headers: HashMap::new(),
            body: br#"{"ok":true}"#.to_vec(),
            text_cache: OnceCell::new(),
            json_cache: OnceCell::new(),
            response_time: Duration::from_millis(5),
            timings: Some(Timings::from_measured(
                Duration::from_millis(2),
                Duration::from_millis(2),
                Duration::from_millis(5),
            )),
            cookies: vec![Cookie {
                name: "s".into(),
                value: "v".into(),
                domain: None,
                path: None,
                http_only: None,
                secure: None,
                same_site: None,
                expires: None,
            }],
            size: 12,
            request_body_size: 0,
            redirects: vec![],
        })
    }

    fn sample_scenario() -> Scenario {
        Scenario {
            info: ScenarioInfo {
                name: "web-smoke".into(),
                description: None,
                schema: None,
            },
            items: vec![ScenarioItem {
                name: "ping".into(),
                request: Some(tropel_sdk::types::Request {
                    url: "https://example.com/ping".into(),
                    method: Method::GET,
                    headers: HashMap::new(),
                    query_params: HashMap::new(),
                    body: None,
                    auth: None,
                    certificate: None,
                    follow_redirects: true,
                    timeout: None,
                    response_type: tropel_sdk::types::ResponseType::Text,
                }),
                prerequest: None,
                test: Some("pm.test('ok', () => pm.expect(true).to.be.true);".into()),
                assertions: vec![],
                items: vec![],
            }],
            variables: HashMap::new(),
            auth: None,
        }
    }

    #[tokio::test]
    async fn run_request_produces_samples() {
        #[cfg(not(target_arch = "wasm32"))]
        crate::http::native_seam::set_handler(Box::new(sample_response));

        let outcome = run_request(RunRequest {
            scenario_json: serde_json::to_string(&sample_scenario()).unwrap(),
            vu_id: 1,
            scenario_name: "default".into(),
            iterations: 2,
            env_vars: HashMap::new(),
            expected_statuses: vec![],
        })
        .await;

        assert!(
            outcome.error.is_none(),
            "unexpected error: {:?}",
            outcome.error
        );
        assert_eq!(outcome.iterations.len(), 2);
        let first = &outcome.iterations[0];
        assert!(first.samples.len() >= 2, "expected http_req_* samples");
        let metrics: Vec<_> = first.samples.iter().map(|s| s.metric.as_ref()).collect();
        assert!(
            metrics.contains(&"http_req_duration"),
            "missing http_req_duration in {metrics:?}"
        );
        // The test script ran (bridge + shims bootstrapped) → no script failure.
        assert_eq!(first.script_failures, 0, "test script should have passed");
    }

    #[test]
    fn wire_roundtrip() {
        let req = RunRequest {
            scenario_json: serde_json::to_string(&sample_scenario()).unwrap(),
            vu_id: 7,
            scenario_name: "default".into(),
            iterations: 3,
            env_vars: HashMap::from([("BASE_URL".into(), "https://x.test".into())]),
            expected_statuses: vec![],
        };
        let bytes = postcard::to_stdvec(&req).unwrap();
        let back: RunRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.vu_id, 7);
        assert_eq!(back.iterations, 3);
        let back_scenario: Scenario = serde_json::from_str(&back.scenario_json).unwrap();
        assert_eq!(back_scenario.info.name, "web-smoke");
        assert_eq!(back.env_vars["BASE_URL"], "https://x.test");
    }

    #[test]
    fn body_custom_serde_survives_postcard() {
        // Body has hand-rolled tagged serde (types.rs) — on the wire the
        // scenario is JSON text, so all Body variants survive the postcard
        // round-trip (JSON → string → JSON is lossless).
        let mut scenario = sample_scenario();
        let inner = scenario.items[0].request.as_mut().unwrap();
        inner.body = Some(Body::Json(serde_json::json!({"a": 1})));
        // Override with UrlEncoded — the last write wins.
        inner.body = Some(Body::UrlEncoded(HashMap::from([("a".into(), "1".into())])));

        // Round-trip through postcard as scenario_json string.
        let req = RunRequest {
            scenario_json: serde_json::to_string(&scenario).unwrap(),
            vu_id: 1,
            scenario_name: "default".into(),
            iterations: 1,
            env_vars: HashMap::new(),
            expected_statuses: vec![],
        };
        let bytes = postcard::to_stdvec(&req).unwrap();
        let back: RunRequest = postcard::from_bytes(&bytes).unwrap();
        let back_scenario: Scenario =
            serde_json::from_str(&back.scenario_json).expect("scenario JSON is valid");
        let body = back_scenario.items[0]
            .request
            .as_ref()
            .and_then(|r| r.body.clone())
            .expect("body round-tripped");
        assert!(matches!(body, Body::UrlEncoded(_)), "got {body:?}");
    }
}
