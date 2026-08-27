//! # F3 — the REAL differential harness: native `tropel-runtime` vs wasm32
//!
//! TROPEL_MODULARIZATION_REVIEW.md F3 — the one-engine claim verified rather
//! than asserted. `driver_parity.rs` compares two *drivers* under one native
//! engine; this test compares the *runtime itself*: the same `RunRequest`
//! fixture walks through the identical `ScenarioRunner` code compiled two
//! ways —
//!
//!   1. **native** — `tropel_web::run_request`, with a deterministic fixture
//!      HTTP handler installed on the native seam (`native_seam`), and
//!   2. **wasm32-wasip1** — the `tropel_web.wasm` artifact (built by
//!      `scripts/wasm-size.sh`) loaded into wasmtime, driven through the
//!      postcard C ABI (`tropel_alloc` / `tropel_run` / `tropel_free`) with
//!      the `env.tropel_host_http` import implemented by the host using the
//!      **same** fixture handler.
//!
//! Both legs get byte-identical requests AND byte-identical responses, so any
//! divergence in the diffed outcome — status, headers, variable state,
//! assertion results, the `setNextRequest` trace — is a real runtime-semantics
//! difference between native and wasm, not a fixture difference.
//!
//! The test **skips** (with a notice) when the wasm artifact is absent so
//! plain `cargo test -p tropel-web` needs no wasm toolchain; set
//! `TROPEL_REQUIRE_WASM=1` (CI's `wasm` job) to turn a missing artifact into
//! a hard failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tropel_sdk::scenario::{Scenario, ScenarioInfo, ScenarioItem};
use tropel_sdk::types::{
    ApiKeyLocation, AuthConfig, Body, Cookie, Method, Request, Response, ResponseType, Sample,
    Timings,
};
use tropel_sdk::Result;
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::WasiCtxBuilder;

use tropel_web::http::native_seam;
use tropel_web::wire::{RunOutcome, RunRequest};

// ── the fixture ──────────────────────────────────────────────────────────

/// Deterministic response for ANY request — shared verbatim by the native
/// seam handler and the wasm host function, so both legs see the same bytes.
fn fixture_response(req: &Request) -> Result<Response> {
    Ok(Response {
        url: req.url.clone(),
        status_code: 200,
        status_text: "OK".into(),
        protocol: "HTTP/1.1".into(),
        headers: HashMap::from([("content-type".to_string(), "application/json".to_string())]),
        body: br#"{"ok":true}"#.to_vec(),
        text_cache: std::sync::OnceLock::new(),
        json_cache: std::sync::OnceLock::new(),
        response_time: std::time::Duration::from_millis(5),
        timings: Some(Timings::from_measured(
            std::time::Duration::from_millis(2),
            std::time::Duration::from_millis(2),
            std::time::Duration::from_millis(5),
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
            max_age: None,
        }],
        size: 12,
        request_body_size: 0,
        redirects: vec![],
    })
}

