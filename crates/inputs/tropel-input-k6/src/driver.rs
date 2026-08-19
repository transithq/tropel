//! # K6 Driver — imperative execution path for k6 scripts
//!
//! Implements `Driver` + `DriverInstance` traits to run k6-style JS/TS test
//! scripts natively through the engine's imperative input path.
//!
//! ## Flow
//!
//! 1. **Pre-process** the raw source for ES-module evaluation: remove k6
//!    virtual imports (`import { … } from "k6/…"`) and unresolvable re-exports
//!    — the k6 shim provides those APIs as globals. All `export` modifiers
//!    are kept (`export const options`, `export default function`, …) because
//!    they are load-bearing in a module.
//!
//! 2. **Transpile** (if `.ts` source): strip TypeScript type annotations via
//!    `tropel_es::typescript_to_javascript_keep_exports` (keeps the `export`
//!    modifiers). ES module bundling is NOT used for k6 scripts — their
//!    imports (k6/http, k6/metrics, etc.) are virtual module names that don't
//!    correspond to files on disk.
//!
//! 3. **Bootstrap**: create a `JsContext`, bootstrap shim libraries (pm-api,
//!    chai, lodash, cryptojs, exec, sleep), install native modules.
//!
//! 4. **Eval as an ES module** (`rquickjs::Module::declare` + `eval` +
//!    `promise.finish`), then install the module's `default` export as the
//!    global `__tropel_iteration`. This is the only mode where
//!    `export const options` (the k6 load profile) survives alongside the
//!    default export.
//!
//! 5. **Options**: `Driver::declared_options` evaluates the script the same
//!    way in a throwaway context and reads the `options` export, so the
//!    engine can apply the script's own load profile (see `options.rs`).
//!
//! 6. **Run**: each call to `run_iteration()` invokes `__tropel_iteration()`
//!    and drains metrics/abort state from the `VuContext`.

use crate::options::K6Options;
use async_trait::async_trait;
use futures::future::join_all;
use futures_util::{SinkExt, StreamExt};
use rquickjs::function::Func;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tropel_js::JsContext;
use tropel_sdk::{
    AuthConfig, Body, Cookie, Method, Request, Response, Sample, SampleType, TagMap, Timings,
};
use tropel_sdk::{
    Driver, DriverDeclaredOptions, DriverHttpClient, DriverInstance, DriverRegistration, VuContext,
};
use tropel_sdk::{Result, TropelError};

// ══════════════════════════════════════════════════════════════════
// K6Driver — the stateless factory
// ══════════════════════════════════════════════════════════════════

/// Per-VU QuickJS heap cap (bytes), set in init() via
/// `JsContext::new_with_force_stop`. A server-controlled response body larger
/// than this must degrade to an error response, NOT `.expect()`-panic across
/// the QuickJS FFI boundary (backlog line 46 P0).
const K6_VU_HEAP_BYTES: usize = 10 * 1024 * 1024;

pub struct K6Driver;

#[async_trait]
impl Driver for K6Driver {
    fn id(&self) -> &str {
        "k6"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        if let Ok(text) = std::str::from_utf8(bytes) {
            // Reject ACTUAL Postman collections (handled by the Postman
            // adapter) using the SAME STRUCTURAL check the Postman adapter
            // uses — a JSON doc whose top-level info.schema points at the
            // getpostman.com collection schema. Substring matching is
            // forbidden (backlog line 61): a k6 script hitting k6's own
            // documented postman-echo.com endpoint legitimately contains
            // the word "postman" and was rejected.
            if crate::is_postman_collection(text) {
                return false;
            }
            let has_export_default = text.contains("export default");
            let has_k6_import = text.contains("from \"k6/") || text.contains("from 'k6/");
            let has_test_patterns = text.contains("http.get")
                || text.contains("http.post")
                || text.contains("check(")
                || text.contains("group(");
            has_export_default || has_k6_import || has_test_patterns
        } else {
            false
        }
    }

    // `#[allow]`: WsSession embeds a std::sync::mpsc::Receiver (not Sync), so
    // Arc<WsSession> is not Send+Sync — but the session registry never
    // crosses threads: each DriverInstance runs on its own VU thread
    // (thread-per-core), the same invariant the unsafe impl Send/Sync for
    // K6DriverInstance below documents.
    #[allow(clippy::arc_with_non_send_sync)]
    async fn init(
        &self,
        bytes: &[u8],
        source_path: Option<&Path>,
        exec: Option<&str>,
    ) -> Result<Box<dyn DriverInstance>> {
        let original = std::str::from_utf8(bytes)
            .map_err(|e| TropelError::Parse(format!("k6 script is not valid UTF-8: {}", e)))?;

        // Step 1: Pre-process — remove k6 virtual imports (the k6 shim
        // provides those APIs as globals) but KEEP all `export` modifiers so
        // the source can be evaluated as an ES module.
        let final_source = prepare_module_source(original, source_path)?;

        // Step 2: Create JS context. The force-stop flag (backlog: gracefulStop
        // force-stop was advisory only) is created HERE so the interrupt handler
        // and the native sleep capture the SAME Arc the engine later links to
        // the scheduler's flag via DriverInstance::set_force_stop_flag.
        let force_stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Direct scheduler-flag link: filled by set_force_stop_flag before the
        // first iteration. The sleep + pre-check read it lock-free; the JS
        // interrupt handler keeps polling the instance-local `force_stop`.
        let sched_link: Arc<OnceLock<Arc<AtomicBool>>> = Arc::new(OnceLock::new());
        let mut js_ctx = JsContext::new_with_force_stop(
            Some(K6_VU_HEAP_BYTES),
            Some(Duration::from_secs(10)),
            force_stop.clone(),
        )
        .await
        .map_err(|e| TropelError::Other(format!("JS context creation failed: {}", e)))?;

        // Step 3: Bootstrap shim libraries & native modules
        bootstrap_js_libs(&mut js_ctx, force_stop.clone(), sched_link.clone()).await?;

        // Install the k6 file-access bridges (open() + SharedArray cache).
        // Needs the script directory for relative path resolution.
        let script_dir = source_path.and_then(|p| p.parent().map(|d| d.to_path_buf()));
        register_k6_file_bridges(&mut js_ctx, script_dir.clone());

        // Register the ES-module resolver/loader so local imports
        // (`import { x } from "./helpers.js"`) resolve to files on disk,
        // with on-the-fly TypeScript transpilation for imported `.ts` files.
        js_ctx.set_module_loader(
            K6ModuleResolver {
                script_dir: script_dir.clone(),
            },
            K6ModuleLoader,
        );

        // Step 4: Eval the source as an ES module and install the entry-point
        // export as the global `__tropel_iteration`. When the scenario names
        // an `exec` function (k6 multi-scenario), install THAT export;
        // otherwise fall back to the module's `default` export. Modules are
        // the only mode where `export const options` (the k6 load profile) and
        // `export default function` survive together.
        install_iteration_global(&mut js_ctx, &final_source, exec)?;

        // Verify __tropel_iteration was defined
        let has_iter = js_ctx
            .get_global("__tropel_iteration")
            .await
            .unwrap_or(None);
        if has_iter.is_none() {
            return Err(TropelError::Parse(
                "k6 script did not define a default export function — \
                 expected `export default function() { ... }` or `export function handleSummary() { ... }`".into(),
            ));
        }

        Ok(Box::new(K6DriverInstance {
            js_ctx,
            _source_path: source_path.map(|p| p.to_path_buf()),
            http_bridge_registered: false,
            script_bridges_registered: false,
            sample_sink: Arc::new(Mutex::new(Vec::new())),
            group_stack: Arc::new(Mutex::new(Vec::new())),
            exec_state: Arc::new(Mutex::new(K6ExecState::default())),
            abort_requested: Arc::new(Mutex::new(None)),
            ws_sessions: Arc::new(Mutex::new(HashMap::new())),
            ws_next_id: Arc::new(AtomicU64::new(0)),
            ws_bridges_registered: false,
            globals_seeded: false,
            force_stop,
            sched_link,
        }))
    }

    async fn declared_options(
        &self,
        bytes: &[u8],
        source_path: Option<&Path>,
        env: &HashMap<String, String>,
    ) -> Result<Option<DriverDeclaredOptions>> {
        // Read the script's `export const options` by evaluating it as an ES
        // module. This is what makes k6's declared load profile (vus/duration/
        // stages/scenarios/thresholds) drive the run instead of being silently
        // ignored.
        //
        // Backlog line 153: `None` (nothing declared) and `Err` (declared but
        // MALFORMED) are now distinct. A type mismatch anywhere in `options`
        // — e.g. `stages: [{duration: 60}]` where k6 wants a string — used to
        // fail the whole parse, warn, and silently fall back to the CLI
        // profile: the run succeeded reporting numbers for a profile nobody
        // asked for. k6 hard-errors, so Tropel aborts with a Parse error too.
        // A non-UTF8 script / module-prep failure / eval failure / missing
        // export all mean "nothing declared" → Ok(None) → the engine falls
        // back to the CLI profile (unchanged from before line 153). Only a
        // PRESENT `options` export that fails to deserialize is fatal (Err).
        let original = match std::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let module_source = match prepare_module_source(original, source_path) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let script_dir = source_path.and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let json_str =
            match eval_module_export_json(&module_source, "options", env, script_dir).await {
                Ok(Some(s)) => s,
                _ => return Ok(None),
            };
        let options: K6Options = match serde_json::from_str(&json_str) {
            Ok(o) => o,
            Err(e) => {
                return Err(TropelError::Parse(format!(
                    "k6 script declares `options` but they failed to parse: {} \
                     (k6 would abort — fix the type mismatch, e.g. `stages: \
                     [{{duration: 60}}]` needs a string duration like \"60s\")",
                    e
                )));
            }
        };
        Ok(options.to_declared())
    }

    async fn handle_summary(
        &self,
        bytes: &[u8],
        source_path: Option<&Path>,
        summary_data_json: &str,
        env: &HashMap<String, String>,
    ) -> Option<HashMap<String, String>> {
        // Run the script's `export function handleSummary(data)` (k6) with
        // the post-run summary data object. Returns a map of filename →
        // content (the `stdout` key prints to stdout). Any failure (not a
        // function, eval error, …) yields None → engine falls back to its
        // default summary / --summary-export.
        let original = std::str::from_utf8(bytes).ok()?;
        let module_source = prepare_module_source(original, source_path).ok()?;
        let script_dir = source_path.and_then(|p| p.parent().map(|d| d.to_path_buf()));
        eval_module_handle_summary(&module_source, summary_data_json, env, script_dir)
            .await
            .ok()?
    }

    async fn setup(
        &self,
        bytes: &[u8],
        source_path: Option<&Path>,
        env: &HashMap<String, String>,
        http_client: Arc<dyn DriverHttpClient + Send + Sync>,
        sink: Arc<Mutex<Vec<Sample>>>,
    ) -> Option<String> {
        // Run the script's `export function setup()` (k6) ONCE per scenario,
        // before any VU spawns. The engine threads the serialized return
        // value into every VU (so `export default function (data)` receives
        // it) and passes it to `teardown(data)` after the run. Returns
        // `None` when the script declares no `setup` export (VUs see
        // `undefined`, matching k6). A throwing setup is LOGGED and yields
        // `None` — the engine falls back to no data rather than aborting
        // (never silently: a throwing setup is a broken artifact and must be
        // visible, even though the run proceeds with undefined data).
        //
        // k6 §4 (backlog line 119): setup() may make HTTP calls — the
        // throwaway context registers the HTTP bridges (via
        // eval_module_call_export) against the scenario's shared client, and
        // the samples land in `sink` for the engine to drain into the run's
        // metrics (k6 counts setup http_reqs). The canonical
        // login-in-setup pattern therefore works: `http.post(...)` resolves
        // and its value can be returned as `data`.
        let original = std::str::from_utf8(bytes).ok()?;
        let module_source = prepare_module_source(original, source_path).ok()?;
        let script_dir = source_path.and_then(|p| p.parent().map(|d| d.to_path_buf()));
        match eval_module_call_export(
            &module_source,
            "setup",
            None,
            env,
            script_dir,
            http_client,
            sink,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("k6 setup() failed: {}", e);
                None
            }
        }
    }

    async fn teardown(
        &self,
        bytes: &[u8],
        source_path: Option<&Path>,
        setup_data_json: Option<&str>,
        env: &HashMap<String, String>,
        http_client: Arc<dyn DriverHttpClient + Send + Sync>,
        sink: Arc<Mutex<Vec<Sample>>>,
    ) {
        // Run the script's `export function teardown(data)` (k6) ONCE after
        // all VUs finish, with the `setup()` return value as `data`. A
        // missing export is a silent no-op (k6); a throwing teardown is
        // logged but never affects the run's exit status (k6 parity).
        // teardown() may also make HTTP calls (k6 §4) — same bridges +
        // sink as setup(), drained by the engine after the call.
        let Some(original) = std::str::from_utf8(bytes).ok() else {
            return;
        };
        let Ok(module_source) = prepare_module_source(original, source_path) else {
            return;
        };
        let script_dir = source_path.and_then(|p| p.parent().map(|d| d.to_path_buf()));
        if let Err(e) = eval_module_call_export(
            &module_source,
            "teardown",
            setup_data_json,
            env,
            script_dir,
            http_client,
            sink,
        )
        .await
        {
            tracing::warn!("k6 teardown() failed: {}", e);
        }
    }
}

// Register K6Driver for compile-time discovery.
inventory::submit!(DriverRegistration::new("k6", || Box::new(K6Driver)).with_priority(10));

// ──────────────────────────────────────────────────────────────────────
// k6 `open()` + `k6/data` SharedArray native cache
// ──────────────────────────────────────────────────────────────────────
//
// k6 semantics: `new SharedArray(name, factory)` computes the data ONCE per
// process (in the init context) and shares it read-only across all VUs. In
// Tropel each VU owns its own JsContext (thread-per-core), so the "shared"
// payload lives on the native side: the first VU context that constructs a
// given SharedArray runs the factory, and its parsed elements are stored in
// this process-global cache as ONE `Arc<Vec<Value>>`. Every other VU context
// gets only a name + length from the accessor bridges and fetches elements
// through `__tropel_k6_shared_array_get(name, i)` — no per-VU copy of the
// array (the old design re-serialized the whole JSON into every context,
// O(VUs × size)).
//
// Keyed by name only — matches k6 (the name is the identity).
//
// Locking model: an `Arc` SNAPSHOT under an `RwLock`. The map is written
// once per SharedArray name (rare) and read per element (hot). Readers do
// ONE shared read-lock acquisition to clone the `Arc` (a refcount bump, no
// string copy), then drop the guard and read their private snapshot
// LOCK-FREE — so the per-element path never holds a lock during key
// building / element serialization, and concurrent readers never collide
// (the old `Mutex` serialized every VU thread on every element read — the
// one place all VUs collided). Writers clone the small map and swap it in.
/// name -> frozen element rows. `Arc<Vec<...>>` lets readers grab a cheap
/// refcount bump; writers clone the small map and swap it in.
type SharedArrayCache = RwLock<Arc<HashMap<String, Arc<Vec<serde_json::Value>>>>>;

static SHARED_ARRAY_CACHE: OnceLock<SharedArrayCache> = OnceLock::new();

fn shared_array_cache() -> &'static SharedArrayCache {
    SHARED_ARRAY_CACHE.get_or_init(|| RwLock::new(Arc::new(HashMap::new())))
}

/// Register the k6 file-access native bridges on a JS context:
///
/// - `__tropel_k6_open(path, mode)` — reads a file (relative to the script's
///   directory, or absolute) and returns its contents: `"t"` mode returns the
///   UTF-8 text, `"b"` mode returns base64-encoded bytes (the shim decodes
///   into an ArrayBuffer, matching k6's `open(path, 'b')`). A missing/unreadable
///   file throws a JS `Error` (k6 behavior).
/// - `__tropel_k6_shared_array_len(name)` — element count, or `-1` if absent
///   (the shim runs the factory only when absent).
/// - `__tropel_k6_shared_array_get(name, i)` — native JS value of ONE element,
///   or `undefined` when absent/out-of-range (no per-element JSON parse).
/// - `__tropel_k6_shared_array_set(name, json)` — parse the computed array
///   ONCE and share it as a process-global `Arc<Vec<Value>>`.
///
/// The bridges must be installed on EVERY k6 context that may evaluate script
/// code (the per-VU init context AND the throwaway options/handleSummary
/// contexts), because k6 scripts routinely call `open()`/`new SharedArray()`
/// at module top level while building `export const options`.
fn register_k6_file_bridges(ctx: &mut JsContext, script_dir: Option<PathBuf>) {
    ctx.with_ctx(|rq_ctx| {
        let globals = rq_ctx.globals();
        let dir = script_dir.clone();
        // Key the SharedArray cache by script dir + name so two different
        // scripts sharing a process (multi-scenario, repeated in-process runs)
        // never collide on the same name (k6 keys by the init context).
        let cache_prefix = dir
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();

        let _ = globals.set(
            "__tropel_k6_open",
            Func::from(
                move |ctx: rquickjs::Ctx,
                      path: String,
                      mode: String|
                      -> std::result::Result<String, rquickjs::Error> {
                    let p = Path::new(&path);
                    let full = if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        match &dir {
                            Some(d) => d.join(p),
                            None => p.to_path_buf(),
                        }
                    };
                    match std::fs::read(&full) {
                        Ok(bytes) => {
                            if mode == "b" {
                                use base64::Engine;
                                Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
                            } else {
                                Ok(String::from_utf8_lossy(&bytes).into_owned())
                            }
                        }
                        Err(e) => {
                            let msg = format!("open('{}'): {}", path, e);
                            let exc = rquickjs::Exception::from_message(ctx.clone(), &msg)
                                .map_err(|_| rquickjs::Error::Exception)?;
                            Err(ctx.throw(exc.into_object().into_value()))
                        }
                    }
                },
            ),
        );

        // The SharedArray bridges live in a generic fn (below) so the element
        // accessor can return a native `Value<'js>` instead of a JSON string.
        register_shared_array_bridges(rq_ctx, cache_prefix);
    });
}

/// Convert a parsed `serde_json::Value` into a native QuickJS value.
///
/// Elements are stored as parsed JSON in the shared cache; this builds the
/// corresponding JS value directly in the QuickJS heap (objects, arrays,
/// primitives, null), so the accessor bridge returns a native value and the
/// shim needs no `JSON.parse` per element.
fn json_to_value<'js>(ctx: &rquickjs::Ctx<'js>, v: &serde_json::Value) -> rquickjs::Value<'js> {
    match v {
        serde_json::Value::Null => rquickjs::Value::new_null(ctx.clone()),
        serde_json::Value::Bool(b) => rquickjs::IntoJs::into_js(*b, ctx)
            .unwrap_or_else(|_| rquickjs::Value::new_undefined(ctx.clone())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rquickjs::IntoJs::into_js(i, ctx)
                    .unwrap_or_else(|_| rquickjs::Value::new_undefined(ctx.clone()))
            } else if let Some(f) = n.as_f64() {
                rquickjs::IntoJs::into_js(f, ctx)
                    .unwrap_or_else(|_| rquickjs::Value::new_undefined(ctx.clone()))
            } else {
                rquickjs::Value::new_undefined(ctx.clone())
            }
        }
        serde_json::Value::String(s) => rquickjs::IntoJs::into_js(s.as_str(), ctx)
            .unwrap_or_else(|_| rquickjs::Value::new_undefined(ctx.clone())),
        serde_json::Value::Array(items) => match rquickjs::Array::new(ctx.clone()) {
            Ok(arr) => {
                for (i, item) in items.iter().enumerate() {
                    let _ = arr.set(i, json_to_value(ctx, item));
                }
                arr.into_value()
            }
            Err(_) => rquickjs::Value::new_undefined(ctx.clone()),
        },
        serde_json::Value::Object(map) => match rquickjs::Object::new(ctx.clone()) {
            Ok(obj) => {
                for (k, val) in map {
                    let _ = obj.set(k.as_str(), json_to_value(ctx, val));
                }
                obj.into_value()
            }
            Err(_) => rquickjs::Value::new_undefined(ctx.clone()),
        },
    }
}

/// Register the SharedArray native bridges on a JS context. Lives in a
/// generic fn — not an inline closure — because the element-accessor bridge's
/// closure must name the QuickJS lifetime `'js` to return a native
/// `Value<'js>` (eliminating the per-element `to_string` + `JSON.parse` round
/// trip), and closures cannot declare lifetimes.
fn register_shared_array_bridges<'js>(rq_ctx: &rquickjs::Ctx<'js>, cache_prefix: String) {
    let globals = rq_ctx.globals();

    // `len` returns -1 when the name is absent (factory must run) or the
    // element count when cached — the JS shim decides between the two. Uses
    // the same Arc-snapshot pattern as `get`: clone the Arc under a shared
    // read lock, drop the guard, read lock-free.
    let len_prefix = cache_prefix.clone();
    let _ = globals.set(
        "__tropel_k6_shared_array_len",
        Func::from(move |name: String| -> i32 {
            let key = format!("{}|{}", len_prefix, name);
            let snapshot = shared_array_cache()
                .read()
                .map(|c| c.clone())
                .unwrap_or_default();
            snapshot.get(&key).map(|v| v.len() as i32).unwrap_or(-1)
        }),
    );

    // Element accessor — returns a NATIVE JS value for ONE element, or
    // `undefined` when absent/out-of-range. The shim consumes the value
    // directly (no `JSON.parse` per element), so no context ever materializes
    // the whole array or re-parses it.
    let get_prefix = cache_prefix.clone();
    let _ = globals.set(
        "__tropel_k6_shared_array_get",
        Func::from(
            move |ctx: rquickjs::Ctx<'js>, name: String, i: i32| -> rquickjs::Value<'js> {
                let key = format!("{}|{}", get_prefix, name);
                // Snapshot: one shared read lock to clone the Arc (refcount
                // bump), then drop the guard and read lock-free.
                let snapshot = shared_array_cache()
                    .read()
                    .map(|c| c.clone())
                    .unwrap_or_default();
                match snapshot.get(&key) {
                    Some(v) if i >= 0 && (i as usize) < v.len() => {
                        json_to_value(&ctx, &v[i as usize])
                    }
                    _ => rquickjs::Value::new_undefined(ctx.clone()),
                }
            },
        ),
    );

    // Parse the computed array ONCE (first VU) and share it as an Arc.
    let set_prefix = cache_prefix.clone();
    let _ = globals.set(
        "__tropel_k6_shared_array_set",
        Func::from(move |name: String, json: String| {
            let key = format!("{}|{}", set_prefix, name);
            if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&json) {
                if let Ok(mut c) = shared_array_cache().write() {
                    // Clone the small map and swap it in (the snapshot model);
                    // per-name writes are rare (once per SharedArray).
                    Arc::make_mut(&mut c).insert(key, Arc::new(parsed));
                }
            }
        }),
    );
}

/// The k6 `open()` + `k6/data` SharedArray shim (globals `open` and
/// `SharedArray`), which delegates to the native bridges above.
const OPEN_DATA_SHIM: &str = include_str!("../../../../js/k6-shim/open-data-shim.js");

// ══════════════════════════════════════════════════════════════════
// K6DriverInstance — per-iteration execution
// ══════════════════════════════════════════════════════════════════

pub struct K6DriverInstance {
    js_ctx: JsContext,
    /// Level-triggered force-stop flag (backlog: gracefulStop force-stop was
    /// advisory only). Created in init(); its value is synced from the
    /// scheduler's flag by `set_force_stop_flag` so the JS interrupt handler
    /// trips on a force-stop that was ALREADY requested at link time.
    force_stop: Arc<AtomicBool>,
    /// Direct link to the SCHEDULER's force-stop flag (backlog: gracefulStop
    /// force-stop was advisory only). `set_force_stop_flag` stores the engine's
    /// Arc here, and the native sleep + the iteration pre-check poll it
    /// DIRECTLY — a forwarding task would be starved while the VU thread is
    /// blocked inside the native `std::thread::sleep` (the exact bug this
    /// fixes: a 60s sleep ran to completion because the forwarder never got
    /// polled, so force-stop never propagated). Filled once per VU before the
    /// first iteration; `OnceLock` keeps the read lock-free.
    sched_link: Arc<OnceLock<Arc<AtomicBool>>>,
    _source_path: Option<std::path::PathBuf>,
    /// Whether the native HTTP bridge (__tropel_k6_http_request) has been
    /// registered. Registration happens on the first run_iteration() call
    /// because the HttpClient isn't available until init() completes.
    http_bridge_registered: bool,
    /// Whether the script-state bridges (__tropel_pm_test,
    /// __tropel_pm_custom_metric_add, __tropel_exec_*, __tropel_test_abort)
    /// have been registered. Same lazy pattern as the HTTP bridge; these
    /// read the per-VU exec_state / abort flag.
    script_bridges_registered: bool,
    /// Shared sink for samples recorded by the native HTTP bridge closures
    /// (__tropel_k6_http_request / __tropel_k6_http_batch). The closures are
    /// 'static and can't reach the VuContext, so they push into this buffer
    /// and run_iteration() drains it into ctx.samples after each iteration.
    sample_sink: Arc<Mutex<Vec<Sample>>>,
    /// Per-VU group() stack (backlog line 154). `group(name)` pushes the
    /// FULL `::a::b` path here (backlog line 63), the matching group_end
    /// pops; the http/checks bridges read the top when stamping samples so
    /// metrics recorded inside a group carry `group=::a::b` (k6 parity)
    /// instead of the innermost raw name or the hardcoded `group=http`.
    group_stack: Arc<Mutex<Vec<String>>>,
    /// Shared exec.* state — the pm.js / k6-shim / exec.js scripts read it
    /// through __tropel_exec_* closures registered lazily; sync_globals()
    /// refreshes it from the VuContext before each iteration.
    exec_state: Arc<Mutex<K6ExecState>>,
    /// test.abort() flag — set by __tropel_test_abort, drained by
    /// run_iteration() into ctx.abort() so the engine stops the run.
    abort_requested: Arc<Mutex<Option<String>>>,
    /// Live WebSocket sessions created by `__tropel_k6_ws_connect`. The
    /// bridge closures are 'static and can't own the VuContext, so the
    /// registry lives here; `__tropel_k6_ws_finish` removes the session and
    /// emits its ws_* samples into the sample_sink.
    ///
    ws_sessions: Arc<Mutex<HashMap<u64, Arc<WsSession>>>>,
    /// Monotonic session-id allocator for ws sessions.
    ws_next_id: Arc<AtomicU64>,
    /// Whether the ws bridges (__tropel_k6_ws_*) have been registered.
    ws_bridges_registered: bool,
    /// Whether the per-VU IMMUTABLE globals (__VU, __tropel_vu_id,
    /// __tropel_scenario, __ENV, __tropel_env) have been seeded. The env is
    /// constant for the whole run, so re-serializing it every iteration was
    /// pure waste; sync_globals() seeds these once on the first iteration and
    /// only refreshes the per-iteration values (__ITER, data row, exec.*)
    /// thereafter.
    globals_seeded: bool,
}

/// Execution-context values exposed to scripts via `exec.*` (k6 API).
/// Populated from the VuContext by sync_globals() before each iteration.
#[derive(Debug, Clone, Default)]
struct K6ExecState {
    scenario_name: String,
    executor_name: String,
    vu_id: u32,
    iteration: u64,
    iterations_completed: u64,
    vus_active: u32,
}

/// A live `ws.connect()` session. The bridge side owns the events channel
/// (JS polls it with `__tropel_k6_ws_step`) and the command channel (JS sends
/// text/ping/close frames via `__tropel_k6_ws_send` / `_ping` / `_close`).
struct WsSession {
    /// Events produced by the background reader task, drained by `step()`.
    events_rx: Receiver<WsEvent>,
    /// Commands (send/ping/close) forwarded to the background writer task.
    cmd_tx: tokio::sync::mpsc::Sender<WsCommand>,
    /// Peer URL (for ws_* metric tags).
    url: String,
    /// Wall-clock when the session started (ws_req_duration).
    start: Instant,
    /// Handshake duration (ws_connecting trend).
    connecting: Duration,
    /// Counters accumulated by the JS-facing bridges (atomics: the session
    /// is shared through an `Arc` across the send/step/finish closures).
    msgs_sent: AtomicU64,
    bytes_sent: AtomicU64,
    msgs_received: AtomicU64,
    bytes_received: AtomicU64,
    /// Backlog line 62: whether the session ended abnormally — an `Error`
    /// event, a close code other than 1000/1001, or the reader dropping
    /// without a close frame (1006). `ws_req_failed` is 1.0 iff this is set.
    failed: AtomicBool,
}

/// A single WebSocket event delivered to JS via `__tropel_k6_ws_step`.
enum WsEvent {
    Open,
    Text(String),
    Binary(usize),
    Ping,
    Pong,
    Close { code: u16, reason: String },
    Error(String),
}

/// A command sent from JS (via the ws bridges) to the background writer task.
enum WsCommand {
    SendText(String),
    Ping,
    Close { code: u16, reason: String },
}

// Safety: each DriverInstance runs on its own VU thread (thread-per-core),
// so it is only ever used from a single thread at a time. JsContext is
// `Send` (see tropel_js) but NOT `Sync` (rquickjs uses a non-atomic Rc), so
// the instance can move to its VU thread but must never be shared across
// threads — which the thread-per-core loop guarantees.
unsafe impl Send for K6DriverInstance {}

/// Lock-free interning for the static http tag keys/values: one `Arc<str>`
/// allocation per key for the whole process instead of one per request.
/// `OnceLock::get_or_init` is a relaxed atomic load after warm-up; the
/// returned clone is a refcount bump, not a string copy.
fn interned(s: &'static str) -> Arc<str> {
    static URL: OnceLock<Arc<str>> = OnceLock::new();
    static METHOD: OnceLock<Arc<str>> = OnceLock::new();
    static STATUS: OnceLock<Arc<str>> = OnceLock::new();
    static NAME: OnceLock<Arc<str>> = OnceLock::new();
    static GROUP: OnceLock<Arc<str>> = OnceLock::new();
    static HTTP: OnceLock<Arc<str>> = OnceLock::new();
    static SCENARIO: OnceLock<Arc<str>> = OnceLock::new();
    match s {
        "url" => URL.get_or_init(|| Arc::from("url")).clone(),
        "method" => METHOD.get_or_init(|| Arc::from("method")).clone(),
        "status" => STATUS.get_or_init(|| Arc::from("status")).clone(),
        "name" => NAME.get_or_init(|| Arc::from("name")).clone(),
        "group" => GROUP.get_or_init(|| Arc::from("group")).clone(),
        "http" => HTTP.get_or_init(|| Arc::from("http")).clone(),
        "scenario" => SCENARIO.get_or_init(|| Arc::from("scenario")).clone(),
        other => Arc::from(other),
    }
}

/// Intern common HTTP method strings to avoid per-sample allocation.
/// The 9 standard methods plus common custom ones cover 99.9%+ of traffic.
fn intern_method(s: &str) -> Arc<str> {
    use std::sync::OnceLock;
    static GET: OnceLock<Arc<str>> = OnceLock::new();
    static POST: OnceLock<Arc<str>> = OnceLock::new();
    static PUT: OnceLock<Arc<str>> = OnceLock::new();
    static PATCH: OnceLock<Arc<str>> = OnceLock::new();
    static DELETE: OnceLock<Arc<str>> = OnceLock::new();
    static HEAD: OnceLock<Arc<str>> = OnceLock::new();
    static OPTIONS: OnceLock<Arc<str>> = OnceLock::new();
    static TRACE: OnceLock<Arc<str>> = OnceLock::new();
    static CONNECT: OnceLock<Arc<str>> = OnceLock::new();
    match s {
        "GET" => GET.get_or_init(|| Arc::from("GET")).clone(),
        "POST" => POST.get_or_init(|| Arc::from("POST")).clone(),
        "PUT" => PUT.get_or_init(|| Arc::from("PUT")).clone(),
        "PATCH" => PATCH.get_or_init(|| Arc::from("PATCH")).clone(),
        "DELETE" => DELETE.get_or_init(|| Arc::from("DELETE")).clone(),
        "HEAD" => HEAD.get_or_init(|| Arc::from("HEAD")).clone(),
        "OPTIONS" => OPTIONS.get_or_init(|| Arc::from("OPTIONS")).clone(),
        "TRACE" => TRACE.get_or_init(|| Arc::from("TRACE")).clone(),
        "CONNECT" => CONNECT.get_or_init(|| Arc::from("CONNECT")).clone(),
        other => Arc::from(other),
    }
}

/// Intern common HTTP status code strings to avoid per-sample allocation.
fn intern_status(s: &str) -> Arc<str> {
    use std::sync::OnceLock;
    static S200: OnceLock<Arc<str>> = OnceLock::new();
    static S201: OnceLock<Arc<str>> = OnceLock::new();
    static S204: OnceLock<Arc<str>> = OnceLock::new();
    static S301: OnceLock<Arc<str>> = OnceLock::new();
    static S302: OnceLock<Arc<str>> = OnceLock::new();
    static S304: OnceLock<Arc<str>> = OnceLock::new();
    static S400: OnceLock<Arc<str>> = OnceLock::new();
    static S401: OnceLock<Arc<str>> = OnceLock::new();
    static S403: OnceLock<Arc<str>> = OnceLock::new();
    static S404: OnceLock<Arc<str>> = OnceLock::new();
    static S500: OnceLock<Arc<str>> = OnceLock::new();
    static S502: OnceLock<Arc<str>> = OnceLock::new();
    static S503: OnceLock<Arc<str>> = OnceLock::new();
    match s {
        "200" => S200.get_or_init(|| Arc::from("200")).clone(),
        "201" => S201.get_or_init(|| Arc::from("201")).clone(),
        "204" => S204.get_or_init(|| Arc::from("204")).clone(),
        "301" => S301.get_or_init(|| Arc::from("301")).clone(),
        "302" => S302.get_or_init(|| Arc::from("302")).clone(),
        "304" => S304.get_or_init(|| Arc::from("304")).clone(),
        "400" => S400.get_or_init(|| Arc::from("400")).clone(),
        "401" => S401.get_or_init(|| Arc::from("401")).clone(),
        "403" => S403.get_or_init(|| Arc::from("403")).clone(),
        "404" => S404.get_or_init(|| Arc::from("404")).clone(),
        "500" => S500.get_or_init(|| Arc::from("500")).clone(),
        "502" => S502.get_or_init(|| Arc::from("502")).clone(),
        "503" => S503.get_or_init(|| Arc::from("503")).clone(),
        other => Arc::from(other),
    }
}

/// Build the standard http_req_* tag set (url/method/status/name/group).
/// The url value is allocated ONCE and shared by both `url` and `name`
/// (refcount bump, not a second string copy); static keys come from
/// [`interned`]. The scenario tag is stamped here when present, so the
/// drain's scenario pass is a no-op for http samples — it never
/// `Arc::make_mut`-clones the map shared by the 5 per-request samples.
fn http_tags(
    req: &Request,
    status: &str,
    scenario: &Arc<str>,
    extra: Option<&HashMap<String, String>>,
    group: Option<&str>,
) -> TagMap {
    http_tags_for(
        &req.url,
        req.method.as_str(),
        status,
        scenario,
        extra,
        group,
    )
}

/// [`http_tags`] with explicit URL/method (redirect hops reuse it).
///
/// `extra` carries k6 `params.tags` for the request — merged over the
/// defaults so a user `name`/`url`/… tag wins (k6 semantics: params.tags
/// add to AND override the auto tags; the `name` tag is the common case,
/// grouping `http_req_duration` for a specific request).
///
/// `group` is the active group() path (backlog line 154: k6 stamps metrics
/// recorded inside `group("x")` with `group=::x`, not the hardcoded
/// `group=http`). None → default `group=http` (back-compat).
fn http_tags_for(
    url: &str,
    method: &str,
    status: &str,
    scenario: &Arc<str>,
    extra: Option<&HashMap<String, String>>,
    group: Option<&str>,
) -> TagMap {
    let mut tags = TagMap::with_capacity(6);
    let url_arc: Arc<str> = Arc::from(url);
    tags.insert(interned("url"), url_arc.clone());
    tags.insert(interned("method"), intern_method(method));
    tags.insert(interned("status"), intern_status(status));
    tags.insert(interned("name"), url_arc);
    tags.insert(
        interned("group"),
        group.map(Arc::from).unwrap_or_else(|| interned("http")),
    );
    if !scenario.is_empty() {
        // Refcount bump — the Arc was created once at bridge registration.
        tags.insert(interned("scenario"), scenario.clone());
    }
    if let Some(extra) = extra {
        for (k, v) in extra {
            tags.insert(k.clone(), v.clone());
        }
    }
    tags
}

/// Tiny status-0 error envelope used when a response allocation fails
/// (backlog line 46 P0): mirrors the invalid-method path so the script sees
/// a FAILED response (checks fail, http_req_failed counts) instead of a
/// cross-FFI panic from `.expect()` on a body larger than the per-VU heap
/// cap. Returns `None` only when the heap has no headroom at all — the
/// caller then propagates a JS exception (never a panic).
fn k6_error_envelope<'js>(
    ctx: &rquickjs::Ctx<'js>,
    msg: &str,
    binary: bool,
) -> Option<rquickjs::Object<'js>> {
    let e = rquickjs::Object::new(ctx.clone()).ok()?;
    let _ = e.set("code", 0_i32);
    let _ = e.set("status", 0_i32);
    let _ = e.set("status_text", msg);
    let _ = e.set("error", msg);
    // W2 line 189: the PRODUCTION envelope must match the tested degrade
    // contract — error_code mapped via k6_error_code (was hardcoded 1000),
    // and a binary response keeps an empty ArrayBuffer body so
    // res.body.byteLength doesn't see a type change (was String "").
    let _ = e.set("error_code", k6_error_code(msg));
    let _ = e.set("headers", rquickjs::Object::new(ctx.clone()).ok()?);
    if binary {
        match rquickjs::ArrayBuffer::new(ctx.clone(), Vec::<u8>::new()) {
            Ok(ab) => {
                let _ = e.set("body", ab);
            }
            Err(_) => {
                let _ = e.set("body", "");
            }
        }
    } else {
        let _ = e.set("body", "");
    }
    let _ = e.set("response_time", 0.0_f64);
    Some(e)
}

/// Record the standard `http_req_*` samples for a completed request.
///
/// Mirrors the declarative runner's tag set and k6's default success
/// semantics: a request is failed unless its status is in 2xx–3xx. `sent`
/// is the request-body byte count (for `data_sent`).
#[allow(clippy::too_many_arguments)] // 10 fields mirror the k6 http_req_* tag/attr set
fn push_http_samples(
    sink: &Mutex<Vec<Sample>>,
    req: &Request,
    status_code: u16,
    duration: Duration,
    size: u64,
    sent: usize,
    timings: Option<&Timings>,
    scenario: &Arc<str>,
    extra_tags: Option<&HashMap<String, String>>,
    group: Option<&str>,
) {
    push_http_samples_for(
        sink,
        &req.url,
        req.method.as_str(),
        status_code,
        duration,
        size,
        sent,
        timings,
        scenario,
        extra_tags,
        group,
    );
}

/// Record http_req_* samples for EVERY redirect hop of a response (k6
/// parity: each hop is its own request — the test.k6.io 302 chain counted
/// 136 http_reqs for 68 iterations while Tropel recorded only the final
/// 64). Called BEFORE the final response's samples so hop order matches k6.
fn push_redirect_hops(
    sink: &Mutex<Vec<Sample>>,
    resp: &Response,
    method: &str,
    scenario: &Arc<str>,
    extra_tags: Option<&HashMap<String, String>>,
    group: Option<&str>,
) {
    for hop in &resp.redirects {
        push_http_samples_for(
            sink,
            &hop.url,
            method,
            hop.status_code,
            hop.response_time,
            hop.size,
            0, // redirect hops carry no request body
            hop.timings.as_ref(),
            scenario,
            extra_tags,
            group,
        );
    }
}

/// Backlog line 97: parse a tags JSON object into a [`TagMap`], coercing
/// non-string values to strings (k6's `check(res, conds, {code: 200})` —
/// k6 `ToString`s every tag value). The old `from_str::<HashMap<String,
/// String>>()` failed on `{"code":200}` and silently dropped the ENTIRE
/// tag map; this lenient parse never fails and never drops the map.
fn stringify_tag_map_into(j: &str, tags: &mut TagMap) {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(j) {
        for (k, val) in map {
            tags.insert(k, coerce_tag_value(&val));
        }
    }
}

/// Coerce a single JSON tag value to its k6 string form. Shared by
/// [`stringify_tag_map_into`] (check()/custom metrics) and
/// [`parse_k6_extras`] (HTTP request tags) so every path behaves
/// identically — W2 line 180: `parse_k6_extras` used `v.as_str()` with
/// `filter_map`, so `http.get(url, {tags: {code: 200}})` silently dropped
/// EVERY non-string tag (the canonical `{status: res.status}` idiom lost
/// the whole map), while `check()` already coerced. k6 `ToString`s every
/// tag value: numbers → digits, bools → true/false, null → "".
fn coerce_tag_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Implementation of [`push_http_samples`] with an explicit URL/method so
/// redirect hops (different URL, same method) reuse the same emitter.
#[allow(clippy::too_many_arguments)] // redirect-hop variant of push_http_samples (same field set + scenario)
fn push_http_samples_for(
    sink: &Mutex<Vec<Sample>>,
    url: &str,
    method: &str,
    status_code: u16,
    duration: Duration,
    size: u64,
    sent: usize,
    timings: Option<&Timings>,
    scenario: &Arc<str>,
    extra_tags: Option<&HashMap<String, String>>,
    group: Option<&str>,
) {
    let now = tropel_js::clock::monotonic_wall_now();
    let tags = Arc::new(http_tags_for(
        url,
        method,
        &status_code.to_string(),
        scenario,
        extra_tags,
        group,
    ));

    let is_failed = !(200..400).contains(&status_code);
    let mut v = sink.lock().unwrap();
    v.push(Sample {
        metric: "http_req_duration".into(),
        value: duration.as_secs_f64() * 1000.0,
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Trend,
    });
    // Backlog line 150: the k6 path now emits the same connection-phase
    // sub-timing samples as the declarative runner — real blocked/dns/
    // connecting/waiting/receiving (from reqwest's resolver + connector
    // hooks); tls_handshaking/sending stay 0 (folded into connecting /
    // waiting by reqwest, same as the declarative path). Emitted with the
    // same tags so thresholds like http_req_waiting:p(95) resolve.
    if let Some(t) = timings {
        let sub = [
            ("http_req_blocked", t.blocked),
            ("http_req_dns", t.dns),
            ("http_req_connecting", t.connecting),
            ("http_req_tls_handshaking", t.tls_handshaking),
            ("http_req_sending", t.sending),
            ("http_req_waiting", t.waiting),
            ("http_req_receiving", t.receiving),
        ];
        for (name, dur) in &sub {
            v.push(Sample {
                metric: (*name).into(),
                value: dur.as_secs_f64() * 1000.0,
                tags: tags.clone(),
                timestamp: now,
                sample_type: SampleType::Trend,
            });
        }
    }
    v.push(Sample {
        metric: "http_reqs".into(),
        value: 1.0,
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Counter,
    });
    v.push(Sample {
        metric: "http_req_failed".into(),
        value: if is_failed { 1.0 } else { 0.0 },
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Rate,
    });
    v.push(Sample {
        metric: "data_received".into(),
        value: size as f64,
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Counter,
    });
    v.push(Sample {
        metric: "data_sent".into(),
        value: sent as f64,
        tags,
        timestamp: now,
        sample_type: SampleType::Counter,
    });
}

/// k6 `params.compression`: gzip/deflate the request body (flate2). Returns
/// the compressed bytes, or `None` when the requested algorithm is unsupported
/// (k6 accepts "gzip", "deflate", or both comma-separated; anything else is
/// silently ignored — matching k6, which warns but proceeds uncompressed).
/// Only called when a body exists (the shim always sends one for POST etc.).
fn compress_k6_body(kind: &str, data: &[u8]) -> Option<Vec<u8>> {
    use flate2::write::{DeflateEncoder, GzEncoder};
    use flate2::Compression;
    use std::io::Write;
    if data.is_empty() {
        return None;
    }
    if kind.contains("gzip") {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).ok()?;
        enc.finish().ok()
    } else if kind.contains("deflate") {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).ok()?;
        enc.finish().ok()
    } else {
        None
    }
}

/// k6-style error codes for `res.error_code`. k6 defines a 1xxx series:
/// 1000 generic, 1010 non-TCP network error, 1020 malformed HTTP, 1050
/// timeout, 1100 DNS, 1200 TCP connect, 1300 TLS, 1600 HTTP/2, 1990
/// unknown. reqwest surfaces most failures as strings, so we map by
/// substring (best-effort, k6 parity for the common `if (res.error)` idiom
/// plus `res.error_code` for programmatic branching).
fn k6_error_code(msg: &str) -> i32 {
    let m = msg.to_ascii_lowercase();
    if m.contains("dns") || m.contains("nodename") || m.contains("resolve") {
        1100
    } else if m.contains("timed out") || m.contains("timeout") || m.contains("deadline") {
        1050
    } else if m.contains("tls") || m.contains("certificate") || m.contains("handshake") {
        1300
    } else if m.contains("http2") || m.contains("h2") {
        1600
    } else if m.contains("connect") || m.contains("refused") || m.contains("reset") {
        1200
    } else if m.contains("url") || m.contains("uri") {
        1010
    } else {
        1000
    }
}

/// Send a ws command to the session's writer task without silently dropping
/// frames. `try_send` + a short bounded retry: the writer task lives on the
/// separate I/O runtime (not the VU's reactor), so parking this VU thread for
/// a few ms never deadlocks it — while `blocking_send` would panic inside the
/// VU runtime (it calls `block_on`). Returns false if the session is gone.
fn try_send_cmd(tx: &tokio::sync::mpsc::Sender<WsCommand>, mut cmd: WsCommand) -> bool {
    for _ in 0..50 {
        match tx.try_send(cmd) {
            Ok(()) => return true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(c)) => {
                // Channel full — writer draining; park briefly and retry.
                cmd = c;
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
    false
}

/// Record samples for a request that failed at the transport level (timeout,
/// connection refused, …). Failed requests must still appear in the summary:
/// `http_reqs` increments and `http_req_failed` (Rate) becomes 1.0 — matching
/// the declarative runner's error branch and k6 semantics.
///
/// W1-B line 161: k6 also records the TIME-TO-FAILURE as an
/// `http_req_duration` (Trend) sample and the request body as `data_sent`.
/// The old code emitted neither, so a target that went fully down let
/// `p(95) < 500` PASS on the handful of pre-outage successes — the failure
/// durations simply never entered the distribution. The caller measures the
/// elapsed time-to-failure around the execute call and passes it here.
fn push_http_failure(
    sink: &Mutex<Vec<Sample>>,
    req: &Request,
    scenario: &Arc<str>,
    extra_tags: Option<&HashMap<String, String>>,
    group: Option<&str>,
    elapsed: Duration,
    sent: usize,
) {
    let now = tropel_js::clock::monotonic_wall_now();
    let tags = Arc::new(http_tags(req, "0", scenario, extra_tags, group));

    let mut v = sink.lock().unwrap();
    // Time-to-failure in ms (same Trend series the success path feeds), so
    // duration thresholds see the outage.
    v.push(Sample {
        metric: "http_req_duration".into(),
        value: elapsed.as_secs_f64() * 1000.0,
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Trend,
    });
    v.push(Sample {
        metric: "http_reqs".into(),
        value: 1.0,
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Counter,
    });
    v.push(Sample {
        metric: "http_req_failed".into(),
        value: 1.0,
        tags: tags.clone(),
        timestamp: now,
        sample_type: SampleType::Rate,
    });
    // Request-body bytes (same wire-size computation as the success path).
    v.push(Sample {
        metric: "data_sent".into(),
        value: sent as f64,
        tags,
        timestamp: now,
        sample_type: SampleType::Counter,
    });
}

#[async_trait]
impl DriverInstance for K6DriverInstance {
    fn set_force_stop_flag(&mut self, flag: Arc<AtomicBool>) {
        // Link the scheduler's flag DIRECTLY (no forwarding task). The engine
        // calls this once per VU before the first iteration; the native sleep
        // and the iteration pre-check poll `sched_link` themselves, so the
        // flag is observed even while the VU thread is blocked inside a native
        // `std::thread::sleep` — a tokio forwarder on the VU's thread-per-core
        // runtime would be starved there (backlog: gracefulStop force-stop was
        // advisory only).
        let _ = self.sched_link.set(flag.clone());
        // If the scheduler already requested force-stop, sync it into the
        // instance-local flag now so the JS interrupt handler trips immediately
        // (the handler only polls `force_stop`, not the link).
        if flag.load(Ordering::Acquire) {
            self.force_stop.store(true, Ordering::Release);
        }
    }

    async fn run_iteration(&mut self, ctx: &mut VuContext) -> Result<()> {
        // Force-stop: skip the eval so the VU loop observes the level-triggered
        // flag and exits promptly (backlog: gracefulStop force-stop was
        // advisory only). Poll the SCHEDULER's flag through the link too — not
        // just the instance-local copy — because the link is what a mid-run
        // force-stop flips (the JS interrupt + interruptible native sleep read
        // it during an eval).
        if self.force_stop.load(Ordering::Acquire)
            || self
                .sched_link
                .get()
                .is_some_and(|f| f.load(Ordering::Acquire))
        {
            return Ok(());
        }
        // Lazy-init: register the native HTTP bridge on the first iteration.
        // Lazy-init: register the native HTTP bridge on the first iteration.
        // The HttpClient is only available at runtime (from engine), not during
        // init(). The bridge calls the async HTTP client synchronously via the
        // shared `tropel_http::blocking::execute_blocking` helper — safe from
        // inside a current-thread VU runtime (no block_on, which would panic
        // or deadlock the VU's own reactor).
        if !self.http_bridge_registered {
            self.maybe_register_http_bridge(ctx).await;
        }
        if !self.script_bridges_registered {
            self.register_script_bridges();
        }
        if !self.ws_bridges_registered {
            self.register_ws_bridges();
        }

        // Backlog line 99: timer state must not leak across iterations.
        // __tropel_timers is module-scope in k6-shim.js; without a reset,
        // a setInterval armed in every iteration accumulates live intervals
        // that all fire on every subsequent pump (linear callback growth +
        // retained closures for the VU's life). Reset at the START of each
        // iteration so only timers armed during THIS iteration stay live —
        // they fire at this iteration's boundary pump, then are cleared.
        let _ = self.js_ctx.with_ctx(|rq_ctx| {
            if let Ok(reset) = rq_ctx
                .globals()
                .get::<_, rquickjs::Function>("__tropel_reset_timers")
            {
                let _ = reset.call::<_, rquickjs::Value>(());
            }
        });

        // Sync VuContext state into JS globals (__tropel_vu_id, etc.)
        self.sync_globals(ctx).await?;

        // Call __tropel_iteration() — the user's k6 script entry point.
        // Uses the cached `Persistent<Function>` fast path: the invocation
        // expression is compiled once (first iteration) and re-invoked from
        // the script cache on every subsequent iteration — no re-parsing.
        //
        // `return` is required: the cached wrapper is `(async function(){...})`
        // and only an explicit `return` makes the wrapper adopt the inner
        // promise. Without it, an async default export's promise would be
        // discarded (side effects still run via the job pump, but its
        // rejections would be swallowed).
        let iter_result = self
            .js_ctx
            .run_script_cached(
                "return __tropel_iteration(__tropel_setup)",
                Some("k6-iteration.js".to_string()),
            )
            .await;

        // k6/timers (backlog line 131): fire due setTimeout/setInterval
        // callbacks at the iteration boundary. k6 runs its timers on the VU
        // event loop; Tropel executes JS synchronously per iteration, so the
        // closest equivalent is pumping them here — which is exactly what
        // unblocks the lodash debounce/throttle shims. A throwing callback
        // is logged (not fatal), matching a best-effort event loop.
        //
        // Backlog line 100: the pump must run BEFORE the sample drain. Timer
        // callbacks record samples (and may test.abort()) through the same
        // bridge closures; draining first meant a callback's samples landed
        // after the drain — picked up next iteration, or silently DISCARDED
        // on the last one, and a timer's abort was delayed or lost.
        // NOTE: the pump also runs BEFORE the error return below so timers
        // armed in this iteration still drain even when the iteration itself
        // threw (strict event-loop behavior — timers are not hostage to a
        // throw).
        let _ = self.js_ctx.with_ctx(|rq_ctx| {
            if let Ok(pump) = rq_ctx
                .globals()
                .get::<_, rquickjs::Function>("__tropel_pump_timers")
            {
                if let Err(e) = pump.call::<_, rquickjs::Value>(()) {
                    tracing::warn!("k6 timer callback error: {}", e);
                }
            }
        });

        // Drain samples recorded by the native bridge closures during this
        // iteration (http_req_*, checks, custom metrics) into the VuContext
        // for the engine's metrics pipeline. k6 tags EVERY sample with the
        // active scenario name (this is what makes scenario-scoped thresholds
        // like `http_req_duration{scenario:api_load}` resolve) — stamp it on
        // the drained samples so they match k6 semantics.
        // drain(..) preserves the Vec's capacity — mem::take would replace
        // with Vec::new() and lose it, causing re-growth every iteration.
        let mut bridge_samples: Vec<_> = self.sample_sink.lock().unwrap().drain(..).collect();
        if !ctx.scenario_name.is_empty() {
            let scenario = ctx.scenario_name.clone();
            for s in &mut bridge_samples {
                // Check BEFORE make_mut: the http_req_* samples already carry
                // the scenario tag (stamped at tag-creation time in
                // http_tags_for), so make_mut never fires for them — no 4x
                // deep-clone of the map shared by the 5 per-request samples.
                // Only samples from other bridges (checks, custom metrics —
                // each an independent refcount-1 Arc) reach make_mut here.
                if s.tags.get("scenario").is_none() {
                    let tags = std::sync::Arc::make_mut(&mut s.tags);
                    tags.insert("scenario", scenario.clone());
                }
            }
        }
        ctx.samples.extend(bridge_samples);

        // Surface test.abort() to the engine so the run stops cleanly.
        // Runs AFTER the pump (line 100: a timer callback's test.abort()
        // must reach the engine this iteration, not next).
        if let Some(msg) = std::mem::take(&mut *self.abort_requested.lock().unwrap()) {
            ctx.abort(Some(msg));
        }

        // A rejected/thrown default export must fail the iteration (the
        // engine logs it and bumps the error path), not be swallowed.
        if let Err(e) = iter_result {
            tracing::warn!("k6 iteration error: {}", e);
            return Err(tropel_sdk::TropelError::Other(format!(
                "k6 iteration failed: {}",
                e
            )));
        }

        // NOTE: `iteration_duration` is NOT emitted here — the shared VU loop
        // (vu_loop.rs) already emits it as a Trend for every iteration. A
        // duplicate emit here was typed Point, so MetricSet took its type from
        // the first sample (Gauge-like) and the stock k6 threshold
        // `iteration_duration: ['p(95)<2000']` compared against 0 → always
        // PASS. The shared Trend emit is the single source of truth.
        Ok(())
    }
}

/// Build a native JS response object for the k6 shim, avoiding the old
/// Rust → escaped-JSON-string → JS `JSON.parse` round trip (3-4 full body
/// copies: `from_utf8`, `json!`, `.to_string()`, `JSON.parse`, then another
/// `res.json()` parse). `Object::new` + `set` materialize the envelope
/// directly in the JS heap; the body becomes a single JS string. The shim
/// treats a native object and the legacy JSON string identically.
///
/// Backlog line 150: the object now carries REAL `timings` (blocked/dns/
/// connecting/tls_handshaking/sending/waiting/receiving/duration in ms, from
/// the reqwest resolver+connector hooks), k6's `error` ("" on success), and
/// `error_code` (0 on success, 1xxx series on transport failure). For
/// `responseType: "binary"` the body is a native JS ArrayBuffer instead of a
/// UTF-8 string (binary payloads are no longer silently destroyed).
///
/// W2 line 189: the PRODUCTION degradation path is [`k6_error_envelope`]
/// (set_memory_limit is QuickJS's HARD limit — the pre-guard fires before
/// any allocation, so the envelope is built while headroom still exists).
#[allow(clippy::too_many_arguments)] // response object fields mirror k6's Response shape
fn build_k6_response_object<'js>(
    ctx: &rquickjs::Ctx<'js>,
    code: u16,
    status_text: String,
    body: Vec<u8>,
    headers: &HashMap<String, String>,
    response_time_ms: f64,
    timings: Option<&Timings>,
    error: &str,
    error_code: i32,
    response_type: &str,
    cookies: &[Cookie],
) -> rquickjs::Result<rquickjs::Object<'js>> {
    // Backlog line 46 (P0): allocation failures must NOT `.expect()`-panic
    // across the QuickJS FFI boundary — a server-controlled binary body can
    // exceed the per-VU heap cap. Small envelope allocations use `?` (the
    // rquickjs Func converts Err into a script-thrown exception); the body
    // ArrayBuffer pre-checks the cap and degrades to the status-0 error
    // response (same shape as the invalid-method path) while the heap still
    // has headroom.
    let obj = rquickjs::Object::new(ctx.clone())?;
    let _ = obj.set("code", code as i32);
    let _ = obj.set("status", code as i32);
    let _ = obj.set("status_text", status_text);
    let _ = obj.set("error", error);
    let _ = obj.set("error_code", error_code);
    let headers_obj = rquickjs::Object::new(ctx.clone())?;
    for (k, v) in headers {
        let _ = headers_obj.set(k.as_str(), v.as_str());
    }
    let _ = obj.set("headers", headers_obj);
    let _ = obj.set("response_time", response_time_ms);
    // Backlog line 102: res.cookies was absent (res.cookies['sid'] threw).
    // k6 shape: { name: [{name, value, domain, path, httpOnly, secure,
    // maxAge, expires, sameSite}] } — an object keyed by cookie name with
    // an ARRAY of cookie objects per name.
    let cookies_obj = rquickjs::Object::new(ctx.clone())?;
    for c in cookies {
        let entry = rquickjs::Object::new(ctx.clone())?;
        let _ = entry.set("name", c.name.as_str());
        let _ = entry.set("value", c.value.as_str());
        if let Some(d) = &c.domain {
            let _ = entry.set("domain", d.as_str());
        }
        if let Some(p) = &c.path {
            let _ = entry.set("path", p.as_str());
        }
        if let Some(v) = c.http_only {
            let _ = entry.set("httpOnly", v);
        }
        if let Some(v) = c.secure {
            let _ = entry.set("secure", v);
        }
        if let Some(e) = &c.expires {
            let _ = entry.set("expires", e.as_str());
        }
        if let Some(s) = &c.same_site {
            let _ = entry.set("sameSite", s.as_str());
        }
        match cookies_obj.get::<_, rquickjs::Value>(c.name.as_str()) {
            Ok(v) if v.is_array() => {
                let _ = v.as_array().map(|a| {
                    let idx = a.len();
                    let _ = a.set(idx, entry);
                });
            }
            _ => {
                let arr = rquickjs::Array::new(ctx.clone())?;
                let _ = arr.set(0, entry);
                let _ = cookies_obj.set(c.name.as_str(), arr);
            }
        }
    }
    let _ = obj.set("cookies", cookies_obj);
    // Real connection-phase timings (k6 keys + Tropel's extra `dns`).
    if let Some(t) = timings {
        let timings_obj = rquickjs::Object::new(ctx.clone())?;
        let _ = timings_obj.set("blocked", t.blocked.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("dns", t.dns.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("connecting", t.connecting.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("tls_handshaking", t.tls_handshaking.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("sending", t.sending.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("waiting", t.waiting.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("receiving", t.receiving.as_secs_f64() * 1000.0);
        let _ = timings_obj.set("duration", response_time_ms);
        let _ = obj.set("timings", timings_obj);
    }
    // `responseType: "binary"` → native ArrayBuffer body (raw bytes survive);
    // otherwise UTF-8 text. `none` (k6) sends an empty body.
    // NOTE (metrics-vs-script divergence): the caller has ALREADY pushed the
    // real http_req_* samples (the request completed on the wire) before this
    // object is built, so degrading to a status-0 envelope here is a JS
    // REPRESENTATION fallback only — the metrics keep the true status, and
    // the envelope is the script-visible failure signal. Do NOT "fix" this
    // by also pushing failure samples (double-counted failures).
    // W1-B line 160: a body at/over the WHOLE heap cap is guaranteed OOM
    // (set_memory_limit is QuickJS's HARD limit) — degrade to the status-0
    // envelope BEFORE attempting any allocation, so it can be built while
    // headroom still exists. This guards BOTH the binary branch (ArrayBuffer)
    // and the text branch (String::from_utf8_lossy): the old code only
    // guarded the former, so an oversized TEXT body silently became
    // `status:200, body:''` — `let _ =` swallowed the OOM and JSON.parse
    // threw with no indication.
    if body.len() >= K6_VU_HEAP_BYTES {
        return match k6_error_envelope(
            ctx,
            "response body exceeds the per-VU JS heap cap",
            response_type.eq_ignore_ascii_case("binary"),
        ) {
            Some(e) => Ok(e),
            None => Err(rquickjs::Error::Exception),
        };
    }
    if response_type.eq_ignore_ascii_case("binary") {
        // In-between sizes (under the cap) try the ArrayBuffer and fall back
        // to the envelope on failure.
        match rquickjs::ArrayBuffer::new(ctx.clone(), body) {
            Ok(ab) => {
                let _ = obj.set("body", ab);
            }
            Err(_) => {
                tracing::warn!(
                    "k6 binary body allocation failed (heap cap) — status-0 error response"
                );
                return match k6_error_envelope(
                    ctx,
                    "response body exceeds the per-VU JS heap cap",
                    true, // binary branch — ArrayBuffer body contract
                ) {
                    Some(e) => Ok(e),
                    None => Err(rquickjs::Error::Exception),
                };
            }
        }
    } else {
        let _ = obj.set("body", String::from_utf8_lossy(&body).into_owned());
    }
    Ok(obj)
}

impl K6DriverInstance {
    /// Lazily register the `__tropel_k6_http_request` native function.
    /// This function wraps the per-VU HttpClient so the k6 shim can
    /// execute HTTP requests synchronously from JS.
    async fn maybe_register_http_bridge(&mut self, ctx: &VuContext) {
        let http_client = match ctx.http_client.clone() {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "K6Driver: http_client not available on first iteration — k6 http.* will fail"
                );
                self.http_bridge_registered = true; // Don't retry
                return;
            }
        };

        let sink = self.sample_sink.clone();
        // The bridge closures must name the QuickJS lifetime `'js` in their
        // signatures (to return a native `Object<'js>`), which a closure
        // cannot declare — the registration lives in the generic free fn
        // below. Behavior is identical; only the marshalling changes.
        let (deadline, max_exec) = self.js_ctx.interrupt_deadline_handle();
        self.js_ctx.with_ctx(|rq_ctx| {
            register_http_bridges(
                rq_ctx,
                http_client.clone(),
                sink.clone(),
                &ctx.scenario_name,
                self.group_stack.clone(),
                deadline,
                max_exec,
            );
        });

        self.http_bridge_registered = true;
        tracing::debug!("K6Driver: registered __tropel_k6_http_request native bridge");
    }
}

/// Register the native HTTP bridges (`__tropel_k6_http_request` /
/// `__tropel_k6_http_batch`) on a per-VU QuickJS context. Lives in a generic
/// fn — not an inline closure — because the request bridge's closure must
/// name the QuickJS lifetime `'js` in its signature to return a native
/// `Object<'js>` (eliminating the escaped-JSON-string round trip), and
/// closures cannot declare lifetimes.
/// Canonical per-request k6 params — ONE wire shape shared by both HTTP
/// bridges (W2 line 169). The single and batch bridges used FOUR different
/// field names (timeoutMs/timeout_ms, tags/tags_json, auth/auth_json,
/// bodyB64/body_b64) and parsed them differently — the batch even dropped
/// the whole tag map when any value was a non-string.
struct K6RequestExtras {
    timeout_ms: f64,
    tags: HashMap<String, String>,
    auth: Option<AuthConfig>,
    redirects: i64,
    compression: String,
    body_b64: bool,
}

fn parse_k6_extras(extras: &serde_json::Value) -> K6RequestExtras {
    K6RequestExtras {
        timeout_ms: extras
            .get("timeoutMs")
            .and_then(|t| t.as_f64())
            .unwrap_or(0.0),
        // W2 line 180: the HTTP paths must coerce non-string tag values the
        // same way check()/custom metrics do — the old v.as_str() filter
        // silently dropped {code: 200} / {status: res.status} maps.
        tags: extras
            .get("tags")
            .and_then(|t| t.as_object())
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), coerce_tag_value(v)))
                    .collect()
            })
            .unwrap_or_default(),
        auth: extras
            .get("auth")
            .filter(|a| !a.is_null())
            .and_then(|a| serde_json::from_value(a.clone()).ok()),
        redirects: extras
            .get("redirects")
            .and_then(|r| r.as_i64())
            .unwrap_or(-1),
        compression: extras
            .get("compression")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        body_b64: extras
            .get("bodyB64")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
    }
}

/// ONE request builder for both HTTP bridges: headers parse, binary/
/// compressed body decode, timeout, auth, redirects, response_type. Returns
/// the Request, the parsed extras (the caller needs params.tags for sample
/// emission), and — when the method token is invalid — the error message
/// (the caller decides how to surface it: single → status-0 envelope, batch
/// → Err in the future).
fn build_k6_request(
    method: String,
    url: String,
    headers_json: String,
    body: String,
    response_type: String,
    extras: &serde_json::Value,
) -> (Request, K6RequestExtras, Option<String>) {
    let params = parse_k6_extras(extras);
    let parsed = Method::parse(&method);
    let method_error = parsed
        .is_none()
        .then(|| format!("invalid HTTP method {}", method.clone()));
    let mut headers = parse_headers_tolerant(&headers_json);
    let mut req_body: Option<Body> = if body.is_empty() {
        None
    } else if params.body_b64 {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(&body)
            .ok()
            .map(Body::Binary)
    } else {
        Some(Body::Raw(body))
    };
    if !params.compression.is_empty() {
        if let Some(Body::Raw(text)) = &req_body {
            if let Some(compressed) = compress_k6_body(&params.compression, text.as_bytes()) {
                req_body = Some(Body::Binary(compressed));
                headers.push((
                    "Content-Encoding".to_string(),
                    if params.compression.contains("gzip") {
                        "gzip".to_string()
                    } else {
                        "deflate".to_string()
                    },
                ));
            }
        }
    }
    let req = Request {
        url,
        method: parsed.unwrap_or_else(|| Method::Custom(method.clone())),
        headers,
        query_params: HashMap::new(),
        body: req_body,
        auth: params.auth.clone(),
        certificate: None,
        follow_redirects: params.redirects != 0,
        timeout: if params.timeout_ms > 0.0 {
            Some(Duration::from_millis(params.timeout_ms as u64))
        } else {
            None
        },
        response_type: tropel_sdk::ResponseType::from_k6(&response_type),
    };
    (req, params, method_error)
}

fn register_http_bridges<'js>(
    rq_ctx: &rquickjs::Ctx<'js>,
    http_client: Arc<dyn DriverHttpClient + Send + Sync>,
    sink: Arc<Mutex<Vec<Sample>>>,
    scenario: &str,
    group_stack: Arc<Mutex<Vec<String>>>,
    deadline: Arc<AtomicU64>,
    max_exec: Duration,
) {
    let globals = rq_ctx.globals();
    let http_client_request = http_client.clone();
    let sink_req = sink.clone();
    // The scenario name is constant per VU — capture it once here (as an
    // Arc<str> so http tag builders stamp it at CREATION time via a refcount
    // bump, zero per-request alloc) so the drain then skips these samples
    // entirely: no Arc::make_mut, no 4x deep-clone of the map shared by the
    // 5 per-request samples.
    let scenario_req = Arc::from(scenario);
    // Backlog line 154: current group() path (::a::b) read from the per-VU
    // group stack at REQUEST time, so http_req_* samples recorded inside a
    // group carry `group=::path` instead of the hardcoded `group=http`.
    let group_stack_req = group_stack.clone();
    // The blocking HTTP call burns wall time; re-arm the per-eval JS
    // deadline after it (backlog line 104).
    let deadline_req = deadline.clone();
    let max_exec_req = max_exec;
    let _ = globals.set(
        "__tropel_k6_http_request",
        Func::from(
            move |ctx: rquickjs::Ctx<'js>,
                  method: String,
                  url: String,
                  headers_json: String,
                  body: String,
                  response_type: String,
                  extras_json: String|
                  -> rquickjs::Result<rquickjs::Object<'js>> {
                // Backlog line 140: the shim packs per-request
                // params.timeout/tags/auth/redirects/compression into
                // ONE JSON string (the bridge closure is arity-capped
                // at ctx + 6 script args). Unpack them here and apply
                // each: timeout bounds the request, tags merge into
                // the http_req_* sample tags, auth becomes
                // Request.auth (the client impl builds the signer),
                // redirects controls follow_redirects, and compression
                // gzip/deflates the request body.
                // W2 line 169: ONE canonical request builder shared with the
                // batch bridge — the two used four different wire fields
                // (timeoutMs/timeout_ms, tags/tags_json, auth/auth_json,
                // bodyB64/body_b64), and the batch even dropped the whole
                // tag map on any non-string value. Both now parse the same
                // shape.
                let extras: serde_json::Value =
                    serde_json::from_str(&extras_json).unwrap_or(serde_json::Value::Null);
                let (req, params, method_error) = build_k6_request(
                    method,
                    url,
                    headers_json,
                    body,
                    response_type.clone(),
                    &extras,
                );
                // A genuinely invalid method token must not silently
                // become GET (a write-path "PURGE" must not degrade
                // into a read-path GET that reports green). Surfaced
                // as a status-0 error response the shim returns to
                // the script (checks fail, http_req_failed counts).
                if let Some(msg) = &method_error {
                    return build_k6_response_object(
                        &ctx,
                        0,
                        msg.clone(),
                        Vec::new(),
                        &HashMap::new(),
                        0.0,
                        None,
                        msg,
                        1000,
                        &response_type,
                        &[],
                    );
                }
                let extra_tags = params.tags;
                // Execute on the dedicated I/O runtime via the shared
                // blocking helper — safe from inside ctx.with on a
                // current-thread VU runtime. No block_on here: that
                // deadlocks the VU's own reactor.
                // Clone the request into the 'static I/O future; the
                // original stays alive for sample-tag construction.
                let req_for_io = req.clone();
                let http_for_io = http_client_request.clone();
                // W1-B line 161: the failure path needs a start instant so
                // http_req_duration can record the time-to-failure (k6
                // records it; the success path reads resp.response_time).
                let start = std::time::Instant::now();
                let result = tropel_http::blocking::execute_blocking(async move {
                    http_for_io.execute(&req_for_io).await
                });
                // The HTTP call burned wall time; re-arm the per-eval JS
                // deadline so slow requests don't trip the interrupt on
                // resume (backlog line 104).
                tropel_js::rearm_deadline(&deadline_req, max_exec_req);
                match result {
                    Ok(resp) => {
                        // Record the standard http_req_* samples so
                        // the summary/thresholds see this request
                        // (mirrors the declarative runner + WASM
                        // driver). The req body is counted for
                        // data_sent; k6 semantics: 2xx-3xx success.
                        // Exact wire size via the SINGLE serializer
                        // (percent-encoded urlencoded, multipart
                        // framing) — the deleted Body::encoded_len
                        // measured raw k=v&k=v with no encoding.
                        let sent = req.body.as_ref().map(tropel_http::body_size).unwrap_or(0);
                        // k6 parity: every redirect hop counts as its
                        // own request (test.k6.io 302 chain = 2 reqs
                        // per iteration, not 1).
                        let group = group_stack_req.lock().unwrap().last().cloned();
                        push_redirect_hops(
                            &sink_req,
                            &resp,
                            req.method.as_str(),
                            &scenario_req,
                            Some(&extra_tags),
                            group.as_deref(),
                        );
                        push_http_samples(
                            &sink_req,
                            &req,
                            resp.status_code,
                            resp.response_time,
                            resp.size,
                            sent,
                            resp.timings.as_ref(),
                            &scenario_req,
                            Some(&extra_tags),
                            group.as_deref(),
                        );
                        build_k6_response_object(
                            &ctx,
                            resp.status_code,
                            resp.status_text,
                            resp.body,
                            &resp.headers,
                            resp.response_time.as_secs_f64() * 1000.0,
                            resp.timings.as_ref(),
                            "",
                            0,
                            &response_type,
                            &resp.cookies,
                        )
                    }
                    Err(e) => {
                        tracing::debug!("k6 http request failed: {}", e);
                        let group = group_stack_req.lock().unwrap().last().cloned();
                        // Same wire-size computation as the success path.
                        let sent = req.body.as_ref().map(tropel_http::body_size).unwrap_or(0);
                        push_http_failure(
                            &sink_req,
                            &req,
                            &scenario_req,
                            Some(&extra_tags),
                            group.as_deref(),
                            start.elapsed(),
                            sent,
                        );
                        let err = e.to_string();
                        build_k6_response_object(
                            &ctx,
                            0,
                            format!("HTTP error: {}", err),
                            Vec::new(),
                            &HashMap::new(),
                            0.0,
                            None,
                            &err,
                            k6_error_code(&err),
                            &response_type,
                            &[],
                        )
                    }
                }
            },
        ),
    );

    let batch_sink = sink.clone();
    let scenario_batch = Arc::from(scenario);
    let group_stack_batch = group_stack.clone();
    let deadline_batch = deadline.clone();
    let max_exec_batch = max_exec;
    let _ = globals.set(
        "__tropel_k6_http_batch",
        Func::from(
            move |ctx: rquickjs::Ctx<'js>,
                  requests_json: String|
                  -> rquickjs::Result<rquickjs::Object<'js>> {
                let batch_requests: Vec<serde_json::Value> =
                    serde_json::from_str(&requests_json).unwrap_or_default();

                let http_for_io = http_client.clone();
                let futures = batch_requests.into_iter().map(move |entry| {
                    let key = entry
                        .get("key")
                        .cloned()
                        .unwrap_or_else(|| serde_json::Value::String(String::new()));
                    let method = entry
                        .get("method")
                        .and_then(|v| v.as_str())
                        .unwrap_or("GET")
                        .to_string();
                    let url = entry
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let headers_json = entry
                        .get("headers_json")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let body = entry
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let response_type = entry
                        .get("response_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text")
                        .to_string();
                    // W2 line 169: ONE canonical extras shape shared with the
                    // single-request bridge (timeoutMs/tags/auth/redirects/
                    // compression/bodyB64) — the old tags_json/auth_json/
                    // body_b64/timeout_ms variants disagreed on four of seven
                    // wire fields (and the batch dropped the whole tag map on
                    // any non-string value).
                    let extras: serde_json::Value = entry
                        .get("extras")
                        .and_then(|v| v.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    // Same loud-failure guard as the single bridge: an invalid
                    // method token must not silently become GET. Carried as an
                    // immediate Err result inside the async block (never
                    // executed) — the caller maps it to a status-0 response +
                    // failure samples.
                    let (req, params, method_error) = build_k6_request(
                        method,
                        url,
                        headers_json.to_string(),
                        body,
                        response_type.clone(),
                        &extras,
                    );
                    let http_client = http_for_io.clone();
                    async move {
                        let start = std::time::Instant::now();
                        let resp = match &method_error {
                            Some(msg) => Err(TropelError::Other(msg.clone())),
                            None => http_client.execute(&req).await,
                        };
                        (key, req, resp, params.tags, start, response_type)
                    }
                });

                let responses = tropel_http::blocking::execute_blocking(async move {
                    let results = join_all(futures).await;
                    Ok(results)
                });
                // The batch burned wall time; re-arm the per-eval JS deadline
                // (backlog line 104).
                tropel_js::rearm_deadline(&deadline_batch, max_exec_batch);

                // W2 line 169: the batch bridge returns a NATIVE object (like the
                // single bridge) instead of a JSON string — this kills the shim's
                // escaped-JSON round trip, the largest per-request cost in the
                // crate. Responses reuse build_k6_response_object, so both
                // bridges produce IDENTICAL wire shapes (real ArrayBuffer bodies
                // for binary, timings, cookies, error_code).
                let resp_obj = rquickjs::Object::new(ctx.clone())?;
                let mut seen_keys = std::collections::HashSet::new();
                if let Ok(results) = responses {
                    for (key, req, result, extra_tags, start, response_type) in results {
                        let key_str = match key {
                            serde_json::Value::String(s) => s,
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            other => serde_json::to_string(&other).unwrap_or_default(),
                        };
                        // W2 line 169: response_map.insert was last-write-wins —
                        // a duplicate key silently dropped the earlier real
                        // response. The shim now dedupes round-trip keys, so a
                        // duplicate here is a contract violation — fail loudly
                        // like the shim's missing-key guard.
                        if !seen_keys.insert(key_str.clone()) {
                            tracing::error!("k6 http.batch: duplicate response key {:?}", key_str);
                            return Err(rquickjs::Error::Exception);
                        }
                        let entry_resp = match result {
                            Ok(resp) => {
                                // Record the standard http_req_* samples
                                // for each batch request (mirrors the
                                // single-request bridge).
                                // Exact wire size via the SINGLE
                                // serializer (see single-request path).
                                let sent =
                                    req.body.as_ref().map(tropel_http::body_size).unwrap_or(0);
                                // k6 parity: every redirect hop counts as
                                // its own request, same as the single-
                                // request path.
                                let group = group_stack_batch.lock().unwrap().last().cloned();
                                push_redirect_hops(
                                    &batch_sink,
                                    &resp,
                                    req.method.as_str(),
                                    &scenario_batch,
                                    Some(&extra_tags),
                                    group.as_deref(),
                                );
                                push_http_samples(
                                    &batch_sink,
                                    &req,
                                    resp.status_code,
                                    resp.response_time,
                                    resp.size,
                                    sent,
                                    resp.timings.as_ref(),
                                    &scenario_batch,
                                    Some(&extra_tags),
                                    group.as_deref(),
                                );
                                // Same native response builder as the single
                                // bridge — binary bodies become real
                                // ArrayBuffers (no base64/body_b64 detour) and
                                // timings/cookies/error_code serialize
                                // identically.
                                build_k6_response_object(
                                    &ctx,
                                    resp.status_code,
                                    resp.status_text,
                                    resp.body,
                                    &resp.headers,
                                    resp.response_time.as_secs_f64() * 1000.0,
                                    resp.timings.as_ref(),
                                    "",
                                    0,
                                    &response_type,
                                    &resp.cookies,
                                )?
                            }
                            Err(e) => {
                                tracing::debug!("k6 batch request failed: {}", e);
                                let group = group_stack_batch.lock().unwrap().last().cloned();
                                let sent =
                                    req.body.as_ref().map(tropel_http::body_size).unwrap_or(0);
                                push_http_failure(
                                    &batch_sink,
                                    &req,
                                    &scenario_batch,
                                    Some(&extra_tags),
                                    group.as_deref(),
                                    start.elapsed(),
                                    sent,
                                );
                                let err = e.to_string();
                                build_k6_response_object(
                                    &ctx,
                                    0,
                                    format!("HTTP error: {}", err),
                                    Vec::new(),
                                    &HashMap::new(),
                                    0.0,
                                    None,
                                    &err,
                                    k6_error_code(&err),
                                    &response_type,
                                    &[],
                                )?
                            }
                        };
                        let _ = resp_obj.set(key_str.as_str(), entry_resp);
                    }
                }
                Ok(resp_obj)
            },
        ),
    );
}

impl K6DriverInstance {
    /// Lazily register the script-state bridges (`__tropel_pm_test`,
    /// `__tropel_pm_custom_metric_add`, `__tropel_exec_*`, `__tropel_test_abort`).
    /// The k6 driver doesn't depend on tropel-sandbox (which installs these for the
    /// declarative path), so it installs its own equivalents backed by the
    /// per-VU sample_sink / exec_state / abort flag.
    fn register_script_bridges(&mut self) {
        let sink = self.sample_sink.clone();
        let exec_state = self.exec_state.clone();
        let abort = self.abort_requested.clone();

        self.js_ctx.with_ctx(|rq_ctx| {
            let globals = rq_ctx.globals();

            // check() / pm.test() → checks Rate sample
            let sink_test = sink.clone();
            let group_test = self.group_stack.clone();
            let _ = globals.set(
                "__tropel_pm_test",
                // 3rd arg: optional k6 check() tags JSON (backlog line 149).
                Func::from(
                    move |name: String, passed: bool, tags_json: Option<String>| {
                        let mut v = sink_test.lock().unwrap();
                        let now = tropel_js::clock::monotonic_wall_now();
                        let mut tags = TagMap::with_capacity(3);
                        tags.insert("check", name);
                        // Backlog line 63: checks carry the current group
                        // path (k6 parity) — nested group() stamps
                        // group=::checkout::payment on the checks sample too.
                        if let Some(g) = group_test.lock().unwrap().last() {
                            tags.insert("group", g.clone());
                        }
                        if let Some(j) = tags_json {
                            // Backlog line 97: tag values may be non-strings
                            // (check(r, {...}, {code: 200}) — k6 coerces every
                            // tag value to a string). from_str::<HashMap<
                            // String,String>>() failed on {"code":200} and
                            // dropped the ENTIRE tag map; parse the object
                            // and stringify each value instead.
                            stringify_tag_map_into(&j, &mut tags);
                        }
                        v.push(Sample {
                            metric: "checks".into(),
                            value: if passed { 1.0 } else { 0.0 },
                            tags: Arc::new(tags),
                            timestamp: now,
                            sample_type: SampleType::Rate,
                        });
                    },
                ),
            );

            // Custom metric .add() → typed sample (Counter/Gauge/Rate/Trend).
            // Backlog line 154: the 5th arg carries k6's `isTime` flag — when
            // true the metric name is registered in the tropel-metrics time
            // registry so json-stream stamps `contains: "time"` and stdout
            // renders it in ms (k6's `new Trend('x', true)` behavior).
            let sink_metric = sink.clone();
            // W2 line 188: custom metrics recorded inside a group() must
            // carry the full ::a::b path like checks (were untagged).
            let group_metric = self.group_stack.clone();
            let _ = globals.set(
                "__tropel_pm_custom_metric_add",
                Func::from(
                    move |name: String,
                          value: f64,
                          tags_json: String,
                          metric_type_str: String,
                          is_time: bool| {
                        // W1-B line 159: refuse non-finite values on the
                        // PRIMARY path. The wasm driver guards at the emitter
                        // (crates/tropel-wasm/src/driver.rs) with the same
                        // rule; the k6 bridge takes `value: f64` straight from
                        // JS, so `myTrend.add(parseFloat(missingHeader))` →
                        // NaN poisoned `sum` forever (avg=NaN → `avg < 500`
                        // false forever) while `f64::NAN.max(0.0) == 0.0`
                        // silently recorded a phantom 0 in the histogram.
                        // Drop the sample — count and population stay
                        // consistent and no aggregate/threshold can be
                        // poisoned.
                        if !value.is_finite() {
                            return;
                        }
                        if is_time {
                            tropel_metrics::time_metrics::register(
                                &name,
                                tropel_metrics::MetricUnit::Time,
                            );
                        }
                        let mut tags = TagMap::new();
                        if !tags_json.is_empty() && tags_json != "{}" {
                            // Backlog line 97: same lenient parse as check() —
                            // metric.add(1, {code: 200}) must not drop the map.
                            stringify_tag_map_into(&tags_json, &mut tags);
                        }
                        // W2 line 188: stamp the active group path unless the
                        // script supplied its own `group` tag (user wins).
                        if let Some(g) = group_metric.lock().unwrap().last() {
                            if tags.get("group").is_none() {
                                tags.insert("group", g.clone());
                            }
                        }
                        let sample_type = match metric_type_str.as_str() {
                            "counter" => SampleType::Counter,
                            "gauge" => SampleType::Point,
                            "rate" => SampleType::Rate,
                            _ => SampleType::Trend,
                        };
                        let mut v = sink_metric.lock().unwrap();
                        v.push(Sample {
                            metric: name.into(),
                            value,
                            tags: Arc::new(tags),
                            timestamp: tropel_js::clock::monotonic_wall_now(),
                            sample_type,
                        });
                    },
                ),
            );

            // exec.scenario.name / executor (value properties — line 141)
            let es_name = exec_state.clone();
            let _ = globals.set(
                "__tropel_exec_scenario_name",
                Func::from(move || es_name.lock().unwrap().scenario_name.clone()),
            );
            let es_executor = exec_state.clone();
            let _ = globals.set(
                "__tropel_exec_scenario_executor",
                Func::from(move || es_executor.lock().unwrap().executor_name.clone()),
            );

            // exec.vu.idInTest() / iterationInScenario()
            let es_vu = exec_state.clone();
            let _ = globals.set(
                "__tropel_exec_vu_id",
                Func::from(move || es_vu.lock().unwrap().vu_id + 1),
            );
            let es_iter = exec_state.clone();
            let _ = globals.set(
                "__tropel_exec_iteration",
                Func::from(move || es_iter.lock().unwrap().iteration),
            );

            // exec.instance.iterationsCompleted() / vusActive()
            let es_completed = exec_state.clone();
            let _ = globals.set(
                "__tropel_exec_iterations_completed",
                Func::from(move || es_completed.lock().unwrap().iterations_completed),
            );
            let es_vus = exec_state.clone();
            let _ = globals.set(
                "__tropel_exec_vus_active",
                Func::from(move || es_vus.lock().unwrap().vus_active),
            );

            // group() → group_duration Trend sample (duration in ms). The
            // shim's group() wraps fn() between __tropel_pm_group_start/end.
            // Backlog line 154: the START bridge pushes the name onto the
            // per-VU group stack (http bridges read the top when stamping
            // http_req_* tags), and END pops it — the two must balance or
            // the stack leaks across iterations. The group_duration sample
            // keeps its k6-style `group=<name>` tag.
            let sink_group = sink.clone();
            let group_start = self.group_stack.clone();
            let _ = globals.set(
                "__tropel_pm_group_start",
                Func::from(move |name: String| {
                    // Backlog line 63: push the FULL ::a::b path, not the bare
                    // leaf — every consumer reads group_stack.last() and k6
                    // tags nested groups group=::checkout::payment. Two
                    // same-named leaves under different parents must NOT merge
                    // into one series.
                    let mut s = group_start.lock().unwrap();
                    let full = match s.last() {
                        Some(parent) => format!("{}::{}", parent, name),
                        None => format!("::{}", name),
                    };
                    s.push(full);
                }),
            );
            let group_end = self.group_stack.clone();
            let _ = globals.set(
                "__tropel_pm_group_end",
                Func::from(move |name: String, duration_ms: f64| {
                    // Backlog line 63: pop the FULL ::a::b path (start pushed
                    // it) and tag group_duration with it — k6 tags nested
                    // groups group=::checkout::payment, not the bare leaf.
                    let full = group_end
                        .lock()
                        .unwrap()
                        .pop()
                        .unwrap_or_else(|| format!("::{}", name));
                    // Defensive: compare the LEAF (the stack stores full
                    // ::a::b paths); a mismatched script drops the stale top
                    // without losing the whole stack.
                    let leaf = full.rsplit("::").next().unwrap_or(&full);
                    if leaf != name {
                        tracing::debug!("k6 group_end mismatch: top={} got={}", full, name);
                    }
                    let mut v = sink_group.lock().unwrap();
                    let mut tags = TagMap::with_capacity(1);
                    tags.insert("group", full);
                    v.push(Sample {
                        metric: "group_duration".into(),
                        value: duration_ms, // ms — the public unit (k6 semantics)
                        tags: Arc::new(tags),
                        timestamp: tropel_js::clock::monotonic_wall_now(),
                        sample_type: SampleType::Trend,
                    });
                }),
            );

            // test.abort(msg)
            let abort_flag = abort.clone();
            let _ = globals.set(
                "__tropel_test_abort",
                Func::from(move |msg: String| {
                    *abort_flag.lock().unwrap() = Some(msg);
                }),
            );
        });

        self.script_bridges_registered = true;
        tracing::debug!("K6Driver: registered script-state bridges");
    }

    /// Lazily register the WebSocket bridges backing the k6-shim's `ws.*`
    /// event-driven API (`ws.connect` + `socket.on('open'|'message'|'close')`).
    ///
    /// Bridge contract (see `js/k6-shim/k6-shim.js`):
    /// - `__tropel_k6_ws_connect(url, headers_json) -> {id, error}` opens the
    ///   connection (blocking, on the dedicated I/O runtime) and returns a
    ///   session id.
    /// - `__tropel_k6_ws_step(id, timeout_ms) -> {type, data?, code?, reason?}`
    ///   blocks up to timeout_ms for the next event (open/message/close/error/
    ///   ping/pong). Each call also resets the per-eval interrupt deadline so a
    ///   long-lived session isn't killed by the eval timeout.
    /// - `__tropel_k6_ws_send(id, data)` / `_ping(id)` / `_close(id, code,
    ///   reason)` forward frames to the background writer task.
    /// - `__tropel_k6_ws_finish(id)` tears the session down and emits its
    ///   `ws_*` samples into the sample_sink (same metric names as the
    ///   declarative WebSocket protocol extension).
    // `#[allow]`: WsSession (with its std::sync::mpsc::Receiver) is !Sync,
    // so Arc::new(WsSession { .. }) is not Send+Sync — but the session
    // registry is confined to this VU's own thread (thread-per-core; see
    // the unsafe impl Send/Sync for K6DriverInstance below).
    #[allow(clippy::arc_with_non_send_sync)]
    fn register_ws_bridges(&mut self) {
        let sessions = self.ws_sessions.clone();
        let next_id = self.ws_next_id.clone();
        let sink = self.sample_sink.clone();
        // W2 line 188: ws_* metrics used to hardcode group="ws". Capture the
        // group() stack so ws samples carry the full ::a::b path like
        // http/checks (k6 parity).
        let group_stack = self.group_stack.clone();
        let (deadline, max_exec) = self.js_ctx.interrupt_deadline_handle();

        self.js_ctx.with_ctx(|rq_ctx| {
            let globals = rq_ctx.globals();

            // ── ws.connect(url, headers_json) -> {id, error} ──
            let sessions_conn = sessions.clone();
            let next_id_conn = next_id.clone();
            let deadline_conn = deadline.clone();
            let max_exec_conn = max_exec;
            // Backlog line 62: a failed handshake must still emit ws_* metrics
            // (ws_connecting + ws_req_failed=1.0), so the failed request is
            // visible to thresholds instead of vanishing.
            let sink_conn = sink.clone();
            let group_stack_conn = group_stack.clone();
            let _ = globals.set(
                "__tropel_k6_ws_connect",
                Func::from(
                    move |url: String, headers_json: String| -> String {
                        let headers = parse_headers_tolerant(&headers_json);
                        // Build the handshake request with the given headers.
                        let mut handshake = match url.clone().into_client_request() {
                            Ok(r) => r,
                            Err(e) => {
                                return serde_json::json!({
                                    "id": 0,
                                    "error": format!("invalid ws url '{url}': {e}"),
                                })
                                .to_string();
                            }
                        };
                        for (k, v) in &headers {
                            if let (Ok(hname), Ok(hv)) = (
                                http::HeaderName::from_bytes(k.as_bytes()),
                                http::HeaderValue::from_str(v),
                            ) {
                                handshake.headers_mut().insert(hname, hv);
                            }
                        }

                        let id = next_id_conn.fetch_add(1, Ordering::Relaxed) + 1;
                        let (events_tx, events_rx) = std::sync::mpsc::channel::<WsEvent>();
                        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<WsCommand>(64);
                        let connect_start = Instant::now();

                        // Connect on the dedicated I/O runtime (safe from inside
                        // ctx.with on a current-thread VU runtime), then spawn a
                        // background reader/writer task that owns the socket and
                        // streams events into the channel. The task lives on the
                        // I/O runtime, so blocking this VU thread on recv_timeout
                        // never deadlocks a VU reactor.
                        // `events_tx` is cloned for the reader task: the outer
                        // future keeps the original to deliver the Open event
                        // before returning, so the two don't fight over ownership.
                        let events_tx_reader = events_tx.clone();
                        let url_err = url.clone();
                        let connect_result = tropel_http::blocking::execute_blocking(
                            async move {
                                let (ws, _resp) =
                                    tokio_tungstenite::connect_async(handshake)
                                        .await
                                        .map_err(|e| {
                                            TropelError::Extension(format!(
                                                "WebSocket connect to '{}': {}",
                                                url_err, e
                                            ))
                                        })?;
                                let connecting = connect_start.elapsed();
                                let (sink, stream) = ws.split();

                                tokio::spawn(async move {
                                    let mut sink = sink;
                                    let mut stream = stream;
                                    let mut cmd_rx = cmd_rx;
                                    loop {
                                        tokio::select! {
                                            cmd = cmd_rx.recv() => match cmd {
                                                Some(WsCommand::SendText(t)) => {
                                                    if sink.send(Message::Text(t.into())).await.is_err() {
                                                        break;
                                                    }
                                                }
                                                Some(WsCommand::Ping) => {
                                                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                                                        break;
                                                    }
                                                }
                                                Some(WsCommand::Close { code, reason }) => {
                                                    // SplitSink has no inherent close();
                                                    // send a Close frame instead.
                                                    let _ = sink
                                                        .send(Message::Close(Some(CloseFrame {
                                                            code: CloseCode::from(code),
                                                            reason: reason.into(),
                                                        })))
                                                        .await;
                                                    break;
                                                }
                                                None => break,
                                            },
                                            msg = stream.next() => match msg {
                                                Some(Ok(Message::Text(t))) => {
                                                    if events_tx_reader
                                                        .send(WsEvent::Text(t.to_string()))
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                Some(Ok(Message::Binary(b))) => {
                                                    if events_tx_reader
                                                        .send(WsEvent::Binary(b.len()))
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                Some(Ok(Message::Ping(_))) => {
                                                    if events_tx_reader.send(WsEvent::Ping).is_err() {
                                                        break;
                                                    }
                                                }
                                                Some(Ok(Message::Pong(_))) => {
                                                    if events_tx_reader.send(WsEvent::Pong).is_err() {
                                                        break;
                                                    }
                                                }
                                                Some(Ok(Message::Frame(_))) => {} // raw frame passthrough

                                                Some(Ok(Message::Close(f))) => {
                                                    let (code, reason) = f
                                                        .map(|f| {
                                                            (u16::from(f.code), f.reason.to_string())
                                                        })
                                                        .unwrap_or((1000, String::new()));
                                                    let _ = events_tx_reader.send(WsEvent::Close {
                                                        code,
                                                        reason,
                                                    });
                                                    break;
                                                }
                                                Some(Err(e)) => {
                                                    let _ = events_tx_reader.send(
                                                        WsEvent::Error(e.to_string()),
                                                    );
                                                    break;
                                                }
                                                None => {
                                                    let _ = events_tx_reader.send(WsEvent::Close {
                                                        code: 1006,
                                                        reason: "connection closed".into(),
                                                    });
                                                    break;
                                                }
                                            },
                                        }
                                    }
                                });

                                // Open is delivered as the first event once the
                                // handshake completes (step() returns it).
                                let _ = events_tx.send(WsEvent::Open);
                                Ok::<Duration, TropelError>(connecting)
                            },
                        );

                        // The connect burned wall time; re-arm the per-eval JS
                        // deadline so a slow handshake doesn't trip the
                        // interrupt on resume (backlog line 104).
                        tropel_js::rearm_deadline(&deadline_conn, max_exec_conn);
                        match connect_result {
                            Ok(connecting) => {
                                sessions_conn.lock().unwrap().insert(
                                    id,
                                    Arc::new(WsSession {
                                        events_rx,
                                        cmd_tx,
                                        url: url.clone(),
                                        start: Instant::now(),
                                        connecting,
                                        msgs_sent: AtomicU64::new(0),
                                        bytes_sent: AtomicU64::new(0),
                                        msgs_received: AtomicU64::new(0),
                                        bytes_received: AtomicU64::new(0),
                                        failed: AtomicBool::new(false),
                                    }),
                                );
                                serde_json::json!({ "id": id, "error": null }).to_string()
                            }
                            Err(e) => {
                                // Backlog line 62: a failed handshake emitted
                                // ZERO ws metrics. k6 parity: emit
                                // ws_connecting (time to failure) + a
                                // ws_req_failed=1.0 Rate sample so the failed
                                // request shows up in summary/thresholds.
                                let elapsed = connect_start.elapsed();
                                let now = tropel_js::clock::monotonic_wall_now();
                                let mut tags = TagMap::with_capacity(5);
                                tags.insert("url", url.clone());
                                tags.insert("method", String::from("GET"));
                                tags.insert("status", String::from("0"));
                                tags.insert("name", url.clone());
                                // W2 line 188: the active group() path (full
                                // ::a::b), not a hardcoded "ws".
                                tags.insert(
                                    "group",
                                    group_stack_conn
                                        .lock()
                                        .unwrap()
                                        .last()
                                        .cloned()
                                        .unwrap_or_else(|| "ws".to_string()),
                                );
                                let tags = Arc::new(tags);
                                let mut v = sink_conn.lock().unwrap();
                                v.push(Sample {
                                    metric: "ws_connecting".into(),
                                    value: elapsed.as_secs_f64() * 1000.0,
                                    tags: tags.clone(),
                                    timestamp: now,
                                    sample_type: SampleType::Trend,
                                });
                                v.push(Sample {
                                    metric: "ws_req_failed".into(),
                                    value: 1.0,
                                    tags,
                                    timestamp: now,
                                    sample_type: SampleType::Rate,
                                });
                                serde_json::json!({
                                    "id": 0,
                                    "error": e.to_string(),
                                })
                                .to_string()
                            }
                        }
                    },
                ),
            );

            // ── ws step(id, timeout_ms) -> event JSON ──
            let sessions_step = sessions.clone();
            let deadline_step = deadline.clone();
            let _ = globals.set(
                "__tropel_k6_ws_step",
                Func::from(
                    move |id: u64, timeout_ms: f64| -> String {
                        // Reset the per-eval interrupt deadline so a long ws
                        // session isn't killed by the eval timeout mid-pump.
                        // Same MONOTONIC base as JsContext's interrupt handler
                        // (backlog P3: an NTP step must not kill a script).
                        // Same arithmetic as the sleep/HTTP/WS-connect bridges
                        // (shared monotonic-clock helper, backlog line 104).
                        tropel_js::rearm_deadline(&deadline_step, max_exec);

                        let timeout = Duration::from_millis(timeout_ms.max(1.0) as u64);
                        let guard = sessions_step.lock().unwrap();
                        let Some(session) = guard.get(&id).cloned() else {
                            return serde_json::json!({
                                "type": "close",
                                "code": 1006,
                                "reason": "session not found",
                            })
                            .to_string();
                        };
                        drop(guard);
                        match session.events_rx.recv_timeout(timeout) {
                            Ok(WsEvent::Open) => {
                                serde_json::json!({"type": "open"}).to_string()
                            }
                            Ok(WsEvent::Text(t)) => {
                                session.msgs_received.fetch_add(1, Ordering::Relaxed);
                                session.bytes_received.fetch_add(t.len() as u64, Ordering::Relaxed);
                                serde_json::json!({"type": "message", "data": t}).to_string()
                            }
                            Ok(WsEvent::Binary(n)) => {
                                session.msgs_received.fetch_add(1, Ordering::Relaxed);
                                session.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                                serde_json::json!({
                                    "type": "message",
                                    "data": format!("<binary {n} bytes>"),
                                })
                                .to_string()
                            }
                            Ok(WsEvent::Ping) => {
                                serde_json::json!({"type": "ping"}).to_string()
                            }
                            Ok(WsEvent::Pong) => {
                                serde_json::json!({"type": "pong"}).to_string()
                            }
                            Ok(WsEvent::Close { code, reason }) => {
                                // Backlog line 62: close codes 1000 (normal)
                                // and 1001 (going away) are clean; everything
                                // else (1006 abnormal, 1002 protocol, 4000+ app
                                // codes) marks the request failed (k6 parity).
                                if code != 1000 && code != 1001 {
                                    session.failed.store(true, Ordering::Relaxed);
                                }
                                serde_json::json!({
                                    "type": "close",
                                    "code": code,
                                    "reason": reason,
                                })
                                .to_string()
                            }
                            Ok(WsEvent::Error(m)) => {
                                session.failed.store(true, Ordering::Relaxed);
                                serde_json::json!({
                                    "type": "error",
                                    "message": m,
                                })
                                .to_string()
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                serde_json::json!({"type": "none"}).to_string()
                            }
                            Err(RecvTimeoutError::Disconnected) => {
                                // Reader dropped without a close frame →
                                // abnormal closure (1006), k6 marks it failed.
                                session.failed.store(true, Ordering::Relaxed);
                                serde_json::json!({
                                    "type": "close",
                                    "code": 1006,
                                    "reason": "connection closed",
                                })
                                .to_string()
                            }
                        }
                    },
                ),
            );

            // ── ws send / ping / close ──
            // `try_send` + a bounded retry: the writer task lives on the
            // separate I/O runtime (NOT this VU's reactor), so parking this
            // VU thread briefly never deadlocks it — and no frame is silently
            // dropped under a send burst. `blocking_send` is NOT used: it
            // block_on's and would panic inside the VU runtime.
            let sessions_send = sessions.clone();
            let _ = globals.set(
                "__tropel_k6_ws_send",
                Func::from(
                    move |id: u64, data: String| -> String {
                        let guard = sessions_send.lock().unwrap();
                        let Some(session) = guard.get(&id).cloned() else {
                            return serde_json::json!({"ok": false}).to_string();
                        };
                        drop(guard);
                        let data_len = data.len() as u64;
                        let ok = try_send_cmd(&session.cmd_tx, WsCommand::SendText(data));
                        if ok {
                            session.msgs_sent.fetch_add(1, Ordering::Relaxed);
                            session.bytes_sent.fetch_add(data_len, Ordering::Relaxed);
                        }
                        serde_json::json!({"ok": ok}).to_string()
                    },
                ),
            );
            let sessions_ping = sessions.clone();
            let _ = globals.set(
                "__tropel_k6_ws_ping",
                Func::from(
                    move |id: u64| -> String {
                        let guard = sessions_ping.lock().unwrap();
                        let Some(session) = guard.get(&id).cloned() else {
                            return serde_json::json!({"ok": false}).to_string();
                        };
                        drop(guard);
                        let ok = try_send_cmd(&session.cmd_tx, WsCommand::Ping);
                        serde_json::json!({"ok": ok}).to_string()
                    },
                ),
            );
            let sessions_close = sessions.clone();
            let _ = globals.set(
                "__tropel_k6_ws_close",
                Func::from(
                    move |id: u64, code: f64, reason: String| -> String {
                        let guard = sessions_close.lock().unwrap();
                        let Some(session) = guard.get(&id).cloned() else {
                            return serde_json::json!({"ok": false}).to_string();
                        };
                        drop(guard);
                        let ok = try_send_cmd(
                            &session.cmd_tx,
                            WsCommand::Close {
                                code: code as u16,
                                reason,
                            },
                        );
                        serde_json::json!({"ok": ok}).to_string()
                    },
                ),
            );

            // ── ws finish(id) -> teardown + ws_* metrics ──
            let sessions_finish = sessions.clone();
            let sink_finish = sink.clone();
            let group_stack_fin = group_stack.clone();
            let _ = globals.set(
                "__tropel_k6_ws_finish",
                Func::from(
                    move |id: u64| -> String {
                        let session = sessions_finish.lock().unwrap().remove(&id);
                        let Some(session) = session else {
                            return serde_json::json!({"ok": false}).to_string();
                        };
                        let duration = session.start.elapsed();
                        let now = tropel_js::clock::monotonic_wall_now();
                        let mut tags = TagMap::with_capacity(5);
                        tags.insert("url", session.url.clone());
                        tags.insert("method", String::from("GET"));
                        tags.insert("status", String::from("101"));
                        tags.insert("name", session.url.clone());
                        // W2 line 188: the active group() path (full ::a::b),
                        // not a hardcoded "ws".
                        tags.insert(
                            "group",
                            group_stack_fin
                                .lock()
                                .unwrap()
                                .last()
                                .cloned()
                                .unwrap_or_else(|| "ws".to_string()),
                        );
                        let tags = Arc::new(tags);

                        let msgs_sent = session.msgs_sent.load(Ordering::Relaxed);
                        let bytes_sent = session.bytes_sent.load(Ordering::Relaxed);
                        let msgs_received = session.msgs_received.load(Ordering::Relaxed);
                        let bytes_received = session.bytes_received.load(Ordering::Relaxed);

                        let mut v = sink_finish.lock().unwrap();
                        v.push(Sample {
                            metric: "ws_connecting".into(),
                            value: session.connecting.as_secs_f64() * 1000.0,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Trend,
                        });
                        v.push(Sample {
                            metric: "ws_msgs_sent".into(),
                            value: msgs_sent as f64,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        v.push(Sample {
                            metric: "ws_msgs_received".into(),
                            value: msgs_received as f64,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        v.push(Sample {
                            metric: "ws_bytes_sent".into(),
                            value: bytes_sent as f64,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        v.push(Sample {
                            metric: "ws_bytes_received".into(),
                            value: bytes_received as f64,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        v.push(Sample {
                            metric: "ws_sessions".into(),
                            value: 1.0,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        v.push(Sample {
                            metric: "ws_req_duration".into(),
                            value: duration.as_secs_f64() * 1000.0,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Trend,
                        });
                        // Backlog line 62: ws_req_failed was hardcoded 0.0 —
                        // it ignored Error events and abnormal close codes.
                        // k6 parity: 1.0 iff the session saw an error or an
                        // abnormal closure (1000/1001 are clean).
                        let failed = session.failed.load(Ordering::Relaxed);
                        v.push(Sample {
                            metric: "ws_req_failed".into(),
                            value: if failed { 1.0 } else { 0.0 },
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Rate,
                        });
                        v.push(Sample {
                            metric: "data_sent".into(),
                            value: bytes_sent as f64,
                            tags: tags.clone(),
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        v.push(Sample {
                            metric: "data_received".into(),
                            value: bytes_received as f64,
                            tags,
                            timestamp: now,
                            sample_type: SampleType::Counter,
                        });
                        serde_json::json!({"ok": true}).to_string()
                    },
                ),
            );
        });

        self.ws_bridges_registered = true;
        tracing::debug!("K6Driver: registered ws bridges");
    }

    /// Sync VuContext state into JS globals so the script can read
    /// environment variables, data rows, etc.
    ///
    /// Split into a one-time seed of the per-VU IMMUTABLE globals (__VU,
    /// __tropel_vu_id, __tropel_scenario, __ENV, __tropel_env — the env is
    /// constant for the whole run) and a per-iteration refresh of the mutable
    /// ones (__ITER, __tropel_iteration_num, __tropel_data_row, exec.*). This
    /// removes the previous per-iteration cost of serializing the env twice
    /// and compile+eval'ing a JSON.parse on every iteration.
    async fn sync_globals(&mut self, ctx: &VuContext) -> Result<()> {
        if !self.globals_seeded {
            self.globals_seeded = true;

            // Numeric, matching __VU and the __tropel_exec_vu_id bridge (a
            // string here would hit the same arithmetic bug as the old __VU:
            // `__tropel_vu_id % len` → NaN).
            let _ = self
                .js_ctx
                .set_global_json("__tropel_vu_id", &serde_json::json!(ctx.vu_id))
                .await;
            let _ = self
                .js_ctx
                .set_global_str("__tropel_scenario", &ctx.scenario_name)
                .await;
            // k6-compatible: __VU is 1-based (like k6); __ITER is 0-based and
            // refreshed below each iteration. Backlog line 142: these must be
            // NUMBERS, not strings — `__ITER === 0` (the once-per-VU setup
            // guard) never fired on "0" and `__VU + 1` produced "11" on
            // string concatenation. set_global_json parses the JSON number
            // natively (no eval, no string round trip).
            let _ = self
                .js_ctx
                .set_global_json("__VU", &serde_json::json!(ctx.vu_id + 1))
                .await;

            // Set env vars as JS globals. k6 scripts read `__ENV` (and
            // Tropel's own `__tropel_env`); both get the same object. Always
            // set __ENV so `__ENV` is never undefined inside the script.
            let env_value = serde_json::to_value(&ctx.env).unwrap_or_default();
            let _ = self.js_ctx.set_global_json("__ENV", &env_value).await;
            let _ = self
                .js_ctx
                .set_global_json("__tropel_env", &env_value)
                .await;

            // Seed the per-VU IMMUTABLE exec.* fields once too (scenario name,
            // executor name, vu id never change for a VU) — the per-iteration
            // refresh below only touches the mutable counters, saving two
            // string clones per iteration.
            let mut es = self.exec_state.lock().unwrap();
            es.scenario_name = ctx.scenario_name.clone();
            es.executor_name = ctx.executor_name.clone();
            es.vu_id = ctx.vu_id;

            // k6 lifecycle: `export default function (data)` receives the
            // script's `setup()` return value. The engine ran setup() once
            // per scenario and threaded the serialized result into
            // ctx.setup_data — parse it into a native JS value ONCE here
            // (it is immutable for the whole run) so every iteration call
            // below passes it without re-serializing. When the script
            // declares no setup, `__tropel_setup` stays undefined, matching
            // k6 (data === undefined).
            let setup_value = ctx
                .setup_data
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            self.js_ctx.with_ctx(|rq_ctx| {
                let val = match &setup_value {
                    Some(v) => json_to_value(rq_ctx, v),
                    None => rquickjs::Value::new_undefined(rq_ctx.clone()),
                };
                let _ = rq_ctx.globals().set("__tropel_setup", val);
            });
        }

        // Per-iteration mutable globals. Backlog line 142: numbers, not
        // strings (see the __VU comment above).
        let _ = self
            .js_ctx
            .set_global_json("__tropel_iteration_num", &serde_json::json!(ctx.iteration))
            .await;
        let _ = self
            .js_ctx
            .set_global_json("__ITER", &serde_json::json!(ctx.iteration))
            .await;

        // Refresh the per-iteration exec.* counters read by the __tropel_exec_*
        // bridges (iteration / iterations_completed / vus_active change every
        // iteration; scenario_name / executor_name / vu_id were seeded once).
        {
            let mut es = self.exec_state.lock().unwrap();
            es.iteration = ctx.iteration;
            es.iterations_completed = ctx.iterations_completed;
            es.vus_active = ctx.vus_active;
        }

        // Set data row (changes per iteration)
        if let Some(ref row) = ctx.data_row {
            let _ = self
                .js_ctx
                .set_global_json(
                    "__tropel_data_row",
                    &serde_json::to_value(row).unwrap_or_default(),
                )
                .await;
        }

        Ok(())
    }
}

// ══════════════════════════════════════════════════════════════════
// ES-module local-import support (module resolver + loader)
// ══════════════════════════════════════════════════════════════════

/// Resolves relative ES-module specifiers to files on disk.
///
/// k6 scripts import local helpers with relative specifiers
/// (`import { x } from "./helpers.js"`). rquickjs consults this resolver
/// whenever a declared module contains an `import`/`export … from`
/// statement. Bare specifiers (`k6`, `k6/http`, npm packages) are not
/// resolvable on disk — k6 virtual modules are stripped by
/// `preprocess_k6_source_module` and provided by the shim, so a bare
/// specifier reaching the resolver is an error.
#[derive(Clone)]
struct K6ModuleResolver {
    script_dir: Option<PathBuf>,
}

impl Resolver for K6ModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &rquickjs::Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        // Only relative/absolute specifiers can point at files. Bare
        // specifiers (k6 virtual modules, npm packages) error loudly.
        if !(name.starts_with("./") || name.starts_with("../") || Path::new(name).is_absolute()) {
            return Err(rquickjs::Error::new_loading_message(
                name,
                "bare module specifiers are not supported (k6 virtual modules are provided by the shim)",
            ));
        }

        // Base directory: the importing module's directory. For the entry
        // module (named "k6-script") or non-path bases, fall back to the
        // script directory.
        let base_dir = if base == "k6-script" || base.is_empty() {
            self.script_dir.clone().unwrap_or_default()
        } else {
            Path::new(base)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default()
        };

        let candidate = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            base_dir.join(name)
        };

        // Extension probing: try as-is, then with common JS/TS extensions,
        // then index files. `with_extension` returns a fresh PathBuf (the
        // original candidate is never mutated), so each attempt is distinct.
        let mut attempts: Vec<PathBuf> = Vec::new();
        attempts.push(candidate.clone());
        if candidate.extension().is_none() {
            for ext in ["js", "mjs", "cjs", "ts", "mts", "tsx"] {
                attempts.push(candidate.with_extension(ext));
            }
            attempts.push(candidate.join("index.js"));
            attempts.push(candidate.join("index.ts"));
            attempts.push(candidate.join("index.mjs"));
        }
        for a in &attempts {
            if a.is_file() {
                return Ok(a.to_string_lossy().into_owned());
            }
        }
        Err(rquickjs::Error::new_loading_message(
            name,
            format!("cannot resolve module '{}' from '{}'", name, base),
        ))
    }
}

/// Loads a resolved module file into the runtime, transpiling TypeScript
/// on the fly when the file is `.ts`/`.mts`/`.tsx`.
struct K6ModuleLoader;

impl Loader for K6ModuleLoader {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<rquickjs::Module<'js>> {
        let raw = std::fs::read_to_string(name).map_err(|e| {
            rquickjs::Error::new_loading_message(name, format!("read error: {}", e))
        })?;
        // Strip k6-virtual imports from the loaded module too — helper files
        // commonly `import { check } from "k6"` / `import http from "k6/http"`,
        // and those specifiers have no on-disk module (the shim provides the
        // globals). Mirroring the entry module's preprocess keeps imported
        // files consistent; local imports inside them still resolve via the
        // resolver.
        let preprocessed = preprocess_k6_source_module(&raw);
        let source = if tropel_es::is_typescript_file(name) {
            tropel_es::typescript_to_javascript_keep_exports(&preprocessed, name).map_err(|e| {
                rquickjs::Error::new_loading_message(name, format!("TS transpile error: {}", e))
            })?
        } else {
            preprocessed
        };
        rquickjs::Module::declare(ctx.clone(), name, source)
    }
}

// ══════════════════════════════════════════════════════════════════
// Source pre-processing
// ══════════════════════════════════════════════════════════════════

/// Pre-process a k6 source string for ES-module evaluation.
///
/// Unlike the old script-mode preprocessor (which stripped `export` modifiers
/// so the source could be eval'd), this variant KEEPS all `export` modifiers —
/// `export const options`, `export default function`, `export function
/// setup()` — because they are valid (and load-bearing) in a module.
///
/// k6 virtual imports and re-exports are removed on the oxc AST (see
/// [`tropel_es::strip_k6_virtual_imports`]): `import … from "k6/…"`,
/// `import "k6/…"`, `export { x } from "k6/…"`, `export * from "k6/…"`, and
/// remote `https://…` (jslib) specifiers — the k6 shim provides those APIs as
/// globals and there is no `k6/*` module or fetched jslib file on disk. The
/// AST-based splice (vs. the old line-anchored regex) also strips multi-line
/// imports (`import {\n check\n} from 'k6';`) and imports with trailing
/// comments, which used to survive, reach the module resolver, hard-error,
/// and kill `init` before iteration 1 → zero-metric, exit-0 runs.
///
/// Local imports (`import { x } from "./helpers.js"`) and local re-exports
/// (`export { x } from "./helpers"`, `export * from "./helpers"`) are KEPT:
/// the ES-module loader registered on the context (`K6ModuleResolver` +
/// `K6ModuleLoader`) resolves them to files on disk, transpiling TypeScript
/// on the fly.
fn preprocess_k6_source_module(source: &str) -> String {
    tropel_es::strip_k6_virtual_imports(source)
}

/// Build the final source for ES-module evaluation: pre-process (keep
/// exports, drop k6 virtual imports) and transpile TypeScript while keeping
/// the `export` modifiers intact (script-mode transpilation strips them).
fn prepare_module_source(original: &str, source_path: Option<&Path>) -> Result<String> {
    let preprocessed = preprocess_k6_source_module(original);

    if let Some(path) = source_path {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("js")
            .to_lowercase();
        if matches!(ext.as_str(), "ts" | "mts" | "tsx") {
            return tropel_es::typescript_to_javascript_keep_exports(
                &preprocessed,
                &path.to_string_lossy(),
            )
            .map_err(|e| TropelError::Parse(format!("TS transpile error: {}", e)));
        }
        return Ok(preprocessed);
    }

    // No path hint — detect TS patterns heuristically.
    if preprocessed.contains(": string")
        || preprocessed.contains(": number")
        || preprocessed.contains(": boolean")
        || preprocessed.contains("interface ")
    {
        return tropel_es::typescript_to_javascript_keep_exports(&preprocessed, "script.js")
            .map_err(|e| TropelError::Parse(format!("TS transpile error: {}", e)));
    }

    Ok(preprocessed)
}

/// Evaluate an ES module and return the named export serialized as JSON.
///
/// Creates a throwaway `JsContext`, sets the k6 globals a script may read at
/// top level (`__ENV` from the job's env, `__VU`, …), evals the module,
/// JSON.stringify()s the requested export, and drops the context. Returns
/// `Ok(None)` when the export is absent/undefined — never an error for a
/// script that simply does not declare the export.
async fn eval_module_export_json(
    source: &str,
    export: &str,
    env: &HashMap<String, String>,
    script_dir: Option<PathBuf>,
) -> Result<Option<String>> {
    let mut js_ctx = JsContext::new(Some(10 * 1024 * 1024), Some(Duration::from_secs(10)))
        .await
        .map_err(|e| TropelError::Other(format!("JS context creation failed: {}", e)))?;

    // k6 scripts often read data files at init time (`JSON.parse(open(...))`
    // or `new SharedArray(...)`) while building `export const options` —
    // install the file bridges AND the open/SharedArray shim so the throwaway
    // context can resolve them (the shim defines the JS globals on top of the
    // native bridges). Also register the module loader so `options` blocks
    // that import local helpers (`import { x } from "./helpers.js"`) resolve.
    register_k6_file_bridges(&mut js_ctx, script_dir.clone());
    js_ctx.set_module_loader(
        K6ModuleResolver {
            script_dir: script_dir.clone(),
        },
        K6ModuleLoader,
    );
    js_ctx
        .bootstrap_library(OPEN_DATA_SHIM)
        .await
        .map_err(|e| {
            TropelError::Other(format!("k6 open/SharedArray shim bootstrap failed: {}", e))
        })?;
    // The k6 shim libs (Rate/check/http/…) must be present: options blocks
    // commonly run k6 API at module top level (e.g. `new Rate('errors')`),
    // which threw QuickJS exceptions when the shim was missing.
    bootstrap_js_libs(
        &mut js_ctx,
        Arc::new(AtomicBool::new(false)),
        Arc::new(OnceLock::new()),
    )
    .await
    .map_err(|e| TropelError::Other(format!("k6 shim bootstrap failed for options eval: {}", e)))?;

    // Minimal globals a k6 script may reference while building its options.
    // `__ENV` carries the job's env vars so options computed from them
    // (e.g. `const baseURL = __ENV.BASE_URL`) resolve instead of silently
    // becoming undefined. Backlog line 142: __VU/__ITER are NUMBERS in k6
    // (script code doing `__ITER === 0` or `__VU + 1` must not see strings).
    let _ = js_ctx.set_global_json("__VU", &serde_json::json!(0)).await;
    let _ = js_ctx
        .set_global_json("__ITER", &serde_json::json!(0))
        .await;
    let env_json = serde_json::to_value(env).unwrap_or_else(|_| serde_json::json!({}));
    let _ = js_ctx.set_global_json("__ENV", &env_json).await;
    let _ = js_ctx.set_global_json("__tropel_env", &env_json).await;

    // Arm the per-eval timeout (module eval bypasses the eval-family methods).
    js_ctx.reset_interrupt();
    match js_ctx.with_ctx(|ctx| read_module_export_string(ctx, source, export)) {
        Ok(Some(s)) => Ok(Some(s)),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::warn!("Failed to read k6 export '{}': {}", export, e);
            Ok(None)
        }
    }
}

/// Evaluate an ES module, call its `handleSummary(data)` export with the
/// given summary-data JSON, and return the script's output map
/// (filename → content; `stdout` prints to stdout). Returns `Ok(None)` when
/// the script declares no `handleSummary` export.
async fn eval_module_handle_summary(
    source: &str,
    data_json: &str,
    env: &HashMap<String, String>,
    script_dir: Option<PathBuf>,
) -> Result<Option<HashMap<String, String>>> {
    let mut js_ctx = JsContext::new(Some(10 * 1024 * 1024), Some(Duration::from_secs(10)))
        .await
        .map_err(|e| TropelError::Other(format!("JS context creation failed: {}", e)))?;

    // `handleSummary` may reference `open()`/SharedArray captured at init, so
    // install the file bridges + shim on the throwaway context too. Also
    // register the module loader so a `handleSummary` module that imports
    // local helpers (`import { x } from "./helpers.js"`) resolves them.
    register_k6_file_bridges(&mut js_ctx, script_dir.clone());
    js_ctx.set_module_loader(
        K6ModuleResolver {
            script_dir: script_dir.clone(),
        },
        K6ModuleLoader,
    );
    js_ctx
        .bootstrap_library(OPEN_DATA_SHIM)
        .await
        .map_err(|e| {
            TropelError::Other(format!("k6 open/SharedArray shim bootstrap failed: {}", e))
        })?;
    // Same k6-shim requirement as the options eval: a script that touches
    // k6 API at module top level must not throw while handleSummary is read.
    bootstrap_js_libs(
        &mut js_ctx,
        Arc::new(AtomicBool::new(false)),
        Arc::new(OnceLock::new()),
    )
    .await
    .map_err(|e| {
        TropelError::Other(format!(
            "k6 shim bootstrap failed for handleSummary eval: {}",
            e
        ))
    })?;

    // Minimal globals a k6 script may reference while building its summary.
    // Backlog line 142: numbers, not strings (see options eval above).
    let _ = js_ctx.set_global_json("__VU", &serde_json::json!(0)).await;
    let _ = js_ctx
        .set_global_json("__ITER", &serde_json::json!(0))
        .await;
    let env_json = serde_json::to_value(env).unwrap_or_else(|_| serde_json::json!({}));
    let _ = js_ctx.set_global_json("__ENV", &env_json).await;
    let _ = js_ctx.set_global_json("__tropel_env", &env_json).await;

    js_ctx.reset_interrupt();
    match js_ctx.with_ctx(|ctx| call_module_handle_summary(ctx, source, data_json)) {
        Ok(Some(map)) => Ok(Some(map)),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::warn!("Failed to run k6 handleSummary: {}", e);
            Ok(None)
        }
    }
}

/// Evaluate an ES module and CALL a named exported function, serializing its
/// return value as JSON.
///
/// Backs the k6 lifecycle functions `setup()` (no argument) and
/// `teardown(data)` (the setup return value as argument). Mirrors
/// [`eval_module_handle_summary`]'s throwaway-context setup: file bridges +
/// module loader + shims installed so a script that touches `open()`,
/// SharedArray, or k6 API at module top level doesn't throw while the
/// export is read. Returns `Ok(None)` when the export is absent/not a
/// function (k6: no setup/teardown declared); `Err` when the call itself
/// throws (setup/teardown body error) so the caller decides how to surface
/// it (setup → None + warn, teardown → warn only, k6 parity).
async fn eval_module_call_export(
    source: &str,
    export: &str,
    arg_json: Option<&str>,
    env: &HashMap<String, String>,
    script_dir: Option<PathBuf>,
    http_client: Arc<dyn DriverHttpClient + Send + Sync>,
    sink: Arc<Mutex<Vec<Sample>>>,
) -> Result<Option<String>> {
    let mut js_ctx = JsContext::new(Some(10 * 1024 * 1024), Some(Duration::from_secs(10)))
        .await
        .map_err(|e| TropelError::Other(format!("JS context creation failed: {}", e)))?;

    register_k6_file_bridges(&mut js_ctx, script_dir.clone());
    js_ctx.set_module_loader(
        K6ModuleResolver {
            script_dir: script_dir.clone(),
        },
        K6ModuleLoader,
    );
    js_ctx
        .bootstrap_library(OPEN_DATA_SHIM)
        .await
        .map_err(|e| {
            TropelError::Other(format!("k6 open/SharedArray shim bootstrap failed: {}", e))
        })?;
    bootstrap_js_libs(
        &mut js_ctx,
        Arc::new(AtomicBool::new(false)),
        Arc::new(OnceLock::new()),
    )
    .await
    .map_err(|e| {
        TropelError::Other(format!("k6 shim bootstrap failed for {export} eval: {}", e))
    })?;

    // k6 §4 (backlog line 119): setup()/teardown() may make HTTP calls —
    // register the native HTTP bridges against the scenario's shared client
    // so `http.*` resolves in the throwaway context (previously it threw and
    // the login-in-setup pattern produced data === undefined). Samples land
    // in `sink`, which the engine drains into the run's metrics. The
    // blocking HTTP calls re-arm the per-eval deadline (backlog line 104).
    let (deadline, max_exec) = js_ctx.interrupt_deadline_handle();
    let group_stack: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    js_ctx.with_ctx(|rq_ctx| {
        register_http_bridges(
            rq_ctx,
            http_client.clone(),
            sink.clone(),
            "", // no scenario tag in the lifecycle context
            group_stack.clone(),
            deadline,
            max_exec,
        );
    });

    // Minimal globals a k6 script may reference at module top level while
    // defining setup()/teardown() (same set as the options/handleSummary
    // evals). Backlog line 142: numbers, not strings.
    let _ = js_ctx.set_global_json("__VU", &serde_json::json!(0)).await;
    let _ = js_ctx
        .set_global_json("__ITER", &serde_json::json!(0))
        .await;
    let env_json = serde_json::to_value(env).unwrap_or_else(|_| serde_json::json!({}));
    let _ = js_ctx.set_global_json("__ENV", &env_json).await;
    let _ = js_ctx.set_global_json("__tropel_env", &env_json).await;

    js_ctx.reset_interrupt();
    match js_ctx.with_ctx(|ctx| call_module_export(ctx, source, export, arg_json)) {
        Ok(Some(s)) => Ok(Some(s)),
        Ok(None) => Ok(None),
        // Do NOT warn here — the callers own the error path: setup() logs a
        // warn and returns None (data stays undefined), teardown() logs a
        // warn and continues (k6 parity: a throwing teardown never affects
        // the run's exit status). A second warn here would double-report.
        Err(e) => Err(TropelError::Other(format!("k6 {export}() failed: {}", e))),
    }
}

/// Call `setup()`/`teardown(data)` inside the given context. `arg_json` is
/// parsed via the global `JSON.parse` (k6 data must be JSON-serializable);
/// the return value is stringified and returned. Absent/non-function export
/// → `Ok(None)`; a throwing call → `Err`.
fn call_module_export(
    ctx: &rquickjs::Ctx,
    source: &str,
    export: &str,
    arg_json: Option<&str>,
) -> std::result::Result<Option<String>, rquickjs::Error> {
    let module = rquickjs::Module::declare(ctx.clone(), "k6-script", source)?;
    let (module, promise) = module.eval()?;
    promise.finish::<()>()?;

    let func: rquickjs::Function = match module.get::<_, rquickjs::Function>(export) {
        Ok(f) => f,
        Err(_) => return Ok(None), // absent or not a function
    };

    let json_obj: rquickjs::Object = ctx.globals().get("JSON")?;
    let result: rquickjs::Value = match arg_json {
        Some(json) => {
            let parse: rquickjs::Function = json_obj.get("parse")?;
            let data: rquickjs::Value = parse.call((json,))?;
            func.call((data,))?
        }
        None => func.call(())?,
    };

    // k6 allows `export async function setup()/teardown(data)`. If the call
    // returned a Promise, finish it (pumps the job queue until settled).
    let result: rquickjs::Value = if let Some(promise) = result.as_promise() {
        promise.finish()?
    } else {
        result
    };

    if result.is_undefined() || result.is_null() {
        return Ok(None);
    }
    let stringify: rquickjs::Function = json_obj.get("stringify")?;
    let s: String = stringify.call((result,))?;
    Ok(Some(s))
}

/// Call `handleSummary(data)` inside the given context. The data object is
/// parsed via the global `JSON.parse` so no lifetime-bound JS value escapes
/// the `with_ctx` closure; the returned map is stringified and parsed here.
fn call_module_handle_summary(
    ctx: &rquickjs::Ctx,
    source: &str,
    data_json: &str,
) -> std::result::Result<Option<HashMap<String, String>>, rquickjs::Error> {
    let module = rquickjs::Module::declare(ctx.clone(), "k6-script", source)?;
    let (module, promise) = module.eval()?;
    promise.finish::<()>()?;

    // Use the established `module.get::<_, Function>` pattern (see
    // install_iteration_global): a missing export OR a non-function export
    // both yield None, matching read_module_export_string's handling.
    let func: rquickjs::Function = match module.get::<_, rquickjs::Function>("handleSummary") {
        Ok(f) => f,
        Err(_) => return Ok(None), // absent or not a function
    };

    let json_obj: rquickjs::Object = ctx.globals().get("JSON")?;
    let parse: rquickjs::Function = json_obj.get("parse")?;
    let data: rquickjs::Value = parse.call((data_json,))?;
    let result: rquickjs::Value = func.call((data,))?;

    // k6 allows `export async function handleSummary(data)`. If the call
    // returned a Promise, finish it (pumps the job queue until settled).
    let result: rquickjs::Value = if let Some(promise) = result.as_promise() {
        promise.finish()?
    } else {
        result
    };

    if result.is_undefined() || result.is_null() {
        return Ok(None);
    }

    let stringify: rquickjs::Function = json_obj.get("stringify")?;
    let s: String = stringify.call((result,))?;
    let parsed: serde_json::Value = serde_json::from_str(&s).unwrap_or_default();

    // k6 allows handleSummary to return a single string (→ stdout) or an
    // object map of filename → content.
    if let Some(text) = parsed.as_str() {
        return Ok(Some(HashMap::from([(
            "stdout".to_string(),
            text.to_string(),
        )])));
    }
    let mut map = HashMap::new();
    if let Some(obj) = parsed.as_object() {
        for (k, v) in obj {
            map.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
        }
    }
    Ok(Some(map))
}

/// Evaluate an ES module in the given context and JSON.stringify() the named
/// export. Returns `Ok(None)` when the export is missing or undefined.
///
/// The string is produced *inside* the context (via the global `JSON`
/// object) so no lifetime-bound JS value escapes the `with_ctx` closure.
fn read_module_export_string(
    ctx: &rquickjs::Ctx,
    source: &str,
    export: &str,
) -> std::result::Result<Option<String>, rquickjs::Error> {
    let module = rquickjs::Module::declare(ctx.clone(), "k6-script", source)?;
    let (module, promise) = module.eval()?;
    promise.finish::<()>()?;

    let value: rquickjs::Value = match module.get(export) {
        Ok(v) => v,
        Err(_) => return Ok(None), // export not present
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }

    let json_obj: rquickjs::Object = ctx.globals().get("JSON")?;
    let stringify: rquickjs::Function = json_obj.get("stringify")?;
    let s: String = stringify.call((value,))?;
    Ok(Some(s))
}

/// Evaluate an ES module and install its entry-point export as the global
/// `__tropel_iteration` (what `run_iteration` invokes). When `exec` names a
/// specific exported function (k6 multi-scenario `exec` selection), that
/// export is installed; otherwise the module's `default` export is used.
fn install_iteration_global(
    js_ctx: &mut JsContext,
    source: &str,
    exec: Option<&str>,
) -> Result<()> {
    // Arm the per-eval timeout: this evals the module directly via with_ctx,
    // bypassing the eval-family methods that normally reset the deadline.
    js_ctx.reset_interrupt();
    js_ctx.with_ctx(|rq_ctx| {
        let module = rquickjs::Module::declare(rq_ctx.clone(), "k6-script", source)
            .map_err(|e| TropelError::Other(format!("k6 script module declare error: {}", e)))?;
        let (module, promise) = module
            .eval()
            .map_err(|e| TropelError::Other(format!("k6 script module eval error: {}", e)))?;
        promise
            .finish::<()>()
            .map_err(|e| TropelError::Other(format!("k6 script module resolve error: {}", e)))?;

        let entry = exec.filter(|e| !e.is_empty()).unwrap_or("default");
        match module.get::<_, rquickjs::Function>(entry) {
            Ok(entry_fn) => {
                rq_ctx
                    .globals()
                    .set("__tropel_iteration", entry_fn)
                    .map_err(|e| {
                        TropelError::Other(format!("failed to install __tropel_iteration: {}", e))
                    })?;
            }
            Err(e) => {
                if entry != "default" {
                    // k6 semantics: a scenario naming a non-existent exec
                    // function errors loudly rather than silently running a
                    // different flow (confusing metrics).
                    return Err(TropelError::Other(format!(
                        "k6 scenario exec '{entry}' is not an exported function ({e}) — \
                         the named exec must be an `export function {entry}(...)` in the script"
                    )));
                }
                // Not fatal: script mode tolerated a missing default export
                // (warned + continued), so module mode does too.
                tracing::warn!("k6 script has no default export function: {}", e);
            }
        }
        Ok(())
    })
}

/// Check if a file path has a TypeScript extension (used in tests).
#[cfg(test)]
fn is_typescript_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_lowercase().as_str(), "ts" | "mts" | "tsx"))
        .unwrap_or(false)
}

// ══════════════════════════════════════════════════════════════════
// JS bootstrapping
// ══════════════════════════════════════════════════════════════════

/// Bootstrap vendored JS libraries into a fresh context.
/// Mirrors the engine's `create_vu_js_context()` setup.
/// Base shim libraries (no native dependencies) concatenated at COMPILE TIME
/// (concat!) into one bundle. Per VU the bundle is loaded from the process-
/// wide bytecode cache (compiled ONCE, then `JS_ReadObject`+run) instead of
/// being re-parsed + re-compiled — at 1000 VUs that saves ~130 MB of QuickJS
/// parsing. Each separate eval also resets the JS interrupt timer and pumps
/// the promise queue, so one eval per phase cuts the per-VU bootstrap
/// overhead ~4× beyond the bytecode win.
const K6_BASE_SHIM_BUNDLE: &str = concat!(
    "// ==== shim: deep-equal-shim ====\n",
    include_str!("../../../../js/shared/deep-equal.js"),
    "\n",
    "// ==== shim: chai-shim ====\n",
    include_str!("../../../../js/chai/chai-shim.js"),
    "\n",
    "// ==== shim: lodash-shim ====\n",
    include_str!("../../../../js/lodash/lodash-shim.js"),
    "\n",
    "// ==== shim: cryptojs-shim ====\n",
    include_str!("../../../../js/cryptojs-shim/cryptojs.js"),
    "\n",
    "// ==== shim: exec-shim ====\n",
    include_str!("../../../../js/exec/exec.js"),
);

/// Native-dependent shim libraries (pm-api, sleep, k6-shim, open/SharedArray)
/// concatenated at COMPILE TIME into one bundle (see K6_BASE_SHIM_BUNDLE).
const K6_NATIVE_SHIM_BUNDLE: &str = concat!(
    "// ==== shim: pm-api ====\n",
    include_str!("../../../../js/scripting-api/pm.js"),
    "\n",
    "// ==== shim: sleep-shim ====\n",
    include_str!("../../../../js/k6-shim/sleep-shim.js"),
    "\n",
    "// ==== shim: k6-shim ====\n",
    include_str!("../../../../js/k6-shim/k6-shim.js"),
    "\n",
    "// ==== shim: jslib-shim ====\n",
    include_str!("../../../../js/k6-shim/jslib-shim.js"),
    "\n",
    "// ==== shim: open-data-shim ====\n",
    include_str!("../../../../js/k6-shim/open-data-shim.js"),
);

/// Process-wide cache of a compiled k6 shim bundle's QuickJS bytecode.
///
/// Compiled ONCE by the first context (qjsc-style `JS_Eval` COMPILE_ONLY +
/// `JS_WriteObject`), then every subsequent context loads the blob and runs
/// it instead of re-parsing + re-compiling the ~130 KB of shim source per VU.
/// QuickJS bytecode is tied to the build (version + feature flags), not to a
/// particular context, so one compilation is valid for every VU context in
/// this process.
///
/// One cache exists per bundle (base + native-dependent): the native bundle
/// can only run AFTER `tropel_native::install_all`, so they are separate
/// blobs and must never be cross-served.
struct ShimBytecodeCache {
    bytecode: OnceLock<Option<Vec<u8>>>,
    /// True once compilation failed — every context then falls back to the
    /// per-VU source eval path instead of retrying the compile each time.
    compile_failed: AtomicBool,
    /// True once the cached bytecode failed to RUN in a context. A run
    /// failure is deterministic (same bytecode + same bundle), so after the
    /// first failure all subsequent contexts short-circuit straight to the
    /// source eval fallback.
    run_failed: AtomicBool,
}

impl ShimBytecodeCache {
    const fn new() -> Self {
        Self {
            bytecode: OnceLock::new(),
            compile_failed: AtomicBool::new(false),
            run_failed: AtomicBool::new(false),
        }
    }

    /// Bootstrap `bundle` into `ctx`: run the cached bytecode when available
    /// and known-good, otherwise fall back to a source eval (and remember the
    /// failure so the doomed path is not retried per VU).
    async fn bootstrap(&self, ctx: &mut JsContext, bundle: &'static str) {
        let bytecode = self.bytecode.get_or_init(|| {
            if self.compile_failed.load(Ordering::Relaxed) {
                return None;
            }
            match ctx.compile_global_bytecode(bundle) {
                Ok(bc) => {
                    tracing::info!(
                        "Compiled k6 shim bundle to bytecode once ({} bytes) — reusing across VUs",
                        bc.len()
                    );
                    Some(bc)
                }
                Err(e) => {
                    self.compile_failed.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "k6 shim bytecode compilation failed ({}); falling back to per-VU source eval",
                        e
                    );
                    None
                }
            }
        });

        if let (Some(bc), false) = (bytecode, self.run_failed.load(Ordering::Relaxed)) {
            if let Err(e) = ctx.run_global_bytecode(bc).await {
                self.run_failed.store(true, Ordering::Relaxed);
                tracing::warn!(
                    "Failed to run k6 shim bytecode: {} (disabling bytecode path; falling back to source eval)",
                    e
                );
                if let Err(e2) = ctx.bootstrap_library(bundle).await {
                    tracing::warn!("Failed to bootstrap k6 shim bundle: {}", e2);
                }
            }
        } else if let Err(e) = ctx.bootstrap_library(bundle).await {
            tracing::warn!("Failed to bootstrap k6 shim bundle: {}", e);
        }
    }
}

/// Bytecode caches for the two k6 shim bundles (see [`ShimBytecodeCache`]).
static K6_BASE_BYTECODE: ShimBytecodeCache = ShimBytecodeCache::new();
static K6_NATIVE_BYTECODE: ShimBytecodeCache = ShimBytecodeCache::new();

async fn bootstrap_js_libs(
    ctx: &mut JsContext,
    force_stop: Arc<AtomicBool>,
    sched_link: Arc<OnceLock<Arc<AtomicBool>>>,
) -> Result<()> {
    // Phase 1: Base shim libraries (no native dependencies) — compiled to
    // bytecode ONCE per process, run in every context thereafter.
    K6_BASE_BYTECODE.bootstrap(ctx, K6_BASE_SHIM_BUNDLE).await;

    // Phase 2: Install native module functions (needed by pm-api and k6-shim)
    if let Err(e) = tropel_native::install_all(ctx).await {
        tracing::warn!("Failed to install native modules: {}", e);
    }

    // Phase 3: Bootstrapping libraries that depend on native functions — a
    // SEPARATE bytecode cache (it must run after install_all above).
    K6_NATIVE_BYTECODE
        .bootstrap(ctx, K6_NATIVE_SHIM_BUNDLE)
        .await;

    // Install __tropel_native_sleep (blocks the OS thread, safe under thread-per-core).
    // The sleep burns WALL time; the per-eval JS interrupt deadline must not
    // count it against the JS execution budget, or a stock k6 pacing idiom
    // like `sleep(Math.random()*10)` is interrupted on resume (backlog line
    // 104). Re-arm the deadline after the blocking sleep, like the WS loop.
    let (deadline, max_exec) = ctx.interrupt_deadline_handle();
    let force_stop_sleep = force_stop.clone();
    // Direct scheduler-flag link for the sleep: the instance-local flag only
    // ever flips at LINK time, but a MID-run force-stop flips the scheduler's
    // flag — which the sleep must see even though the VU thread is blocked
    // inside `std::thread::sleep` (no runtime task can run there). Poll both:
    // the local flag covers a force-stop requested before linking, the link
    // covers one requested during the run (backlog: gracefulStop force-stop
    // was advisory only).
    let sched_link_sleep = sched_link.clone();
    ctx.with_ctx(|rq_ctx| {
        let globals = rq_ctx.globals();
        let deadline_sleep = deadline.clone();
        let _ = globals.set(
            "__tropel_native_sleep",
            rquickjs::function::Func::from(move |ms: f64| {
                if ms > 0.0 {
                    // Interruptible sleep: poll the force-stop flags in small
                    // slices. On force-stop, zero the JS interrupt deadline so
                    // the eval is interrupted the moment control returns to JS
                    // (the flag-aware handler unwinds it) — backlog: gracefulStop
                    // force-stop was advisory only.
                    let step = Duration::from_millis(10);
                    let mut remaining = Duration::from_secs_f64(ms / 1000.0);
                    let link_set = || {
                        sched_link_sleep
                            .get()
                            .is_some_and(|f| f.load(Ordering::Acquire))
                    };
                    while remaining > Duration::ZERO {
                        if force_stop_sleep.load(Ordering::Acquire) || link_set() {
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

    // The sleep(seconds) wrapper is included in K6_NATIVE_SHIM_BUNDLE above
    // (js/k6-shim/sleep-shim.js), which is evaluated BEFORE __tropel_native_sleep
    // is installed in the with_ctx block above this comment. That ordering is
    // safe because the shim only dereferences `typeof __tropel_native_sleep` at
    // call time (inside sleep()), never at eval time.
    Ok(())
}

/// Parse a headers JSON string into a `HashMap`, accepting both the plain
/// object form (`{"k":"v"}`) and the Postman/array form
/// (`[{"key":"k","value":"v"}]`). The old code used
/// `serde_json::from_str(...).unwrap_or_default()`, which silently dropped
/// ALL headers whenever the payload wasn't a plain object — a silent
/// correctness divergence (P3 · k6 header-parse divergence).
fn parse_headers_tolerant(json: &str) -> Vec<(String, String)> {
    if json.is_empty() || json == "{}" || json == "[]" {
        return Vec::new();
    }
    // Object form must tolerate non-string values (e.g. {"Content-Length":
    // 123}) — the old `HashMap<String, String>` parse fell through to the
    // array form and returned an EMPTY map whenever any value was
    // non-string, silently dropping every header (backlog P3). Scalars are
    // stringified; null/complex values are skipped. W2 #203: an ordered Vec
    // (declaration order, duplicates preserved).
    if json.trim_start().starts_with('{') {
        if let Ok(map) = serde_json::from_str::<HashMap<String, serde_json::Value>>(json) {
            let mut headers = Vec::new();
            for (k, v) in map {
                match v {
                    serde_json::Value::String(s) => {
                        headers.push((k, s));
                    }
                    serde_json::Value::Number(n) => {
                        headers.push((k, n.to_string()));
                    }
                    serde_json::Value::Bool(b) => {
                        headers.push((k, b.to_string()));
                    }
                    _ => {}
                }
            }
            return headers;
        }
    }
    if json.trim_start().starts_with('[') {
        if let Ok(arr) = serde_json::from_str::<Vec<HashMap<String, serde_json::Value>>>(json) {
            let mut headers = Vec::new();
            for entry in arr {
                let key = entry.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if !key.is_empty() {
                    headers.push((key.to_string(), value.to_string()));
                }
            }
            return headers;
        }
    }
    Vec::new()
}

// ══════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_export_default() {
        let driver = K6Driver;
        let data = br#"export default function() { http.get("https://example.com"); }"#;
        assert!(driver.detect(data));
    }

    #[test]
    fn test_detect_k6_import() {
        let driver = K6Driver;
        let data = br#"import { check } from "k6"; export default function() {}"#;
        assert!(driver.detect(data));
    }

    #[test]
    fn test_detect_postman_not_k6() {
        let driver = K6Driver;
        let data = br#"{"info":{"name":"Test","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(
            !driver.detect(data),
            "Postman JSON should not be detected as k6"
        );
    }

    #[test]
    fn test_driver_id() {
        assert_eq!(K6Driver.id(), "k6");
    }

    #[test]
    fn test_is_typescript_ext() {
        assert!(is_typescript_ext(Path::new("script.ts")));
        assert!(is_typescript_ext(Path::new("script.mts")));
        assert!(is_typescript_ext(Path::new("script.tsx")));
        assert!(!is_typescript_ext(Path::new("script.js")));
        assert!(!is_typescript_ext(Path::new("script.json")));
    }

    // ── ES-module preprocessing (keeps exports) ──

    #[test]
    fn test_module_preprocess_keeps_exports() {
        let code = r#"
            import http from "k6/http";
            import { check } from "k6";
            export const options = { vus: 10, duration: '30s' };
            export function setup() { return {}; }
            export default function() { http.get('https://example.com'); }
        "#;
        let result = preprocess_k6_source_module(code);
        // k6 virtual imports are removed (shim provides globals)
        assert!(
            !result.contains("from \"k6/http\""),
            "k6 import kept: {result}"
        );
        assert!(!result.contains("from \"k6\""), "k6 import kept: {result}");
        // exports are PRESERVED — module eval needs them
        assert!(
            result.contains("export const options"),
            "export const options stripped: {result}"
        );
        assert!(
            result.contains("export function setup"),
            "export function stripped: {result}"
        );
        assert!(
            result.contains("export default function"),
            "export default stripped: {result}"
        );
    }

    #[test]
    fn test_module_preprocess_strips_only_k6_reexports() {
        // Local re-exports are KEPT — the ES-module loader resolves them to
        // files on disk. Only k6-virtual re-exports (no such module on disk,
        // shim provides globals) are stripped.
        let code = r#"
            export { default } from "./other";
            export * from "./helpers";
            export { check } from "k6";
            export * from "k6/http";
            export const options = {};
            export default function() {}
        "#;
        let result = preprocess_k6_source_module(code);
        assert!(
            result.contains("./other"),
            "local re-export stripped: {result}"
        );
        assert!(
            result.contains("./helpers"),
            "local re-export stripped: {result}"
        );
        assert!(
            !result.contains("from \"k6\""),
            "k6 re-export kept: {result}"
        );
        assert!(
            !result.contains("from \"k6/http\""),
            "k6 re-export kept: {result}"
        );
        assert!(
            result.contains("export const options"),
            "options lost: {result}"
        );
        assert!(
            result.contains("export default function"),
            "default export lost: {result}"
        );
    }

    #[test]
    fn test_module_preprocess_strips_multiline_import() {
        // The §0-7 regression: the old line-anchored regex left multi-line
        // imports (`import {\n check\n} from 'k6';`) in place → they reached
        // the module resolver, hard-errored, and killed init before iteration
        // 1 → zero metrics, exit 0. The AST splice must remove them.
        let code = r#"
            import {
                check,
                group,
            } from "k6";
            import http from "k6/http";
            export const options = { vus: 2 };
            export default function() {}
        "#;
        let result = preprocess_k6_source_module(code);
        assert!(
            !result.contains("from \"k6\""),
            "multiline k6 import kept: {result}"
        );
        assert!(
            !result.contains("check,"),
            "multiline k6 import body kept: {result}"
        );
        assert!(
            !result.contains("from \"k6/http\""),
            "k6/http import kept: {result}"
        );
        assert!(
            result.contains("export const options"),
            "options lost: {result}"
        );
    }

    #[test]
    fn test_module_preprocess_strips_import_with_trailing_comment() {
        // `import http from 'k6/http'; // c` — the old line-anchored regex
        // required the line to END after the specifier, so the trailing
        // comment made it survive. The AST splice strips the statement
        // regardless of trailing comment.
        let code =
            "import http from 'k6/http'; // shim provides this\nexport default function() {}\n";
        let result = preprocess_k6_source_module(code);
        assert!(
            !result.contains("from 'k6/http'"),
            "k6 import with trailing comment kept: {result}"
        );
        assert!(
            result.contains("export default function"),
            "default export lost: {result}"
        );
    }

    #[test]
    fn test_module_preprocess_strips_jslib_url_import() {
        // `https://jslib.k6.io/...` imports can't be fetched by the local
        // module resolver — strip them so init doesn't hard-fail. But the
        // APIs they import (randomIntBetween / uuidv4 / htmlReport) must
        // still RESOLVE: the jslib-shim defines them as globals, mirroring
        // the k6/* virtual-module pattern (backlog line 118 — previously
        // the import was deleted with ZERO definitions behind it).
        let code =
            "import { randomIntBetween } from 'https://jslib.k6.io/k6-utils/1.4.0/index.js';\nexport default function() {}\n";
        let result = preprocess_k6_source_module(code);
        assert!(
            !result.contains("jslib.k6.io"),
            "jslib URL import kept: {result}"
        );
        assert!(
            result.contains("export default function"),
            "default export lost: {result}"
        );
    }

    /// Backlog line 118: the jslib symbols must RESOLVE, not just have their
    /// imports deleted — the preprocessor strip is only safe because the
    /// jslib-shim provides the APIs as globals. Boots the real shim bundle
    /// and calls randomIntBetween / uuidv4 / htmlReport / findBetween,
    /// asserting k6-utils + k6-summary behaviour AND randomSeed
    /// determinism (all randomness flows through Math.random).
    #[tokio::test]
    async fn test_jslib_shim_symbols_resolve_and_are_deterministic() {
        let mut ctx = ctx_with_base_shims().await;

        // randomSeed(42) must make randomIntBetween/uuidv4 reproducible
        // (k6's contract — the jslib RNG goes through Math.random, which
        // randomSeed replaces with mulberry32).
        let first = ctx
            .eval(
                "randomSeed(42); randomIntBetween(1, 100) + ',' + uuidv4() + ',' + randomString(8)",
            )
            .await
            .expect("randomSeed + jslib RNG should eval");
        let second = ctx
            .eval(
                "randomSeed(42); randomIntBetween(1, 100) + ',' + uuidv4() + ',' + randomString(8)",
            )
            .await
            .expect("repeat should eval");
        assert_eq!(
            first, second,
            "jslib RNG must be deterministic under randomSeed (k6 parity)"
        );

        // k6-utils semantics: inclusive bounds, array pick, charset, markers.
        // randomString's exact draw depends on the mulberry32 state, so
        // assert its PROPERTIES (length + charset membership), not a pinned
        // output — the RNG stream position is not a contract.
        let out = ctx
            .eval(
                "randomIntBetween(5, 5) + '|' + randomItem(['only']) + '|' + \
                 (function(){ var s = randomString(3, 'abc'); \
                   return s.length + ':' + (/^[abc]{3}$/.test(s) ? 'ok' : s); })() + '|' + \
                 findBetween('a[bc]d', '[', ']', false)",
            )
            .await
            .expect("k6-utils calls should eval");
        assert_eq!(out, "5|only|3:ok|bc", "k6-utils semantics wrong: {out}");

        // findBetween with repeat=true returns ALL matches as an array.
        let repeats = ctx
            .eval("JSON.stringify(findBetween('x[1]y[2]z', '[', ']', true))")
            .await
            .expect("findBetween repeat should eval");
        assert_eq!(
            repeats, "[\"1\",\"2\"]",
            "findBetween repeat wrong: {repeats}"
        );

        // uuidv4 shape: 8-4-4-4-12 hex with version 4 + variant bits.
        let uuid = ctx.eval("uuidv4()").await.expect("uuidv4 should eval");
        assert_eq!(uuid.len(), 36, "uuidv4 shape wrong: {uuid}");
        assert_eq!(&uuid[14..15], "4", "uuidv4 version bit wrong: {uuid}");

        // k6-summary: htmlReport + textSummary must be functions and
        // produce non-empty output from a summary-data object (the
        // handleSummary(data) shape — the silent-degradation path).
        let html = ctx
            .eval(
                "htmlReport({ metrics: { http_reqs: { type: 'counter', contains: 'default', values: { count: 4, rate: 0.8 } } }, state: { iterations: 4, vusMax: 2, http_reqs: 4, checksPassed: 1, checksFailed: 0, testRunDurationMs: 5000 }, thresholds: {} })",
            )
            .await
            .expect("htmlReport should eval");
        assert!(
            html.starts_with("<!DOCTYPE html>") && html.contains("http_reqs"),
            "htmlReport output malformed: {}",
            &html[..html.len().min(120)]
        );
        let text = ctx
            .eval(
                "textSummary({ metrics: { http_reqs: { type: 'counter', contains: 'default', values: { count: 4, rate: 0.8 } } }, state: { iterations: 4, vusMax: 2, http_reqs: 4, checksPassed: 1, checksFailed: 0, testRunDurationMs: 5000 }, thresholds: { 'http_reqs{expected_response:true}': true } })",
            )
            .await
            .expect("textSummary should eval");
        assert!(
            text.contains("iterations") && text.contains("http_reqs"),
            "textSummary output malformed: {}",
            &text[..text.len().min(120)]
        );
    }

    #[test]
    fn test_module_preprocess_keeps_local_import() {
        // `import { x } from "./helpers.js"` must survive preprocessing —
        // the registered module resolver resolves it at eval time.
        let code = "import { triple } from './helpers.js';\nexport default function() {}\n";
        let result = preprocess_k6_source_module(code);
        assert!(
            result.contains("from './helpers.js'"),
            "local import stripped: {result}"
        );
    }

    #[test]
    fn test_module_preprocess_keeps_named_export_block() {
        let code = "const x = 1; export { x };\nexport default function() {}";
        let result = preprocess_k6_source_module(code);
        assert!(
            result.contains("export { x }"),
            "standalone named export block stripped: {result}"
        );
    }

    // ── ES-module evaluation ──

    /// Read an export via a raw rquickjs context (no JsContext needed).
    fn read_export_for_test(source: &str, export: &str) -> Option<String> {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let module = rquickjs::Module::declare(ctx.clone(), "test-script", source).unwrap();
            let (module, promise) = module.eval().unwrap();
            promise.finish::<()>().unwrap();
            let value: rquickjs::Value = match module.get(export) {
                Ok(v) => v,
                Err(_) => return None,
            };
            if value.is_undefined() || value.is_null() {
                return None;
            }
            let json_obj: rquickjs::Object = ctx.globals().get("JSON").unwrap();
            let stringify: rquickjs::Function = json_obj.get("stringify").unwrap();
            let s: String = stringify.call((value,)).unwrap();
            Some(s)
        })
    }

    #[test]
    fn test_module_eval_reads_options_export() {
        let source = r#"
            export const options = { vus: 5, duration: "30s" };
            export default function() {}
        "#;
        let json =
            read_export_for_test(source, "options").expect("options export should be readable");
        let opts: crate::options::K6Options = serde_json::from_str(&json).unwrap();
        assert_eq!(opts.vus, Some(5));
        assert_eq!(opts.duration.as_deref(), Some("30s"));
    }

    #[test]
    fn test_module_eval_missing_export_is_none() {
        let source = "export default function() {}\n";
        assert!(
            read_export_for_test(source, "options").is_none(),
            "missing export should yield None, not an error"
        );
    }

    #[test]
    fn test_module_eval_exports_default_function() {
        let source = "export default function() { return 42; }\n";
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let module = rquickjs::Module::declare(ctx, "test-script", source).unwrap();
            let (module, promise) = module.eval().unwrap();
            promise.finish::<()>().unwrap();
            let f: rquickjs::Function = module.get("default").unwrap();
            let n: i32 = f.call(()).unwrap();
            assert_eq!(n, 42);
        });
    }

    #[test]
    fn test_module_eval_keeps_k6_globals_visible() {
        // k6 shim globals are set on the global object; module code must see
        // them (http.get inside the default function resolves via globals).
        let source = "export default function() { return typeof globalThis; }\n";
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.globals().set("someK6Global", 123).unwrap();
            let module = rquickjs::Module::declare(ctx.clone(), "test-script", source).unwrap();
            let (module, promise) = module.eval().unwrap();
            promise.finish::<()>().unwrap();
            let f: rquickjs::Function = module.get("default").unwrap();
            let s: String = f.call(()).unwrap();
            assert_eq!(s, "object");
            // global is visible from module code
            let src2 = "export default function() { return someK6Global; }\n";
            let module2 = rquickjs::Module::declare(ctx.clone(), "test-script-2", src2).unwrap();
            let (module2, promise2) = module2.eval().unwrap();
            promise2.finish::<()>().unwrap();
            let f2: rquickjs::Function = module2.get("default").unwrap();
            let n: i32 = f2.call(()).unwrap();
            assert_eq!(n, 123);
        });
    }

    #[test]
    fn test_http_params_headers_not_mutated_and_multipart_boundary() {
        // Backlog line 138: serializeK6Body wrote Content-Type onto the
        // CALLER's headers object (every real k6 script hoists params to
        // module scope) — iteration 2 posted a string body still labelled
        // application/json. And the `!headers['Content-Type']` multipart
        // guard was false exactly when the user declared multipart/form-data,
        // so the generated boundary never reached the header → every
        // multipart request unparseable.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            // Stub the native HTTP bridge so no real network is needed;
            // capture exactly what the shim would hand to the bridge.
            ctx.eval::<(), _>(
                r#"
                globalThis.__captured = [];
                globalThis.__tropel_k6_http_request = function (method, url, headersJson, body) {
                    globalThis.__captured.push({ headers: JSON.parse(headersJson), body: body });
                    return { code: 200, status: 200, body: '{}', headers: {}, responseTime: 5 };
                };
            "#,
            )
            .expect("stub should eval");
            ctx.eval::<(), _>(
                r#"
                var params = { headers: {} };
                http.post('https://example.com/a', { a: 1 }, params);   // iter 1: object body
                http.post('https://example.com/b', 'plain text', params); // iter 2: string body
                var mp = { headers: { 'Content-Type': 'multipart/form-data' } };
                http.post('https://example.com/c', { field: 'value' }, mp);
                var mpLower = { headers: { 'content-type': 'multipart/form-data' } };
                http.post('https://example.com/d', { f2: 'v2' }, mpLower);
            "#,
            )
            .expect("script should eval");

            // 1. Caller's module-scope params.headers must NOT be mutated.
            let params_after: String = ctx
                .eval("JSON.stringify(params.headers)")
                .expect("read params.headers");
            assert_eq!(
                params_after, "{}",
                "params.headers was mutated in place: {params_after}"
            );

            // 2. Iteration 2 (string body) must not inherit Content-Type from
            //    iteration 1's object-body mutation.
            let second_headers: String = ctx
                .eval("JSON.stringify(__captured[1].headers)")
                .expect("read captured[1] headers");
            assert!(
                !second_headers.to_lowercase().contains("content-type"),
                "string-body request inherited stale Content-Type: {second_headers}"
            );
            let second_body: String = ctx
                .eval("__captured[1].body")
                .expect("read captured[1] body");
            assert_eq!(second_body, "plain text");

            // 3. Iteration 1 (object body) gets application/x-www-form-urlencoded
            //    (k6 default for object bodies without explicit Content-Type).
            let first_headers: String = ctx
                .eval("JSON.stringify(__captured[0].headers)")
                .expect("read captured[0] headers");
            assert!(
                first_headers
                    .to_lowercase()
                    .contains("x-www-form-urlencoded"),
                "object body not labelled form-urlencoded per k6: {first_headers}"
            );

            // 4. Multipart: the generated boundary MUST reach the header.
            let mp_headers: String = ctx
                .eval("JSON.stringify(__captured[2].headers)")
                .expect("read captured[2] headers");
            assert!(
                mp_headers.contains("boundary="),
                "multipart boundary missing from header: {mp_headers}"
            );
            let mp_body: String = ctx
                .eval("__captured[2].body")
                .expect("read captured[2] body");
            assert!(
                mp_body.contains("----TropelFormBoundary"),
                "multipart body missing framing: {mp_body}"
            );

            // 5. A lowercase `content-type` declaration must be replaced (not
            //    duplicated) — leaving both would send TWO Content-Type
            //    headers, one boundary-less.
            let mp_lower: String = ctx
                .eval("JSON.stringify(__captured[3].headers)")
                .expect("read captured[3] headers");
            assert!(
                mp_lower.contains("boundary="),
                "lowercase multipart boundary missing: {mp_lower}"
            );
            assert!(
                !mp_lower
                    .to_lowercase()
                    .contains("\"content-type\":\"multipart/form-data\""),
                "boundary-less content-type variant leaked into header: {mp_lower}"
            );
        });
    }

    #[test]
    fn test_k6_response_cookies_request_proto_remote_ip_html() {
        // Backlog line 102: res.cookies / res.request / proto / remote_ip /
        // html() were absent — `res.cookies['sid']` threw TypeError. The
        // shim must surface all five from the native bridge result: cookies
        // in k6's shape (name -> array of {name,value,...}), the REQUEST
        // that produced the response (headers/body, not the response's),
        // best-effort proto/remote_ip, and a jQuery-like html() selection.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_k6_http_request = function (method, url, headersJson, body) {
                    return {
                        code: 200, status: 200,
                        body: '<html><body><h1 class="title">Hello</h1><p id="sub">World</p></body></html>',
                        headers: { 'Content-Type': 'text/html' },
                        responseTime: 5,
                        cookies: {
                            sid: [{ name: 'sid', value: 'abc123', domain: 'example.com', path: '/', httpOnly: true, secure: true }]
                        }
                    };
                };
            "#,
            )
            .expect("stub should eval");
            ctx.eval::<(), _>(
                r#"
                var res = http.get('https://example.com/', { headers: { 'X-Request-Id': 'req-7' } });
                globalThis.__out = JSON.stringify([
                    res.cookies.sid ? res.cookies.sid[0].value : null,
                    res.cookies.sid ? res.cookies.sid[0].httpOnly : null,
                    res.request.method,
                    res.request.url,
                    res.request.headers['X-Request-Id'],
                    res.proto,
                    typeof res.remote_ip,
                    res.html().find('.title').text(),
                    res.html().find('#sub').text()
                ]);
            "#,
            )
            .expect("script should eval");
            let out: String = ctx.eval("__out").expect("read __out");
            assert_eq!(
                out,
                concat!(
                    "[\"abc123\",true,\"GET\",\"https://example.com/\",",
                    "\"req-7\",\"HTTP/1.1\",\"string\",\"Hello\",\"World\"]"
                ),
                "res.cookies/request/proto/remote_ip/html() mismatch: {out}"
            );
        });
    }

    #[test]
    fn test_response_headers_keep_canonical_case() {
        // Backlog line 139: the shim force-lowercased response header keys
        // (hk.toLowerCase()), so `res.headers['Content-Type']` — every k6
        // doc example — returned undefined. The native bridge now delivers
        // Go MIME canonical keys (Content-Type, X-Request-Id); the shim must
        // keep them EXACTLY as-is.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_k6_http_request = function (method, url, headersJson, body) {
                    // Canonical MIME form, exactly what client.rs now emits.
                    return { code: 200, status: 200, body: '{}',
                             headers: { 'Content-Type': 'application/json',
                                        'X-Request-Id': 'abc-123',
                                        'Location': 'https://example.com/2' },
                             responseTime: 5 };
                };
                globalThis.__res = http.get('https://example.com/1', {});
                globalThis.__ct = __res.headers['Content-Type'];
                globalThis.__xri = __res.headers['X-Request-Id'];
                globalThis.__loc = __res.headers['Location'];
            "#,
            )
            .expect("script should eval");
            let ct: String = ctx.eval("__ct").expect("read Content-Type");
            assert_eq!(ct, "application/json");
            let xri: String = ctx.eval("__xri").expect("read X-Request-Id");
            assert_eq!(xri, "abc-123");
            let loc: String = ctx.eval("__loc").expect("read Location");
            assert_eq!(loc, "https://example.com/2");
            // The keys must be stored verbatim (canonical), not lowercased.
            let keys: String = ctx
                .eval("JSON.stringify(Object.keys(__res.headers))")
                .expect("read header keys");
            assert!(
                keys.contains("Content-Type"),
                "canonical key Content-Type missing: {keys}"
            );
            assert!(
                keys.contains("X-Request-Id") && keys.contains("Location"),
                "canonical keys missing: {keys}"
            );
            let has_lowercase: bool = ctx
                .eval("Object.keys(__res.headers).indexOf('content-type') !== -1")
                .expect("check lowercase key absent");
            assert!(
                !has_lowercase,
                "lowercase key 'content-type' present instead of canonical: {keys}"
            );
        });
    }

    #[test]
    fn test_http_params_extras_reach_the_bridge() {
        // Backlog line 140: params.tags/auth/redirects/cookies/compression
        // were silently dropped and timeout was parsed then discarded. The
        // shim must pack ALL of them into the 6th bridge arg (the extras JSON
        // string — the closure is arity-capped), with auth translated into
        // the tagged AuthConfig form the Rust side deserializes.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__captured = [];
                globalThis.__tropel_k6_http_request = function (method, url, headersJson, body, responseType, extrasJson) {
                    globalThis.__captured.push({ extras: JSON.parse(extrasJson) });
                    return { code: 200, status: 200, body: '{}', headers: {}, responseTime: 5 };
                };
                http.get('https://example.com/x', {
                    tags: { name: 'my-request', foo: 'bar' },
                    auth: { token: 's3cret' },
                    redirects: 0,
                    compression: 'gzip',
                    timeout: '2s',
                });
            "#,
            )
            .expect("script should eval");

            let extras: String = ctx
                .eval("JSON.stringify(__captured[0].extras)")
                .expect("read captured extras");
            // timeout: '2s' must be parsed to ms and actually shipped.
            assert!(
                extras.contains("\"timeoutMs\":2000"),
                "timeout parsed then dropped: {extras}"
            );
            assert!(extras.contains("\"name\":\"my-request\""), "tags dropped: {extras}");
            assert!(extras.contains("\"foo\":\"bar\""), "tags dropped: {extras}");
            assert!(extras.contains("\"redirects\":0"), "redirects dropped: {extras}");
            assert!(extras.contains("\"compression\":\"gzip\""), "compression dropped: {extras}");
            // auth must be the tagged AuthConfig form ({type:'bearer',…}), not
            // k6's bare {token} — that's what the Rust AuthConfig enum parses.
            assert!(
                extras.contains("\"type\":\"bearer\"") && extras.contains("\"token\":\"s3cret\""),
                "auth not translated to tagged AuthConfig: {extras}"
            );
            // No auth param → null (the bridge must not invent one).
            ctx.eval::<(), _>(
                r#"
                http.get('https://example.com/y', {});
            "#,
            )
            .expect("script should eval");
            let second: String = ctx
                .eval("JSON.stringify(__captured[1].extras)")
                .expect("read second captured extras");
            assert!(second.contains("\"auth\":null"), "auth not null when absent: {second}");
        });
    }

    #[test]
    fn test_parse_k6_extras_coerces_non_string_tag_values() {
        // W2 line 180: parse_k6_extras used v.as_str() with filter_map, so
        // HTTP-path tags silently dropped EVERY non-string value — the
        // canonical k6 idiom http.get(url, {tags: {status: res.status}})
        // lost the whole map while check()/custom metrics coerced. The HTTP
        // paths now share coerce_tag_value with stringify_tag_map_into.
        let extras: serde_json::Value = serde_json::json!({
            "timeoutMs": 1000,
            "tags": {
                "kind": "a",
                "code": 200,
                "ok": true,
                "nil": null
            },
            "auth": null,
            "redirects": 0,
            "compression": "",
            "bodyB64": false
        });
        let p = parse_k6_extras(&extras);
        assert_eq!(
            p.tags.get("kind").map(String::as_str),
            Some("a"),
            "string tag dropped"
        );
        assert_eq!(
            p.tags.get("code").map(String::as_str),
            Some("200"),
            "numeric tag dropped — the {{code: 200}} idiom"
        );
        assert_eq!(
            p.tags.get("ok").map(String::as_str),
            Some("true"),
            "bool tag dropped"
        );
        assert_eq!(
            p.tags.get("nil").map(String::as_str),
            Some(""),
            "null tag must coerce to empty string, not vanish"
        );
        // The whole map must survive — no wholesale drop on first non-string.
        assert_eq!(p.tags.len(), 4, "tag map partially dropped: {:?}", p.tags);
    }

    #[test]
    fn test_http_batch_entries_carry_per_request_params() {
        // W2 line 169: batch entries must carry the same per-request
        // tags/auth/redirects/compression/timeout as the single-request path
        // — ONE canonical extras shape (the old tags_json/auth_json/
        // timeout_ms/body_b64 variants disagreed on four of seven fields).
        // auth rides as an OBJECT parsed by the shared parse_k6_extras.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__captured = [];
                // Echo stub: the new missing-key guard throws on a bare '{}'
                // — return a response for every key the shim sends.
                globalThis.__tropel_k6_http_batch = function (requestsJson) {
                    var reqs = JSON.parse(requestsJson);
                    var out = {};
                    for (var ri = 0; ri < reqs.length; ri++) {
                        out[reqs[ri].key] = { status: 200, code: 200, body: 'ok', headers: {}, timings: {} };
                    }
                    globalThis.__captured.push(reqs);
                    return JSON.stringify(out);
                };
                http.batch([
                    ['GET', 'https://example.com/1', null, {
                        tags: { name: 'b1' },
                        auth: { username: 'u', password: 'p' },
                        redirects: 0,
                        timeout: '1s',
                    }],
                    { 'https://example.com/2': { method: 'POST', body: 'x' } },
                ]);
            "#,
            )
            .expect("script should eval");

            let entries: String = ctx
                .eval("JSON.stringify(__captured[0])")
                .expect("read batch entries");
            // Entry 1: every per-request param present in the canonical
            // extras wire shape (W2 line 169).
            assert!(
                entries.contains("\\\"tags\\\":{\\\"name\\\":\\\"b1\\\"}"),
                "batch tags missing: {entries}"
            );
            assert!(
                entries.contains("\\\"redirects\\\":0"),
                "batch redirects missing: {entries}"
            );
            assert!(
                entries.contains("\\\"timeoutMs\\\":1000"),
                "batch timeout missing: {entries}"
            );
            // auth rides as an OBJECT (not a double-encoded string) — parsed
            // by the shared parse_k6_extras like the single bridge.
            assert!(
                entries.contains("\\\"auth\\\":{\\\"type\\\":\\\"basic\\\""),
                "batch auth not translated to tagged AuthConfig: {entries}"
            );
            // Entry 2 (no params): no auth, no tags, default redirects.
            assert!(
                entries.contains("\\\"auth\\\":null"),
                "batch auth not null when absent: {entries}"
            );
        });
    }

    #[test]
    fn test_http_batch_object_form_params_duplicate_keys_and_loud_failures() {
        // Backlog §3: the batch path missed every single-request fix.
        // - object-form entries dropped auth/redirects/compression/cookies
        // - object-form forced timeout '30s' (overriding the global)
        // - duplicate `name` keys collided (one real response lost)
        // - http.batch('abc') returned {} silently
        // - body: req.body || null dropped 0/''/false
        // - a missing native key fabricated a silent status-0 response
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__captured = [];
                // Echo stub: return a response for EVERY key the shim sends —
                // the new missing-key guard throws otherwise (that guard is
                // what the separate '{}' stub below exercises).
                globalThis.__tropel_k6_http_batch = function (requestsJson) {
                    var reqs = JSON.parse(requestsJson);
                    var out = {};
                    for (var ri = 0; ri < reqs.length; ri++) {
                        out[reqs[ri].key] = { status: 200, code: 200, body: 'ok', headers: {}, timings: {} };
                    }
                    globalThis.__captured.push(reqs);
                    return JSON.stringify(out);
                };
                // Object-form entry: every per-request param must survive;
                // falsy body 0 must NOT become null; no forced '30s'.
                http.batch([{
                    url: 'https://example.com/o1',
                    method: 'POST',
                    body: 0,
                    params: {
                        auth: { username: 'u', password: 'p' },
                        redirects: 0,
                        compression: 'gzip',
                        cookies: { sid: 's1' },
                    },
                }]);
                // Duplicate names: array input keys by INDEX (W2 line 169),
                // so both same-named entries survive the response map
                // positionally — no name collision, no dedupe needed.
                http.batch([
                    { url: 'https://example.com/d1', name: 'dup' },
                    { url: 'https://example.com/d2', name: 'dup' },
                ]);
                // http.batch('abc') must throw, not return an object silently.
                var threwAbc = false;
                try { http.batch('abc'); } catch (e) { threwAbc = true; }
                // A missing native key must throw, not fabricate status-0 —
                // this needs a bare '{}' stub (the echo stub never misses).
                var threwMissing = false;
                globalThis.__tropel_k6_http_batch = function () { return '{}'; };
                try { http.batch([['GET', 'https://example.com/m']]); } catch (e) { threwMissing = true; }
                globalThis.__threwAbc = threwAbc;
                globalThis.__threwMissing = threwMissing;
            "#,
            )
            .expect("script should eval");

            // First call: object-form params all present + falsy body kept.
            let first: String = ctx
                .eval("JSON.stringify(__captured[0])")
                .expect("read first batch call");
            // W2 line 169: auth/redirects/compression/timeout ride in the
            // canonical extras shape.
            assert!(
                first.contains("\\\"auth\\\":{\\\"type\\\":\\\"basic\\\""),
                "object-form auth dropped: {first}"
            );
            assert!(first.contains("\\\"redirects\\\":0"), "object-form redirects dropped: {first}");
            assert!(
                first.contains("\\\"compression\\\":\\\"gzip\\\""),
                "object-form compression dropped: {first}"
            );
            // cookies are merged into the Cookie header by normalizeK6Request.
            assert!(
                first.contains("sid=s1"),
                "object-form cookies not merged into Cookie header: {first}"
            );
            assert!(
                first.contains("\"body\":\"0\""),
                "falsy body 0 dropped to null: {first}"
            );
            assert!(
                first.contains("\\\"timeoutMs\\\":0"),
                "object-form forced a timeout instead of leaving it 0: {first}"
            );

            // Second call: array input keys by INDEX (the caller's key) — two
            // same-named entries no longer collide in the response map; both
            // responses survive positionally.
            let second: String = ctx
                .eval("JSON.stringify(__captured[1].map(function(e){return e.key;}))")
                .expect("read second batch call keys");
            assert_eq!(
                second, "[\"0\",\"1\"]",
                "array batch keys must be positional indices: {second}"
            );

            assert!(
                ctx.eval::<bool, _>("__threwAbc").expect("read threwAbc"),
                "http.batch('abc') must throw, not return an object silently"
            );
            assert!(
                ctx.eval::<bool, _>("__threwMissing").expect("read threwMissing"),
                "missing native key must throw, not fabricate status-0"
            );
        });
    }

    #[test]
    fn test_compress_k6_body_gzip_and_deflate() {
        // Backlog line 140: compression param was dropped; the helper must
        // produce valid gzip/deflate and pass through the uncompressed bytes
        // (no-op) for an unsupported algorithm.
        let data = b"{\"hello\":\"world\"}".repeat(3);
        let gz = compress_k6_body("gzip", &data).expect("gzip should compress");
        assert_ne!(gz, data, "gzip must change the bytes");
        let mut dec = flate2::read::GzDecoder::new(gz.as_slice());
        use std::io::Read;
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, data, "gzip round-trip must restore the body");

        let df = compress_k6_body("deflate", &data).expect("deflate should compress");
        let mut dec2 = flate2::read::DeflateDecoder::new(df.as_slice());
        let mut out2 = Vec::new();
        dec2.read_to_end(&mut out2).unwrap();
        assert_eq!(out2, data, "deflate round-trip must restore the body");

        // Unsupported algorithm → None (k6 proceeds uncompressed).
        assert!(
            compress_k6_body("br", &data).is_none(),
            "unsupported must be None"
        );
        assert!(
            compress_k6_body("gzip", b"").is_none(),
            "empty body must be None"
        );
    }

    #[test]
    fn test_pm_send_request_headers_not_mutated_and_multipart_boundary() {
        // Backlog line 138 (pm.js half): pm.sendRequest aliased
        // `headers = options.headers`, so the formdata branch stamped the
        // generated Content-Type onto the CALLER's object, and the
        // `!headers['Content-Type']` guard dropped the boundary whenever the
        // caller declared multipart/form-data.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__pm_captured = [];
                globalThis.__tropel_pm_send_request = function (method, url, headersJson, body) {
                    globalThis.__pm_captured.push({ headers: JSON.parse(headersJson), body: body });
                    return JSON.stringify({ code: 200, body: '{}', headers: {}, responseTime: 5 });
                };
                var opts = { url: 'https://example.com/u', method: 'POST',
                             headers: { 'Content-Type': 'multipart/form-data' },
                             body: { mode: 'formdata', formdata: [{ key: 'field', value: 'value' }] } };
                pm.sendRequest(opts);
                var opts2 = { url: 'https://example.com/v', method: 'POST',
                              headers: { 'Content-Type': 'multipart/form-data' },
                              body: { mode: 'formdata', formdata: [{ key: 'x', value: 'y' }] } };
                pm.sendRequest(opts2);
            "#,
            )
            .expect("script should eval");
            // Caller's options.headers must not be mutated.
            let caller_headers: String = ctx
                .eval("JSON.stringify(opts.headers)")
                .expect("read opts.headers");
            assert_eq!(
                caller_headers, "{\"Content-Type\":\"multipart/form-data\"}",
                "caller's options.headers was mutated: {caller_headers}"
            );
            // Generated boundary must reach the request header.
            let first: String = ctx
                .eval("JSON.stringify(__pm_captured[0].headers)")
                .expect("read captured headers");
            assert!(
                first.contains("boundary="),
                "pm multipart boundary missing from header: {first}"
            );
            // A fresh boundary per call (not reused/stale across iterations).
            let second: String = ctx
                .eval("JSON.stringify(__pm_captured[1].headers)")
                .expect("read captured headers");
            assert!(
                second.contains("boundary=") && second != first,
                "pm boundary reused across calls: {second}"
            );
        });
    }

    #[test]
    fn test_pm_send_request_transport_failure_fires_error_callback() {
        // Backlog line 147: pm.sendRequest reported transport failures
        // (DNS/conn refused/timeout) as SUCCESS — callback(null, {code: 0}) —
        // so the universal `if (err)` guard in user scripts never fired and
        // auth-token-fetch retry logic was dead. The bridge now stamps an
        // `error` field; the shim must surface it as the first (err) arg.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Transport failure — what the real bridge returns on a
                // connection error (code 0 + error field).
                globalThis.__tropel_pm_send_request = function () {
                    return JSON.stringify({
                        error: 'Request failed: error sending request for url (http://down:9/)',
                        code: 0, statusText: '', body: '', headers: {}, responseTime: 0
                    });
                };
                globalThis.__cb_args = null;
                pm.sendRequest('http://down:9/', function (err, resp) {
                    globalThis.__cb_args = { err: err ? err.message : null, resp: resp };
                });

                // A healthy response must still arrive via (null, resp).
                globalThis.__tropel_pm_send_request = function () {
                    return JSON.stringify({ code: 200, statusText: 'OK', body: '{}', headers: {},
                                            responseTime: 5 });
                };
                globalThis.__ok_args = null;
                pm.sendRequest('http://ok/', function (err, resp) {
                    globalThis.__ok_args = { err: err ? err.message : null, resp: resp };
                });
            "#,
            )
            .expect("script should eval");

            let err_msg: String = ctx
                .eval("globalThis.__cb_args.err")
                .expect("read err message");
            assert!(
                err_msg.contains("Request failed"),
                "transport failure must fire callback(err, null): {err_msg}"
            );
            let resp_null: bool = ctx
                .eval("globalThis.__cb_args.resp === null")
                .expect("read resp nullity");
            assert!(resp_null, "err path must pass null response");

            let ok_err: bool = ctx
                .eval("globalThis.__ok_args.err === null")
                .expect("read ok err");
            assert!(ok_err, "healthy response must fire callback(null, resp)");
            let ok_code: i64 = ctx
                .eval("globalThis.__ok_args.resp.code")
                .expect("read ok code");
            assert_eq!(ok_code, 200, "success path code must round-trip");
        });
    }

    #[test]
    fn test_pm_send_request_callback_runs_once_and_real_error_propagates() {
        // W1-B line 149: pm.sendRequest called the callback TWICE and
        // replaced the real error. callback(null, resp) was invoked INSIDE
        // the try, so a throw from the user's callback (a failing pm.expect
        // — the entire point) was caught by the sibling catch and the
        // callback re-entered with a bogus "Failed to parse sendRequest
        // response" error. The callback must run exactly once and its real
        // error must surface unchanged.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_send_request = function () {
                    return JSON.stringify({ code: 200, statusText: 'OK', body: '{}',
                                            headers: {}, responseTime: 5 });
                };
                globalThis.__cb_runs = 0;
                var thrown = null;
                try {
                    pm.sendRequest('http://ok/', function (err, resp) {
                        globalThis.__cb_runs++;
                        // A failing assertion inside the callback — the
                        // canonical auth-token-fetch pattern.
                        pm.expect(resp.code).to.eql(999);
                    });
                } catch (e) {
                    thrown = e.message;
                }
                globalThis.__thrown = thrown;
            "#,
            )
            .expect("script should eval");
            let runs: i64 = ctx
                .eval("globalThis.__cb_runs")
                .expect("read callback run count");
            assert_eq!(
                runs, 1,
                "callback must run exactly ONCE, not be re-entered by the sibling catch"
            );
            let thrown: String = ctx
                .eval("globalThis.__thrown")
                .expect("read thrown message");
            assert!(
                thrown.contains("eql 999"),
                "the REAL assertion error must propagate: {thrown}"
            );
            assert!(
                !thrown.contains("Failed to parse sendRequest response"),
                "the bogus parse error must NOT replace the real error: {thrown}"
            );
        });
    }

    #[test]
    fn test_pm_response_members_are_value_properties() {
        // Backlog line 143: pm.response.code/status/responseTime/headers/
        // cookies are VALUE properties in Postman, not functions. The old
        // function-object form made `pm.expect(pm.response.code).to.eql(200)`
        // compare a Function to 200 (never eql) and `pm.response.headers.get('X')`
        // throw a TypeError (headers was a function). Only text()/json() are
        // methods in Postman.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            // Stub the response bridges with known values.
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__tropel_pm_response_status = function () { return 'OK'; };
                globalThis.__tropel_pm_response_time = function () { return 42.5; };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                globalThis.__tropel_pm_response_header = function (key) {
                    if (String(key).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_cookies = function () {
                    return { session: 'abc123' };
                };
                globalThis.__type_code = typeof pm.response.code;
                globalThis.__type_headers = typeof pm.response.headers;
                globalThis.__type_cookies = typeof pm.response.cookies;
                globalThis.__code = pm.response.code;
                globalThis.__status = pm.response.status;
                globalThis.__rtime = pm.response.responseTime;
                globalThis.__hdr = pm.response.headers.get('content-type');
                globalThis.__hdr_all = JSON.stringify(pm.response.headers.toObject());
                globalThis.__ck = pm.response.cookies[0].value;
                globalThis.__ck_get = pm.response.cookies.get('session').value;
                // The two canonical idioms that the function-form broke.
                // Coerce to String so the result is always eval-able as Rust
                // String (a raw boolean/string mix fails String::from_js).
                globalThis.__eql_ok = String((function () {
                    try { pm.expect(pm.response.code).to.eql(200); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
                globalThis.__status_ok = String((function () {
                    try { pm.expect(pm.response).to.have.status(200); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__type_code").unwrap(),
                "number",
                "pm.response.code must be a number value, not a function"
            );
            assert_eq!(
                ctx.eval::<String, _>("__type_headers").unwrap(),
                "object",
                "pm.response.headers must be a Headers object"
            );
            assert_eq!(
                ctx.eval::<String, _>("__type_cookies").unwrap(),
                "object",
                "pm.response.cookies must be a list"
            );
            assert_eq!(ctx.eval::<i64, _>("__code").unwrap(), 200);
            assert_eq!(ctx.eval::<String, _>("__status").unwrap(), "OK");
            assert_eq!(ctx.eval::<f64, _>("__rtime").unwrap(), 42.5);
            assert_eq!(
                ctx.eval::<String, _>("__hdr").unwrap(),
                "application/json",
                "pm.response.headers.get() must work (was a TypeError)"
            );
            assert!(
                ctx.eval::<String, _>("__hdr_all")
                    .unwrap()
                    .contains("Content-Type"),
                "headers.toObject() must expose the map"
            );
            assert_eq!(
                ctx.eval::<String, _>("__ck").unwrap(),
                "abc123",
                "cookies[0].value must read as a value property"
            );
            assert_eq!(
                ctx.eval::<String, _>("__ck_get").unwrap(),
                "abc123",
                "cookies.get('session') must find by name"
            );
            assert_eq!(
                ctx.eval::<String, _>("__eql_ok").unwrap(),
                "true",
                "pm.expect(pm.response.code).to.eql(200) must pass (compared a Function before)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__status_ok").unwrap(),
                "true",
                "pm.expect(pm.response).to.have.status(200) must pass"
            );
        });
    }

    #[test]
    fn test_bru_res_get_status_returns_code_and_status_text() {
        // TROPEL_PARITY_BRUNO.md §0: res.getStatus() returned the status TEXT
        // ("OK") while Bruno's docs say it returns the numeric code — the
        // canonical `expect(res.getStatus()).to.equal(200)` idiom silently
        // failed. res.getStatusText() is the member that returns the text, and
        // it didn't exist.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/bru.js"))
                .expect("bru shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__tropel_pm_response_status = function () { return 'OK'; };
                globalThis.__status_code = res.getStatus();
                globalThis.__status_text = res.getStatusText();
                globalThis.__status_code_type = typeof res.getStatus();
                globalThis.__eql_ok = String((function () {
                    try { return res.getStatus() === 200; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
                // Non-200 trial: proves the bridge value is actually read
                // (a hardcoded 200 constant could not distinguish itself).
                globalThis.__tropel_pm_response_code = function () { return 404; };
                globalThis.__tropel_pm_response_status = function () { return 'Not Found'; };
                globalThis.__status_404 = res.getStatus();
                globalThis.__status_404_text = res.getStatusText();
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<i64, _>("__status_code").unwrap(),
                200,
                "res.getStatus() must return the numeric code (was the text 'OK')"
            );
            assert_eq!(
                ctx.eval::<String, _>("__status_code_type").unwrap(),
                "number",
                "res.getStatus() must be a number, not a string"
            );
            assert_eq!(
                ctx.eval::<String, _>("__status_text").unwrap(),
                "OK",
                "res.getStatusText() must return the status text"
            );
            assert_eq!(
                ctx.eval::<String, _>("__eql_ok").unwrap(),
                "true",
                "res.getStatus() === 200 must hold (the canonical Bruno assertion)"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__status_404").unwrap(),
                404,
                "res.getStatus() must reflect a non-200 bridge value (404), proving the bridge is read"
            );
            assert_eq!(
                ctx.eval::<String, _>("__status_404_text").unwrap(),
                "Not Found",
                "res.getStatusText() must reflect the 404 status text"
            );
        });
    }

    #[test]
    fn test_bru_runtime_vars_route_through_variables_store() {
        // TROPEL_PARITY_BRUNO.md §2: bru.getVar/setVar used to map to the
        // COLLECTION vars bridges — but Bruno's getVar/setVar are RUNTIME-scope
        // (in-memory, per collection run). The mis-scoping silently broke the
        // core request-chaining idiom (setVar in one request, getVar in the
        // next). The shim now routes through __tropel_pm_variables_* (the same
        // fall-through store pm.variables uses), and the family
        // hasVar/deleteVar/getAllVars/deleteAllVars is exposed.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/bru.js"))
                .expect("bru shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Route-spy: the VARIABLES bridge must be the one called.
                var __vars_calls = [];
                globalThis.__tropel_pm_variables_get = function (key) {
                    __vars_calls.push('get:' + key);
                    if (key === 'userId') return '42';
                    if (key === 'token') return '"abc"';
                    return null;
                };
                globalThis.__tropel_pm_variables_set = function (key, value) {
                    __vars_calls.push('set:' + key + '=' + value);
                };
                globalThis.__tropel_pm_variables_unset = function (key) {
                    __vars_calls.push('unset:' + key);
                };
                // The COLLECTION bridge must NOT be touched.
                globalThis.__tropel_pm_collection_vars_get = function () {
                    throw new Error('getVar must not read collection vars');
                };
                globalThis.__tropel_pm_collection_vars_set = function () {
                    throw new Error('setVar must not write collection vars');
                };
                globalThis.__tropel_pm_collection_vars_to_object = function () {
                    throw new Error('getAllVars must not read collection vars');
                };
                // W2 line 182: getAllVars reads the LOCAL store setVar writes
                // (the old code read collection_vars while setVar wrote
                // local_vars — a runtime var never appeared in getAllVars).
                globalThis.__tropel_pm_variables_to_object = function () {
                    return { userId: '42', token: '"abc"', flag: 'true' };
                };
                var v1 = bru.getVar('userId');
                bru.setVar('userId', 42);
                var h1 = bru.hasVar('userId');
                var h2 = bru.hasVar('missing');
                bru.deleteVar('token');
                var all = bru.getAllVars();
                bru.deleteAllVars();
                globalThis.__v1 = v1;
                globalThis.__v1_type = typeof v1;
                globalThis.__h1 = String(h1);
                globalThis.__h2 = String(h2);
                globalThis.__all_keys = Object.keys(all).sort().join(',');
                globalThis.__all_userId = all.userId;
                globalThis.__calls = __vars_calls.join('|');
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__v1_type").unwrap(),
                "number",
                "bru.getVar must JSON.parse the bridge value (42 → number, not '42' string)"
            );
            assert_eq!(ctx.eval::<i64, _>("__v1").unwrap(), 42);
            assert_eq!(
                ctx.eval::<String, _>("__h1").unwrap(),
                "true",
                "bru.hasVar must be true for an existing var"
            );
            assert_eq!(
                ctx.eval::<String, _>("__h2").unwrap(),
                "false",
                "bru.hasVar must be false for a missing var"
            );
            assert_eq!(
                ctx.eval::<String, _>("__all_keys").unwrap(),
                "flag,token,userId",
                "bru.getAllVars must return all runtime vars (JSON-parsed)"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__all_userId").unwrap(),
                42,
                "bru.getAllVars must include the runtime var set via setVar as a JSON-parsed NUMBER (request-chaining reads it back)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__calls").unwrap(),
                "get:userId|set:userId=42|get:userId|get:missing|unset:token|unset:userId|unset:token|unset:flag",
                "getVar/setVar must hit the variables bridges (not collection), hasVar one lookup each, and deleteAllVars must unset every key"
            );
        });
    }

    #[test]
    fn test_bru_get_env_var_unquotes_json_encoded() {
        // W2 line 182: the environment_get bridge returns values JSON-encoded
        // ("https://api.x" WITH literal quotes) so the correct JS type
        // round-trips — the old bru.getEnvVar returned the raw string, so
        // every URL built from the var was malformed. It must JSON.parse like
        // pm.environment.get.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/bru.js"))
                .expect("bru shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_environment_get = function (key) {
                    if (key === 'baseUrl') return '"https://api.example.com"';
                    if (key === 'port') return '8080';
                    return null;
                };
                globalThis.__base = bru.getEnvVar('baseUrl');
                globalThis.__port = bru.getEnvVar('port');
                globalThis.__missing_null = bru.getEnvVar('nope') === null;
                // The canonical Bruno idiom: URL built from an unquoted var.
                globalThis.__url = bru.getEnvVar('baseUrl') + '/users';
                globalThis.__base_type = typeof bru.getEnvVar('baseUrl');
            "#,
            )
            .expect("script should eval");
            assert_eq!(
                ctx.eval::<String, _>("__base").unwrap(),
                "https://api.example.com",
                "getEnvVar must strip the JSON quotes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__url").unwrap(),
                "https://api.example.com/users",
                "a URL built from getEnvVar must be well-formed (was malformed with literal quotes)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__base_type").unwrap(),
                "string",
                "getEnvVar must return a string, not a quoted JSON literal"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__port").unwrap(),
                8080,
                "JSON-encoded number restores as a number, like pm.environment.get"
            );
            assert!(
                ctx.eval::<bool, _>("__missing_null").unwrap(),
                "getEnvVar must return null on a miss"
            );
        });
    }

    #[test]
    fn test_bru_assert_records_via_bool_bridge() {
        // W2 line 182: bru.assert passed an INT (passed ? 1 : 0) where the
        // __tropel_pm_test bridge takes a BOOL — rquickjs 0.12 has no bool
        // coercion, so every call THREW (pm.js:506-508 warns about the rule).
        // It also passed only 2 of the bridge's 3 args. It now passes a real
        // bool + the empty tags string.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/bru.js"))
                .expect("bru shim should eval");
            ctx.eval::<(), _>(
                r#"
                var __calls = [];
                globalThis.__tropel_pm_test = function (name, passed, tags) {
                    __calls.push(name + '|' + passed + '|' + typeof passed + '|' + tags);
                };
                bru.assert('1 === 1', 'one equals one');
                bru.assert('1 === 2', 'one equals two');
                // String form: the expression text is the default name.
                bru.assert('2 > 1');
                globalThis.__calls = __calls.join('\n');
                globalThis.__n = __calls.length;
            "#,
            )
            .expect("script should eval");
            let calls: String = ctx.eval("__calls").expect("read recorded calls");
            let lines: Vec<&str> = calls.lines().collect();
            assert_eq!(
                lines.len(),
                3,
                "three bru.assert calls must record: {calls}"
            );
            // Bool, not int, and the 3rd (tags) arg present.
            assert!(
                lines[0].contains("|true|boolean|"),
                "pass must be a real bool: {calls}"
            );
            assert!(
                lines[1].contains("|false|boolean|"),
                "fail must be a real bool: {calls}"
            );
            assert!(
                lines[0].contains("one equals one") && lines[2].contains("2 > 1"),
                "name must carry the message or the expression: {calls}"
            );
        });
    }

    #[test]
    fn test_bru_collection_vars_family() {
        // TROPEL_PARITY_BRUNO.md §2: Bruno exposes the collection scope via
        // getCollectionVar/setCollectionVar/hasCollectionVar/delete* — the
        // shim only had getVar/setVar (aliased to collection). The explicit
        // family now maps to the __tropel_pm_collection_vars_* bridges.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/bru.js"))
                .expect("bru shim should eval");
            ctx.eval::<(), _>(
                r#"
                var __store = { baseUrl: '"https://api.example.com"', retries: '3' };
                globalThis.__tropel_pm_collection_vars_get = function (key) {
                    return Object.prototype.hasOwnProperty.call(__store, key) ? __store[key] : null;
                };
                globalThis.__tropel_pm_collection_vars_set = function (key, value) {
                    __store[key] = value;
                };
                globalThis.__tropel_pm_collection_vars_has = function (key) {
                    return Object.prototype.hasOwnProperty.call(__store, key);
                };
                globalThis.__tropel_pm_collection_vars_unset = function (key) {
                    delete __store[key];
                };
                globalThis.__tropel_pm_collection_vars_to_object = function () {
                    var out = {};
                    for (var k in __store) out[k] = __store[k];
                    return out;
                };
                var b1 = bru.getCollectionVar('baseUrl');
                bru.setCollectionVar('token', '"abc"');
                var h1 = bru.hasCollectionVar('baseUrl');
                var h2 = bru.hasCollectionVar('missing');
                bru.deleteCollectionVar('retries');
                var after = JSON.stringify(__store);
                bru.deleteAllCollectionVars();
                var after_all = JSON.stringify(__store);
                globalThis.__b1_type = typeof b1;
                globalThis.__h1 = String(h1);
                globalThis.__h2 = String(h2);
                globalThis.__after = after;
                globalThis.__after_all = after_all;
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__b1_type").unwrap(),
                "string",
                "bru.getCollectionVar must JSON.parse the bridge value (\"https://…\" is a string, not an object)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__h1").unwrap(),
                "true",
                "bru.hasCollectionVar must be true for an existing collection var"
            );
            assert_eq!(
                ctx.eval::<String, _>("__h2").unwrap(),
                "false",
                "bru.hasCollectionVar must be false for a missing var"
            );
            assert_eq!(
                ctx.eval::<String, _>("__after").unwrap(),
                "{\"baseUrl\":\"\\\"https://api.example.com\\\"\",\"token\":\"\\\"abc\\\"\"}",
                "setCollectionVar must add, deleteCollectionVar must remove (the stub store keeps JSON-encoded values, hence the escaped quotes)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__after_all").unwrap(),
                "{}",
                "bru.deleteAllCollectionVars must clear every collection var"
            );
        });
    }

    #[test]
    fn test_pm_expect_eql_is_deep_equal() {
        // Backlog line 144: pm.expect(...).to.eql() was strict === while
        // .equal() delegated to eql — inverted vs chai. Deep-equal means a
        // freshly-parsed response body can eql a literal; strict equal must
        // NOT deep-compare (=== semantics).
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__deep_ok = String((function () {
                    // Key order differs — === would fail, deep eql must pass.
                    try { pm.expect({ b: 2, a: { x: [1, 2] } }).to.eql({ a: { x: [1, 2] }, b: 2 }); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
                globalThis.__deep_neg = String((function () {
                    // .not.to.eql with differing values must pass.
                    try { pm.expect({ a: 1 }).not.to.eql({ a: 2 }); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
                globalThis.__strict_ok = String((function () {
                    // .equal stays strict: same-primitive passes.
                    try { pm.expect(200).to.equal(200); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
                globalThis.__strict_distinct = String((function () {
                    // .equal must NOT deep-compare objects (chai semantics).
                    try { pm.expect({ a: 1 }).to.equal({ a: 1 }); return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__eql_json_body = String((function () {
                    // The canonical Postman idiom: parsed body vs literal.
                    var body = JSON.parse('{"userId":1,"name":"ada"}');
                    try { pm.expect(body).to.eql({ name: 'ada', userId: 1 }); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                })());
                globalThis.__json_body_ok = String((function () {
                    // to.have.jsonBody must deep-compare too (was key-order
                    // sensitive JSON.stringify comparison).
                    var saved = globalThis.__tropel_pm_response_json;
                    globalThis.__tropel_pm_response_json = function () {
                        return '{"name":"ada","userId":1}';
                    };
                    try { pm.expect({}).to.have.jsonBody({ userId: 1, name: 'ada' }); return true; }
                    catch (e) { return 'threw: ' + e.message; }
                    finally { globalThis.__tropel_pm_response_json = saved; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(ctx.eval::<String, _>("__deep_ok").unwrap(), "true", "eql must deep-compare (key order irrelevant)");
            assert_eq!(ctx.eval::<String, _>("__deep_neg").unwrap(), "true", "not.to.eql must deep-compare");
            assert_eq!(ctx.eval::<String, _>("__strict_ok").unwrap(), "true", "equal must still accept identical primitives");
            assert_eq!(ctx.eval::<String, _>("__strict_distinct").unwrap(), "threw", "equal must stay strict (no deep-compare)");
            assert_eq!(ctx.eval::<String, _>("__eql_json_body").unwrap(), "true", "parsed JSON body must eql a literal object");
            assert_eq!(ctx.eval::<String, _>("__json_body_ok").unwrap(), "true", "to.have.jsonBody must deep-compare (key order insensitive)");
        });
    }

    #[test]
    fn test_eql_typed_and_circular_values_compare_by_value() {
        // Backlog line 85: Date/Set/Map/RegExp collapsed to Object.keys() = []
        // so ANY two instances compared equal (pm.expect(new Date(1))
        // .to.eql(new Date(2)) passed). They now compare by value; circular
        // structures no longer overflow the stack. The same fix landed in all
        // three deep-equal implementations (pm.js deepEqual, chai-shim
        // jsDeepEqual, lodash-shim isEqualDeep) — this test locks all three.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/lodash/lodash-shim.js"))
                .expect("lodash shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__r = {};
                function trial(key, fn) {
                    globalThis.__r[key] = String((function () {
                        try { fn(); return true; }
                        catch (e) { return e.name === 'RangeError' ? 'stack-overflow' : 'threw'; }
                    })());
                }
                trial('pm_date_same', function () { pm.expect(new Date(1700000000000)).to.eql(new Date(1700000000000)); });
                trial('pm_date_diff', function () { pm.expect(new Date(1700000000000)).to.eql(new Date(1)); });
                trial('pm_re_same', function () { pm.expect(/a+b/i).to.eql(/a+b/i); });
                trial('pm_re_flags', function () { pm.expect(/a+b/i).to.eql(/a+b/g); });
                trial('pm_re_flag_order', function () { pm.expect(/a+b/gi).to.eql(/a+b/ig); });
                trial('pm_set_order', function () { pm.expect(new Set([1, 2, 3])).to.eql(new Set([3, 1, 2])); });
                trial('pm_set_diff', function () { pm.expect(new Set([1, 2])).to.eql(new Set([1, 3])); });
                trial('pm_map_same', function () { pm.expect(new Map([['a', 1]])).to.eql(new Map([['a', 1]])); });
                trial('pm_map_diff', function () { pm.expect(new Map([['a', 1]])).to.eql(new Map([['a', 2]])); });
                trial('pm_cycle_same', function () {
                    var a = { x: 1 }; a.self = a;
                    var b = { x: 1 }; b.self = b;
                    pm.expect(a).to.eql(b);
                });
                trial('pm_cycle_diff', function () {
                    var a = { x: 1 }; a.self = a;
                    var b = { x: 1 }; b.self = {};
                    pm.expect(a).to.eql(b);
                });
                trial('chai_set_same', function () { chai.expect(new Set([1])).to.eql(new Set([1])); });
                trial('chai_set_diff', function () { chai.expect(new Set([1])).to.eql(new Set([9])); });
                trial('lodash_date_same', function () {
                    if (!_.isEqual(new Date(1700000000000), new Date(1700000000000))) throw new Error('not equal');
                });
                trial('lodash_date_diff', function () {
                    if (_.isEqual(new Date(1), new Date(2))) throw new Error('equal');
                });
                trial('lodash_cycle', function () {
                    var a = { x: 1 }; a.self = a;
                    if (!_.isEqual(a, a)) throw new Error('not equal');
                });
                trial('pm_map_self', function () {
                    var m = new Map(); m.set('self', m);
                    var m2 = new Map(); m2.set('self', m2);
                    pm.expect(m).to.eql(m2);
                });
                trial('pm_map_self_diff', function () {
                    var m = new Map(); m.set('self', m);
                    var m2 = new Map(); m2.set('self', {});
                    pm.expect(m).to.eql(m2);
                });
                trial('pm_set_self', function () {
                    var s = new Set(); s.add(s);
                    var s2 = new Set(); s2.add(s2);
                    pm.expect(s).to.eql(s2);
                });
                trial('pm_map_key_diff', function () {
                    // Same-size self-referential Maps with different keys: must
                    // exercise the guarded mate-matching failure pop (not the
                    // instanceof type-check short-circuit).
                    var m = new Map(); m.set('a', m);
                    var m2 = new Map(); m2.set('b', m2);
                    pm.expect(m).to.eql(m2);
                });
                "#,
            )
            .expect("script should eval");

            let r = |k: &str| ctx.eval::<String, _>(format!("__r['{}']", k)).unwrap();
            assert_eq!(r("pm_date_same"), "true", "equal Dates must eql");
            assert_eq!(r("pm_date_diff"), "threw", "different Dates must NOT eql");
            assert_eq!(r("pm_re_same"), "true", "same RegExp source+flags must eql");
            assert_eq!(r("pm_re_flags"), "threw", "differing RegExp flags must NOT eql");
            assert_eq!(r("pm_re_flag_order"), "true", "RegExp flag order is normalized (/gi == /ig)");
            assert_eq!(r("pm_set_order"), "true", "Sets are order-insensitive");
            assert_eq!(r("pm_set_diff"), "threw", "differing Set members must NOT eql");
            assert_eq!(r("pm_map_same"), "true", "Maps with equal entries must eql");
            assert_eq!(r("pm_map_diff"), "threw", "differing Map values must NOT eql");
            assert_eq!(r("pm_cycle_same"), "true", "circular equal structures must eql");
            assert_eq!(r("pm_cycle_diff"), "threw", "circular differing structures must throw, not overflow");
            assert_eq!(r("chai_set_same"), "true", "chai: equal Sets must eql");
            assert_eq!(r("chai_set_diff"), "threw", "chai: differing Sets must NOT eql");
            assert_eq!(r("lodash_date_same"), "true", "lodash: equal Dates are isEqual");
            assert_eq!(r("lodash_date_diff"), "true", "lodash: differing Dates are not isEqual");
            assert_eq!(r("lodash_cycle"), "true", "lodash: circular self-equality must not overflow");
            assert_eq!(r("pm_map_self"), "true", "self-referential Maps must eql without stack overflow");
            assert_eq!(r("pm_map_self_diff"), "threw", "differing self-referential Map values must NOT eql");
            assert_eq!(r("pm_set_self"), "true", "self-referential Sets must eql without stack overflow");
            assert_eq!(r("pm_map_key_diff"), "threw", "same-size self-referential Maps with different keys must NOT eql");
        });
    }

    #[test]
    fn test_to_not_chain_matches_not_to() {
        // Backlog line 87: Postman snippets emit `pm.expect(x).to.not.*`
        // (negation AFTER .to), but only `.not.to.*` existed — the to.not
        // spelling read as `unknown assertion property 'not'` and recorded
        // FAIL while chai.expect handled it fine. Both spellings now share
        // one negated chain.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__r = {};
                function trial(key, fn) {
                    globalThis.__r[key] = String((function () {
                        try { fn(); return true; }
                        catch (e) { return e.name === 'RangeError' ? 'stack-overflow' : 'threw'; }
                    })());
                }
                // .to.not.* (Postman's spelling) — pass when values differ.
                trial('to_not_equal_diff', function () { pm.expect(1).to.not.equal(2); });
                trial('to_not_equal_same', function () { pm.expect(1).to.not.equal(1); });
                trial('to_not_eql_diff', function () { pm.expect({ a: 1 }).to.not.eql({ b: 2 }); });
                trial('to_not_eql_same', function () { pm.expect({ a: 1 }).to.not.eql({ a: 1 }); });
                trial('to_not_be_true_neg', function () { pm.expect(false).to.not.be.true; });
                trial('to_not_be_true_pos', function () { pm.expect(true).to.not.be.true; });
                trial('to_not_be_an_neg', function () { pm.expect('x').to.not.be.an('number'); });
                trial('to_not_be_an_pos', function () { pm.expect(1).to.not.be.an('number'); });
                trial('to_not_be_a_neg', function () { pm.expect(1).to.not.be.a('string'); });
                // Negated include/match/have (Postman's "must not contain").
                trial('to_not_include_absent', function () { pm.expect('abcdef').to.not.include('zzz'); });
                trial('to_not_include_present', function () { pm.expect('abcdef').to.not.include('bcd'); });
                trial('to_not_match_no', function () { pm.expect('abcdef').to.not.match(/zzz/); });
                trial('to_not_match_yes', function () { pm.expect('abcdef').to.not.match(/bcd/); });
                trial('to_not_have_prop_absent', function () { pm.expect({ a: 1 }).to.not.have.property('b'); });
                trial('to_not_have_prop_present', function () { pm.expect({ a: 1 }).to.not.have.property('a'); });
                // Old spelling must still work.
                trial('not_to_equal_same', function () { pm.expect(1).not.to.equal(1); });
                trial('not_to_be_true_pos', function () { pm.expect(true).not.to.be.true; });
                // Guard still applies on the negated chains.
                trial('to_not_unknown', function () { pm.expect(1).to.not.bogus; });
                trial('not_to_unknown', function () { pm.expect(1).not.to.bogus; });
                "#,
            )
            .expect("script should eval");

            let r = |k: &str| ctx.eval::<String, _>(format!("__r['{}']", k)).unwrap();
            assert_eq!(r("to_not_equal_diff"), "true", "to.not.equal passes when values differ");
            assert_eq!(r("to_not_equal_same"), "threw", "to.not.equal throws when values match");
            assert_eq!(r("to_not_eql_diff"), "true", "to.not.eql passes when values differ");
            assert_eq!(r("to_not_eql_same"), "threw", "to.not.eql throws when values deep-match");
            assert_eq!(r("to_not_be_true_neg"), "true", "to.not.be.true passes for false");
            assert_eq!(r("to_not_be_true_pos"), "threw", "to.not.be.true throws for true");
            assert_eq!(r("to_not_be_an_neg"), "true", "to.not.be.an passes on wrong type");
            assert_eq!(r("to_not_be_an_pos"), "threw", "to.not.be.an throws on matching type");
            assert_eq!(r("to_not_be_a_neg"), "true", "to.not.be.a passes on wrong type");
            assert_eq!(r("to_not_include_absent"), "true", "to.not.include passes when value absent");
            assert_eq!(r("to_not_include_present"), "threw", "to.not.include throws when value present");
            assert_eq!(r("to_not_match_no"), "true", "to.not.match passes when no match");
            assert_eq!(r("to_not_match_yes"), "threw", "to.not.match throws when matched");
            assert_eq!(r("to_not_have_prop_absent"), "true", "to.not.have.property passes when absent");
            assert_eq!(r("to_not_have_prop_present"), "threw", "to.not.have.property throws when present");
            assert_eq!(r("not_to_equal_same"), "threw", "not.to.equal still throws when values match");
            assert_eq!(r("not_to_be_true_pos"), "threw", "not.to.be.true still throws for true");
            assert_eq!(r("to_not_unknown"), "threw", "unknown property on to.not throws");
            assert_eq!(r("not_to_unknown"), "threw", "unknown property on not.to throws");
        });
    }

    #[test]
    fn test_pm_expect_delegates_to_chai_assertion_surface() {
        // W1-B: 6 of Postman's 17 stock snippets FAILED against the old
        // AssertChain surface — pm.expect(...).to.be.below(200),
        // .to.have.lengthOf(3), .to.be.oneOf([200,201]),
        // .to.deep.include({name:"x"}) all read "unknown assertion
        // property". pm.expect now delegates to chai-shim's Assertion when
        // chai is loaded (the runtime bundle always loads both), so the
        // full chai surface works through pm.expect: below/above/least/
        // most/lessThan/lengthOf/oneOf/instanceOf/throw/keys/contain/
        // members/closeTo/within and deep-aware include. The Postman
        // extensions status/header/jsonBody live on the chai Assertion too
        // (chai-postman parity), so pm.expect(pm.response).to.have.status
        // (200) still works after delegation.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__r = {};
                function trial(key, fn) {
                    globalThis.__r[key] = String((function () {
                        try { fn(); return true; }
                        catch (e) { return e.name === 'RangeError' ? 'stack-overflow' : 'threw'; }
                    })());
                }
                // Stub the response bridges for the Postman extensions.
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__tropel_pm_response_status = function () { return 'OK'; };
                globalThis.__tropel_pm_response_time = function () { return 42.5; };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                globalThis.__tropel_pm_response_header = function (key) {
                    if (String(key).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_cookies = function () { return {}; };
                globalThis.__tropel_pm_response_body = function () { return '{}'; };
                globalThis.__tropel_pm_response_json = function () { return {}; };

                // name: 'x' at the TOP LEVEL — chai's deep.include checks the
                // expected object's keys directly against the target (no
                // recursive search), so the stock-snippet fixture must carry
                // the asserted key as a top-level member.
                var json = { name: 'x', items: [{ name: 'a' }, { name: 'b' }, { name: 'c' }] };

                // The 6 failing stock snippets (W1-B EXEC list).
                trial('below_ok', function () { pm.expect(pm.response.responseTime).to.be.below(200); });
                trial('below_bad', function () { pm.expect(pm.response.responseTime).to.be.below(10); });
                trial('lengthOf_ok', function () { pm.expect(json.items).to.have.lengthOf(3); });
                trial('lengthOf_bad', function () { pm.expect(json.items).to.have.lengthOf(2); });
                trial('oneOf_ok', function () { pm.expect(pm.response.code).to.be.oneOf([200, 201]); });
                trial('oneOf_bad', function () { pm.expect(pm.response.code).to.be.oneOf([404, 500]); });
                trial('deep_include_ok', function () { pm.expect(json).to.deep.include({ name: 'x' }); });
                trial('deep_include_bad', function () { pm.expect(json).to.deep.include({ name: 'zzz' }); });
                // Deep-include of a NON-OBJECT must fail — Object.keys(5)=[]
                // would vacuously pass (false-green guard).
                trial('deep_include_primitive', function () { pm.expect({ a: 1 }).to.deep.include(5); });

                // The rest of the formerly-absent surface.
                trial('above_ok', function () { pm.expect(5).to.be.above(4); });
                trial('above_bad', function () { pm.expect(5).to.be.above(6); });
                trial('least_ok', function () { pm.expect(5).to.be.at.least(5); });
                trial('least_bad', function () { pm.expect(5).to.be.at.least(6); });
                trial('most_ok', function () { pm.expect(5).to.be.at.most(5); });
                trial('most_bad', function () { pm.expect(5).to.be.at.most(4); });
                trial('lessThan_ok', function () { pm.expect(5).to.be.lessThan(6); });
                trial('lessThan_bad', function () { pm.expect(5).to.be.lessThan(4); });
                trial('within_ok', function () { pm.expect(5).to.be.within(4, 6); });
                trial('within_bad', function () { pm.expect(5).to.be.within(6, 7); });
                trial('closeTo_ok', function () { pm.expect(5).to.be.closeTo(5.1, 0.2); });
                trial('closeTo_bad', function () { pm.expect(5).to.be.closeTo(5.1, 0.01); });
                trial('instanceOf_ok', function () { pm.expect([]).to.be.instanceOf(Array); });
                trial('instanceOf_bad', function () { pm.expect({}).to.be.instanceOf(Array); });
                trial('keys_ok', function () { pm.expect({ a: 1, b: 2 }).to.have.keys('a', 'b'); });
                trial('keys_bad', function () { pm.expect({ a: 1 }).to.have.keys('a', 'b'); });
                trial('contain_ok', function () { pm.expect([1, 2, 3]).to.contain(2); });
                trial('contain_bad', function () { pm.expect([1, 2, 3]).to.contain(9); });
                trial('members_ok', function () { pm.expect([1, 2, 3]).to.have.members([3, 1, 2]); });
                trial('members_bad', function () { pm.expect([1, 2, 3]).to.have.members([9]); });
                // chai's plain .members is SAME-SET (order-insensitive, equal
                // size) — a subset must NOT pass (false-green guard).
                trial('members_subset', function () { pm.expect([1, 2, 3]).to.have.members([1, 2]); });
                // Multiset semantics: same elements with different COUNTS must
                // fail (naive length+every/some would pass this).
                trial('members_multiset', function () { pm.expect([1, 1, 2]).to.have.members([1, 2, 2]); });
                trial('throw_ok', function () {
                    pm.expect(function () { throw new Error('boom'); }).to.throw(Error, 'boom');
                });
                trial('throw_bad', function () {
                    pm.expect(function () {}).to.throw(Error);
                });

                // Postman extensions survive delegation.
                trial('status_ok', function () { pm.expect(pm.response).to.have.status(200); });
                trial('status_bad', function () { pm.expect(pm.response).to.have.status(404); });
                trial('header_ok', function () {
                    pm.expect(pm.response).to.have.header('Content-Type', 'application/json');
                });
                trial('header_bad', function () {
                    pm.expect(pm.response).to.have.header('X-Missing', 'nope');
                });

                // The guard still trips on real typos through the delegated chain.
                trial('typo', function () { pm.expect(1).to.be.tostring; });
            "#,
            )
            .expect("script should eval");

            let r = |k: &str| ctx.eval::<String, _>(format!("__r['{}']", k)).unwrap();
            // 6 failing stock snippets now pass; each also fails closed when
            // the assertion does not hold.
            assert_eq!(r("below_ok"), "true", "pm.expect(...).to.be.below must pass (stock snippet)");
            assert_eq!(r("below_bad"), "threw", "below must fail closed on a bad bound");
            assert_eq!(r("lengthOf_ok"), "true", "pm.expect(...).to.have.lengthOf must pass (stock snippet)");
            assert_eq!(r("lengthOf_bad"), "threw", "lengthOf must fail closed");
            assert_eq!(r("oneOf_ok"), "true", "pm.expect(...).to.be.oneOf must pass (stock snippet)");
            assert_eq!(r("oneOf_bad"), "threw", "oneOf must fail closed");
            assert_eq!(r("deep_include_ok"), "true", "pm.expect(...).to.deep.include must pass (stock snippet)");
            assert_eq!(r("deep_include_bad"), "threw", "deep.include must fail closed");
            assert_eq!(r("deep_include_primitive"), "threw", "deep.include of a non-object must fail (Object.keys(5)=[] false-green guard)");
            assert_eq!(r("above_ok"), "true", "above must pass");
            assert_eq!(r("above_bad"), "threw", "above must fail closed");
            assert_eq!(r("least_ok"), "true", "at.least must pass");
            assert_eq!(r("least_bad"), "threw", "at.least must fail closed");
            assert_eq!(r("most_ok"), "true", "at.most must pass");
            assert_eq!(r("most_bad"), "threw", "at.most must fail closed");
            assert_eq!(r("lessThan_ok"), "true", "lessThan must pass");
            assert_eq!(r("lessThan_bad"), "threw", "lessThan must fail closed");
            assert_eq!(r("within_ok"), "true", "within must pass");
            assert_eq!(r("within_bad"), "threw", "within must fail closed");
            assert_eq!(r("closeTo_ok"), "true", "closeTo must pass");
            assert_eq!(r("closeTo_bad"), "threw", "closeTo must fail closed");
            assert_eq!(r("instanceOf_ok"), "true", "instanceOf must pass");
            assert_eq!(r("instanceOf_bad"), "threw", "instanceOf must fail closed");
            assert_eq!(r("keys_ok"), "true", "keys must pass");
            assert_eq!(r("keys_bad"), "threw", "keys must fail closed");
            assert_eq!(r("contain_ok"), "true", "contain must pass");
            assert_eq!(r("contain_bad"), "threw", "contain must fail closed");
            assert_eq!(r("members_ok"), "true", "members must pass on an equal set");
            assert_eq!(r("members_bad"), "threw", "members must fail closed");
            assert_eq!(r("members_subset"), "threw", "plain .members is SAME-SET — a subset must not pass (false-green guard)");
            assert_eq!(r("members_multiset"), "threw", "members is MULTISET — differing element counts must not pass (false-green guard)");
            assert_eq!(r("throw_ok"), "true", "throw must pass on a matching error");
            assert_eq!(r("throw_bad"), "threw", "throw must fail closed when nothing throws");
            assert_eq!(r("status_ok"), "true", "pm.expect(pm.response).to.have.status must survive delegation");
            assert_eq!(r("status_bad"), "threw", "status must fail closed");
            assert_eq!(r("header_ok"), "true", "header must survive delegation");
            assert_eq!(r("header_bad"), "threw", "header must fail closed");
            assert_eq!(r("typo"), "threw", "the guard must still trip on real typos through the delegated chain");
        });
    }

    #[test]
    fn test_include_uses_chai_value_semantics() {
        // Backlog line 88: pm.expect(arr).to.include(v) was a SUBSTRING test
        // (String(arr).indexOf) — [11,22].include(1) passed and
        // {a:1}.include('object') passed. chai-shim implements it correctly
        // (array indexOf for arrays, `key in obj` for objects). pm now
        // mirrors chai: substring for strings, element membership for
        // arrays, key membership for objects — on BOTH the positive chain
        // and the negated chain.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__r = {};
                function trial(key, fn) {
                    globalThis.__r[key] = String((function () {
                        try { fn(); return true; }
                        catch (e) { return e.name === 'RangeError' ? 'stack-overflow' : 'threw'; }
                    })());
                }
                // Arrays: element membership, not substring.
                trial('pm_arr_include_yes', function () { pm.expect([11, 22]).to.include(22); });
                trial('pm_arr_include_no', function () { pm.expect([11, 22]).to.include(1); });
                trial('pm_arr_include_str', function () { pm.expect(['a', 'b']).to.include('b'); });
                trial('pm_arr_include_str_no', function () { pm.expect(['a', 'b']).to.include('ab'); });
                // Objects: key membership.
                trial('pm_obj_include_key', function () { pm.expect({ a: 1 }).to.include('a'); });
                trial('pm_obj_include_nokey', function () { pm.expect({ a: 1 }).to.include('object'); });
                // Strings stay substring.
                trial('pm_str_include_yes', function () { pm.expect('abcdef').to.include('bcd'); });
                trial('pm_str_include_no', function () { pm.expect('abcdef').to.include('zzz'); });
                // Negated chain mirrors the same semantics.
                trial('pm_arr_not_include_yes', function () { pm.expect([11, 22]).to.not.include(1); });
                trial('pm_arr_not_include_no', function () { pm.expect([11, 22]).to.not.include(22); });
                // chai parity: both expect()s must agree.
                trial('chai_arr_include_yes', function () { chai.expect([11, 22]).to.include(22); });
                trial('chai_arr_include_no', function () { chai.expect([11, 22]).to.include(1); });
                trial('chai_obj_include_key', function () { chai.expect({ a: 1 }).to.include('a'); });
                trial('chai_obj_include_nokey', function () { chai.expect({ a: 1 }).to.include('object'); });
                "#,
            )
            .expect("script should eval");

            let r = |k: &str| ctx.eval::<String, _>(format!("__r['{}']", k)).unwrap();
            assert_eq!(r("pm_arr_include_yes"), "true", "array include passes for a present element");
            assert_eq!(r("pm_arr_include_no"), "threw", "array include must NOT match substrings (11.include(1) was the bug)");
            assert_eq!(r("pm_arr_include_str"), "true", "string-array include passes for a present member");
            assert_eq!(r("pm_arr_include_str_no"), "threw", "string-array include must not substring-match ('ab' not in ['a','b'])");
            assert_eq!(r("pm_obj_include_key"), "true", "object include passes for a present key");
            assert_eq!(r("pm_obj_include_nokey"), "threw", "object include must test keys, not type names ('object' was the bug)");
            assert_eq!(r("pm_str_include_yes"), "true", "string include still substring-tests");
            assert_eq!(r("pm_str_include_no"), "threw", "string include throws when absent");
            assert_eq!(r("pm_arr_not_include_yes"), "true", "negated array include passes for an absent element");
            assert_eq!(r("pm_arr_not_include_no"), "threw", "negated array include throws for a present element");
            assert_eq!(r("chai_arr_include_yes"), "true", "chai array include agrees (element present)");
            assert_eq!(r("chai_arr_include_no"), "threw", "chai array include agrees (substring must not match)");
            assert_eq!(r("chai_obj_include_key"), "true", "chai object include agrees (key present)");
            assert_eq!(r("chai_obj_include_nokey"), "threw", "chai object include agrees (key test)");
        });
    }

    #[test]
    fn test_unimplemented_assertion_properties_fail_closed() {
        // Backlog §1 P0: unimplemented assertion PROPERTIES (pm.expect(false)
        // .to.be.true, .to.be.null/.undefined/.ok/.empty, pm.expect(null)
        // .to.exist, and chai .empty/.exist/.NaN/.finite) used to read as
        // `undefined` and pm.test recorded GREEN — a silent pass. The Proxy
        // guard now THROWS on unknown assertion names, and the common
        // property getters are implemented for real. Backlog line 73: the
        // `.should` getter must go through the SAME guard — previously it
        // returned a raw Assertion, so `({a:1}).should.be.sealed` read as
        // undefined and passed silently.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Implemented getters must WORK (pass silently is fine — no throw).
                globalThis.__pm_true_ok = String((function () {
                    try { pm.expect(true).to.be.true; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_exist_ok = String((function () {
                    try { pm.expect(1).to.exist; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_empty_ok = String((function () {
                    try { pm.expect([]).to.be.empty; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_ok_fail = String((function () {
                    // pm.expect(false).to.be.true must THROW (was silent pass).
                    try { pm.expect(false).to.be.true; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_null_exist_fail = String((function () {
                    // pm.expect(null).to.exist must THROW.
                    try { pm.expect(null).to.exist; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_unknown_prop = String((function () {
                    // A typo'd/unimplemented name must THROW, not pass silently.
                    try { pm.expect(1).to.be.bogusAssertion; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_not_true_fail = String((function () {
                    // Negated: true IS true, so .not.to.be.true must THROW.
                    try { pm.expect(true).not.to.be.true; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_not_true_ok = String((function () {
                    // Negated: false is NOT true, so .not.to.be.true passes.
                    try { pm.expect(false).not.to.be.true; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__pm_not_exist_fail = String((function () {
                    // Negated exist: 1 EXISTS, so .not.to.exist must THROW.
                    // (chai: expect(null).not.to.exist PASSES — null doesn't
                    // exist — so use a value that exists for the throw case.)
                    try { pm.expect(1).not.to.exist; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                // chai side: implemented getters work, unknown throws.
                globalThis.__chai_empty_ok = String((function () {
                    try { chai.expect([]).to.be.empty; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__chai_nan_ok = String((function () {
                    try { chai.expect(NaN).to.be.NaN; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__chai_exist_fail = String((function () {
                    try { chai.expect(null).to.exist; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__chai_finite_fail = String((function () {
                    try { chai.expect(Infinity).to.be.finite; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__chai_unknown_prop = String((function () {
                    try { chai.expect(1).to.be.bogusAssertion; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__chai_inspect_ok = String((function () {
                    // Inspection/promise-interop names must resolve normally
                    // (the guard's allowlist) — JSON.stringify must not throw.
                    try { JSON.stringify(chai.expect(1)); return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__chai_true_fail = String((function () {
                    try { chai.expect(false).to.be.true; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                // ── chai.should (backlog line 73) ──
                chai.should();
                globalThis.__should_sealed_fail = String((function () {
                    // VERIFIED bug: ({a:1}).should.be.sealed returned
                    // undefined (raw Assertion, no Proxy) — must now throw.
                    try { ({ a: 1 }).should.be.sealed; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__should_true_ok = String((function () {
                    // Implemented getter must still WORK through the guard.
                    try { (true).should.be.true; return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__should_false_true_fail = String((function () {
                    try { (false).should.be.true; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__should_unknown_prop = String((function () {
                    try { (1).should.be.bogusShouldProp; return 'passed'; }
                    catch (e) { return 'threw'; }
                })());
                globalThis.__should_method_chain_ok = String((function () {
                    // Method-chain positive: `equal` reads this._obj through
                    // the Proxy receiver — must still resolve.
                    try { (5).should.equal(5); return 'ok'; }
                    catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__pm_true_ok").unwrap(),
                "ok",
                "pm true passes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_exist_ok").unwrap(),
                "ok",
                "pm exist passes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_empty_ok").unwrap(),
                "ok",
                "pm empty passes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_ok_fail").unwrap(),
                "threw",
                "pm false.to.be.true must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_null_exist_fail").unwrap(),
                "threw",
                "pm null.to.exist must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_unknown_prop").unwrap(),
                "threw",
                "pm unknown assertion prop must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_not_true_fail").unwrap(),
                "threw",
                "pm true.not.to.be.true must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_not_true_ok").unwrap(),
                "ok",
                "pm false.not.to.be.true passes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__pm_not_exist_fail").unwrap(),
                "threw",
                "pm 1.not.to.exist must throw (1 exists)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_empty_ok").unwrap(),
                "ok",
                "chai empty passes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_nan_ok").unwrap(),
                "ok",
                "chai NaN passes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_exist_fail").unwrap(),
                "threw",
                "chai null.to.exist must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_finite_fail").unwrap(),
                "threw",
                "chai Infinity.to.be.finite must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_unknown_prop").unwrap(),
                "threw",
                "chai unknown assertion prop must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_inspect_ok").unwrap(),
                "ok",
                "chai JSON.stringify must not throw (allowlist)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__chai_true_fail").unwrap(),
                "threw",
                "chai false.to.be.true must throw"
            );
            // ── chai.should (backlog line 73) ──
            assert_eq!(
                ctx.eval::<String, _>("__should_sealed_fail").unwrap(),
                "threw",
                "should.be.sealed must throw (was silent undefined)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__should_true_ok").unwrap(),
                "ok",
                "(true).should.be.true must still pass"
            );
            assert_eq!(
                ctx.eval::<String, _>("__should_false_true_fail").unwrap(),
                "threw",
                "(false).should.be.true must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__should_unknown_prop").unwrap(),
                "threw",
                "should unknown assertion prop must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__should_method_chain_ok").unwrap(),
                "ok",
                "(5).should.equal(5) method chain must pass through the guard"
            );
        });
    }

    #[test]
    fn test_object_prototype_members_do_not_bypass_guards() {
        // W1-A: both proxy guards used `prop in t`, which walks the ENTIRE
        // prototype chain — `.toString`, `.constructor`, `.hasOwnProperty`,
        // `.valueOf`, `.__proto__` resolved to truthy Functions inherited
        // from Object.prototype and every one recorded PASS, so a typo like
        // `pm.expect(x).to.be.tostring` could never fail (silent green). The
        // guards now use own-property checks that stop before
        // Object.prototype: the real assertion members still resolve, but
        // Object.prototype members THROW. Internal instance state (`_actual`,
        // `_obj`, `__flags`) stays readable — real chai exposes those too and
        // the shims' own methods read them through the proxy.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Stub the response bridges — the TODO's EXEC case is
                // "against a 200", i.e. the plain-literal guard path
                // (pm.response.to.be.*), which wraps a DIFFERENT target kind
                // than the AssertChain instances pm.expect uses.
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__tropel_pm_response_status = function () { return 'OK'; };
                globalThis.__tropel_pm_response_time = function () { return 42.5; };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                globalThis.__tropel_pm_response_header = function (key) {
                    if (String(key).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_cookies = function () { return {}; };
                globalThis.__tropel_pm_response_body = function () { return '{}'; };
                globalThis.__tropel_pm_response_json = function () { return {}; };

                globalThis.__r = {};
                // Install the `.should` getter (chai.should() defines
                // Object.prototype.should) — without it `(5).should.equal(5)`
                // below would throw "should is not a function" before the
                // guard is ever reached.
                chai.should();
                function trial(key, fn) {
                    globalThis.__r[key] = String((function () {
                        try { fn(); return true; }
                        catch (e) { return e.name === 'RangeError' ? 'stack-overflow' : 'threw'; }
                    })());
                }
                // pm.response.to (plain-literal guard path): Object.prototype
                // members must throw — the TODO's "against a 200" EXEC.
                trial('resp_toString', function () { pm.response.to.be.toString; });
                trial('resp_constructor', function () { pm.response.to.constructor; });
                trial('resp_hasOwnProperty', function () { pm.response.to.be.hasOwnProperty; });
                trial('resp_valueOf', function () { pm.response.to.valueOf; });
                trial('resp_proto', function () { pm.response.to.be.__proto__; });
                // pm.expect chains (AssertChain guard path): same.
                trial('pm_toString', function () { pm.expect(1).to.be.toString; });
                trial('pm_constructor', function () { pm.expect(1).to.be.constructor; });
                trial('pm_hasOwnProperty', function () { pm.expect(1).to.be.hasOwnProperty; });
                trial('pm_valueOf', function () { pm.expect(1).to.be.valueOf; });
                trial('pm_proto', function () { pm.expect(1).to.be.__proto__; });
                // chai.expect chains: same.
                trial('chai_toString', function () { chai.expect(1).to.be.toString; });
                trial('chai_constructor', function () { chai.expect(1).to.be.constructor; });
                trial('chai_hasOwnProperty', function () { chai.expect(1).to.be.hasOwnProperty; });
                trial('chai_valueOf', function () { chai.expect(1).to.be.valueOf; });
                trial('chai_proto', function () { chai.expect(1).to.be.__proto__; });
                // Real assertions still resolve and pass.
                trial('pm_real', function () { pm.expect(true).to.be.true; });
                trial('resp_status_real', function () { pm.response.to.have.status(200); });
                trial('chai_real', function () { chai.expect({ a: 1 }).to.deep.equal({ a: 1 }); });
                trial('chai_should_real', function () { (5).should.equal(5); });
                // Line 146: .deep.equal must honor the deep flag (deep-equal by
                // value, fail-closed on mismatch, .not.deep.equal inverts,
                // .eql stays deep, and PLAIN .equal on distinct objects must
                // still throw — chai parity on all five).
                trial('chai_deep_equal_ok', function () {
                    chai.expect({ a: 1, b: { c: [1, 2] } }).to.deep.equal({ a: 1, b: { c: [1, 2] } });
                });
                trial('chai_deep_equal_mismatch', function () { chai.expect({ a: 1 }).to.deep.equal({ a: 2 }); });
                trial('chai_deep_equal_negated', function () { chai.expect({ a: 1 }).to.not.deep.equal({ a: 2 }); });
                trial('chai_eql_still_deep', function () { chai.expect({ a: 1 }).to.eql({ a: 1 }); });
                trial('chai_shallow_equal_throws', function () { chai.expect({ a: 1 }).to.equal({ a: 1 }); });
                // Real typos still throw (guard parity).
                trial('pm_typo', function () { pm.expect(1).to.be.tostring; });
                trial('chai_typo', function () { chai.expect(1).to.be.tostring; });
                "#,
            )
            .expect("guard leak script should eval");
            for (name, want) in [
                ("resp_toString", "threw"),
                ("resp_constructor", "threw"),
                ("resp_hasOwnProperty", "threw"),
                ("resp_valueOf", "threw"),
                ("resp_proto", "threw"),
                ("pm_toString", "threw"),
                ("pm_constructor", "threw"),
                ("pm_hasOwnProperty", "threw"),
                ("pm_valueOf", "threw"),
                ("pm_proto", "threw"),
                ("chai_toString", "threw"),
                ("chai_constructor", "threw"),
                ("chai_hasOwnProperty", "threw"),
                ("chai_valueOf", "threw"),
                ("chai_proto", "threw"),
                ("pm_real", "true"),
                ("resp_status_real", "true"),
                ("chai_real", "true"),
                ("chai_should_real", "true"),
                ("chai_deep_equal_ok", "true"),
                ("chai_deep_equal_mismatch", "threw"),
                ("chai_deep_equal_negated", "true"),
                ("chai_eql_still_deep", "true"),
                ("chai_shallow_equal_throws", "threw"),
                ("pm_typo", "threw"),
                ("chai_typo", "threw"),
            ] {
                let expr = format!("__r.{name}");
                assert_eq!(
                    ctx.eval::<String, _>(expr).unwrap(),
                    want,
                    "{name}: Object.prototype members must not bypass the guard"
                );
            }
        });
    }

    #[test]
    fn test_pm_expect_chain_allocates_once_not_per_read() {
        // Backlog line 105: pm.expect was ~10x slower than chai.expect
        // (2511 ms vs 235 ms over 200k assertions) because EVERY call built
        // a fresh chain literal with 18 Object.defineProperty calls
        // (addPropAssertions x4) and guardChain wrapped each object-valued
        // property read in a NEW Proxy. The chain is now a single class with
        // the surface on AssertChain.prototype. This test pins the
        // STRUCTURAL property deterministically (no timing flakiness): after
        // load, running chains must not call Object.defineProperty at all,
        // and Proxy allocations must be ~1 per expect + ~1 per .not access,
        // NOT one per chain-property read.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Instrument AFTER load so module-level defineProperty (the
                // one-time prototype install) is not counted.
                globalThis.__defCalls = 0;
                var __realDefine = Object.defineProperty;
                Object.defineProperty = function () {
                    globalThis.__defCalls++;
                    return __realDefine.apply(Object, arguments);
                };
                globalThis.__proxyCalls = 0;
                var __realProxy = Proxy;
                Proxy = function () {
                    globalThis.__proxyCalls++;
                    return new __realProxy(arguments[0], arguments[1]);
                };

                // Warm-up + behaviour must be unchanged. This test evals
                // ONLY pm.js (no chai), so pm.expect takes the AssertChain
                // fallback (W1-B: when chai IS loaded, pm.expect delegates
                // to chai's Assertion and gains the full chai surface). The
                // fallback chain's surface is eql/equal/include/match/an/a/
                // property/status/header/jsonBody, and the guard must still
                // throw on anything else.)
                pm.expect('x').to.be.an('string').and.to.equal('x');
                pm.expect(5).not.to.be.a('string');
                pm.expect([1, 2]).to.include(2);
                try { pm.expect(1).to.be.bogus; } catch (e) { /* guard still throws */ }

                var defBefore = globalThis.__defCalls;
                var proxyBefore = globalThis.__proxyCalls;
                for (var i = 0; i < 500; i++) {
                    pm.expect('x').to.be.an('string').and.to.equal('x');
                }
                globalThis.__defDelta = globalThis.__defCalls - defBefore;
                globalThis.__proxyDelta = globalThis.__proxyCalls - proxyBefore;
            "#,
            )
            .expect("script should eval");

            let def_delta: i64 = ctx.eval("__defDelta").expect("read defDelta");
            let proxy_delta: i64 = ctx.eval("__proxyDelta").expect("read proxyDelta");
            assert_eq!(
                def_delta, 0,
                "pm.expect must not call Object.defineProperty per call (got {def_delta})"
            );
            assert_eq!(
                proxy_delta, 500,
                "pm.expect must allocate exactly one Proxy per call, not per read (got {proxy_delta})"
            );
        });
    }

    #[test]
    fn test_chai_a_an_and_numeric_contain_instanceof_oneof_throw() {
        // Backlog line 104: chai a/an were NOT callable (plain getters, so
        // `expect(x).to.be.a('string')` threw "a is not a function"), and
        // above/below/least/most/contain/instanceof/oneOf/throw all hit the
        // unknown-name Proxy guard and threw — a large slice of valid chai
        // turned red. Each must now work, support negation, chain, and keep
        // the unknown-name guard active afterwards.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__out = JSON.stringify([
                    // a/an are callable and assert the type
                    (function () { try { chai.expect('x').to.be.a('string'); return 'a-ok'; } catch (e) { return 'a-fail'; } })(),
                    (function () { try { chai.expect([1]).to.be.an('array'); return 'an-ok'; } catch (e) { return 'an-fail'; } })(),
                    (function () { try { chai.expect(42).to.be.a('number'); return 'num-ok'; } catch (e) { return 'num-fail'; } })(),
                    (function () { try { chai.expect({}).to.be.an('object'); return 'obj-ok'; } catch (e) { return 'obj-fail'; } })(),
                    (function () { try { chai.expect(42).to.be.a('string'); return 'passed'; } catch (e) { return 'threw'; } })(),
                    // negation
                    (function () { try { chai.expect(42).not.to.be.a('string'); return 'not-ok'; } catch (e) { return 'not-fail'; } })(),
                    (function () { try { chai.expect(42).not.to.be.a('number'); return 'passed'; } catch (e) { return 'threw'; } })(),
                    // chaining after a/an (guard must stay active)
                    (function () { try { chai.expect('x').to.be.a('string').and.to.equal('x'); return 'chain-ok'; } catch (e) { return 'chain-fail'; } })(),
                    (function () { try { chai.expect('x').to.be.a('string').bogusAssertion; return 'passed'; } catch (e) { return 'threw'; } })(),
                    // numeric comparisons
                    (function () { try { chai.expect(10).to.be.above(5); return 'above-ok'; } catch (e) { return 'above-fail'; } })(),
                    (function () { try { chai.expect(1).to.be.below(5); return 'below-ok'; } catch (e) { return 'below-fail'; } })(),
                    (function () { try { chai.expect(5).to.be.at.least(5); return 'least-ok'; } catch (e) { return 'least-fail'; } })(),
                    (function () { try { chai.expect(5).to.be.at.most(5); return 'most-ok'; } catch (e) { return 'most-fail'; } })(),
                    (function () { try { chai.expect(10).to.be.below(5); return 'passed'; } catch (e) { return 'threw'; } })(),
                    (function () { try { chai.expect(1).to.be.above(5); return 'passed'; } catch (e) { return 'threw'; } })(),
                    // contain
                    (function () { try { chai.expect([1, 2, 3]).to.contain(2); return 'contain-ok'; } catch (e) { return 'contain-fail'; } })(),
                    (function () { try { chai.expect('hello').to.contain('ell'); return 'str-ok'; } catch (e) { return 'str-fail'; } })(),
                    (function () { try { chai.expect([1, 2, 3]).to.contain(9); return 'passed'; } catch (e) { return 'threw'; } })(),
                    // instanceof
                    (function () { try { chai.expect(new Error('e')).to.be.instanceof(Error); return 'inst-ok'; } catch (e) { return 'inst-fail'; } })(),
                    (function () { try { chai.expect({}).to.be.instanceof(Error); return 'passed'; } catch (e) { return 'threw'; } })(),
                    // oneOf
                    (function () { try { chai.expect('b').to.be.oneOf(['a', 'b', 'c']); return 'one-ok'; } catch (e) { return 'one-fail'; } })(),
                    (function () { try { chai.expect('z').to.be.oneOf(['a', 'b']); return 'passed'; } catch (e) { return 'threw'; } })(),
                    // throw
                    (function () { try { chai.expect(function () { throw new Error('boom'); }).to.throw(); return 'throw-ok'; } catch (e) { return 'throw-fail'; } })(),
                    (function () { try { chai.expect(function () { throw new TypeError('boom'); }).to.throw(TypeError); return 'type-ok'; } catch (e) { return 'type-fail'; } })(),
                    (function () { try { chai.expect(function () { throw new Error('boom'); }).to.throw('boom'); return 'msg-ok'; } catch (e) { return 'msg-fail'; } })(),
                    (function () { try { chai.expect(function () {}).to.throw(); return 'passed'; } catch (e) { return 'threw'; } })(),
                    (function () { try { chai.expect(function () {}).not.to.throw(); return 'nothrow-ok'; } catch (e) { return 'nothrow-fail'; } })()
                ]);
            "#,
            )
            .expect("script should eval");
            let out: String = ctx.eval("__out").expect("read __out");
            assert_eq!(
                out,
                concat!(
                    "[\"a-ok\",\"an-ok\",\"num-ok\",\"obj-ok\",\"threw\",",
                    "\"not-ok\",\"threw\",\"chain-ok\",\"threw\",",
                    "\"above-ok\",\"below-ok\",\"least-ok\",\"most-ok\",",
                    "\"threw\",\"threw\",\"contain-ok\",\"str-ok\",\"threw\",",
                    "\"inst-ok\",\"threw\",\"one-ok\",\"threw\",",
                    "\"throw-ok\",\"type-ok\",\"msg-ok\",\"threw\",\"nothrow-ok\"]"
                ),
                "chai a/an/numeric/contain/instanceof/oneOf/throw mismatch: {out}"
            );
        });
    }

    #[test]
    fn test_pm_collection_vars_globals_request_cookies() {
        // Backlog line 145: pm.collectionVariables / pm.globals / pm.request /
        // pm.cookies / pm.expect.fail / pm.test.skip / postman.setNextRequest
        // were all missing — collections using them threw TypeError and failed
        // the run. pm.request mutations must feed back through the bridges.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Variable-store stubs.
                globalThis.__tropel_pm_collection_vars_get = function (k) {
                    if (k === 'base') return '"https://api.example.com"';
                    return null;
                };
                globalThis.__tropel_pm_collection_vars_set = function (k, v) { globalThis.__cv_set = k + '=' + v; };
                globalThis.__tropel_pm_collection_vars_unset = function (k) { globalThis.__cv_unset = k; };
                globalThis.__tropel_pm_collection_vars_has = function (k) { return k === 'base'; };
                globalThis.__tropel_pm_collection_vars_to_object = function () { return { base: '"x"', n: '3' }; };
                globalThis.__tropel_pm_globals_get = function (k) { return k === 'g' ? '"global"' : null; };
                globalThis.__tropel_pm_globals_set = function (k, v) { globalThis.__g_set = k + '=' + v; };
                globalThis.__tropel_pm_globals_unset = function (k) { globalThis.__g_unset = k; };
                globalThis.__tropel_pm_globals_has = function (k) { return k === 'g'; };
                globalThis.__tropel_pm_globals_to_object = function () { return { g: '"global"' }; };
                globalThis.__tropel_pm_environment_has = function (k) { return k === 'env'; };
                globalThis.__tropel_pm_environment_to_object = function () { return { env: 'e' }; };

                // pm.request stubs — capture what the shim sends back.
                globalThis.__tropel_pm_request_url = function () { return 'http://x/old'; };
                globalThis.__tropel_pm_request_url_set = function (u) { globalThis.__r_url = u; };
                globalThis.__tropel_pm_request_method = function () { return 'GET'; };
                globalThis.__tropel_pm_request_method_set = function (m) { globalThis.__r_method = m; };
                globalThis.__tropel_pm_request_headers = function () { return { Authorization: 'Bearer old' }; };
                globalThis.__tropel_pm_request_header_get = function (k) { return k.toLowerCase() === 'authorization' ? 'Bearer old' : null; };
                globalThis.__tropel_pm_request_header_set = function (k, v) { globalThis.__r_hdr = k + '=' + v; };
                globalThis.__tropel_pm_request_header_unset = function (k) { globalThis.__r_hdr_unset = k; };
                globalThis.__tropel_pm_request_body = function () { return 'old-body'; };
                globalThis.__tropel_pm_request_body_set = function (b) { globalThis.__r_body = b; };
                globalThis.__tropel_pm_request_auth_set = function (a) { globalThis.__r_auth = a; };

                // Response cookies + test-skip + setNextRequest stubs.
                globalThis.__tropel_pm_response_cookies = function () { return { sid: 'abc' }; };
                globalThis.__tropel_pm_test_skip = function (n) { globalThis.__skipped = n; };
                globalThis.__tropel_pm_set_next_request = function (n) { globalThis.__next_req = n; };

                // collectionVariables / globals surface.
                globalThis.__cv_get = pm.collectionVariables.get('base');
                globalThis.__cv_has = pm.collectionVariables.has('base');
                globalThis.__cv_obj = JSON.stringify(pm.collectionVariables.toObject());
                pm.collectionVariables.set('k', 'v');
                pm.collectionVariables.unset('k');
                globalThis.__g_get = pm.globals.get('g');
                globalThis.__g_has = pm.globals.has('g');
                globalThis.__env_has = pm.environment.has('env');
                globalThis.__env_obj = JSON.stringify(pm.environment.toObject());

                // pm.request mutation idioms. Capture AFTER each write so
                // later calls (upsert/remove) can't clobber the captured value.
                pm.request.url = 'http://x/new';
                globalThis.__r_url = globalThis.__r_url;
                pm.request.method = 'POST';
                globalThis.__r_method = globalThis.__r_method;
                pm.request.headers.add({ key: 'Authorization', value: 'Bearer new' });
                globalThis.__r_hdr_add = globalThis.__r_hdr;
                pm.request.headers.upsert({ key: 'X-Extra', value: '1' });
                globalThis.__r_hdr_upsert = globalThis.__r_hdr;
                pm.request.headers.remove('X-Extra');
                globalThis.__r_hdr_unset = globalThis.__r_hdr_unset;
                pm.request.body = 'new-body';
                pm.request.body.raw = 'raw-body';
                globalThis.__r_body = globalThis.__r_body;
                pm.request.auth = { type: 'bearer', token: 't' };
                globalThis.__r_auth = globalThis.__r_auth;
                globalThis.__r_get_hdr = pm.request.headers.get('authorization');
                // Canonical Postman body idiom: pm.request.body.raw = ...
                globalThis.__r_body_raw = pm.request.body.raw;

                // pm.cookies surface.
                globalThis.__ck_get = pm.cookies.get('sid');
                globalThis.__ck_has = pm.cookies.has('sid');
                globalThis.__ck_obj = JSON.stringify(pm.cookies.toObject());

                // pm.expect.fail always throws.
                globalThis.__expect_fail_threw = String((function () {
                    try { pm.expect.fail('boom'); return 'no'; } catch (e) { return 'threw'; }
                })());

                // pm.test.skip records without running.
                pm.test.skip('slow-test');

                // postman.setNextRequest legacy global delegates.
                postman.setNextRequest('login');
            "#,
            )
            .expect("script should eval");

            assert_eq!(ctx.eval::<String, _>("__cv_get").unwrap(), "https://api.example.com", "pm.collectionVariables.get");
            assert!(ctx.eval::<bool, _>("__cv_has").unwrap(), "pm.collectionVariables.has");
            assert_eq!(ctx.eval::<String, _>("__cv_obj").unwrap(), r#"{"base":"x","n":3}"#, "toObject must JSON-decode values");
            assert_eq!(
                ctx.eval::<String, _>("__cv_set").unwrap(),
                "k=\"v\"",
                "collectionVariables.set must reach the bridge JSON-encoded (shim encodes on set)"
            );
            assert_eq!(ctx.eval::<String, _>("__cv_unset").unwrap(), "k", "collectionVariables.unset must reach the bridge");
            assert_eq!(ctx.eval::<String, _>("__g_get").unwrap(), "global", "pm.globals.get");
            assert!(ctx.eval::<bool, _>("__g_has").unwrap(), "pm.globals.has");
            assert!(ctx.eval::<bool, _>("__env_has").unwrap(), "pm.environment.has");
            assert_eq!(ctx.eval::<String, _>("__env_obj").unwrap(), r#"{"env":"e"}"#, "environment.toObject");

            assert_eq!(ctx.eval::<String, _>("__r_url").unwrap(), "http://x/new", "pm.request.url setter must reach the bridge");
            assert_eq!(ctx.eval::<String, _>("__r_method").unwrap(), "POST", "pm.request.method setter must reach the bridge");
            assert_eq!(ctx.eval::<String, _>("__r_hdr_add").unwrap(), "Authorization=Bearer new", "pm.request.headers.add must reach the bridge");
            assert_eq!(ctx.eval::<String, _>("__r_hdr_upsert").unwrap(), "X-Extra=1", "pm.request.headers.upsert must reach the bridge");
            assert_eq!(ctx.eval::<String, _>("__r_hdr_unset").unwrap(), "X-Extra", "pm.request.headers.remove must reach the bridge");
            assert_eq!(ctx.eval::<String, _>("__r_body").unwrap(), "raw-body", "pm.request.body setter must reach the bridge (last write: body.raw)");
            assert_eq!(ctx.eval::<String, _>("__r_body_raw").unwrap(), "old-body", "pm.request.body.raw getter must read through the bridge");
            assert_eq!(ctx.eval::<String, _>("__r_auth").unwrap(), r#"{"type":"bearer","token":"t"}"#, "pm.request.auth must JSON-encode the config");
            assert_eq!(ctx.eval::<String, _>("__r_get_hdr").unwrap(), "Bearer old", "pm.request.headers.get must be case-insensitive");

            assert_eq!(ctx.eval::<String, _>("__ck_get").unwrap(), "abc", "pm.cookies.get");
            assert!(ctx.eval::<bool, _>("__ck_has").unwrap(), "pm.cookies.has");
            assert_eq!(ctx.eval::<String, _>("__ck_obj").unwrap(), r#"{"sid":"abc"}"#, "pm.cookies.toObject");
            assert_eq!(ctx.eval::<String, _>("__expect_fail_threw").unwrap(), "threw", "pm.expect.fail must always throw");
            assert_eq!(ctx.eval::<String, _>("__skipped").unwrap(), "slow-test", "pm.test.skip must record via the bridge");
            assert_eq!(ctx.eval::<String, _>("__next_req").unwrap(), "login", "postman.setNextRequest must delegate");
        });
    }

    #[test]
    fn test_pm_variables_set_coerces_and_skip_request() {
        // Backlog line 146: pm.variables.set('id', 42) threw TypeError (the
        // shim passed the RAW value into a strict String bridge param) while
        // environment.set/collectionVariables.set/globals.set all coerced;
        // pm.execution.skipRequest() threw (routed null into a String) AND
        // semantically stopped the whole run instead of skipping the current
        // item; and variable values were silently retyped by JSON.parse
        // ("1.10" → 1.1). All three are fixed here.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Bridge stubs — variables_get returns JSON-encoded strings
                // (the real bridge JSON-encodes so the shim can restore the
                // type), set/skipRequest capture what the shim sends.
                globalThis.__tropel_pm_variables_get = function (k) {
                    if (k === 'id') return '"42"';
                    if (k === 'one10') return '"1.10"';
                    return null;
                };
                globalThis.__tropel_pm_variables_set = function (k, v) { globalThis.__v_set = k + '=' + v; };
                globalThis.__tropel_pm_variables_unset = function (k) { globalThis.__v_unset = k; };
                globalThis.__tropel_pm_skip_request = function () { globalThis.__skipped = true; };
                globalThis.__tropel_pm_set_next_request = function (n) { globalThis.__next_req = n; };

                // 1) Number value must not throw, must be coerced like the
                // other stores, and must reach the bridge as a string.
                pm.variables.set('id', 42);
                globalThis.__v_set_num = globalThis.__v_set;

                // 2) get must restore types WITHOUT retyping string values:
                // "42" stays the string "42" (not number 42) and "1.10"
                // stays the string "1.10" (not number 1.1).
                globalThis.__v_get_num = pm.variables.get('id');
                globalThis.__v_get_num_type = typeof globalThis.__v_get_num;
                globalThis.__v_get_one10 = pm.variables.get('one10');
                globalThis.__v_get_one10_type = typeof globalThis.__v_get_one10;

                // 3) skipRequest must call the dedicated bridge (no throw,
                // no setNextRequest(null) routing).
                pm.execution.skipRequest();
                globalThis.__skipped_after = globalThis.__skipped;
                globalThis.__next_req_after_skip = globalThis.__next_req === undefined ? 'unset' : String(globalThis.__next_req);

                // 4) setNextRequest(null) must not throw (null → Option<String>).
                pm.execution.setNextRequest(null);
                globalThis.__next_req_null = globalThis.__next_req === undefined || globalThis.__next_req === null ? 'null-or-unset' : String(globalThis.__next_req);

                // 5) W1-A: stopOnError must be ABSENT — it was an invented
                // method on a skip_tests flag that nothing ever read, so the
                // author's intent was silently ignored. Real Postman has no
                // stopOnError (only setNextRequest/skipRequest); being absent
                // makes the call throw "is not a function" like real Postman.
                globalThis.__stop_on_error_type = typeof pm.execution.stopOnError;
                try {
                    pm.execution.stopOnError();
                    globalThis.__stop_on_error_call = 'no-throw';
                } catch (e) {
                    globalThis.__stop_on_error_call = e.name;
                }
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__v_set_num").unwrap(),
                "id=42",
                "pm.variables.set must coerce a number to a string like environment.set"
            );
            assert_eq!(
                ctx.eval::<String, _>("__v_get_num").unwrap(),
                "42",
                "pm.variables.get must return the string \"42\", never the number 42"
            );
            assert_eq!(
                ctx.eval::<String, _>("__v_get_num_type").unwrap(),
                "string",
                "numeric-looking string must not be retyped to a number"
            );
            assert_eq!(
                ctx.eval::<String, _>("__v_get_one10").unwrap(),
                "1.10",
                "\"1.10\" must round-trip as the string \"1.10\", not the number 1.1"
            );
            assert_eq!(
                ctx.eval::<String, _>("__v_get_one10_type").unwrap(),
                "string",
                "trailing-zero string must not be silently retyped by JSON.parse"
            );
            assert!(
                ctx.eval::<bool, _>("__skipped_after").unwrap(),
                "pm.execution.skipRequest must reach the dedicated __tropel_pm_skip_request bridge"
            );
            assert_eq!(
                ctx.eval::<String, _>("__next_req_after_skip").unwrap(),
                "unset",
                "skipRequest must NOT route through setNextRequest"
            );
            assert_eq!(
                ctx.eval::<String, _>("__next_req_null").unwrap(),
                "null-or-unset",
                "setNextRequest(null) must not throw (Option<String> bridge param)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__stop_on_error_type").unwrap(),
                "undefined",
                "pm.execution.stopOnError must be ABSENT so calling it throws like real Postman"
            );
            assert_eq!(
                ctx.eval::<String, _>("__stop_on_error_call").unwrap(),
                "TypeError",
                "calling pm.execution.stopOnError must throw like real Postman, never silently no-op"
            );
        });
    }

    #[test]
    fn test_pm_set_get_roundtrip_preserves_type() {
        // Backlog line 89: setters String()-coerced and getters JSON.parse'd
        // were NOT inverses — a plain string '1234' set through
        // pm.environment/globals came back as the NUMBER 1234 (and objects
        // became "[object Object]"). The shim now JSON-encodes on set; these
        // bridge stubs mirror the REAL decode-on-set/encode-on-get contract
        // (crates/tropel-sandbox trp.rs), so the round trip must restore the
        // exact type: strings stay strings, numbers stay numbers, objects
        // stay objects.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // decode_json_encoded equivalent: JSON string → plain string,
                // anything else (number/bool/object) → its JSON text.
                function decodeEnv(v) {
                    try { var p = JSON.parse(v); return typeof p === 'string' ? p : v; }
                    catch (e) { return v; }
                }
                // decode_json_value equivalent: parse to a serde-like Value.
                function decodeVal(v) {
                    try { return JSON.parse(v); } catch (e) { return v; }
                }

                var env = {};
                globalThis.__tropel_pm_environment_set = function (k, v) { env[k] = decodeEnv(v); };
                globalThis.__tropel_pm_environment_get = function (k) { return k in env ? JSON.stringify(env[k]) : null; };

                var col = {};
                globalThis.__tropel_pm_collection_vars_set = function (k, v) { col[k] = decodeVal(v); };
                globalThis.__tropel_pm_collection_vars_get = function (k) { return k in col ? JSON.stringify(col[k]) : null; };

                var gl = {};
                globalThis.__tropel_pm_globals_set = function (k, v) { gl[k] = decodeVal(v); };
                globalThis.__tropel_pm_globals_get = function (k) { return k in gl ? JSON.stringify(gl[k]) : null; };

                // env: '1234' string stays the STRING '1234' (never number).
                pm.environment.set('s', '1234');
                globalThis.__env_s = pm.environment.get('s');
                globalThis.__env_s_type = typeof globalThis.__env_s;
                // env: number 42 round-trips as the STRING '42' (env is strings-only).
                pm.environment.set('n', 42);
                globalThis.__env_n = pm.environment.get('n');
                globalThis.__env_n_type = typeof globalThis.__env_n;
                // env: object round-trips as its JSON text string.
                pm.environment.set('o', { a: 1 });
                globalThis.__env_o = pm.environment.get('o');

                // collection: numeric string stays string, object stays object.
                pm.collectionVariables.set('s', '42');
                globalThis.__col_s = pm.collectionVariables.get('s');
                globalThis.__col_s_type = typeof globalThis.__col_s;
                pm.collectionVariables.set('o', { b: [1, 2] });
                globalThis.__col_o = JSON.stringify(pm.collectionVariables.get('o'));
                globalThis.__col_o_type = typeof pm.collectionVariables.get('o');

                // globals: number stays number, string stays string.
                pm.globals.set('n', 42);
                globalThis.__gl_n = pm.globals.get('n');
                globalThis.__gl_n_type = typeof globalThis.__gl_n;
                pm.globals.set('s', '1234');
                globalThis.__gl_s = pm.globals.get('s');
                globalThis.__gl_s_type = typeof globalThis.__gl_s;
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__env_s").unwrap(),
                "1234",
                "env string '1234' must round-trip as the STRING '1234'"
            );
            assert_eq!(
                ctx.eval::<String, _>("__env_s_type").unwrap(),
                "string",
                "numeric-looking env string must not be retyped to a number"
            );
            assert_eq!(
                ctx.eval::<String, _>("__env_n").unwrap(),
                "42",
                "env number 42 round-trips as the string '42' (env is strings-only)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__env_n_type").unwrap(),
                "string",
                "env values are always strings"
            );
            assert_eq!(
                ctx.eval::<String, _>("__env_o").unwrap(),
                "{\"a\":1}",
                "env object round-trips as its JSON text string"
            );
            assert_eq!(
                ctx.eval::<String, _>("__col_s").unwrap(),
                "42",
                "collection string '42' must stay the STRING '42'"
            );
            assert_eq!(
                ctx.eval::<String, _>("__col_s_type").unwrap(),
                "string",
                "collection must not retype numeric-looking strings"
            );
            assert_eq!(
                ctx.eval::<String, _>("__col_o").unwrap(),
                "{\"b\":[1,2]}",
                "collection object must round-trip as an object"
            );
            assert_eq!(
                ctx.eval::<String, _>("__col_o_type").unwrap(),
                "object",
                "collection object must stay an object"
            );
            assert_eq!(
                ctx.eval::<f64, _>("__gl_n").unwrap(),
                42.0,
                "globals number 42 round-trips as the number 42"
            );
            assert_eq!(
                ctx.eval::<String, _>("__gl_n_type").unwrap(),
                "number",
                "globals numbers stay numbers"
            );
            assert_eq!(
                ctx.eval::<String, _>("__gl_s").unwrap(),
                "1234",
                "globals string '1234' round-trips as the STRING '1234'"
            );
            assert_eq!(
                ctx.eval::<String, _>("__gl_s_type").unwrap(),
                "string",
                "globals numeric-looking string must stay a string"
            );
        });
    }

    #[test]
    fn test_ws_local_close_dispatches_close_handler() {
        // Backlog line 148: the event-driven k6/ws API (socket.on/send/ping/
        // close/setTimeout + native __tropel_k6_ws_* bridges) is implemented,
        // but a LOCAL socket.close() only set _closed — the synchronous pump
        // then exited at the next iteration and `socket.on('close', ...)`
        // never fired. The k6 idiom of closing inside on('open')/'message'
        // leaked the final cleanup callback.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            // Stub the native ws bridges — no real socket needed.
            ctx.eval::<(), _>(
                r#"
                globalThis.__ws_step_calls = 0;
                globalThis.__tropel_k6_ws_connect = function (url, headersJson) {
                    return JSON.stringify({ id: 42, error: null });
                };
                globalThis.__tropel_k6_ws_step = function (id, timeoutMs) {
                    globalThis.__ws_step_calls++;
                    // First step delivers 'open'; a local close() ends the
                    // pump right after, so later steps are never reached.
                    if (globalThis.__ws_step_calls === 1) {
                        return JSON.stringify({ type: 'open' });
                    }
                    return JSON.stringify({ type: 'none' });
                };
                globalThis.__tropel_k6_ws_send = function (id, data) { globalThis.__ws_sent = data; return '{"ok":true}'; };
                globalThis.__tropel_k6_ws_ping = function (id) { globalThis.__ws_pinged = true; return '{"ok":true}'; };
                globalThis.__tropel_k6_ws_close = function (id, code, reason) {
                    globalThis.__ws_close_bridge = code + ':' + reason;
                    return '{"ok":true}';
                };
                globalThis.__tropel_k6_ws_finish = function (id) { globalThis.__ws_finished = id; return '{"ok":true}'; };

                globalThis.__ws_close_events = [];
                var ret = ws.connect('ws://localhost:1/', {}, function (socket) {
                    socket.on('open', function () {
                        socket.send('hi');
                        socket.ping();
                        socket.close(1000, 'bye');
                    });
                    socket.on('close', function (code, reason) {
                        globalThis.__ws_close_events.push(code + ':' + reason);
                    });
                });
                // ws.connect returns the socket (k6 semantics).
                globalThis.__ret_is_socket = ret && typeof ret.on === 'function';
                // Capture BEFORE the second scenario runs (its defensive
                // socket.close() would otherwise clobber __ws_close_bridge
                // via the unconditional native bridge call).
                globalThis.__ws_close_bridge_a = globalThis.__ws_close_bridge;

                // Second scenario: SERVER closes first, and the close handler
                // defensively calls socket.close() again. The pump marks the
                // socket closed BEFORE dispatching, so 'close' must fire once.
                globalThis.__ws_step_calls = 0;
                globalThis.__tropel_k6_ws_step = function (id, timeoutMs) {
                    globalThis.__ws_step_calls++;
                    if (globalThis.__ws_step_calls === 1) {
                        return JSON.stringify({ type: 'close', code: 1006, reason: 'server gone' });
                    }
                    return JSON.stringify({ type: 'none' });
                };
                globalThis.__ws_close2 = [];
                ws.connect('ws://localhost:2/', {}, function (socket) {
                    socket.on('close', function (code, reason) {
                        globalThis.__ws_close2.push(code + ':' + reason);
                        socket.close(1000, 'defensive'); // must not double-dispatch
                    });
                });
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__ws_close_bridge_a").unwrap(),
                "1000:bye",
                "native close bridge must receive code+reason"
            );
            let close_events: String = ctx
                .eval("JSON.stringify(__ws_close_events)")
                .expect("read close events");
            assert_eq!(
                close_events, "[\"1000:bye\"]",
                "local close() must dispatch the close handler exactly once: {close_events}"
            );
            assert_eq!(
                ctx.eval::<String, _>("__ws_sent").unwrap(),
                "hi",
                "socket.send must reach the native bridge"
            );
            assert!(
                ctx.eval::<bool, _>("__ws_pinged").unwrap(),
                "socket.ping must reach the native bridge"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__ws_finished").unwrap(),
                42,
                "ws.connect must tear the session down in its finally block"
            );
            assert!(
                ctx.eval::<bool, _>("__ret_is_socket").unwrap(),
                "ws.connect must return the K6Socket"
            );
            assert!(
                ctx.eval::<i64, _>("__ws_step_calls").unwrap() >= 1,
                "event pump must have run"
            );
            let close2_events: String = ctx
                .eval("JSON.stringify(__ws_close2)")
                .expect("read server-close events");
            assert_eq!(
                close2_events, "[\"1006:server gone\"]",
                "server close must dispatch once even with a defensive user close(): {close2_events}"
            );
        });
    }

    /// Backlog line 62: ws_req_failed was hardcoded 0.0 and a failed
    /// ws.connect handshake emitted ZERO ws metrics. k6 parity: a refused
    /// connection must emit ws_connecting (time to failure) AND a
    /// ws_req_failed=1.0 Rate sample, so thresholds see the failure instead
    /// of the request silently vanishing.
    #[tokio::test]
    async fn test_ws_failed_handshake_emits_failed_metrics() {
        let driver = K6Driver;
        // Port 1 is not listening on any CI/local box → immediate
        // ECONNREFUSED, no server needed.
        let script = br#"
            export default function () {
                ws.connect('ws://127.0.0.1:1/', {}, function (socket) {});
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        // The shim throws 'ws.connect failed: …' — the iteration errors, but
        // the connect bridge pushed its failure samples BEFORE returning the
        // error JSON, and run_iteration drains the sink unconditionally.
        let _ = inst.run_iteration(&mut ctx).await;
        let failed: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "ws_req_failed")
            .collect();
        assert!(
            !failed.is_empty(),
            "failed handshake must emit ws_req_failed, got: {:?}",
            ctx.samples
                .iter()
                .map(|s| s.metric.as_ref())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            failed[0].value, 1.0,
            "ws_req_failed must be 1.0 on a refused connection"
        );
        assert!(
            ctx.samples.iter().any(|s| s.metric == "ws_connecting"),
            "failed handshake must still emit ws_connecting"
        );
    }

    /// W2 line 188: ws_* metrics hardcoded group="ws" — the ws bridges never
    /// captured the group stack. A failed handshake INSIDE group('g') must
    /// carry group=::g on ws_req_failed/ws_connecting (k6 parity with
    /// http/checks), not the literal "ws".
    #[tokio::test]
    async fn test_ws_failed_handshake_inside_group_carries_group_path() {
        let driver = K6Driver;
        // Port 1 is not listening on any CI/local box → immediate
        // ECONNREFUSED, no server needed.
        let script = br#"
            export default function () {
                group('g', function () {
                    ws.connect('ws://127.0.0.1:1/', {}, function (socket) {});
                });
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        let _ = inst.run_iteration(&mut ctx).await;

        let failed: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "ws_req_failed")
            .collect();
        assert!(
            !failed.is_empty(),
            "failed handshake must emit ws_req_failed, got: {:?}",
            ctx.samples
                .iter()
                .map(|s| s.metric.as_ref())
                .collect::<Vec<_>>()
        );
        let group = failed[0].tags.get("group");
        assert_eq!(
            group,
            Some("::g"),
            "ws inside group('g') must carry group=::g, got: {:?}",
            group
        );
    }

    /// W2 line 188: custom metrics recorded inside a group() carried NO
    /// group tag at all — a Trend.add inside group('checkout') must stamp
    /// group=::checkout like checks/group_duration, so group-filtered
    /// thresholds can see it.
    #[tokio::test]
    async fn test_custom_metric_inside_group_carries_group_path() {
        let driver = K6Driver;
        let script = br#"
            export default function () {
                group('checkout', function () {
                    var t = new Trend('latency');
                    t.add(42);
                });
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration with grouped custom metric must succeed");

        let latency: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "latency")
            .collect();
        assert_eq!(latency.len(), 1, "one Trend.add must be recorded");
        let group = latency[0].tags.get("group");
        assert_eq!(
            group,
            Some("::checkout"),
            "custom metric inside group('checkout') must carry group=::checkout, got: {:?}",
            group
        );
    }

    /// W1-B line 159: a non-finite custom-metric value from JS must be
    /// DROPPED at the bridge, not recorded. The wasm driver guards at the
    /// emitter; the k6 bridge used to take `value: f64` straight from JS, so
    /// `myTrend.add(parseFloat(missingHeader))` → NaN poisoned `sum` forever
    /// (avg=NaN → `avg < 500` false forever) while `f64::NAN.max(0.0) == 0.0`
    /// silently recorded a phantom 0 in the histogram.
    #[tokio::test]
    async fn test_custom_metric_add_drops_non_finite_values() {
        let driver = K6Driver;
        let script = br#"
            export default function () {
                var t = new Trend('latency', true);
                t.add(parseFloat('not-a-number')); // NaN -> must be dropped
                t.add(42);                          // valid -> must survive
                var c = new Counter('events');
                c.add(1);
                c.add(Number('oops'));              // NaN -> must be dropped
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must complete");

        let latency: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "latency")
            .collect();
        assert_eq!(
            latency.len(),
            1,
            "NaN Trend.add must be dropped, got: {:?}",
            latency.iter().map(|s| s.value).collect::<Vec<_>>()
        );
        assert_eq!(latency[0].value, 42.0, "valid Trend.add must survive");

        let events: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "events")
            .collect();
        assert_eq!(
            events.len(),
            1,
            "NaN Counter.add must be dropped, got: {:?}",
            events.iter().map(|s| s.value).collect::<Vec<_>>()
        );
        assert_eq!(events[0].value, 1.0, "valid Counter.add must survive");
    }

    /// Backlog line 62: an abnormal closure (server drops without a close
    /// frame → 1006) must mark the session failed, so finish() emits
    /// ws_req_failed=1.0 — previously hardcoded 0.0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ws_abnormal_close_marks_req_failed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Complete the WebSocket handshake, then drop the connection
            // WITHOUT a close frame → the client reader sees EOF and emits
            // Close{1006} (abnormal closure).
            let _ = tokio_tungstenite::accept_async(stream).await;
            // ws dropped here
        });

        let driver = K6Driver;
        let script = format!(
            "export default function () {{ ws.connect('ws://127.0.0.1:{port}/', {{}}, function (socket) {{ socket.on('open', function () {{}}); }}); }}"
        );
        let mut inst = driver.init(script.as_bytes(), None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must complete after the abnormal close");
        let failed: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "ws_req_failed")
            .collect();
        assert!(
            !failed.is_empty(),
            "finish() must emit ws_req_failed, got: {:?}",
            ctx.samples
                .iter()
                .map(|s| s.metric.as_ref())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            failed[0].value, 1.0,
            "abnormal close (1006) must set ws_req_failed=1.0"
        );
    }

    /// Backlog line 62: a NORMAL close (1000) must keep ws_req_failed=0.0 —
    /// only errors and abnormal closures are failures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ws_normal_close_stays_not_failed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "bye".into(),
                })))
                .await;
        });

        let driver = K6Driver;
        let script = format!(
            "export default function () {{ ws.connect('ws://127.0.0.1:{port}/', {{}}, function (socket) {{ socket.on('open', function () {{}}); }}); }}"
        );
        let mut inst = driver.init(script.as_bytes(), None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must complete after the normal close");
        let failed: Vec<&Sample> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "ws_req_failed")
            .collect();
        assert!(
            !failed.is_empty(),
            "finish() must emit ws_req_failed, got: {:?}",
            ctx.samples
                .iter()
                .map(|s| s.metric.as_ref())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            failed[0].value, 0.0,
            "normal close (1000) must keep ws_req_failed=0.0"
        );
    }

    /// Backlog line 63: group() tagged samples with the INNERMOST raw name
    /// (group=payment) instead of the k6 full path (group=::checkout::payment)
    /// — and checks samples carried NO group tag at all. Nested group()
    /// must tag checks + group_duration with the full ::a::b path.
    #[tokio::test]
    async fn test_nested_group_tags_use_full_path() {
        let driver = K6Driver;
        let script = br#"
            export default function () {
                group('checkout', function () {
                    group('payment', function () {
                        check(true, { 'ok': true });
                    });
                });
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration with nested groups must succeed");

        // checks sample must carry group=::checkout::payment (was untagged).
        let check_groups: Vec<String> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "checks")
            .filter_map(|s| s.tags.get("group").map(|g| g.to_string()))
            .collect();
        assert_eq!(
            check_groups,
            vec!["::checkout::payment".to_string()],
            "checks must carry the full group path, got: {:?}",
            check_groups
        );

        // group_duration samples: one per level, tagged with the full path
        // (::checkout and ::checkout::payment), not the bare leaf.
        let mut durations: Vec<String> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "group_duration")
            .filter_map(|s| s.tags.get("group").map(|g| g.to_string()))
            .collect();
        durations.sort();
        assert_eq!(
            durations,
            vec!["::checkout".to_string(), "::checkout::payment".to_string()],
            "group_duration must use full ::a::b paths, got: {:?}",
            durations
        );
    }

    /// Backlog line 63: http_req_* samples recorded inside nested groups
    /// must carry group=::checkout::payment (k6 parity) — the old code
    /// stamped the innermost raw name, so two same-named leaf groups under
    /// different parents merged into one series.
    #[tokio::test]
    async fn test_http_in_nested_group_carries_full_path() {
        let driver = K6Driver;
        let script = br#"
            export default function () {
                group('checkout', function () {
                    group('payment', function () {
                        http.get('http://example.com/');
                    });
                });
            }
        "#;
        let (client, _sink) = test_ctx().await;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        // Wire the stub HTTP client so the native http bridge registers on
        // the first iteration (same pattern as test_setup_can_make_http_calls).
        ctx.http_client = Some(client);
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration with http inside nested groups must succeed");

        let groups: Vec<String> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "http_req_duration")
            .filter_map(|s| s.tags.get("group").map(|g| g.to_string()))
            .collect();
        assert_eq!(
            groups,
            vec!["::checkout::payment".to_string()],
            "http_req_duration must carry the full group path, got: {:?}",
            groups
        );
    }

    /// Backlog line 74: pm.test(name, asyncFn) ALWAYS passed — the shim
    /// recorded `result !== false` synchronously, and a Promise is never
    /// false, so an async body whose pm.expect failed (and a body returning
    /// Promise.reject) both recorded PASS. The check must now be deferred
    /// until the promise settles: the driver's job pump (inside
    /// run_script_cached) fires the .then handlers, which record the REAL
    /// pass/fail into the sink before run_iteration drains it.
    #[tokio::test]
    async fn test_pm_test_async_body_records_real_result() {
        let driver = K6Driver;
        let script = br#"
            export default function () {
                pm.test('async failing expect', async function () {
                    pm.expect(1).to.equal(2);
                });
                pm.test('async rejecting', async function () {
                    return Promise.reject(new Error('boom'));
                });
                pm.test('async passing', async function () {
                    await Promise.resolve();
                    pm.expect(1).to.equal(1);
                });
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration with async pm.test bodies must succeed");

        let checks: Vec<(String, f64)> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "checks")
            .map(|s| {
                let name = s
                    .tags
                    .get("check")
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                (name, s.value)
            })
            .collect();
        // W1-A: failures record under the ORIGINAL name (Postman/k6 parity) —
        // renaming to `name + ' (error)'` minted a second series that a CI
        // gate on `checks{check:...}` never read (100% pass by construction).
        assert!(
            checks
                .iter()
                .any(|(n, v)| n == "async failing expect" && *v == 0.0),
            "failing async expect must record FAIL under its own name, got: {:?}",
            checks
        );
        assert!(
            checks
                .iter()
                .any(|(n, v)| n == "async rejecting" && *v == 0.0),
            "Promise.reject body must record FAIL under its own name, got: {:?}",
            checks
        );
        assert!(
            checks
                .iter()
                .any(|(n, v)| n == "async passing" && *v == 1.0),
            "passing async body must record PASS, got: {:?}",
            checks
        );
    }

    #[test]
    fn test_check_throws_on_nonsense_and_forwards_tags() {
        // Backlog line 149: check() accepted nonsense as success —
        // check(1, null) and check(1, 'x') returned true (k6 throws); it
        // dropped k6's 3rd tags arg; prefixed names with "check "; and
        // swallowed a throwing predicate (k6 records a failed check then
        // propagates the error to fail the iteration).
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__recorded = [];
                globalThis.__tropel_pm_test = function (name, passed, tagsJson) {
                    globalThis.__recorded.push({
                        name: name,
                        passed: passed,
                        tags: tagsJson ? JSON.parse(tagsJson) : null
                    });
                };

                // 1) Nonsense conds must THROW, not return true.
                globalThis.__null_threw = false;
                try { check(1, null); } catch (e) { globalThis.__null_threw = e instanceof TypeError; }
                globalThis.__str_threw = false;
                try { check(1, 'x'); } catch (e) { globalThis.__str_threw = e instanceof TypeError; }

                // 2) Raw names (no "check " prefix) + 3rd tags arg forwarded.
                check(1, { 'status is 200': function (v) { return v === 1; } }, { tag1: 'a', tag2: 'b' });
                // 3) Non-function conditions are boolean constants (k6 parity).
                check(1, { constTrue: true, constFalse: false });

                // 4) Throwing predicate: records a failed check, then propagates.
                globalThis.__threw_after = null;
                try {
                    check(1, { boom: function () { throw new Error('boom-check'); } });
                } catch (e) {
                    globalThis.__threw_after = e.message;
                }
            "#,
            )
            .expect("script should eval");

            assert!(
                ctx.eval::<bool, _>("__null_threw").unwrap(),
                "check(1, null) must throw a TypeError (k6 parity), not return true"
            );
            assert!(
                ctx.eval::<bool, _>("__str_threw").unwrap(),
                "check(1, 'x') must throw a TypeError (k6 parity), not return true"
            );
            assert_eq!(
                ctx.eval::<String, _>("__recorded[0].name").unwrap(),
                "status is 200",
                "check name must NOT be prefixed with 'check '"
            );
            let tags: String = ctx
                .eval("JSON.stringify(__recorded[0].tags)")
                .expect("read recorded tags");
            assert_eq!(
                tags, "{\"tag1\":\"a\",\"tag2\":\"b\"}",
                "k6 3rd tags arg must reach the bridge: {tags}"
            );
            assert!(
                ctx.eval::<bool, _>("__recorded[1].passed").unwrap(),
                "non-function truthy condition must pass (ToBoolean k6 parity)"
            );
            assert!(
                !ctx.eval::<bool, _>("__recorded[2].passed").unwrap(),
                "non-function falsy condition must fail (ToBoolean k6 parity)"
            );
            // Throwing predicate: failed check recorded, then error propagates.
            assert!(
                !ctx.eval::<bool, _>("__recorded[3].passed").unwrap(),
                "throwing predicate must record a failed check"
            );
            assert_eq!(
                ctx.eval::<String, _>("__threw_after").unwrap(),
                "boom-check",
                "throwing predicate must propagate (k6 fails the iteration)"
            );
        });
    }

    #[test]
    fn test_pm_test_async_body_records_on_settlement() {
        // Backlog line 84: pm.test(name, asyncFn) always passed — the body's
        // Promise is never `=== false`, so the check recorded GREEN before the
        // async body settled; a rejected body ALSO passed. The check must be
        // recorded at settlement time: rejected async body → FAILED check
        // (mirrors the sync throw path), resolved body → value !== false.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                // Bare Context::full has no console global; pm.test's error
                // paths call console.error, so stub it (real k6/sandbox
                // contexts install one during bootstrap).
                globalThis.console = {
                    error: function () {},
                    log: function () {},
                    warn: function () {}
                };
                globalThis.__recorded = [];
                globalThis.__tropel_pm_test = function (name, passed, tagsJson) {
                    globalThis.__recorded.push({
                        name: name,
                        passed: passed,
                        tags: tagsJson ? JSON.parse(tagsJson) : null
                    });
                };
                globalThis.__run = (async function () {
                    // Rejected async body (stand-in for a failing pm.expect
                    // inside an async fn): must record FAILED, not PASS.
                    await pm.test('async-fail', async function () {
                        throw new Error('boom');
                    });
                    // Body returning a rejected promise: same.
                    await pm.test('async-reject', function () {
                        return Promise.reject(new Error('nope'));
                    });
                    // Resolved async body: value semantics (true → PASS).
                    await pm.test('async-pass', async function () { return true; });
                    // Sync paths unchanged.
                    pm.test('sync-pass', function () { return true; });
                    pm.test('sync-fail', function () { throw new Error('sync'); });
                })();
            "#,
            )
            .expect("async pm.test driver script should eval");

            let run: rquickjs::Promise = ctx
                .eval("globalThis.__run")
                .expect("read the async driver promise");
            run.finish::<()>()
                .expect("async pm.test bodies must settle (job pump)");

            let by_name = |name: &str| -> (String, bool) {
                let q = format!(
                    "(function(){{ var r = __recorded.find(x => x.name === '{}'); return r ? [r.name, r.passed] : null; }})()",
                    name
                );
                // rquickjs 0.12 `eval` takes `Into<Vec<u8>>` — `&String`
                // doesn't impl it, so eval the `&str`.
                let v: rquickjs::Value = ctx.eval(q.as_str()).unwrap_or_else(|e| {
                    panic!("find {}: {}", name, e)
                });
                if v.is_null() {
                    panic!("no recorded check named {}", name);
                }
                // `Value::as_array` here returns `Option<&Array>` (not Cow),
                // so `.clone()` yields an owned Array.
                let arr = v.as_array().expect("recorded entry array").clone();
                let n: String = arr.get(0).expect("name");
                let p: bool = arr.get(1).expect("passed");
                (n, p)
            };

            // W1-A: failures record under the ORIGINAL name — a gate on
            // `checks{check:...}` must see the failures, not a derived
            // ` (error)` series that is pass-by-construction.
            let (n, p) = by_name("async-fail");
            assert_eq!(n, "async-fail", "rejected async body name");
            assert!(!p, "rejected async body must record a FAILED check, not PASS");
            let (_n, p) = by_name("async-reject");
            assert!(!p, "Promise.reject body must record a FAILED check");
            let (n, p) = by_name("async-pass");
            assert_eq!(n, "async-pass");
            assert!(p, "resolved async body returning true must PASS");
            let (_n, p) = by_name("sync-pass");
            assert!(p, "sync true body must still PASS");
            let (_n, p) = by_name("sync-fail");
            assert!(!p, "sync throwing body must still FAIL under its own name");
        });
    }

    #[test]
    fn test_timings_error_binary_and_http_file() {
        // Backlog line 150: the k6 path emitted no sub-timing samples, res.
        // timings.* were 0 except waiting/duration, res.error/error_code did
        // not exist, binary bodies were destroyed (ArrayBuffer → ''), and
        // http.file() was missing. This exercises the shim + bridge contract:
        // real timings flow to res.timings, errors set res.error/error_code,
        // binary responses become ArrayBuffer, and http.file() uploads base64
        // bytes flagged bodyB64.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/k6-shim/k6-shim.js"))
                .expect("k6 shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__captured = null;
                globalThis.__tropel_k6_http_request = function (method, url, headersJson, body, responseType, extrasJson) {
                    globalThis.__captured = {
                        method: method,
                        url: url,
                        body: body,
                        responseType: responseType,
                        extras: JSON.parse(extrasJson)
                    };
                    // Success path with real timings.
                    return {
                        code: 200,
                        status: 200,
                        status_text: 'OK',
                        headers: { 'Content-Type': 'text/plain' },
                        response_time: 150.0,
                        error: '',
                        error_code: 0,
                        timings: {
                            blocked: 1.0, dns: 2.0, connecting: 3.0,
                            tls_handshaking: 0, sending: 0,
                            waiting: 100.0, receiving: 50.0, duration: 150.0
                        },
                        body: 'hello'
                    };
                };

                // 1) Real timings + error/error_code on success.
                var res1 = http.get('http://example.com/');
                globalThis.__t_waiting = res1.timings.waiting;
                globalThis.__t_blocked = res1.timings.blocked;
                globalThis.__t_receiving = res1.timings.receiving;
                globalThis.__err = res1.error;
                globalThis.__ecode = res1.error_code;

                // 2) Binary response body via body_b64 → ArrayBuffer.
                var b64bytes = '';
                // base64("\x00\x01\xFF") = "AAH/"
                globalThis.__tropel_k6_http_request = function (m, u, h, b, rt, ex) {
                    return { code: 200, status_text: 'OK', headers: {}, response_time: 5,
                             body: 'AAH/', body_b64: true, error: '', error_code: 0 };
                };
                var res2 = http.get('http://example.com/bin', { responseType: 'binary' });
                globalThis.__bin = res2.body instanceof ArrayBuffer;
                globalThis.__bin_len = res2.body instanceof ArrayBuffer ? res2.body.byteLength : -1;
                var bytes = new Uint8Array(res2.body);
                globalThis.__bin_0 = bytes[0];
                globalThis.__bin_1 = bytes[1];
                globalThis.__bin_2 = bytes[2];

                // 3) http.file() with binary data → base64 + bodyB64 flag.
                var file = http.file(new Uint8Array([104, 105, 33]), 'hello.bin', 'application/octet-stream');
                globalThis.__file_data_len = file.data.byteLength;
                globalThis.__file_name = file.filename;
                globalThis.__file_ct = file.content_type;
                // Re-arm the CAPTURING stub before the upload: the binary
                // stub above does not capture, so reading __captured here
                // without re-arming would see the stale GET from step 1.
                globalThis.__tropel_k6_http_request = function (method, url, headersJson, body, responseType, extrasJson) {
                    globalThis.__captured = {
                        method: method,
                        url: url,
                        body: body,
                        responseType: responseType,
                        extras: JSON.parse(extrasJson)
                    };
                    return { code: 200, status_text: 'OK', headers: {}, response_time: 5,
                             error: '', error_code: 0, body: 'ok' };
                };
                http.post('http://example.com/upload', file);
                globalThis.__up_body = globalThis.__captured.body;
                globalThis.__up_b64 = globalThis.__captured.extras.bodyB64;

                // 4) Transport failure → res.error / res.error_code set.
                globalThis.__tropel_k6_http_request = function (m, u, h, b, rt, ex) {
                    return { code: 0, status: 0, status_text: 'HTTP error: dns lookup failed',
                             headers: {}, response_time: 0, error: 'dns lookup failed',
                             error_code: 1100, body: '' };
                };
                var res3 = http.get('http://nope.invalid/');
                globalThis.__err3 = res3.error;
                globalThis.__ecode3 = res3.error_code;
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<f64, _>("__t_waiting").unwrap(),
                100.0,
                "res.timings.waiting must carry the REAL TTFB (was duration)"
            );
            assert_eq!(
                ctx.eval::<f64, _>("__t_blocked").unwrap(),
                1.0,
                "res.timings.blocked must carry the real pool-wait value"
            );
            assert_eq!(
                ctx.eval::<f64, _>("__t_receiving").unwrap(),
                50.0,
                "res.timings.receiving must carry the real body-receive value"
            );
            assert_eq!(
                ctx.eval::<String, _>("__err").unwrap(),
                "",
                "res.error must be '' on success (k6 parity)"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__ecode").unwrap(),
                0,
                "res.error_code must be 0 on success (k6 parity)"
            );
            assert!(
                ctx.eval::<bool, _>("__bin").unwrap(),
                "binary response body must surface as an ArrayBuffer"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__bin_len").unwrap(),
                3,
                "binary response body must keep its byte length"
            );
            assert_eq!(ctx.eval::<i64, _>("__bin_0").unwrap(), 0, "byte 0");
            assert_eq!(ctx.eval::<i64, _>("__bin_1").unwrap(), 1, "byte 1");
            assert_eq!(ctx.eval::<i64, _>("__bin_2").unwrap(), 255, "byte 2");
            assert_eq!(
                ctx.eval::<i64, _>("__file_data_len").unwrap(),
                3,
                "http.file().data must keep the raw bytes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__file_name").unwrap(),
                "hello.bin",
                "http.file() must preserve the filename"
            );
            assert_eq!(
                ctx.eval::<String, _>("__file_ct").unwrap(),
                "application/octet-stream",
                "http.file() must preserve the content type"
            );
            assert_eq!(
                ctx.eval::<String, _>("__up_body").unwrap(),
                "aGkh",
                "http.file() binary upload must base64-encode the bytes for the bridge"
            );
            assert!(
                ctx.eval::<bool, _>("__up_b64").unwrap(),
                "binary upload must flag bodyB64 so Rust decodes to raw bytes"
            );
            assert_eq!(
                ctx.eval::<String, _>("__err3").unwrap(),
                "dns lookup failed",
                "transport failure must set res.error (k6 `if (res.error)` idiom)"
            );
            assert_eq!(
                ctx.eval::<i64, _>("__ecode3").unwrap(),
                1100,
                "transport failure must set a k6-style error_code (DNS=1100)"
            );
        });
    }

    #[test]
    fn test_push_http_samples_emits_sub_timing_metrics() {
        // Backlog line 150: the k6 path emitted ONLY http_req_duration /
        // http_reqs / http_req_failed / data_* — no waiting/blocked/connecting
        // samples, so k6 thresholds like http_req_waiting:p(95) could never
        // resolve. push_http_samples_for now emits all seven connection-phase
        // Trend samples from the real Timings.
        let sink = std::sync::Mutex::new(Vec::<Sample>::new());
        let scenario: Arc<str> = Arc::from("s");
        let timings = Timings {
            blocked: Duration::from_micros(100),
            dns: Duration::from_micros(200),
            connecting: Duration::from_micros(300),
            tls_handshaking: Duration::ZERO,
            sending: Duration::ZERO,
            waiting: Duration::from_micros(4000),
            receiving: Duration::from_micros(500),
            total: Duration::from_micros(5100),
        };
        push_http_samples_for(
            &sink,
            "http://example.com/",
            "GET",
            200,
            Duration::from_micros(5100),
            42,
            7,
            Some(&timings),
            &scenario,
            None,
            None,
        );
        let samples = sink.lock().unwrap();
        let names: Vec<&str> = samples.iter().map(|s| s.metric.as_ref()).collect();
        for m in [
            "http_req_duration",
            "http_req_blocked",
            "http_req_dns",
            "http_req_connecting",
            "http_req_tls_handshaking",
            "http_req_sending",
            "http_req_waiting",
            "http_req_receiving",
        ] {
            assert!(names.contains(&m), "k6 path must emit {m}, got {names:?}");
        }
        let waiting = samples
            .iter()
            .find(|s| s.metric == "http_req_waiting")
            .unwrap();
        // Values are milliseconds end-to-end (backlog §0): 4000 µs → 4.0 ms,
        // 100 µs → 0.1 ms.
        assert_eq!(waiting.value, 4.0, "waiting must carry the TTFB in ms");
        let blocked = samples
            .iter()
            .find(|s| s.metric == "http_req_blocked")
            .unwrap();
        assert_eq!(blocked.value, 0.1, "blocked must carry the pool-wait in ms");
        assert_eq!(samples.len(), 12, "5 base + 7 sub-timing samples");
    }

    #[test]
    fn test_push_http_failure_emits_duration_and_data_sent() {
        // W1-B line 161: transport failures emitted ONLY http_reqs +
        // http_req_failed — no http_req_duration (time-to-failure) or
        // data_sent, so a fully-down target let p(95)<500 pass on the
        // pre-outage successes and the failure's wire size vanished. The
        // failure path now records the same four samples as the success
        // path (duration as the elapsed time-to-failure).
        let sink = std::sync::Mutex::new(Vec::<Sample>::new());
        let scenario: Arc<str> = Arc::from("s");
        let req = Request {
            url: "http://down:9/".into(),
            method: Method::GET,
            headers: Vec::new(),
            query_params: HashMap::new(),
            body: Some(Body::Raw("payload".to_string())),
            auth: None,
            certificate: None,
            follow_redirects: true,
            timeout: None,
            response_type: tropel_sdk::ResponseType::Text,
        };
        push_http_failure(
            &sink,
            &req,
            &scenario,
            None,
            None,
            Duration::from_millis(1500),
            7,
        );
        let samples = sink.lock().unwrap();
        let duration = samples
            .iter()
            .find(|s| s.metric == "http_req_duration")
            .expect("failure must emit http_req_duration (time-to-failure)");
        assert_eq!(
            duration.value, 1500.0,
            "http_req_duration must carry the elapsed ms"
        );
        assert_eq!(
            duration.sample_type,
            SampleType::Trend,
            "same Trend series as the success path"
        );
        let reqs = samples
            .iter()
            .find(|s| s.metric == "http_reqs")
            .expect("failure must emit http_reqs");
        assert_eq!(reqs.value, 1.0);
        let failed = samples
            .iter()
            .find(|s| s.metric == "http_req_failed")
            .expect("failure must emit http_req_failed");
        assert_eq!(failed.value, 1.0);
        let sent = samples
            .iter()
            .find(|s| s.metric == "data_sent")
            .expect("failure must emit data_sent (wire size)");
        assert_eq!(
            sent.value, 7.0,
            "data_sent must carry the request-body bytes"
        );
        assert_eq!(samples.len(), 4, "duration + reqs + failed + data_sent");
    }

    #[test]
    fn test_pm_response_to_be_status_classes() {
        // Backlog line 145: pm.response.to.be.* — the chai-postman status-class
        // getters and to.have.header/body/jsonBody. Getters THROW on failure
        // (so pm.test() records the single failed check).
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 404; };
                globalThis.__tropel_pm_response_header = function (k) {
                    if (String(k).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                globalThis.__tropel_pm_response_json = function () { return '{"a":1}'; };
                globalThis.__tropel_pm_response_body = function () { return 'not found'; };

                globalThis.__be_success = String((function () {
                    try { pm.response.to.be.success; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__be_client_error = String((function () {
                    try { pm.response.to.be.clientError; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__be_server_error = String((function () {
                    try { pm.response.to.be.serverError; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__be_error = String((function () {
                    try { pm.response.to.be.error; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__be_json = String((function () {
                    try { pm.response.to.be.json(); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // Backlog line 42 (P0): Postman snippets read `to.be.json;` as a
                // PROPERTY (no parens). A bare function value used to read as
                // truthy → silent PASS; the getter must run the check on the read.
                globalThis.__be_json_prop = String((function () {
                    try { pm.response.to.be.json; return 'passed'; } catch (e) { return 'threw'; }
                })());
                // Backlog line 41 (P0): the specific chai-postman status helpers
                // used to be absent → undefined → silent PASS. Now real getters.
                globalThis.__be_not_found = String((function () {
                    try { pm.response.to.be.notFound; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__be_unauthorized = String((function () {
                    try { pm.response.to.be.unauthorized; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__be_with_body = String((function () {
                    try { pm.response.to.be.withBody; return 'passed'; } catch (e) { return 'threw'; }
                })());
                // Backlog line 41 (P0): the guardChain Proxy — ANY unknown
                // assertion name must THROW (failed check), never read as
                // undefined and record green.
                globalThis.__be_unknown = String((function () {
                    try { pm.response.to.be.nonexistentAssertion; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__have_hdr = String((function () {
                    try { pm.response.to.have.header('Content-Type', 'application/json'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__have_body = String((function () {
                    try { pm.response.to.have.body('not'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__have_json_body = String((function () {
                    try { pm.response.to.have.jsonBody({ a: 1 }); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // W1-B line 153: a STRING arg is a KEY PATH (chai-postman
                // semantics), not a deep-equal of the whole body — the old
                // code `deepEqual(body, 'a')` always threw on an object body,
                // so `to.have.jsonBody('key')` was a false failure.
                globalThis.__have_json_body_key = String((function () {
                    try { pm.response.to.have.jsonBody('a'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__have_json_body_key_missing = String((function () {
                    try { pm.response.to.have.jsonBody('b'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // Two-arg form asserts the VALUE at the path too.
                globalThis.__have_json_body_key_value_ok = String((function () {
                    try { pm.response.to.have.jsonBody('a', 1); return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__have_json_body_key_value_bad = String((function () {
                    try { pm.response.to.have.jsonBody('a', 2); return 'passed'; } catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(ctx.eval::<String, _>("__be_success").unwrap(), "threw", "404 must NOT be success");
            assert_eq!(ctx.eval::<String, _>("__be_client_error").unwrap(), "passed", "404 must be clientError");
            assert_eq!(ctx.eval::<String, _>("__be_server_error").unwrap(), "threw", "404 must NOT be serverError");
            assert_eq!(ctx.eval::<String, _>("__be_error").unwrap(), "passed", "404 must be error (>=400)");
            assert_eq!(ctx.eval::<String, _>("__be_json").unwrap(), "passed", "content-type json + valid body must pass to.be.json()");
            assert_eq!(ctx.eval::<String, _>("__be_json_prop").unwrap(), "passed", "property read to.be.json must run the check (valid JSON body)");
            assert_eq!(ctx.eval::<String, _>("__be_not_found").unwrap(), "passed", "404 must be notFound");
            assert_eq!(ctx.eval::<String, _>("__be_unauthorized").unwrap(), "threw", "404 must NOT be unauthorized (401)");
            assert_eq!(ctx.eval::<String, _>("__be_with_body").unwrap(), "passed", "non-empty body must pass to.be.withBody");
            assert_eq!(ctx.eval::<String, _>("__be_unknown").unwrap(), "threw", "unknown to.be.<name> must throw, not silently pass");
            assert_eq!(ctx.eval::<String, _>("__have_hdr").unwrap(), "passed", "to.have.header must pass when header matches");
            assert_eq!(ctx.eval::<String, _>("__have_body").unwrap(), "passed", "to.have.body must pass when substring present");
            assert_eq!(ctx.eval::<String, _>("__have_json_body").unwrap(), "passed", "to.have.jsonBody must deep-compare");
            assert_eq!(ctx.eval::<String, _>("__have_json_body_key").unwrap(), "passed", "jsonBody('a') must pass when the key exists (string = key path)");
            assert_eq!(ctx.eval::<String, _>("__have_json_body_key_missing").unwrap(), "threw", "jsonBody('b') must throw when the key is missing");
            assert_eq!(ctx.eval::<String, _>("__have_json_body_key_value_ok").unwrap(), "passed", "jsonBody('a', 1) must pass when the value matches");
            assert_eq!(ctx.eval::<String, _>("__have_json_body_key_value_bad").unwrap(), "threw", "jsonBody('a', 2) must throw when the value mismatches");

            // The EXACT silent-pass the backlog verified (line 42): a bare
            // PROPERTY read `pm.response.to.be.json;` on a non-JSON body used
            // to record PASS (truthy function). With the getter form it must
            // throw — re-point the json bridge at a throwing body.
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_json = function () { throw new Error('body is not JSON'); };
                globalThis.__be_json_prop_bad = String((function () {
                    try { pm.response.to.be.json; return 'passed'; } catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("second eval should succeed");
            assert_eq!(
                ctx.eval::<String, _>("__be_json_prop_bad").unwrap(),
                "threw",
                "bare to.be.json property read on a non-JSON body must throw (line 42 silent pass)"
            );
        });
    }

    #[test]
    fn oversized_binary_body_degrades_to_status0_envelope_not_panic() {
        // Backlog line 46 (P0): a server-controlled binary response body at/over
        // the per-VU heap cap used to `.expect()`-panic ACROSS the QuickJS FFI
        // boundary. build_k6_response_object must return the status-0 error
        // envelope (same shape as the invalid-method path) instead.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let obj = build_k6_response_object(
                &ctx,
                200,
                "OK".into(),
                vec![0u8; K6_VU_HEAP_BYTES], // >= cap → guaranteed-OOM pre-check
                &HashMap::new(),
                5.0,
                None,
                "",
                0,
                "binary",
                &[],
            )
            .expect("status-0 envelope build must succeed");
            let code: i32 = obj.get("code").expect("code field");
            assert_eq!(
                code, 0,
                "oversized binary body must degrade to status 0, got {code}"
            );
            let err: String = obj.get("error").expect("error field");
            assert!(!err.is_empty(), "envelope must carry an error message");
        });
    }

    #[test]
    fn oversized_text_body_degrades_to_status0_envelope_not_empty_string() {
        // W1-B line 160: the binary branch got the heap-cap pre-guard, the
        // TEXT branch didn't — `let _ = obj.set("body", String::from_utf8_lossy(
        // &body))` swallowed the OOM, so an oversized text body silently
        // became `status:200, body:''` (JSON.parse threw with no indication).
        // The hoisted pre-guard must now degrade BOTH branches to the
        // status-0 error envelope.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            // response_type "" → text branch (default k6 responseType).
            let obj = build_k6_response_object(
                &ctx,
                200,
                "OK".into(),
                vec![b'x'; K6_VU_HEAP_BYTES], // >= cap → guaranteed-OOM pre-check
                &HashMap::new(),
                5.0,
                None,
                "",
                0,
                "",
                &[],
            )
            .expect("status-0 envelope build must succeed");
            let code: i32 = obj.get("code").expect("code field");
            assert_eq!(
                code, 0,
                "oversized text body must degrade to status 0, got {code}"
            );
            let err: String = obj.get("error").expect("error field");
            assert!(!err.is_empty(), "envelope must carry an error message");
            let body: String = obj.get("body").unwrap_or_default();
            assert!(
                body.is_empty(),
                "degraded envelope must not carry a partial/empty-string body"
            );
        });
    }

    #[test]
    fn test_pm_jsonbody_string_key_path_parity() {
        // W1-B line 153: jsonBody("key") must be a KEY-PATH existence check
        // (chai-postman), not a deep-equal of the whole body — the old code
        // `deepEqual(body, 'a')` always threw on an object body. Also pins
        // the lodash-`get` reached-tracking corners: a present-null key
        // ({a: null}) passes jsonBody('a'), a null MID-path ({a:null}, 'a.b')
        // is MISSING (so the negated form passes). Exercises BOTH
        // implementations: pm.js's pm.response.to.have chain and chai-shim's
        // Assertion (pm.expect delegates to chai when it is loaded).
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/chai/chai-shim.js"))
                .expect("chai shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__tropel_pm_response_header = function (k) {
                    if (String(k).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                // Present-NULL body: {a: null} — the key EXISTS with a null
                // value, so jsonBody('a') must PASS (lodash get parity).
                globalThis.__tropel_pm_response_json = function () { return '{"a":null}'; };
                globalThis.__tropel_pm_response_body = function () { return '{"a":null}'; };

                // pm.js chain: present-null key passes.
                globalThis.__jb_null_key_pm = String((function () {
                    try { pm.response.to.have.jsonBody('a'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // pm.js chain: null MID-path is MISSING → throws.
                globalThis.__jb_null_midpath_pm = String((function () {
                    try { pm.response.to.have.jsonBody('a.b'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // chai path (pm.expect delegates to chai): present-null passes.
                globalThis.__jb_null_key_chai = String((function () {
                    try { pm.expect(pm.response).to.have.jsonBody('a'); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // chai path NEGATED: null mid-path is missing, so
                // .not.jsonBody('a.b') must PASS (no TypeError).
                globalThis.__jb_null_midpath_chai_not = String((function () {
                    try { pm.expect(pm.response).to.not.have.jsonBody('a.b'); return 'passed'; } catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(ctx.eval::<String, _>("__jb_null_key_pm").unwrap(), "passed", "jsonBody('a') on {{a:null}} must pass (present-null key, pm.js chain)");
            assert_eq!(ctx.eval::<String, _>("__jb_null_midpath_pm").unwrap(), "threw", "jsonBody('a.b') on {{a:null}} must throw (null mid-path = missing)");
            assert_eq!(ctx.eval::<String, _>("__jb_null_key_chai").unwrap(), "passed", "chai pm.expect jsonBody('a') on {{a:null}} must pass");
            assert_eq!(ctx.eval::<String, _>("__jb_null_midpath_chai_not").unwrap(), "passed", "negated .not.jsonBody('a.b') on {{a:null}} must pass, not TypeError");
        });
    }

    #[test]
    fn test_pm_response_to_be_guarded_status_assertions() {
        // Backlog line 41: pm.response.to.be.* was a bare object literal, so
        // .notFound/.unauthorized/.forbidden/.badRequest/.accepted/.rateLimited
        // /.withBody/.teapot all read as `undefined` and recorded PASS on any
        // response. The tree is now guardChain-wrapped: exact-status getters
        // THROW on mismatch, and ANY unknown assertion name throws too.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 404; };
                globalThis.__tropel_pm_response_header = function (k) {
                    if (String(k).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                globalThis.__tropel_pm_response_json = function () { return '{"a":1}'; };
                globalThis.__tropel_pm_response_body = function () { return 'not found'; };

                // Exact-status getters: only notFound passes on 404.
                globalThis.__b_not_found = String((function () {
                    try { pm.response.to.be.notFound; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_unauthorized = String((function () {
                    try { pm.response.to.be.unauthorized; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_forbidden = String((function () {
                    try { pm.response.to.be.forbidden; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_bad_request = String((function () {
                    try { pm.response.to.be.badRequest; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_accepted = String((function () {
                    try { pm.response.to.be.accepted; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_rate_limited = String((function () {
                    try { pm.response.to.be.rateLimited; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_teapot = String((function () {
                    try { pm.response.to.be.teapot; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__b_with_body = String((function () {
                    try { pm.response.to.be.withBody; return 'passed'; } catch (e) { return 'threw'; }
                })());

                // guardChain: a name that is not implemented ANYWHERE must
                // throw, not silently pass.
                globalThis.__b_unknown = String((function () {
                    try { pm.response.to.be.nonexistentAssertion; return 'passed'; } catch (e) { return 'threw'; }
                })());

                // withBody/withoutBody respond to the actual body.
                globalThis.__b_without_body = String((function () {
                    try { pm.response.to.be.withoutBody; return 'passed'; } catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(ctx.eval::<String, _>("__b_not_found").unwrap(), "passed", "404 must pass notFound");
            assert_eq!(ctx.eval::<String, _>("__b_unauthorized").unwrap(), "threw", "404 must NOT be unauthorized");
            assert_eq!(ctx.eval::<String, _>("__b_forbidden").unwrap(), "threw", "404 must NOT be forbidden");
            assert_eq!(ctx.eval::<String, _>("__b_bad_request").unwrap(), "threw", "404 must NOT be badRequest");
            assert_eq!(ctx.eval::<String, _>("__b_accepted").unwrap(), "threw", "404 must NOT be accepted");
            assert_eq!(ctx.eval::<String, _>("__b_rate_limited").unwrap(), "threw", "404 must NOT be rateLimited");
            assert_eq!(ctx.eval::<String, _>("__b_teapot").unwrap(), "threw", "404 must NOT be teapot");
            assert_eq!(ctx.eval::<String, _>("__b_with_body").unwrap(), "passed", "non-empty body must pass withBody");
            assert_eq!(ctx.eval::<String, _>("__b_unknown").unwrap(), "threw", "unknown assertion must throw (no silent PASS)");
            assert_eq!(ctx.eval::<String, _>("__b_without_body").unwrap(), "threw", "non-empty body must NOT pass withoutBody");

            // The original silent-PASS probe from the backlog: on a 200,
            // .notFound must now FAIL instead of recording PASS.
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__b_not_found_200 = String((function () {
                    try { pm.response.to.be.notFound; return 'passed'; } catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("script should eval");
            assert_eq!(
                ctx.eval::<String, _>("__b_not_found_200").unwrap(),
                "threw",
                "200 must FAIL notFound (the silent-PASS bug)"
            );
        });
    }

    #[test]
    fn test_pm_response_to_be_json_html_text_property_form() {
        // Backlog line 42: chai-postman exposes .json/.html/.text as
        // PROPERTIES — Postman's own snippets emit `pm.response.to.be.json;`
        // with NO parens. They were methods here, so reading the property
        // yielded a truthy Function → silent PASS on any body. Now getters:
        // the bare property read runs the check and THROWS on mismatch, and
        // the paren form still works.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/shared/deep-equal.js"))
                .expect("shared deep-equal should eval");
            ctx.eval::<(), _>(include_str!("../../../../js/scripting-api/pm.js"))
                .expect("pm shim should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_pm_response_code = function () { return 200; };
                globalThis.__tropel_pm_response_header = function (k) {
                    if (String(k).toLowerCase() === 'content-type') return 'text/html';
                    return null;
                };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'text/html' };
                };
                globalThis.__tropel_pm_response_json = function () { return '<html>'; }; // NOT JSON
                globalThis.__tropel_pm_response_body = function () { return '<html>'; };

                // The silent-PASS probe from the backlog: bare property read
                // on a text/html body must THROW now (was PASS).
                globalThis.__p_json = String((function () {
                    try { pm.response.to.be.json; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__p_html = String((function () {
                    try { pm.response.to.be.html; return 'passed'; } catch (e) { return 'threw'; }
                })());
                // Paren form on a NON-matching body must also throw.
                globalThis.__p_json_paren = String((function () {
                    try { pm.response.to.be.json(); return 'passed'; } catch (e) { return 'threw'; }
                })());

                // Flip to a valid JSON body: both forms must pass.
                globalThis.__tropel_pm_response_header = function (k) {
                    if (String(k).toLowerCase() === 'content-type') return 'application/json';
                    return null;
                };
                globalThis.__tropel_pm_response_headers = function () {
                    return { 'Content-Type': 'application/json' };
                };
                globalThis.__tropel_pm_response_json = function () { return '{"a":1}'; };
                globalThis.__tropel_pm_response_body = function () { return '{"a":1}'; };
                globalThis.__p_json_ok = String((function () {
                    try { pm.response.to.be.json; return 'passed'; } catch (e) { return 'threw'; }
                })());
                globalThis.__p_json_paren_ok = String((function () {
                    try { pm.response.to.be.json(); return 'passed'; } catch (e) { return 'threw'; }
                })());
                // html/text on a JSON body must throw.
                globalThis.__p_html_json = String((function () {
                    try { pm.response.to.be.html; return 'passed'; } catch (e) { return 'threw'; }
                })());
            "#,
            )
            .expect("script should eval");

            assert_eq!(
                ctx.eval::<String, _>("__p_json").unwrap(),
                "threw",
                "bare to.be.json on text/html must THROW (was silent PASS)"
            );
            assert_eq!(
                ctx.eval::<String, _>("__p_html").unwrap(),
                "passed",
                "to.be.html must pass on text/html"
            );
            assert_eq!(
                ctx.eval::<String, _>("__p_json_paren").unwrap(),
                "threw",
                "to.be.json() on text/html must throw"
            );
            assert_eq!(
                ctx.eval::<String, _>("__p_json_ok").unwrap(),
                "passed",
                "bare to.be.json on JSON body must pass"
            );
            assert_eq!(
                ctx.eval::<String, _>("__p_json_paren_ok").unwrap(),
                "passed",
                "to.be.json() on JSON body must pass"
            );
            assert_eq!(
                ctx.eval::<String, _>("__p_html_json").unwrap(),
                "threw",
                "to.be.html on JSON body must throw"
            );
        });
    }

    #[test]
    fn test_exec_selection_installs_named_export() {
        // A scenario naming `exec: "browse"` must run the `browse` export,
        // NOT the default export (k6 multi-scenario semantics).
        let source = r#"
            export function browse() { return "browse-ran"; }
            export function checkout() { return "checkout-ran"; }
            export default function() { return "default-ran"; }
        "#;
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let module = rquickjs::Module::declare(ctx, "exec-script", source).unwrap();
            let (module, promise) = module.eval().unwrap();
            promise.finish::<()>().unwrap();

            // Same selection logic install_iteration_global uses.
            let browse: rquickjs::Function = module.get("browse").unwrap();
            let s: String = browse.call(()).unwrap();
            assert_eq!(s, "browse-ran");

            // A missing exec export errors (module.get fails) — k6 errors
            // loudly rather than silently running the default flow.
            assert!(
                module.get::<_, rquickjs::Function>("nope").is_err(),
                "missing exec export must error"
            );
        });
    }

    #[test]
    fn test_exec_members_are_value_properties() {
        // Backlog line 141: exec.vu.idInTest etc. must be VALUE properties
        // (k6 semantics), not functions. The old function-object form broke
        // `if (exec.vu.iterationInScenario === 0)` (always truthy) and
        // `data[exec.vu.idInTest % len]` (NaN → undefined). exec.test must
        // exist so exec.test.abort() doesn't TypeError.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(include_str!("../../../../js/exec/exec.js"))
                .expect("exec shim should eval");
            // Stub the native bridges with known values.
            ctx.eval::<(), _>(
                r#"
                globalThis.__tropel_exec_scenario_name = function () { return 'api_load'; };
                globalThis.__tropel_exec_scenario_executor = function () { return 'shared-iterations'; };
                globalThis.__tropel_exec_vu_id = function () { return 3; };
                globalThis.__tropel_exec_iteration = function () { return 0; };
                globalThis.__tropel_exec_iterations_completed = function () { return 42; };
                globalThis.__tropel_exec_vus_active = function () { return 2; };
                globalThis.__tropel_test_abort = function (msg) { globalThis.__aborted = msg; };
                globalThis.__aborted = null;
            "#,
            )
            .expect("stub should eval");
            ctx.eval::<(), _>(
                r#"
                globalThis.__type_id = typeof exec.vu.idInTest;
                globalThis.__type_iter = typeof exec.vu.iterationInScenario;
                // The two idioms the function-form broke:
                globalThis.__is_first_iter = (exec.vu.iterationInScenario === 0);
                var data = ['a', 'b', 'c', 'd'];
                globalThis.__indexed = data[exec.vu.idInTest % data.length];
                globalThis.__id = exec.vu.idInTest;
                globalThis.__completed = exec.instance.iterationsCompleted;
                globalThis.__vus = exec.instance.vusActive;
                globalThis.__scen = exec.scenario.name;
                globalThis.__exec = exec.scenario.executor;
                // exec.test.abort must exist and reach the bridge.
                globalThis.__test_type = typeof exec.test.abort;
                exec.test.abort('stop now');
            "#,
            )
            .expect("script should eval");

            assert_eq!(ctx.eval::<String, _>("__type_id").unwrap(), "number", "exec.vu.idInTest must be a number value, not a function");
            assert_eq!(ctx.eval::<String, _>("__type_iter").unwrap(), "number", "exec.vu.iterationInScenario must be a number value");
            assert!(ctx.eval::<bool, _>("__is_first_iter").unwrap(), "iterationInScenario === 0 must fire (was always truthy as a function)");
            assert_eq!(ctx.eval::<String, _>("__indexed").unwrap(), "d", "data[idInTest % len] must index (was NaN → undefined)");
            assert_eq!(ctx.eval::<i64, _>("__id").unwrap(), 3);
            assert_eq!(ctx.eval::<i64, _>("__completed").unwrap(), 42);
            assert_eq!(ctx.eval::<i64, _>("__vus").unwrap(), 2);
            assert_eq!(ctx.eval::<String, _>("__scen").unwrap(), "api_load");
            assert_eq!(ctx.eval::<String, _>("__exec").unwrap(), "shared-iterations");
            assert_eq!(ctx.eval::<String, _>("__test_type").unwrap(), "function", "exec.test.abort must be a function");
            assert_eq!(ctx.eval::<String, _>("__aborted").unwrap(), "stop now", "exec.test.abort must reach the native bridge");
        });
    }

    #[test]
    fn test_module_eval_handle_summary_returns_map() {
        // `export function handleSummary(data)` must be callable with the
        // summary data and return a filename → content map (stdout prints).
        let source = r#"
            export function handleSummary(data) {
                return {
                    "stdout": "custom stdout: " + data.state.iterations,
                    "summary.html": "<html>" + data.metrics.http_reqs.type + "</html>",
                };
            }
            export default function() {}
        "#;
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let map: Option<HashMap<String, String>> = ctx.with(|ctx| {
            call_module_handle_summary(
                &ctx,
                source,
                r#"{"metrics":{"http_reqs":{"type":"counter"}},"state":{"iterations":7}}"#,
            )
            .unwrap()
        });
        let map = map.expect("handleSummary must produce output");
        assert_eq!(
            map.get("stdout").map(|s| s.as_str()),
            Some("custom stdout: 7")
        );
        assert_eq!(
            map.get("summary.html").map(|s| s.as_str()),
            Some("<html>counter</html>")
        );
    }

    #[test]
    fn test_module_eval_handle_summary_absent_is_none() {
        let source = "export default function() {}\n";
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let result: Option<HashMap<String, String>> =
            ctx.with(|ctx| call_module_handle_summary(&ctx, source, "{}").unwrap());
        assert!(result.is_none(), "no handleSummary export → None");
    }

    #[test]
    fn test_module_eval_handle_summary_async() {
        // k6 permits async handleSummary — the Promise must be finished.
        let source = r#"
            export async function handleSummary(data) {
                return { "stdout": "async " + data.state.vusMax };
            }
            export default function() {}
        "#;
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let result: Option<HashMap<String, String>> = ctx.with(|ctx| {
            call_module_handle_summary(&ctx, source, r#"{"state":{"vusMax":3}}"#).unwrap()
        });
        let map = result.expect("async handleSummary must produce output");
        assert_eq!(map.get("stdout").map(|s| s.as_str()), Some("async 3"));
    }

    // ── k6 `open()` + `k6/data` SharedArray ──

    /// Create a JsContext with the k6 file bridges + shim installed.
    async fn ctx_with_file_bridges(script_dir: Option<PathBuf>) -> JsContext {
        let mut js_ctx = JsContext::new(None, Some(Duration::from_secs(5)))
            .await
            .expect("context creation should succeed");
        register_k6_file_bridges(&mut js_ctx, script_dir);
        js_ctx
            .bootstrap_library(OPEN_DATA_SHIM)
            .await
            .expect("open-data shim should bootstrap");
        js_ctx
    }

    // ── lodash / CryptoJS shim fidelity (backlog line 155) ──

    /// JsContext with native modules + the full base shim bundle (lodash,
    /// CryptoJS, chai, exec) installed — the same bootstrap production VUs
    /// get, so shim behavior is verified under real native crypto.
    async fn ctx_with_base_shims() -> JsContext {
        let mut js_ctx = JsContext::new(None, Some(Duration::from_secs(10)))
            .await
            .expect("context creation should succeed");
        bootstrap_js_libs(
            &mut js_ctx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(OnceLock::new()),
        )
        .await
        .expect("shim bootstrap should succeed");
        js_ctx
    }

    /// Regression (backlog line 104): the per-iteration JS interrupt
    /// deadline counted WALL time spent in blocking host calls, so a stock
    /// k6 pacing idiom `http.get(u); sleep(Math.random()*10);` was
    /// interrupted on resume and the run exited non-zero. The sleep bridge
    /// re-arms the deadline, so a sleep LONGER than the eval budget must
    /// still let the following JS run.
    #[tokio::test]
    async fn test_sleep_rearms_interrupt_deadline() {
        // 1 s per-eval deadline; the sleep below is 2 s — longer than the
        // whole budget. With the re-arm the eval must complete.
        let mut js_ctx = JsContext::new(None, Some(Duration::from_secs(1)))
            .await
            .expect("context creation should succeed");
        bootstrap_js_libs(
            &mut js_ctx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(OnceLock::new()),
        )
        .await
        .expect("shim bootstrap should succeed");
        let out = js_ctx
            .eval("__tropel_native_sleep(2000); 1 + 1")
            .await
            .expect("sleep must not count against the JS execution deadline");
        assert_eq!(out.trim(), "2");
    }

    #[tokio::test]
    async fn test_lodash_get_bracket_and_primitive_paths() {
        // Backlog line 155: `_.get(o,'a[0].b')` returned undefined and
        // `_.get({name:'bob'},'name.length')` THREW ('length' in 'bob').
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    _.get({ a: [{ b: 42 }] }, 'a[0].b'),
                    _.get({ name: 'bob' }, 'name.length'),
                    _.get({ a: { b: null } }, 'a.b.c', 'fallback'),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[42,3,\"fallback\"]",
            "bracket paths + primitive .length + default"
        );
    }

    #[tokio::test]
    async fn test_lodash_every_object_matcher() {
        // Backlog line 155: `_.every([{active:false}],{active:true})`
        // returned TRUE (truthiness branch ignored the object predicate).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    _.every([{active:false}], {active:true}),
                    _.every([{active:true},{active:true}], {active:true}),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(out, "[false,true]", "object matcher must be honored");
    }

    #[tokio::test]
    async fn test_lodash_string_and_property_shorthand() {
        // Backlog line 93: string shorthand (`'active'`) and pair shorthand
        // (`['active',true]`) were broken across filter/find/every/some/
        // findIndex, and `_.reject(coll,{matcher})` THREW (raw predicate
        // called as a function). All must normalize through toPredicate.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var users = [
                    { name: 'a', active: true },
                    { name: 'b', active: false },
                    { name: 'c', active: true },
                ];
                JSON.stringify([
                    // String shorthand: filter by property truthiness.
                    _.filter(users, 'active').map(function (u) { return u.name; }),
                    // find returns the FIRST active user.
                    _.find(users, 'active').name,
                    // findIndex returns the index of the first active user.
                    _.findIndex(users, 'active'),
                    // every/some honor the string shorthand.
                    _.every(users, 'active'),
                    _.some(users, 'active'),
                    // Pair shorthand: [key, value] equality.
                    _.filter(users, ['active', true]).length,
                    // reject with an object matcher must not throw.
                    _.reject(users, { active: false }).map(function (u) { return u.name; }),
                    // some with an object matcher.
                    _.some(users, { active: true }),
                    // Dotted path shorthand via _.get.
                    _.filter([{ a: { b: 1 } }, { a: { b: 0 } }], 'a.b').length,
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[[\"a\",\"c\"],\"a\",0,false,true,2,[\"a\",\"c\"],true,1]",
            "string/pair/matcher shorthand across the collection family"
        );
    }

    #[tokio::test]
    async fn test_lodash_set_blocks_proto_pollution() {
        // Backlog line 155: `_.set({}, '__proto__.polluted', 1)` polluted
        // the per-VU context for the whole run.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                _.set({}, '__proto__.polluted', 1);
                ({}).polluted === undefined && Object.prototype.polluted === undefined
                "#,
            )
            .await
            .unwrap();
        assert_eq!(out, "true", "prototype pollution must be blocked");
    }

    #[tokio::test]
    async fn test_lodash_clone_deep_preserves_dates_and_cycles() {
        // Backlog line 155: cloneDeep was JSON round-trip — Dates became
        // strings and cycles threw.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var d = new Date(12345);
                var cyc = { a: 1 }; cyc.self = cyc;
                var c = _.cloneDeep({ d: d, u: undefined });
                var cc = _.cloneDeep(cyc);
                JSON.stringify([
                    c.d instanceof Date,
                    c.d.getTime(),
                    'u' in c,
                    cc.self === cc,
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[true,12345,true,true]",
            "Dates preserved + cycles handled"
        );
    }

    #[tokio::test]
    async fn test_lodash_debounce_coalesces_and_throttle_default_fires() {
        // Backlog line 155: debounce fired once PER CALL in a timer-less
        // runtime (3 calls -> 3 invocations); throttle with no wait NEVER
        // fired (`now - lastCall >= undefined` was always false).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var n = 0;
                var d = _.debounce(function(){ n++; }, 100);
                d(); d(); d();
                var debounced = n;
                n = 0;
                var t = _.throttle(function(){ n++; });
                t(); t(); t();
                JSON.stringify([debounced, n])
                "#,
            )
            .await
            .unwrap();
        // The eval pumps the microtask queue, so the coalesced debounce fires
        // exactly once by the time we read `debounced`... actually the read
        // happens synchronously in the same eval BEFORE the microtask. Assert
        // debounced is 0 here and verify the coalesced single-fire below via
        // an awaited tick.
        assert_eq!(
            out, "[0,3]",
            "debounce must not fire per call; throttle(no wait) fires"
        );
    }

    #[tokio::test]
    async fn test_lodash_debounce_single_trailing_invocation() {
        // Backlog line 131: k6/timers now defines real global setTimeout, so
        // lodash's debounce schedules an actual 100ms timer instead of the
        // timer-less microtask fallback. The driver pumps due timers at
        // iteration boundaries — sleep past the wait, then pump, and the
        // 3 sync calls must still coalesce into ONE trailing invocation.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval_async(
                r#"
                var n = 0;
                var d = _.debounce(function(){ n++; }, 100);
                d(); d(); d();
                __tropel_native_sleep(150);
                __tropel_pump_timers();
                Promise.resolve().then(function(){ return n; })
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "1",
            "3 sync calls must coalesce to ONE trailing invocation"
        );
    }

    #[tokio::test]
    async fn test_lodash_object_collections_and_take_drop_chunk() {
        // Backlog line 155: map/filter/find returned EMPTY for object
        // collections; n===0 off-by-one in take/drop/chunk.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    _.map({a:1,b:2}, function(v){ return v * 2; }),
                    _.filter({a:1,b:0,c:3}, function(v){ return v > 0; }),
                    _.find({a:1,b:2}, function(v){ return v === 2; }),
                    _.take([1,2,3], 0),
                    _.drop([1,2,3], 0),
                    _.chunk([1,2,3], 0),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[[2,4],[1,3],2,[],[1,2,3],[]]",
            "object collections + n===0 semantics"
        );
    }

    #[tokio::test]
    async fn test_lodash_merge_and_reduce_exist() {
        // Backlog line 155: `_.merge`/`_.reduce` did not exist.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    _.merge({a:{b:1}}, {a:{c:2}}),
                    _.reduce([1,2,3,4], function(a,b){ return a + b; }, 0),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[{\"a\":{\"b\":1,\"c\":2}},10]",
            "merge deep-merges; reduce sums"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_hashes_are_real() {
        // Backlog line 155: the fallback FABRICATED a plausible digest
        // (SHA1 and SHA512 of 'hello' both returned 05e918d2...).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    CryptoJS.SHA1('hello').toString(),
                    CryptoJS.SHA256('hello').toString(),
                    CryptoJS.SHA256('').toString(),
                    CryptoJS.SHA512('hello').toString(),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            "[\"aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d\",\"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\",\"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\",\"9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043\"]",
            "real digests must match known vectors"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_sha3_is_keccak_512() {
        // Backlog line 155: SHA3 was SHA3-256; CryptoJS.SHA3 defaults to
        // KECCAK-512. SHA3('hello') must match the Keccak-512 vector.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    CryptoJS.SHA3('hello').toString(),
                    CryptoJS.SHA3('hello', 256).toString(),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            "[\"52fa80662e64c128f8389c9ea6c73d4c02368004bf4463491900d11aaadca39d47de1b01361f207c512cfa79f0f92c3395c67ff7928e3f5ce3e3c852b392f976\",\"1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8\"]",
            "SHA3 default must be Keccak-512 (hello = 52fa…6f976, 256-bit = 1c8a…eac8)"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_base64_parse_padding_and_wordarray_create() {
        // Backlog line 155: Base64.parse corrupted EVERY padded input ('='
        // was in the alphabet at index 64); WordArray.create ignored args.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    CryptoJS.enc.Utf8.stringify(CryptoJS.enc.Base64.parse('aGVsbG8=')),
                    CryptoJS.enc.Utf8.stringify(CryptoJS.enc.Base64.parse('aGk=')),
                    CryptoJS.lib.WordArray.create([0x12345678], 4).toString(),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[\"hello\",\"hi\",\"12345678\"]",
            "padded base64 round-trips; WordArray.create honors args"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_utf8_emoji_is_4byte() {
        // Backlog line 155: Utf8.parse emitted CESU-8 (two 3-byte sequences)
        // for emoji instead of one 4-byte UTF-8 code point.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                CryptoJS.enc.Utf8.parse('\u{1F600}').toString(CryptoJS.enc.Hex)
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "f09f9880",
            "emoji must encode as a single 4-byte UTF-8 sequence"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_aes_default_cbc_and_mode_pad_namespaces() {
        // Backlog line 155: AES defaulted to GCM (CryptoJS defaults to
        // CBC/PKCS7), and CryptoJS.mode/CryptoJS.pad did not exist so the
        // universal incantation TypeErrored.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var key = CryptoJS.lib.WordArray.random(32);
                var iv = CryptoJS.lib.WordArray.random(16);
                var ct = CryptoJS.AES.encrypt('hello world', key, {
                    mode: CryptoJS.mode.CBC,
                    padding: CryptoJS.pad.Pkcs7,
                    iv: iv
                });
                var pt = CryptoJS.enc.Utf8.stringify(
                    CryptoJS.AES.decrypt(ct, key, {
                        mode: CryptoJS.mode.CBC,
                        padding: CryptoJS.pad.Pkcs7
                    })
                );
                // Also verify the default (no options) is a working CBC round-trip.
                var ct2 = CryptoJS.AES.encrypt('abc', key);
                var pt2 = CryptoJS.enc.Utf8.stringify(CryptoJS.AES.decrypt(ct2, key));
                JSON.stringify([pt, pt2])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[\"hello world\",\"abc\"]",
            "CBC/PKCS7 with mode+pad namespaces must round-trip"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_aes_128_192_keys_accepted() {
        // Backlog line 155: AES-128/192 were rejected (hardcoded 32-byte).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var k16 = CryptoJS.enc.Hex.parse('000102030405060708090a0b0c0d0e0f');
                var k24 = CryptoJS.enc.Hex.parse('000102030405060708090a0b0c0d0e0f1011121314151617');
                var c16 = CryptoJS.AES.encrypt('sixteen', k16);
                var c24 = CryptoJS.AES.encrypt('twentyfour', k24);
                JSON.stringify([
                    CryptoJS.enc.Utf8.stringify(CryptoJS.AES.decrypt(c16, k16)),
                    CryptoJS.enc.Utf8.stringify(CryptoJS.AES.decrypt(c24, k24)),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[\"sixteen\",\"twentyfour\"]",
            "AES-128/192 must round-trip"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_aes_decrypt_uses_passed_key() {
        // Backlog line 155: `ciphertext.key || key` IGNORED the passed key,
        // so a wrong-password decrypt "succeeded" against the embedded key.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var key = CryptoJS.enc.Hex.parse('000102030405060708090a0b0c0d0e0f');
                var wrong = CryptoJS.enc.Hex.parse('000102030405060708090a0b0c0d0e10');
                var ct = CryptoJS.AES.encrypt('secret', key);
                var threw = false;
                try {
                    var out = CryptoJS.enc.Utf8.stringify(CryptoJS.AES.decrypt(ct, wrong));
                    if (out !== 'secret') threw = true;
                } catch (e) { threw = true; }
                threw
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "true",
            "wrong key must fail (not silently use embedded key)"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_aes_passphrase_roundtrip() {
        // Passphrase path: EVP_BytesToKey + Salted__ header round-trip.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var ct = CryptoJS.AES.encrypt('phrase msg', 'correct horse');
                var s = ct.toString();
                var back = CryptoJS.enc.Utf8.stringify(CryptoJS.AES.decrypt(s, 'correct horse'));
                JSON.stringify([s.slice(0, 8) === 'U2FsdGVk', back])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[true,\"phrase msg\"]",
            "passphrase encrypt/decrypt round-trips"
        );
    }

    #[tokio::test]
    async fn test_cryptojs_modes_padding_utf16_wordarray_and_missing_algorithms() {
        // Backlog line 95: ECB/CTR/CFB/OFB silently ran GCM (wrong cipher,
        // no error); {padding: NoPadding} was ignored (16 bytes → 32);
        // WordArray.concat corrupted non-4-byte-aligned data ('abc'+'de' →
        // "abc\0d"); enc.Utf16 was an alias of Utf8; RIPEMD160/SHA224/
        // HmacSHA384 were undefined.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var key = CryptoJS.lib.WordArray.random(16);
                var iv = CryptoJS.lib.WordArray.random(16);

                // Unsupported modes must FAIL LOUDLY, not silently run GCM.
                var ecbThrew = (function () {
                    try { CryptoJS.AES.encrypt('x', key, { mode: CryptoJS.mode.ECB, iv: iv }); return 'no'; }
                    catch (e) { return /ECB/.test(e.message) ? 'ecb' : 'other:' + e.message; }
                })();
                var ctrThrew = (function () {
                    try { CryptoJS.AES.encrypt('x', key, { mode: CryptoJS.mode.CTR, iv: iv }); return 'no'; }
                    catch (e) { return /CTR/.test(e.message) ? 'ctr' : 'other:' + e.message; }
                })();
                var cfbThrew = (function () {
                    try { CryptoJS.AES.decrypt(CryptoJS.enc.Hex.parse('00'.repeat(16)), key, { mode: CryptoJS.mode.CFB, iv: iv }); return 'no'; }
                    catch (e) { return /CFB/.test(e.message) ? 'cfb' : 'other:' + e.message; }
                })();

                // Unsupported padding must FAIL LOUDLY, not silently pad.
                var noPadThrew = (function () {
                    try { CryptoJS.AES.encrypt('x', key, { mode: CryptoJS.mode.CBC, padding: CryptoJS.pad.NoPadding, iv: iv }); return 'no'; }
                    catch (e) { return /NoPadding/.test(e.message) ? 'nopad' : 'other:' + e.message; }
                })();

                // WordArray.concat must be bit-aligned: 'abc' + 'de' = 'abcde'.
                var concatStr = CryptoJS.enc.Utf8.stringify(
                    CryptoJS.enc.Utf8.parse('abc').concat(CryptoJS.enc.Utf8.parse('de'))
                );

                // Utf16 must be UTF-16BE (not a Utf8 alias) and round-trip.
                var utf16 = CryptoJS.enc.Utf16.stringify(CryptoJS.enc.Utf16.parse('héllo'));
                var utf16Hex = CryptoJS.enc.Utf16.parse('hi').toString(CryptoJS.enc.Hex);
                var utf16beAlias = CryptoJS.enc.Utf16BE === CryptoJS.enc.Utf16;

                // Missing algorithms now defined and matching known vectors.
                var sha224 = CryptoJS.SHA224('hello').toString();
                var ripemd160 = CryptoJS.RIPEMD160('hello').toString();
                var hmac384 = CryptoJS.HmacSHA384('hello', 'key').toString();

                JSON.stringify([
                    ecbThrew, ctrThrew, cfbThrew, noPadThrew,
                    concatStr, utf16, utf16Hex, utf16beAlias,
                    sha224, ripemd160, hmac384
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            "[\"ecb\",\"ctr\",\"cfb\",\"nopad\",\"abcde\",\"héllo\",\"00680069\",true,\"ea09ae9cc6768c50fcee903ed054556e5bfc8347907f12598aa24193\",\"108f07b8382412612c048d07d13f814118445acd\",\"eacbad575c301fa68afb26dae48b25bf5cd42fd08ed28c08c274ce62df7928f01249976cd8aaf1ab0681d3accedc9543\"]",
            "modes fail loudly, padding fails loudly, concat aligns, Utf16 is BE, missing algorithms defined"
        );
    }

    fn temp_script_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tropel-k6-open-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── ES-module local imports (module resolver + loader) ──

    #[tokio::test]
    async fn test_module_local_import_resolves_to_disk() {
        // k6 script importing a local helper: `import { x } from "./helpers.js"`
        // must resolve via the registered module resolver/loader, not fail at
        // eval time (the pre-existing behavior before the loader landed).
        let dir = temp_script_dir("localimport");
        std::fs::write(
            dir.join("helpers.js"),
            "export function triple(x) { return x * 3; }\n",
        )
        .unwrap();
        let source = r#"
            import { triple } from "./helpers.js";
            export default function() { globalThis.__tropel_import_result = triple(14); }
        "#;
        let mut js_ctx = JsContext::new(None, Some(Duration::from_secs(5)))
            .await
            .expect("context creation should succeed");
        js_ctx.set_module_loader(
            K6ModuleResolver {
                script_dir: Some(dir.clone()),
            },
            K6ModuleLoader,
        );
        install_iteration_global(&mut js_ctx, source, None)
            .expect("module with local import should install");
        js_ctx
            .run_script_cached(
                "return __tropel_iteration()",
                Some("k6-iteration.js".to_string()),
            )
            .await
            .expect("iteration should run");
        let result = js_ctx.get_global("__tropel_import_result").await.unwrap();
        assert_eq!(result.as_deref(), Some("42"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_module_local_import_typescript_transpiles() {
        // Imported `.ts` helpers must be transpiled on the fly by the loader.
        let dir = temp_script_dir("localimportts");
        std::fs::write(
            dir.join("calc.ts"),
            "export function add(a: number, b: number): number { return a + b; }\n",
        )
        .unwrap();
        let source = r#"
            import { add } from "./calc.ts";
            export default function() { globalThis.__tropel_ts_result = add(20, 22); }
        "#;
        let mut js_ctx = JsContext::new(None, Some(Duration::from_secs(5)))
            .await
            .expect("context creation should succeed");
        js_ctx.set_module_loader(
            K6ModuleResolver {
                script_dir: Some(dir.clone()),
            },
            K6ModuleLoader,
        );
        install_iteration_global(&mut js_ctx, source, None)
            .expect("module with TS local import should install");
        js_ctx
            .run_script_cached(
                "return __tropel_iteration()",
                Some("k6-iteration.js".to_string()),
            )
            .await
            .expect("iteration should run");
        let result = js_ctx.get_global("__tropel_ts_result").await.unwrap();
        assert_eq!(result.as_deref(), Some("42"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_module_local_import_missing_file_errors() {
        // A local import that doesn't exist on disk must fail loudly at
        // module-install time (matches k6's behavior for unresolvable
        // imports), not silently no-op.
        let dir = temp_script_dir("localimportmissing");
        let source = r#"
            import { nope } from "./does-not-exist.js";
            export default function() {}
        "#;
        let mut js_ctx = JsContext::new(None, Some(Duration::from_secs(5)))
            .await
            .expect("context creation should succeed");
        js_ctx.set_module_loader(
            K6ModuleResolver {
                script_dir: Some(dir.clone()),
            },
            K6ModuleLoader,
        );
        let err = install_iteration_global(&mut js_ctx, source, None).err();
        assert!(
            err.is_some(),
            "unresolvable local import must fail module install"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_open_reads_text_relative_to_script_dir() {
        let dir = temp_script_dir("text");
        std::fs::write(dir.join("data.txt"), "hello from open").unwrap();
        let mut js_ctx = ctx_with_file_bridges(Some(dir.clone())).await;
        let out = js_ctx
            .eval("open('data.txt')")
            .await
            .expect("open should succeed");
        assert_eq!(out, "hello from open");
        // Absolute path also works.
        let abs = dir.join("data.txt").to_string_lossy().to_string();
        let out = js_ctx
            .eval(&format!("open('{}')", abs.replace('\\', "\\\\")))
            .await
            .expect("absolute open should succeed");
        assert_eq!(out, "hello from open");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_open_binary_returns_array_buffer() {
        let dir = temp_script_dir("bin");
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 255, 128]).unwrap();
        let mut js_ctx = ctx_with_file_bridges(Some(dir.clone())).await;
        let out = js_ctx
            .eval(
                "var b = open('blob.bin', 'b');\
                 (b instanceof ArrayBuffer) ? 'AB:' + b.byteLength : 'not-ab';",
            )
            .await
            .expect("binary open should succeed");
        assert_eq!(out, "AB:5");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_open_missing_file_throws_js_error() {
        let dir = temp_script_dir("missing");
        let mut js_ctx = ctx_with_file_bridges(Some(dir.clone())).await;
        let out = js_ctx
            .eval(
                "try { open('nope.txt'); 'no-throw'; }\
                 catch (e) { 'threw:' + (e && e.message ? e.message : String(e)); }",
            )
            .await
            .expect("eval should succeed");
        assert!(
            out.starts_with("threw:"),
            "missing file must throw a JS error, got: {out}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_shared_array_computes_once_across_contexts() {
        // First context constructs the SharedArray and populates the native
        // cache; a second context (another VU) must see the same data WITHOUT
        // re-running the factory (k6 semantics: computed once, shared).
        let dir = temp_script_dir("shared");
        let name = "tropel-shared-test-1";
        let mut js_ctx1 = ctx_with_file_bridges(Some(dir.clone())).await;
        let script = format!(
            "var calls = 0;\
             var sa = new SharedArray('{name}', function () {{ calls++; return [10, 20, 30]; }});\
             JSON.stringify({{ len: sa.length, first: sa[0], at: sa.at(1), calls: calls }});"
        );
        let out1 = js_ctx1
            .eval(&script)
            .await
            .expect("first SharedArray construction should succeed");
        assert!(
            out1.contains("\"len\":3")
                && out1.contains("\"first\":10")
                && out1.contains("\"at\":20")
                && out1.contains("\"calls\":1"),
            "first context must run the factory once, got: {out1}"
        );

        // Second context: same name -> cached, factory NOT re-run.
        let mut js_ctx2 = ctx_with_file_bridges(Some(dir.clone())).await;
        let script2 = format!(
            "var calls = 0;\
             var sa = new SharedArray('{name}', function () {{ calls++; return [99]; }});\
             JSON.stringify({{ len: sa.length, first: sa[0], calls: calls }});"
        );
        let out2 = js_ctx2
            .eval(&script2)
            .await
            .expect("cached SharedArray construction should succeed");
        assert!(
            out2.contains("\"len\":3")
                && out2.contains("\"first\":10")
                && out2.contains("\"calls\":0"),
            "second context must reuse cached data without re-running the factory, got: {out2}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_shared_array_is_read_only() {
        let dir = temp_script_dir("ro");
        let mut js_ctx = ctx_with_file_bridges(Some(dir.clone())).await;
        let out = js_ctx
            .eval(
                "var sa = new SharedArray('tropel-shared-test-ro', function () { return [1, 2, 3]; });\
                 try { sa[0] = 999; 'no-throw'; }\
                 catch (e) { 'threw:' + (e && e.message ? e.message : String(e)); }",
            )
            .await
            .expect("read-only assignment should be catchable");
        assert!(
            out.starts_with("threw:"),
            "SharedArray writes must throw, got: {out}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_declared_options_driver_e2e() {
        // Full path: K6Driver::declared_options on a script with options.
        // Uses a raw module eval through the same helper the driver uses.
        let source = r#"
            export const options = { vus: 3, iterations: 10 };
            export default function() {}
        "#;
        let json = read_export_for_test(source, "options").unwrap();
        let opts: crate::options::K6Options = serde_json::from_str(&json).unwrap();
        let decl = opts.to_declared().unwrap();
        assert!(decl.execution.is_some());
        assert!(decl.scenarios.is_none());
        match decl.execution.unwrap() {
            tropel_sdk::config::ExecutionConfig::SharedIterations {
                iterations, vus, ..
            } => {
                assert_eq!(iterations, 10);
                assert_eq!(vus, 3);
            }
            other => panic!("expected SharedIterations, got {other:?}"),
        }
    }

    // ── options type-mismatch hard error (backlog line 153) ──

    #[tokio::test]
    async fn test_declared_options_malformed_returns_err() {
        // Backlog line 153: a script that DECLARES `options` but with a type
        // mismatch must hard-error (k6 aborts) — NOT silently fall back to
        // the CLI profile. `stages[].duration` must be a string; a number is
        // the canonical k6 mistake.
        let driver = K6Driver;
        let script = br#"
            export const options = {
                stages: [ { duration: 60, target: 10 } ]
            };
            export default function() {}
        "#;
        let res = driver.declared_options(script, None, &HashMap::new()).await;
        assert!(
            res.is_err(),
            "malformed options must return Err, got {res:?}"
        );
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("k6 script declares `options` but they failed to parse"),
            "error should explain the parse failure, got: {msg}"
        );

        // And a WELL-FORMED script still returns Ok(Some(_)) — the hard-error
        // path must not leak into the happy path.
        let good = br#"
            export const options = { vus: 2, duration: "10s" };
            export default function() {}
        "#;
        let res = driver.declared_options(good, None, &HashMap::new()).await;
        assert!(res.is_ok(), "well-formed options must be Ok, got {res:?}");
        assert!(res.unwrap().is_some(), "declared options must be Some");
    }

    // ── k6 lifecycle: setup() / teardown() (backlog line 127) ──

    // k6 §4 (backlog line 119): setup()/teardown() may make HTTP calls.
    // Stub client returning a canned 200 so the tests exercise the bridge
    // wiring (registration + sink), not real I/O.
    struct StubClient;

    #[async_trait]
    impl DriverHttpClient for StubClient {
        async fn execute(&self, req: &Request) -> Result<Response> {
            Ok(Response {
                url: req.url.clone(),
                status_code: 200,
                status_text: "OK".into(),
                headers: HashMap::new(),
                body: b"ok".to_vec(),
                text_cache: std::cell::OnceCell::new(),
                json_cache: std::cell::OnceCell::new(),
                response_time: std::time::Duration::from_millis(2),
                timings: None,
                cookies: vec![],
                size: 2,
                request_body_size: 0,
                redirects: vec![],
            })
        }
    }

    async fn test_ctx() -> (
        Arc<dyn DriverHttpClient + Send + Sync>,
        Arc<Mutex<Vec<Sample>>>,
    ) {
        let client: Arc<dyn DriverHttpClient + Send + Sync> = Arc::new(StubClient);
        let sink: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
        (client, sink)
    }

    #[tokio::test]
    async fn test_setup_runs_and_serializes_return_value() {
        // K6Driver::setup must run the script's `export function setup()`
        // ONCE and return its return value serialized as JSON (the engine
        // threads it into every VU as the default function's `data`).
        let driver = K6Driver;
        let script = br#"
            export function setup() { return { token: "abc", n: 42 }; }
            export default function() {}
        "#;
        let (client, sink) = test_ctx().await;
        let data = driver
            .setup(script, None, &HashMap::new(), client, sink)
            .await;
        let json = data.expect("setup() must return serialized data");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["token"], "abc");
        assert_eq!(v["n"], 42);
    }

    #[tokio::test]
    async fn test_setup_absent_returns_none() {
        // A script without `export function setup()` yields None — VUs then
        // see `undefined` data (k6 parity), never a hard error.
        let driver = K6Driver;
        let script = br#"export default function() {}"#;
        let (client, sink) = test_ctx().await;
        assert!(
            driver
                .setup(script, None, &HashMap::new(), client, sink)
                .await
                .is_none(),
            "no setup export must yield None"
        );
    }

    #[tokio::test]
    async fn test_teardown_receives_setup_data() {
        // teardown(data) must receive the setup() return value: with the
        // correct data it completes silently; with WRONG data it throws —
        // which is warn-only and must never surface as a panic/error (k6
        // parity: a throwing teardown never affects the run's exit status).
        let driver = K6Driver;
        let script = br#"
            export function setup() { return { token: "abc" }; }
            export function teardown(data) {
                if (!data || data.token !== "abc") throw new Error("teardown got wrong data");
            }
            export default function() {}
        "#;
        let (client, sink) = test_ctx().await;
        let data = driver
            .setup(script, None, &HashMap::new(), client.clone(), sink.clone())
            .await;
        // Happy path: correct data — no panic, no error.
        driver
            .teardown(
                script,
                None,
                data.as_deref(),
                &HashMap::new(),
                client.clone(),
                sink.clone(),
            )
            .await;
        // Throwing path: wrong data — teardown throws internally, but the
        // driver only logs (warn) and returns; no panic, no error.
        driver
            .teardown(
                script,
                None,
                Some("{\"token\":\"WRONG\"}"),
                &HashMap::new(),
                client.clone(),
                sink.clone(),
            )
            .await;
        driver
            .teardown(
                script,
                None,
                Some("not-json"),
                &HashMap::new(),
                client,
                sink,
            )
            .await;
    }

    #[tokio::test]
    async fn test_setup_can_make_http_calls() {
        // k6 §4 (backlog line 119) regression: the throwaway setup() context
        // previously registered ONLY the file/SharedArray bridges, so
        // `http.get()` threw, the call was logged, and the canonical
        // login-in-setup pattern gave every VU `data === undefined`. The
        // HTTP bridges must now be registered: the call succeeds, its
        // samples land in the sink (for the engine to drain into the run
        // totals — k6 counts setup http_reqs), and its return value is
        // serialized as data.
        let driver = K6Driver;
        let script = br#"
            import http from 'k6/http';
            export function setup() {
                const res = http.get('http://example.com/');
                return { status: res.status };
            }
            export default function() {}
        "#;
        let (client, sink) = test_ctx().await;
        let data = driver
            .setup(script, None, &HashMap::new(), client, sink.clone())
            .await;
        let json = data.expect("setup() with http.get must return data, not None");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["status"], 200,
            "setup http response status must flow through"
        );

        // The http.get call must have recorded samples into the sink.
        let samples = sink.lock().unwrap();
        let names: Vec<&str> = samples.iter().map(|s| s.metric.as_ref()).collect();
        assert!(
            names.contains(&"http_req_duration"),
            "setup http.get must record http_req_duration, got: {:?}",
            names
        );
        assert!(
            names.contains(&"http_reqs"),
            "setup http.get must record http_reqs, got: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_setup_data_reaches_default_function() {
        // Full path: the engine sets VuContext.setup_data, the driver seeds
        // __tropel_setup from it once, and the cached iteration call passes
        // it to `export default function (data)`. If data were undefined,
        // `data.token` would throw and run_iteration would error — so a
        // successful iteration proves the data flowed through.
        let driver = K6Driver;
        let script = br#"
            export default function (data) {
                if (!data || data.token !== "abc") throw new Error("setup data not passed");
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        ctx.setup_data = Some("{\"token\":\"abc\"}".to_string());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must succeed with setup data");
    }

    #[tokio::test]
    async fn test_no_setup_data_is_undefined() {
        // Without setup data, `data` must be undefined (k6 parity) — a
        // script asserting `data === undefined` succeeds.
        let driver = K6Driver;
        let script = br#"
            export default function (data) {
                if (data !== undefined) throw new Error("expected undefined data");
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must succeed with undefined data");
    }

    #[tokio::test]
    async fn test_vu_and_iter_globals_are_numbers() {
        // Backlog line 142: __VU/__ITER must be NUMBERS (k6), not strings.
        // The old set_global_str made `__ITER === 0` never true (the
        // once-per-VU guard never fired), `__VU + 1` produce "11" (string
        // concat), and typeof was "string". __VU is also 1-based (k6).
        let driver = K6Driver;
        let script = br#"
            export default function () {
                if (typeof __VU !== 'number') throw new Error('__VU not a number: ' + typeof __VU);
                if (typeof __ITER !== 'number') throw new Error('__ITER not a number: ' + typeof __ITER);
                if (__VU !== 1) throw new Error('__VU must be 1-based, got ' + __VU);
                if (__VU + 1 !== 2) throw new Error('__VU arithmetic broken: ' + (__VU + 1));
                if (__ITER !== 0) throw new Error('__ITER must start at 0, got ' + __ITER);
                // The idiom the string form broke: `=== 0` must fire.
                globalThis.__guard = (globalThis.__guard || 0) + (__ITER === 0 ? 1 : 0);
                if (globalThis.__guard !== 1) throw new Error('__ITER === 0 guard never fired');
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must succeed with numeric __VU/__ITER");
    }

    #[tokio::test]
    async fn test_check_tags_accept_non_string_values() {
        // Backlog line 97: check(r, {...}, {code: 200}) — a NUMBER tag value
        // — dropped the ENTIRE tag map (shim JSON.stringify → bridge
        // from_str::<HashMap<String,String>> failed on {"code":200}, no
        // warning). k6 coerces tag values to strings, so `code` must survive
        // as "200" alongside string/bool tags. Exercised through the REAL
        // bridge (register_script_bridges) and the drained sample sink.
        let driver = K6Driver;
        let script = br#"
            export default function () {
                check(1, { 'status is 200': function (v) { return v === 1; } },
                      { code: 200, ok: true, name: 'health' });
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must succeed");
        let checks: Vec<_> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "checks")
            .collect();
        assert_eq!(
            checks.len(),
            1,
            "one check sample expected, got {:?}",
            ctx.samples
                .iter()
                .map(|s| s.metric.as_ref())
                .collect::<Vec<_>>()
        );
        let sample = checks[0];
        assert_eq!(
            sample.tags.get("code"),
            Some("200"),
            "numeric tag value must survive as a string (k6 coerces)"
        );
        assert_eq!(
            sample.tags.get("ok"),
            Some("true"),
            "boolean tag value must survive as a string"
        );
        assert_eq!(
            sample.tags.get("name"),
            Some("health"),
            "string tag value must survive"
        );
        assert_eq!(
            sample.tags.get("check"),
            Some("status is 200"),
            "the check name must still be stamped"
        );
    }

    // ── k6/crypto + k6/encoding + k6/timers + randomSeed + x509 ──

    #[tokio::test]
    async fn test_k6_crypto_one_shots_all_encodings() {
        // Backlog line 126: crypto.sha256(s,'hex') call sites. All nine
        // one-shot hashes must resolve and the five output encodings must
        // match k6 (binary → ArrayBuffer, base64rawurl = unpadded urlsafe).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    crypto.sha256('hello', 'hex'),
                    crypto.sha1('hello', 'hex'),
                    crypto.md5('hello', 'hex'),
                    crypto.md4('abc', 'hex'),
                    crypto.sha512_224('abc', 'hex'),
                    crypto.sha512_256('abc', 'hex'),
                    crypto.ripemd160('hello', 'hex'),
                    crypto.sha256('hello', 'base64'),
                    crypto.sha256('hello', 'base64url'),
                    crypto.sha256('hello', 'base64rawurl'),
                    Array.from(new Uint8Array(crypto.sha256('hello', 'binary'))).join(','),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            concat!(
                "[\"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\",",
                "\"aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d\",",
                "\"5d41402abc4b2a76b9719d911017c592\",",
                "\"a448017aaf21d8525fc10ae87aa6729d\",",
                "\"4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa\",",
                "\"53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23\",",
                "\"108f07b8382412612c048d07d13f814118445acd\",",
                "\"LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=\",",
                "\"LPJNul-wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=\",",
                "\"LPJNul-wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ\",",
                "\"44,242,77,186,95,176,163,14,38,232,59,42,197,185,226,158,27,22,30,92,31,167,66,94,115,4,51,98,147,139,152,36\"",
                "]"
            )
        );
    }

    #[tokio::test]
    async fn test_k6_crypto_hmac_and_hasher_stateful() {
        // RFC 4231 test case 1 (HMAC-SHA256) + k6's stateful Hasher:
        // createHash/createHMAC return {update, digest} and update chains.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var key = [11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11,11];
                var keyBuf = new Uint8Array(key).buffer;
                JSON.stringify([
                    crypto.hmac('sha256', keyBuf, 'Hi There', 'hex'),
                    crypto.createHash('sha256').update('he').update('llo').digest('hex'),
                    crypto.createHMAC('sha256', keyBuf).update('Hi').update(' There').digest('hex'),
                    // Exercising the digest-0.11 md5 arm of the hmac dispatcher
                    // (RFC 1320 test suite: HMAC-MD5 of "what do ya want for
                    // nothing?" with key "Jefe" = 750c783e6ab0b503eaa86e310a5db738).
                    // Strings are passed straight through (k6ToBytes output is a
                    // plain JS array, which the shim's k6ToBytes rejects).
                    crypto.hmac('md5', 'Jefe', 'what do ya want for nothing?', 'hex'),
                    crypto.hexEncode('hello'),
                    new Uint8Array(crypto.randomBytes(8)).length,
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            concat!(
                "[\"b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7\",",
                "\"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\",",
                "\"b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7\",",
                "\"750c783e6ab0b503eaa86e310a5db738\",",
                "\"68656c6c6f\",8]"
            )
        );
    }

    #[tokio::test]
    async fn test_k6_encoding_b64_roundtrips() {
        // Backlog line 125: b64encode/b64decode with std/rawstd/url/rawurl;
        // b64decode returns a string only for format 's' else ArrayBuffer;
        // unknown encodings silently fall back to std.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                JSON.stringify([
                    encoding.b64encode('hello', 'std'),
                    encoding.b64encode('hello', 'rawstd'),
                    encoding.b64encode(new Uint8Array([251,255,239]).buffer, 'url'),
                    encoding.b64encode(new Uint8Array([251,255,239]).buffer, 'rawurl'),
                    encoding.b64decode('aGVsbG8=', 'std', 's'),
                    encoding.b64decode('aGVsbG8', 'rawstd', 's'),
                    encoding.b64decode('-_8', 'rawurl', 's'),
                    Array.from(new Uint8Array(encoding.b64decode('aGVsbG8=', 'std'))).join(','),
                    encoding.b64decode('aGVsbG8=', 'bogus', 's'),
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            concat!(
                "[\"aGVsbG8=\",\"aGVsbG8\",\"-__v\",\"-__v\",\"hello\",\"hello\",",
                "\"ûÿ\",\"104,101,108,108,111\",\"hello\"]"
            )
        );
    }

    #[tokio::test]
    async fn test_k6_timers_fire_on_pump() {
        // Backlog line 131: setTimeout/clearTimeout/setInterval globals; the
        // driver pumps due timers at the iteration boundary.
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var fired = 0;
                var id = setTimeout(function () { fired = 1; }, 5);
                clearTimeout(id);
                __tropel_pump_timers();
                var afterClear = fired;
                // ms=0 keeps the test deterministic: a due interval fires on
                // every pump and re-arms (no wall-clock dependency).
                var iv = setInterval(function () { fired++; }, 0);
                __tropel_pump_timers();
                __tropel_pump_timers();
                clearInterval(iv);
                var afterClearInterval = fired;
                __tropel_pump_timers();
                JSON.stringify([afterClear, afterClearInterval, fired])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out, "[0,2,2]",
            "cleared timeout must not fire; interval re-arms per pump; clearInterval stops it"
        );
    }

    #[tokio::test]
    async fn test_driver_pumps_timers_at_iteration_boundary() {
        // Backlog line 131 (reviewer follow-up): the iteration-boundary pump
        // in run_iteration must actually fire due timers in a real driver
        // run — a timer armed in iteration N fires at the N+1 boundary.
        let driver = K6Driver;
        // The script asserts the invariant itself (throwing on violation) —
        // the VuContext has no eval, so the driver's run_iteration success/fail
        // IS the observable. The call counter is a JS-internal global rather
        // than `__ITER`: a bare VuContext::new(...) never advances ctx.iteration
        // between run_iteration calls (the engine does that in production), so
        // gating on __ITER would silently skip the assertion entirely (a false
        // positive). ms=0 keeps the test deterministic (same trick as the
        // standalone timers test): a 0ms one-shot is due IMMEDIATELY, so the
        // pump at the end of call 1 fires it — no wall-clock dependency.
        let script = br#"
            export default function (data) {
                var n = (globalThis.__calls = (globalThis.__calls || 0) + 1);
                if (n === 1) {
                    setTimeout(function () { globalThis.__timer_fired = true; }, 0);
                } else if (n === 3) {
                    if (globalThis.__timer_fired !== true) {
                        throw new Error('iteration-boundary pump never fired the due timer');
                    }
                }
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        for _ in 0..3 {
            inst.run_iteration(&mut ctx)
                .await
                .expect("iteration must succeed — the boundary pump fires the timer");
        }
    }

    #[tokio::test]
    async fn test_timer_state_resets_each_iteration() {
        // Backlog line 99: timer state leaked across iterations —
        // __tropel_timers is module-scope with no per-iteration reset, so a
        // setInterval armed in EVERY iteration accumulated live intervals
        // that all fired on every subsequent pump (linear growth in
        // callbacks and retained closures for the VU's life). The driver now
        // calls __tropel_reset_timers() at the start of each iteration.
        //
        // The script asserts the invariant itself: at the start of
        // iteration N>1 the timer table must be EMPTY (the previous
        // iteration's interval was cleared by the reset). Long ms keeps the
        // interval pending so it never fires and never self-cleans — with
        // the leak, iteration 2 would start with 1 live interval and throw.
        let driver = K6Driver;
        let script = br#"
            export default function (data) {
                var n = (globalThis.__calls = (globalThis.__calls || 0) + 1);
                // Start of iteration N>1: the previous iteration's interval
                // must have been cleared by the reset - zero live timers.
                if (n > 1) {
                    var liveAtStart = Object.keys(__tropel_timers).length;
                    if (liveAtStart > 0) {
                        throw new Error('timer leak across iterations: ' + liveAtStart + ' live at start of iter ' + n);
                    }
                }
                setInterval(function () { globalThis.__fired = (globalThis.__fired || 0) + 1; }, 1000000);
                // After arming, exactly ONE interval is live (the current
                // iteration's) - never the accumulated set.
                if (Object.keys(__tropel_timers).length !== 1) {
                    throw new Error('expected exactly 1 live timer, got ' + Object.keys(__tropel_timers).length);
                }
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        for _ in 0..3 {
            inst.run_iteration(&mut ctx)
                .await
                .expect("iteration must succeed — timers must not leak across iterations");
        }
    }

    #[tokio::test]
    async fn test_timer_callback_samples_and_abort_survive_final_iteration() {
        // Backlog line 100: run_iteration drained the sample sink BEFORE
        // pumping timers, so anything a timer callback recorded landed AFTER
        // the drain — picked up next iteration, or SILENTLY DISCARDED on the
        // last one; test.abort() from a timer was delayed or lost. With a
        // SINGLE iteration (which is the final iteration), a setTimeout(0)
        // callback that records a check AND calls exec.test.abort() must
        // have both effects visible after run_iteration returns.
        let driver = K6Driver;
        let script = br#"
            export default function (data) {
                setTimeout(function () {
                    check(1, { 'from timer': function (v) { return v === 1; } });
                    exec.test.abort('stop-from-timer');
                }, 0);
            }
        "#;
        let mut inst = driver.init(script, None, None).await.unwrap();
        let mut ctx = VuContext::new(0, 0, "default".into());
        inst.run_iteration(&mut ctx)
            .await
            .expect("iteration must succeed");
        // The timer callback's check sample must be drained THIS iteration
        // (the only one — nothing follows to pick it up).
        let checks: Vec<_> = ctx
            .samples
            .iter()
            .filter(|s| s.metric == "checks")
            .collect();
        assert_eq!(
            checks.len(),
            1,
            "timer-callback check sample must survive the final iteration, got {:?}",
            ctx.samples
                .iter()
                .map(|s| s.metric.as_ref())
                .collect::<Vec<_>>()
        );
        // The timer's test.abort() must reach the engine THIS iteration.
        assert!(
            ctx.abort_requested,
            "test.abort() from a timer callback must not be lost"
        );
        assert_eq!(
            ctx.abort_message.as_deref(),
            Some("stop-from-timer"),
            "abort message must be the timer's"
        );
    }

    #[tokio::test]
    async fn test_k6_random_seed_is_deterministic() {
        // Backlog line 132: randomSeed() makes Math.random reproducible per
        // VU (each VU owns its JsContext, so this is naturally per-VU).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                randomSeed(42);
                var a = Math.random();
                randomSeed(42);
                var b = Math.random();
                randomSeed(7);
                var c = Math.random();
                JSON.stringify([a === b, a !== c, a, c])
                "#,
            )
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed[0].as_bool(),
            Some(true),
            "same seed must reproduce the sequence"
        );
        assert_eq!(
            parsed[1].as_bool(),
            Some(true),
            "different seed must diverge"
        );
        // mulberry32(42) first value: deterministic, stable across runs.
        assert!(
            (parsed[2].as_f64().unwrap() - 0.6011037519201636).abs() < 1e-12,
            "mulberry32(42) first value must be deterministic"
        );
    }

    #[tokio::test]
    async fn test_k6_x509_parse_against_real_cert() {
        // Backlog line 126: k6/crypto/x509 parse/getSubject/getIssuer/
        // getAltNames against a real self-signed cert (openssl-generated,
        // embedded verbatim below).
        let mut ctx = ctx_with_base_shims().await;
        let out = ctx
            .eval(
                r#"
                var pem = `-----BEGIN CERTIFICATE-----
MIIDzTCCArWgAwIBAgIUcrmV4E5ut2K1ZnLmrzw238cQsXkwDQYJKoZIhvcNAQEL
BQAwTTELMAkGA1UEBhMCVVMxFDASBgNVBAoMC1Ryb3BlbCBUZXN0MQswCQYDVQQL
DAJRQTEbMBkGA1UEAwwSdHJvcGVsLmV4YW1wbGUuY29tMB4XDTI2MDgwODIwMDUy
NloXDTI3MDgwODIwMDUyNlowTTELMAkGA1UEBhMCVVMxFDASBgNVBAoMC1Ryb3Bl
bCBUZXN0MQswCQYDVQQLDAJRQTEbMBkGA1UEAwwSdHJvcGVsLmV4YW1wbGUuY29t
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAu2YbuIwcsDs5A3pSrlUG
ZCGmUcTBVCN1WSSRXb2hfxvaiAaOPEcgrH50rJE42ElBS5NiDn78G+B3B54mDgzV
SoJD5OoPUt6XQag13BpyEaz+wdpI9vqNI4x3Xj2krFqIcw7xCbtLliiMwKvlDF8j
9JIXOu+Mv6teYWYCahysZ7+l9ICkuvkfdgdJnhXk/UfSpuCE/dKmlH9A6xf34ZIX
cTFspYlpMhqRe43cxqEv2f1sdShl9J948y38c55xG6CDFV96kKqz6y7mYf92lohe
kPnqgyIIL/tzYnIZONMjgxo7IQscOBiL4AFIeWx0t2w8PtbnNAUOlnj7bennUPsh
3wIDAQABo4GkMIGhMB0GA1UdDgQWBBTpDo4pBzz/fJGZzP4JLQYk59H1RTAfBgNV
HSMEGDAWgBTpDo4pBzz/fJGZzP4JLQYk59H1RTAPBgNVHRMBAf8EBTADAQH/ME4G
A1UdEQRHMEWCEnRyb3BlbC5leGFtcGxlLmNvbYIWd3d3LnRyb3BlbC5leGFtcGxl
LmNvbYcEfwAAAYERYWRtaW5AZXhhbXBsZS5jb20wDQYJKoZIhvcNAQELBQADggEB
AFDK10h1Hv42MMFlkWpKhw66OHQsnO+m55Qz4SkW5wR1cs++4dW1hQR7kCTtxf9I
9ZzdmK5F59RMrq24QxTN1D+OdQddiXLVoaeqn4/5nrniD7+gVIjyplqwAs1zTQAE
Jkl+yvsXfsK6LubdeYbJ6o47WRTkqp9/t/5G8ZJhB5V76K45puWuooVuzfFYRsZ2
ZBMe25FkBzGAQEuuQjzBC1iLhIDB/lj+HPuUOGHgAF0KB5x0Uxk74h4Qc+0XMtCy
wbHEy5icnC8tmXV0duDtg4Xky4q9zw84BSC8yzDIijhZYsCMvSWnVcH8Xkyc585q
+Cy9E1kAs+8uHJKbres43a4=
-----END CERTIFICATE-----`;
                var c = x509.parse(pem);
                JSON.stringify([
                    c.subject.commonName,
                    c.subject.organizationName,
                    c.subject.organizationalUnitName[0],
                    c.subject.country,
                    c.issuer.commonName,
                    c.signatureAlgorithm,
                    c.publicKey.algorithm,
                    c.altNames,
                    c.fingerPrint.length,
                    c.notBefore.slice(0, 10),
                    c.notAfter.slice(0, 10),
                    x509.getAltNames(pem).length,
                    x509.getSubject(pem).commonName,
                ])
                "#,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            concat!(
                "[\"tropel.example.com\",\"Tropel Test\",\"QA\",\"US\",",
                "\"tropel.example.com\",\"SHA256-RSA\",\"RSA\",",
                "[\"tropel.example.com\",\"www.tropel.example.com\",\"admin@example.com\",",
                "\"127.0.0.1\"],20,\"2026-08-08\",\"2027-08-08\",4,\"tropel.example.com\"]"
            )
        );
    }

    /// Backlog line 46: the per-VU QuickJS heap is capped at 10 MB
    /// (`K6Driver::init`), so a server-controlled binary body near that cap
    /// used to panic across the FFI boundary inside the response bridge. The
    /// degradation CONTRACT is the PRODUCTION [`k6_error_envelope`] — the old
    /// `#[cfg(test)]` `degrade_to_status0_error` was dead code whose
    /// ArrayBuffer-body contract production never matched (W2 line 189).
    /// `set_memory_limit` is QuickJS's HARD limit, so the bridge pre-guards
    /// (body.len() >= K6_VU_HEAP_BYTES) and degrades BEFORE any allocation
    /// while headroom still exists; part (2) pins the no-panic FFI property
    /// with a body far beyond the cap (the old `.expect()` is what made a
    /// theoretical OOM a cross-boundary panic).
    #[tokio::test]
    async fn test_k6_binary_body_alloc_failure_degrades_to_status0_error() {
        let mut js_ctx = JsContext::new(Some(10 * 1024 * 1024), Some(Duration::from_secs(10)))
            .await
            .unwrap();
        // (1) The status-0 degradation contract, exercised directly on the
        //     PRODUCTION envelope (binary=true → empty ArrayBuffer body).
        js_ctx.with_ctx(|ctx| {
            let obj = k6_error_envelope(ctx, "binary response body allocation failed", true)
                .expect("envelope must build while headroom exists");
            let code: i32 = obj.get("code").unwrap();
            assert_eq!(code, 0, "degraded response is status-0");
            let status: i32 = obj.get("status").unwrap();
            assert_eq!(status, 0, "degraded response status is 0");
            let err: String = obj.get("error").unwrap();
            assert!(
                !err.is_empty(),
                "the status-0 fallback sets a diagnostic error message"
            );
            let err_code: i32 = obj.get("error_code").unwrap();
            assert_eq!(
                err_code,
                k6_error_code("binary response body allocation failed"),
                "error_code must mirror k6_error_code(msg), not a hardcoded 1000"
            );
            // The body stays an ArrayBuffer (empty) so scripts probing
            // res.body.byteLength don't see a type change.
            let body: rquickjs::Value = obj.get("body").unwrap();
            assert!(
                body.as_object().is_some_and(|o| o.is_array_buffer()),
                "binary degradation must keep an ArrayBuffer body, got: {body:?}"
            );
        });
        // (2) No-panic property: a 32 MB binary body on a 10 MB heap must
        //     never panic across the FFI boundary. The pre-allocation guard
        //     (body.len() >= K6_VU_HEAP_BYTES) deterministically fires for
        //     this body, so the builder returns the degraded status-0
        //     envelope as Ok — the allocation is never attempted.
        js_ctx.with_ctx(|ctx| {
            let resp = build_k6_response_object(
                ctx,
                200,
                "OK".to_string(),
                vec![0u8; 32 * 1024 * 1024],
                &HashMap::new(),
                0.0,
                None,
                "",
                0,
                "binary",
                &[],
            )
            .expect("pre-guard returns degraded status-0 Ok; 32 MB body never reaches the alloc");
            let code: i32 = resp.get("code").unwrap();
            assert_eq!(
                code, 0,
                "oversized body must degrade to status-0 (was a vacuous `code==0 || code==200`)"
            );
            let err_code: i32 = resp.get("error_code").unwrap();
            assert_eq!(
                err_code,
                k6_error_code("response body exceeds the per-VU JS heap cap"),
                "degraded envelope carries the mapped k6 error code"
            );
            let body: rquickjs::Value = resp.get("body").unwrap();
            assert!(
                body.as_object().is_some_and(|o| o.is_array_buffer()),
                "binary degradation must keep an ArrayBuffer body, got: {body:?}"
            );
        });
    }

    /// Backlog line 51: open-data-shim used to redefine `base64ToBytes` (a
    /// plain-Array variant) and load LAST in K6_NATIVE_SHIM_BUNDLE, so the
    /// k6-shim `Uint8Array` variant that binary response paths call with
    /// `.buffer` was clobbered — http.batch binary entries got
    /// `new Uint8Array(undefined)` → length 0, silently. Eval the shims in
    /// the exact production order and assert the k6 variant survives (the
    /// old standalone test evaled k6-shim alone and passed while production
    /// was broken).
    #[test]
    fn test_k6_shim_bundle_base64_collision_keeps_arraybuffer_view() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let bundle = format!(
                "{}\n{}\n",
                include_str!("../../../../js/k6-shim/k6-shim.js"),
                include_str!("../../../../js/k6-shim/open-data-shim.js")
            );
            ctx.eval::<(), _>(bundle.as_str())
                .expect("shims in production order must eval");
            let is_ab: bool = ctx
                .eval(
                    "var b = base64ToBytes('aGk='); \
                     b instanceof Uint8Array && new Uint8Array(b.buffer).length === 3",
                )
                .expect("base64ToBytes must eval");
            assert!(
                is_ab,
                "base64ToBytes must be k6-shim's Uint8Array variant in the production bundle order"
            );
        });
    }
}