/// The fixture scenario: two items wired with `setNextRequest`, a variable
/// carried across the jump, a header assertion, and status assertions — every
/// axis F3 names (status, headers, variable state, assertion results, trace).
fn fixture_run_request() -> RunRequest {
    let item = |name: &str, url: &str, test: Option<&str>, body: Option<Body>| ScenarioItem {
        name: name.into(),
        id: None,
        request: Some(Request {
            url: url.into(),
            method: Method::GET,
            headers: Vec::new(),
            query_params: HashMap::new(),
            body,
            auth: None,
            certificate: None,
            follow_redirects: true,
            host: None,
            cookies: Vec::new(),
            timeout: None,
            response_type: ResponseType::Text,
        }),
        prerequest: vec![],
        test: test.map(|t| vec![t.to_string()]).unwrap_or_default(),
        assertions: vec![],
        items: vec![],
    };

    let scenario = Scenario {
        info: ScenarioInfo {
            name: "f3-diff".into(),
            description: None,
            schema: None,
        },
        items: vec![
            // Backlog line 44: the FIRST item carries a JSON body so the
            // wasm leg actually crosses a non-Raw Body over the tropel_host_http
            // bridge. Under the old postcard wire this decode failed (or the
            // TS host sent a one-byte junk body); the fixture pins the JSON
            // round-trip on BOTH legs.
            item(
                "first",
                "https://fixture.test/first",
                Some(
                    "pm.variables.set('carried', 'yes');\
                     pm.test('status is 200', () => pm.expect(pm.response.code).to.eql(200));\
                     pm.test('header content-type', () => pm.expect(pm.response.headers.get('content-type')).to.eql('application/json'));\
                     pm.execution.setNextRequest('second');",
                ),
                Some(Body::Json(serde_json::json!({ "ok": true }))),
            ),
            item(
                "second",
                "https://fixture.test/second",
                Some(
                    "pm.test('carried variable', () => pm.expect(pm.variables.get('carried')).to.eql('yes'));\
                     pm.test('second status', () => pm.expect(pm.response.code).to.eql(200));",
                ),
                None,
            ),
        ],
        variables: HashMap::new(),
        auth: None,
        conversion_notes: Vec::new(),
    };

    RunRequest {
        scenario_json: serde_json::to_string(&scenario).expect("scenario serializes"),
        vu_id: 1,
        scenario_name: "f3-diff".into(),
        iterations: 2,
        env_vars: HashMap::new(),
        // Spec strings — NOT ExpectedStatus: postcard refuses untagged enums
        // (the F3 harness caught this as a real wasm-ABI bug).
        expected_statuses: vec!["200".to_string()],
    }
}

// ── wasm artifact location ───────────────────────────────────────────────

fn wasm_artifact_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TROPEL_WASM_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        // CI's `wasm` job (wasm-size.sh builds to the default target dir).
        manifest.join("../../target/wasm32-wasip1/release-wasm/tropel_web.wasm"),
        // Machine-local target-dir override (see .cargo/config.toml).
        PathBuf::from("C:/tropel-native-target/wasm32-wasip1/release-wasm/tropel_web.wasm"),
        // Back-compat: pre-size-discipline artifacts built with --release.
        manifest.join("../../target/wasm32-wasip1/release/tropel_web.wasm"),
        PathBuf::from("C:/tropel-native-target/wasm32-wasip1/release/tropel_web.wasm"),
    ];
    let found = candidates.into_iter().find(|p| p.exists());
    // A stale pre-profile release/ artifact (3.98 MB, embedded-shims era)
    // must never silently pass the differential — the release-wasm profile
    // is the size-tuned source (API_CLIENT_WEB_PAYLOAD.md §2.4).
    if let Some(ref p) = found {
        if p.to_string_lossy().ends_with("/release/tropel_web.wasm") {
            eprintln!(
                "WARNING: F3 using pre-profile release/ artifact {} — run scripts/wasm-size.sh to build release-wasm",
                p.display()
            );
        }
    }
    found
}

// ── the wasm leg ─────────────────────────────────────────────────────────

/// Store state for wasmtime: the WASI preview1 context the wasm module links
/// against (wasm32-wasip1 needs WASI for the C libc underneath QuickJS).
struct HostState {
    wasi: WasiP1Ctx,
}

/// Implements the `env.tropel_host_http` import the wasm build declares
/// (http.rs): read the JSON-encoded `Request` from linear memory, answer with
/// the SAME fixture response, allocate the reply in wasm memory via the
/// module's own `tropel_alloc`, and return the packed `(ptr << 32) | len`.
///
/// Backlog line 44: the bridge carries `Request` as JSON, not postcard — the
/// SDK's `Body`/`AuthConfig` serde is JSON-oriented (deserialize_any /
/// internally-tagged) and postcard refuses it, so a postcard decode failed
/// on every non-Raw body. This host decodes the same bytes the JS host does.
fn host_http(mut caller: Caller<'_, HostState>, req_ptr: i32, req_len: i32) -> i64 {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .expect("wasm exports memory");
    let mut req_buf = vec![0u8; req_len as usize];
    memory
        .read(&caller, req_ptr as usize, &mut req_buf)
        .expect("read request");
    let req: Request = serde_json::from_slice(&req_buf).expect("decode Request");
    let resp = fixture_response(&req).expect("fixture response");
    let resp_bytes = postcard::to_stdvec(&resp).expect("encode Response");

    // Re-entrant call into the instance to allocate the reply buffer — the
    // same thing the JS host does.
    let alloc = caller
        .get_export("tropel_alloc")
        .and_then(|e| e.into_func())
        .expect("tropel_alloc export");
    let alloc_typed = alloc
        .typed::<(i32,), (i32,)>(&caller)
        .expect("typed tropel_alloc");
    let (ptr,) = alloc_typed
        .call(&mut caller, (resp_bytes.len() as i32,))
        .expect("alloc response");
    memory
        .write(&mut caller, ptr as usize, &resp_bytes)
        .expect("write response");
    ((ptr as i64) << 32) | (resp_bytes.len() as i64)
}

/// Implements the `env.tropel_host_shim` import (bootstrap.rs, N1): the wasm
/// slice takes its shim bundle from the host instead of embedding it. This
/// harness supplies the same sources the native leg embeds
/// (bootstrap.rs::SHIM_SOURCES), joined with "\n" exactly like
/// `shim_bundle()` does, so the two legs bootstrap identical shims.
fn host_shim(mut caller: Caller<'_, HostState>) -> i64 {
    const SHIM_SOURCES: [&str; 7] = [
        include_str!("../../../js/shared/deep-equal.js"),
        include_str!("../../../js/scripting-api/pm.js"),
        include_str!("../../../js/chai/chai-shim.js"),
        include_str!("../../../js/lodash/lodash-shim.js"),
        include_str!("../../../js/cryptojs-shim/cryptojs.js"),
        include_str!("../../../js/exec/exec.js"),
        include_str!("../../../js/scripting-api/bru.js"),
    ];
    let bundle = SHIM_SOURCES.join("\n");
    let bundle_bytes = bundle.as_bytes();

    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .expect("wasm exports memory");
    let alloc = caller
        .get_export("tropel_alloc")
        .and_then(|e| e.into_func())
        .expect("tropel_alloc export");
    let alloc_typed = alloc
        .typed::<(i32,), (i32,)>(&caller)
        .expect("typed tropel_alloc");
    let (ptr,) = alloc_typed
        .call(&mut caller, (bundle_bytes.len() as i32,))
        .expect("alloc shim bundle");
    memory
        .write(&mut caller, ptr as usize, bundle_bytes)
        .expect("write shim bundle");
    ((ptr as i64) << 32) | (bundle_bytes.len() as i64)
}

/// Run the fixture through the wasm32 build of `tropel-runtime` (the
/// `tropel_web.wasm` artifact) under wasmtime, mirroring the JS host ABI.
fn wasm_leg(wasm_path: &Path, req: &RunRequest) -> RunOutcome {
    let engine = Engine::default();
    let module = Module::from_file(&engine, wasm_path).expect("load tropel_web.wasm");

    let mut store = Store::new(
        &engine,
        HostState {
            wasi: WasiCtxBuilder::new().build_p1(),
        },
    );

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |h: &mut HostState| &mut h.wasi)
        .expect("link wasi preview1");
    linker
        .func_wrap("env", "tropel_host_http", host_http)
        .expect("define host http import");
    // N1 (TROPEL_MODULARIZATION_REVIEW_R2.md): the wasm slice no longer
    // embeds the shims — the host supplies them via tropel_host_shim. This
    // host function hands the wasm leg the SAME sources the native leg
    // embeds, joined the same way bootstrap.rs::shim_bundle() joins them, so
    // both legs bootstrap byte-identical shims (the diff stays valid).
    linker
        .func_wrap("env", "tropel_host_shim", host_shim)
        .expect("define host shim import");
    // Nothing else should be imported; trap defensively if the artifact
    // unexpectedly references more.
    linker
        .define_unknown_imports_as_traps(&module)
        .expect("trap unknown imports");

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate tropel_web");

    let alloc = instance
        .get_typed_func::<(i32,), (i32,)>(&mut store, "tropel_alloc")
        .expect("tropel_alloc");
    let free = instance
        .get_typed_func::<(i32, i32), ()>(&mut store, "tropel_free")
        .expect("tropel_free");
    let run = instance
        .get_typed_func::<(i32, i32), i64>(&mut store, "tropel_run")
        .expect("tropel_run");
    let memory: Memory = instance.get_memory(&mut store, "memory").expect("memory");

    let req_bytes = postcard::to_stdvec(req).expect("encode RunRequest");
    let (req_ptr,) = alloc
        .call(&mut store, (req_bytes.len() as i32,))
        .expect("alloc request");
    memory
        .write(&mut store, req_ptr as usize, &req_bytes)
        .expect("write request");

    let packed = run
        .call(&mut store, (req_ptr, req_bytes.len() as i32))
        .expect("tropel_run");
    if packed == 0 {
        return RunOutcome::failed("tropel_run returned 0 (fatal internal failure)");
    }
    let out_ptr = (packed >> 32) as usize;
    let out_len = (packed & 0xFFFF_FFFF) as usize;
    let mut out = vec![0u8; out_len];
    memory
        .read(&store, out_ptr, &mut out)
        .expect("read outcome");

    let _ = free.call(&mut store, (req_ptr, req_bytes.len() as i32));
    let _ = free.call(&mut store, (out_ptr as i32, out_len as i32));

    postcard::from_bytes(&out).expect("decode RunOutcome")
}

// ── native postcard round-trip probe ──────────────────────────────────────

/// F3 diagnostic: the full `RunOutcome` crosses the postcard wire for the
/// first time in this harness. Pin that round-trip natively (encode → decode
/// with the real types) so any postcard-incompatible type in the outcome
/// shows up here with a clear error, independent of the wasm ABI.
#[test]
fn native_outcome_postcard_roundtrip() {
    native_seam::set_handler(Box::new(fixture_response));
    let req = fixture_run_request();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("native runtime");
    let outcome = rt.block_on(tropel_web::run_request(req));
    assert!(outcome.error.is_none(), "run error: {:?}", outcome.error);
    assert!(
        !outcome.iterations.is_empty(),
        "expected at least one iteration"
    );

    let bytes = postcard::to_stdvec(&outcome).expect("encode RunOutcome");
    let back: RunOutcome = postcard::from_bytes(&bytes).expect("decode RunOutcome");
    assert_eq!(back.iterations.len(), outcome.iterations.len());
    assert_eq!(back.error, outcome.error);
}

// ── native postcard round-trip probe ──────────────────────────────────────

/// F3 diagnostic: bisect WHICH outcome type is asymmetric under postcard
/// (encode succeeds, decode of the same bytes fails with
/// `DeserializeUnexpectedEnd`). Each sub-probe pins one wire type.
#[test]
fn postcard_bisect_outcome_wire_types() {
    use std::sync::Arc;
    use std::time::SystemTime;
    use tropel_runtime::IterationResult;
    use tropel_sdk::types::{Sample, SampleType, TagMap};

    // 1. SystemTime alone (the most exotic wire member).
    {
        let t = SystemTime::now();
        let bytes = postcard::to_stdvec(&t).expect("encode SystemTime");
        let _: SystemTime = postcard::from_bytes(&bytes).expect("decode SystemTime");
    }

    // 2. TagMap alone (hand-rolled serde through HashMap<String,String>).
    {
        let mut m = TagMap::new();
        m.insert("url", "https://x.test".to_string());
        m.insert("status", "200".to_string());
        let bytes = postcard::to_stdvec(&m).expect("encode TagMap");
        let back: TagMap = postcard::from_bytes(&bytes).expect("decode TagMap");
        assert_eq!(back.get("status"), Some("200"));
    }

    // 3. Sample alone (Cow metric + Arc<TagMap> + SystemTime + enum).
    {
        let mut m = TagMap::new();
        m.insert("url", "https://x.test".to_string());
        let s = Sample {
            metric: "http_req_duration".into(),
            value: 5.0,
            tags: Arc::new(m),
            timestamp: SystemTime::now(),
            sample_type: SampleType::Trend,
        };
        let bytes = postcard::to_stdvec(&s).expect("encode Sample");
        let back: Sample = postcard::from_bytes(&bytes).expect("decode Sample");
        assert_eq!(back.value, 5.0);
        assert_eq!(back.sample_type, SampleType::Trend);
    }

    // 4. IterationResult (Vec<Sample> + counters).
    {
        let m = TagMap::new();
        let s = Sample {
            metric: "checks".into(),
            value: 1.0,
            tags: Arc::new(m),
            timestamp: SystemTime::now(),
            sample_type: SampleType::Rate,
        };
        let it = IterationResult {
            samples: vec![s],
            iteration_index: 0,
            script_failures: 0,
        };
        let bytes = postcard::to_stdvec(&it).expect("encode IterationResult");
        let back: IterationResult = postcard::from_bytes(&bytes).expect("decode IterationResult");
        assert_eq!(back.samples.len(), 1);
    }

    // 5. THE REAL outcome from the fixture: every real sample must round-trip
    // individually — pinpoints the exact content that breaks the full wire.
    {
        native_seam::set_handler(Box::new(fixture_response));
        let req = fixture_run_request();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("native runtime");
        let outcome = rt.block_on(tropel_web::run_request(req));
        assert!(outcome.error.is_none(), "run error: {:?}", outcome.error);

        for (it_i, it) in outcome.iterations.iter().enumerate() {
            for (s_i, s) in it.samples.iter().enumerate() {
                let bytes =
                    postcard::to_stdvec(s).unwrap_or_else(|e| panic!("enc it{it_i} s{s_i}: {e}"));
                let back: Sample = postcard::from_bytes(&bytes)
                    .unwrap_or_else(|e| panic!("dec it{it_i} s{s_i} {}: {e}", s.metric));
                assert_eq!(back.metric, s.metric, "metric mismatch it{it_i} s{s_i}");
            }
            let bytes = postcard::to_stdvec(&it.samples)
                .unwrap_or_else(|e| panic!("enc Vec<Sample> it{it_i}: {e}"));
            let _: Vec<Sample> = postcard::from_bytes(&bytes)
                .unwrap_or_else(|e| panic!("dec Vec<Sample> it{it_i}: {e}"));
        }
    }
}

// ── the diff ─────────────────────────────────────────────────────────────

/// A sample normalized for comparison: timestamps are wall-clock and differ
/// between legs by construction; everything else must be byte-identical.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NormSample {
    metric: String,
    value_bits: u64,
    sample_type: String,
    tags: Vec<(String, String)>,
}

fn norm_sample(s: &Sample) -> NormSample {
    let mut tags: Vec<(String, String)> = s
        .tags
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    tags.sort();
    NormSample {
        metric: s.metric.to_string(),
        value_bits: s.value.to_bits(),
        sample_type: format!("{:?}", s.sample_type),
        tags,
    }
}

/// A normalized iteration: the runner may interleave samples per hop, so the
/// sample list is sorted before comparing (order is not the contract; the
/// SET of samples per iteration is).
#[derive(Debug, PartialEq, Eq)]
struct NormIter {
    index: u64,
    script_failures: u64,
    samples: Vec<NormSample>,
}

fn normalize(outcome: &RunOutcome) -> Vec<NormIter> {
    let mut iters: Vec<NormIter> = outcome
        .iterations
        .iter()
        .map(|it| {
            let mut samples: Vec<NormSample> = it.samples.iter().map(norm_sample).collect();
            samples.sort();
            NormIter {
                index: it.iteration_index,
                script_failures: it.script_failures,
                samples,
            }
        })
        .collect();
    iters.sort_by_key(|i| i.index);
    iters
}

/// The one-engine claim, pinned: the same fixture produces the same full
/// outcome — status, headers, variable state, assertion results, and the
/// `setNextRequest` trace — whether the runtime is native or wasm32.
#[test]
fn native_and_wasm_runtime_produce_identical_outcomes() {
    let Some(wasm_path) = wasm_artifact_path() else {
        if std::env::var("TROPEL_REQUIRE_WASM").as_deref() == Ok("1") {
            panic!(
                "F3 harness: tropel_web.wasm not found but TROPEL_REQUIRE_WASM=1 \
                 (run scripts/wasm-size.sh first)"
            );
        }
        eprintln!(
            "SKIP native_vs_wasm: tropel_web.wasm not built. \
             Run `scripts/wasm-size.sh` (or the CI wasm job) to exercise the real diff."
        );
        return;
    };

    // Both legs share the SAME deterministic handler → identical responses.
    native_seam::set_handler(Box::new(fixture_response));

    let req = fixture_run_request();

    // Native leg: the identical `run_request` the wasm build runs, compiled
    // for the host.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("native runtime");
    let native = rt.block_on(tropel_web::run_request(req.clone()));

    // Wasm leg: the wasm32 build, driven through the C ABI.
    let wasm = wasm_leg(&wasm_path, &req);

    // Fatal error must match (both should be None for the fixture).
    assert_eq!(
        native.error, wasm.error,
        "fatal error must match: native={:?} wasm={:?}",
        native.error, wasm.error
    );

    let native_norm = normalize(&native);
    let wasm_norm = normalize(&wasm);

    assert_eq!(
        native_norm,
        wasm_norm,
        "native and wasm32 outcomes diverged — the one-engine claim fails.\n\
         native:\n{native:#?}\nwasm:\n{wasm:#?}",
        native = native_norm,
        wasm = wasm_norm
    );

    // The fixture must actually have exercised the trace on BOTH legs — a
    // boring pass (e.g. both skipped the jump) would not prove anything.
    let first = &native_norm[0];
    let reqs: Vec<&NormSample> = first
        .samples
        .iter()
        .filter(|s| s.metric == "http_reqs")
        .collect();
    assert_eq!(
        reqs.len(),
        2,
        "both items must have run (setNextRequest trace): {:#?}",
        first.samples
    );
    assert!(
        reqs.iter().any(|s| s
            .tags
            .iter()
            .any(|(k, v)| k == "url" && v == "https://fixture.test/second")),
        "the jump target must appear in the trace: {:#?}",
        first.samples
    );
    assert_eq!(
        first.script_failures, 0,
        "all scripts must pass on the native leg: {:#?}",
        first.samples
    );
    let wasm_first = &wasm_norm[0];
    assert_eq!(
        wasm_first.script_failures, 0,
        "all scripts must pass on the wasm leg: {:#?}",
        wasm_first.samples
    );

    // Note: the response buffers `host_http` allocates via re-entrant
    // tropel_alloc are reclaimed by the wasm module itself — http.rs's wasm
    // `bridge()` calls `tropel_free` after decoding (the F3 reviewer flagged
    // the leak; per-request growth would be unbounded on long web runs).
}

/// Build a single-item `RunRequest` from a raw `Request` — the corpus
/// primitive for the differential.
fn run_request_for(req: Request) -> RunRequest {
    use tropel_sdk::scenario::{Scenario, ScenarioInfo, ScenarioItem};
    let scenario = Scenario {
        info: ScenarioInfo {
            name: "corpus".into(),
            description: None,
            schema: None,
        },
        items: vec![ScenarioItem {
            id: None,
            name: "corpus-item".into(),
            request: Some(req),
            prerequest: vec![],
            test: vec![],
            assertions: vec![],
            items: vec![],
        }],
        variables: HashMap::new(),
        auth: None,
        conversion_notes: vec![],
    };
    RunRequest {
        scenario_json: serde_json::to_string(&scenario).expect("scenario serializes"),
        vu_id: 1,
        scenario_name: "corpus".into(),
        iterations: 1,
        env_vars: HashMap::new(),
        expected_statuses: vec!["200".to_string()],
    }
}

/// TR-408 (partial): the one-engine claim over a request CORPUS — different
/// methods, bodies, query params, and every auth scheme — not just the single
/// fixture. Same deterministic handler, both legs, identical outcomes.
#[test]
fn native_and_wasm_agree_over_request_corpus() {
    let Some(wasm_path) = wasm_artifact_path() else {
        if std::env::var("TROPEL_REQUIRE_WASM").as_deref() == Ok("1") {
            panic!("F3 corpus: tropel_web.wasm not found but TROPEL_REQUIRE_WASM=1");
        }
        eprintln!("SKIP native_vs_wasm corpus: tropel_web.wasm not built");
        return;
    };

    native_seam::set_handler(Box::new(fixture_response));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("native runtime");

    let base = |url: &str| Request {
        url: url.to_string(),
        method: Method::GET,
        headers: vec![],
        query_params: HashMap::new(),
        body: None,
        auth: None,
        certificate: None,
        follow_redirects: true,
        host: None,
        cookies: vec![],
        timeout: None,
        response_type: ResponseType::Text,
    };

    let mut corpus: Vec<Request> = vec![
        base("https://fixture.test/get"),
        {
            let mut r = base("https://fixture.test/post");
            r.method = Method::POST;
            r.body = Some(Body::Json(serde_json::json!({ "a": 1 })));
            r.headers = vec![("Content-Type".into(), "application/json".into())];
            r
        },
        {
            let mut r = base("https://fixture.test/query");
            r.query_params.insert("q".into(), "hello world".into());
            r
        },
        {
            let mut r = base("https://fixture.test/bearer");
            r.auth = Some(AuthConfig::Bearer { token: "tok".into() });
            r
        },
        {
            let mut r = base("https://fixture.test/basic");
            r.auth = Some(AuthConfig::Basic {
                username: "u".into(),
                password: "p".into(),
            });
            r
        },
        {
            let mut r = base("https://fixture.test/apikey");
            r.auth = Some(AuthConfig::ApiKey {
                key: "X-Key".into(),
                value: "k".into(),
                location: ApiKeyLocation::Header,
            });
            r
        },
        {
            let mut r = base("https://fixture.test/digest");
            r.auth = Some(AuthConfig::Digest {
                username: "u".into(),
                password: "p".into(),
            });
            r
        },
    ];

    // A failing script must ALSO agree (script_failures on both legs).
    {
        use tropel_sdk::scenario::{Scenario, ScenarioInfo, ScenarioItem};
        let scenario = Scenario {
            info: ScenarioInfo {
                name: "corpus-fail".into(),
                description: None,
                schema: None,
            },
            items: vec![ScenarioItem {
                id: None,
                name: "fail-item".into(),
                request: Some(base("https://fixture.test/fail")),
                prerequest: vec![],
                test: vec!["pm.test('boom', () => { throw new Error('boom'); });".into()],
                assertions: vec![],
                items: vec![],
            }],
            variables: HashMap::new(),
            auth: None,
            conversion_notes: vec![],
        };
        let run = RunRequest {
            scenario_json: serde_json::to_string(&scenario).expect("scenario serializes"),
            vu_id: 1,
            scenario_name: "corpus-fail".into(),
            iterations: 1,
            env_vars: HashMap::new(),
            expected_statuses: vec!["200".to_string()],
        };
        let native = rt.block_on(tropel_web::run_request(run.clone()));
        let wasm = wasm_leg(&wasm_path, &run);
        let n = normalize(&native);
        let w = normalize(&wasm);
        assert_eq!(n, w, "failing-script outcome diverged");
        assert_eq!(
            n[0].script_failures, 1,
            "the throwing script must fail on the native leg"
        );
    }

    for (i, req) in corpus.iter().enumerate() {
        let run = run_request_for(req.clone());
        let native = rt.block_on(tropel_web::run_request(run.clone()));
        let wasm = wasm_leg(&wasm_path, &run);
        let n = normalize(&native);
        let w = normalize(&wasm);
        assert_eq!(
            n, w,
            "corpus item {i} diverged ({}): native and wasm32 disagree",
            req.url
        );
        assert_eq!(n.len(), 1, "corpus item {i} must produce one iteration");
        assert_eq!(n[0].script_failures, 0, "corpus item {i} scripts must pass");
    }
}
