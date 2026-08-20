use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tropel_js::JsContext;
use tropel_sandbox::state::{PmState, SharedPmState};
use tropel_sdk::config::ExpectedStatus;
use tropel_sdk::scenario::{Scenario, ScenarioItem};
use tropel_sdk::traits::{DriverHttpClient, Protocol};
use tropel_sdk::types::{Sample, SampleType, TagMap};
use tropel_sdk::Result;

/// Result of running a VU iteration.
///
/// `Serialize`/`Deserialize` support the P5b web slice: `tropel-web`
/// postcard-encodes `IterationResult`s into `RunOutcome` so the wasm host
/// can read them (TROPEL_WASM_BUILD.md Step 5A).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IterationResult {
    pub samples: Vec<Sample>,
    pub iteration_index: u64,
    /// Number of prerequest/test scripts that errored this iteration. Surfaced
    /// as failed `checks` samples AND counted all the way to the CLI, so a run
    /// where every script throws exits non-zero instead of reporting success.
    /// `u64` matches the run-wide counter type (no cast noise).
    pub script_failures: u64,
}

/// Configuration for a VU runner.
#[derive(Clone, Default)]
pub struct RunnerConfig {
    pub max_iterations: Option<u64>,
    pub max_duration: Option<Duration>,
}

/// Postman caps `setNextRequest` loops at 10,000 jumps. This counter is
/// PER-ITERATION (stricter than Postman's per-run cap — a run may legitimately
/// span many iterations, but 10k jumps inside one iteration is a runaway
/// loop). Without it, a script that jumps to an earlier item spins forever
/// inside ONE iteration — the JS interrupt doesn't apply to this Rust item
/// loop, so the run never terminates (backlog line 161).
const MAX_SET_NEXT_REQUEST_JUMPS: usize = 10_000;

/// Per-VU iteration runner with full HTTP/JS/PM integration.
///
/// Each VU owns its own HTTP client behind the `DriverHttpClient` trait
/// (own connection pool, cookie jar, and discard_bodies setting) —
/// eliminating connection contention and the N1 race condition where VUs
/// shared a response slot. The trait object is what decouples this crate
/// from `tropel-http` (P4): the executor talks to HTTP only through the
/// SDK trait, so a browser/wasm slice can supply its own implementation.
pub struct ScenarioRunner {
    scenario: Arc<Scenario>,
    /// Depth-first flatten of the scenario item tree into execution order.
    /// Folder items (children present) are containers: their leaf children
    /// run in order. Postman folders are the norm, so the walk MUST descend
    /// or folder-organized collections would run 0 requests. Shared as an
    /// `Arc` across all VUs: the flatten is computed ONCE per scenario (in
    /// the engine) instead of re-cloned per VU at construction. Also makes
    /// `setNextRequest` indexing/name lookup consistent with run order.
    execution_items: Arc<Vec<ScenarioItem>>,
    pm_state: SharedPmState,
    client: Arc<dyn DriverHttpClient>,
    config: RunnerConfig,
    js_ctx: Option<Box<JsContext>>,
    /// Registered protocols keyed by URL scheme (e.g. `grpc`, `ws`, or any
    /// third-party scheme), instantiated once per scenario from the
    /// extension registry and shared across VUs. Dispatch is generic: a
    /// URL's scheme is looked up here, so ANY registered protocol runs —
    /// not just hardcoded gRPC/WebSocket slots.
    protocols: Arc<HashMap<String, Arc<dyn Protocol>>>,
    /// Expected status codes/ranges that determine request success.
    /// Controls http_req_failed metric: 1.0 when status is NOT expected.
    expected_statuses: Vec<ExpectedStatus>,
    /// Level-triggered force-stop flag from the scheduler. Checked between
    /// items inside one iteration so a force-stopped VU stops walking the
    /// collection promptly instead of finishing it (backlog: gracefulStop
    /// force-stop was advisory only).
    force_stop: Arc<AtomicBool>,
    // ── Execution context (k6 exec.* API) ──
    /// Unique VU identifier.
    pub vu_id: u32,
    /// Name of the currently running scenario.
    pub scenario_name: String,
}

impl ScenarioRunner {
    /// Create a new VU runner with a dedicated HTTP client.
    pub fn new(
        scenario: Arc<Scenario>,
        execution_items: Arc<Vec<ScenarioItem>>,
        execution_names: Arc<Vec<String>>,
        client: Arc<dyn DriverHttpClient>,
        vu_id: u32,
        scenario_name: String,
    ) -> Self {
        // Request names for setNextRequest resolution, precomputed ONCE per
        // scenario by the engine and shared across VUs (no per-VU clone).
        let pm_state = Arc::new(Mutex::new(PmState::new()));
        {
            let mut state = pm_state.lock().unwrap();
            state.set_request_names(execution_names);
            // Postman item ids for setNextRequest id-first resolution
            // (backlog §4): derived from the SAME flattened execution list,
            // one entry per item, empty string when an item has no id.
            state.set_request_ids(Arc::new(
                execution_items
                    .iter()
                    .map(|i| i.id.clone().unwrap_or_default())
                    .collect(),
            ));
            state.vu_id = vu_id;
            state.scenario_name = scenario_name.clone();
            // Seed collection variables from the scenario (the Postman
            // collection's top-level `variable` section lands in
            // scenario.variables) so `{{var}}` references in URLs, headers,
            // and bodies resolve. CLI env vars were already merged into
            // scenario.variables by the engine before this point.
            state.collection_vars.extend(scenario.variables.clone());
        }
        Self {
            scenario,
            execution_items,
            pm_state,
            client,
            config: RunnerConfig::default(),
            js_ctx: None,
            protocols: Arc::new(HashMap::new()),
            // Default: 2xx-3xx = success (matches k6 behavior)
            expected_statuses: vec![ExpectedStatus::Range("200-399".to_string())],
            force_stop: Arc::new(AtomicBool::new(false)),
            vu_id,
            scenario_name,
        }
    }

    /// Attach a JS context for script execution.
    pub fn with_js_context(mut self, js_ctx: Box<JsContext>) -> Self {
        self.js_ctx = Some(js_ctx);
        self
    }

    /// Attach the registry-instantiated protocol map so any non-HTTP URL
    /// scheme (`grpc`, `ws`, third-party) dispatches to its registered
    /// protocol instead of the HTTP client.
    pub fn with_protocols(mut self, protocols: Arc<HashMap<String, Arc<dyn Protocol>>>) -> Self {
        self.protocols = protocols;
        self
    }

    /// Set the runner configuration.
    pub fn with_config(mut self, config: RunnerConfig) -> Self {
        // Backlog line 101: pm.info.iterationCount must be live, not the
        // hardcoded stub's 1 — the configured max_iterations is the count
        // when known (None for duration-based runs).
        if let Some(max_iterations) = config.max_iterations {
            self.pm_state.lock().unwrap().total_iterations = Some(max_iterations);
        }
        self.config = config;
        self
    }

    /// Set expected status codes/ranges for http_req_failed evaluation.
    pub fn with_expected_statuses(mut self, expected: Vec<ExpectedStatus>) -> Self {
        // Backlog line 353: pre-parse Range strings into bounds so
        // matches() avoids re-parsing on every HTTP response.
        self.expected_statuses = expected.iter().map(|e| e.pre_parse()).collect();
        self
    }

    /// Link the runner's item loop to the scheduler's force-stop flag so a
    /// force-stopped VU stops mid-iteration instead of walking the whole
    /// collection (backlog: gracefulStop force-stop was advisory only).
    pub fn with_force_stop_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.force_stop = flag;
        self
    }

    /// Attach the execution-context info from the scheduler: the executor
    /// type name and shared handles to the scheduler's ACTIVE-VU and GLOBAL
    /// iteration counters. These back `exec.scenario.executor()`,
    /// `exec.instance.vusActive()`, and `exec.instance.iterationsCompleted()`
    /// (a total across ALL VUs, not just this one).
    pub fn with_exec_context(
        self,
        executor_name: String,
        active_vus: Arc<AtomicU32>,
        global_iterations: Arc<AtomicU64>,
    ) -> Self {
        {
            let mut state = self.pm_state.lock().unwrap();
            state.attach_exec_context(executor_name, active_vus, global_iterations);
        }
        self
    }

    /// Access the PM state.
    pub fn pm_state(&self) -> &SharedPmState {
        &self.pm_state
    }

    /// Run a single iteration through the scenario items.
    pub async fn run_iteration(
        &mut self,
        iteration_index: u64,
        data_row: Option<HashMap<String, serde_json::Value>>,
        env_vars: &HashMap<String, String>,
    ) -> IterationResult {
        let mut result = IterationResult {
            iteration_index,
            ..Default::default()
        };

        // Set iteration data and execution context in PM state.
        // vu_id and scenario_name are already set once in new() and never
        // change — only iteration_index is updated each iteration.
        {
            let mut state = self.pm_state.lock().unwrap();
            state.set_iteration_data(data_row.clone());
            state.iteration_index = iteration_index;
            // Backlog §4: a setNextRequest jump set by the LAST item of the
            // previous iteration was never consumed (the loop already exited)
            // and leaked into this iteration — iteration 2 started
            // mid-collection, re-armed the jump, and every subsequent
            // iteration ran exactly one request. Jumps are per-iteration in
            // Postman; clear any stale pending jump at iteration start.
            state.next_request = None;
            // Newman scopes pm.variables per request — clear accumulated
            // local_vars from the previous iteration so they don't grow
            // monotonically (backlog line 353).
            state.local_vars.clear();
        }

        // Walk through the flattened execution list (folders descended).

        // Variable scope is built per-item below (line 334).
        let resolver = tropel_variables::VariableResolver::new();

        // Walk through the flattened execution list in order
        let item_count = self.execution_items.len();
        let mut current_index = 0usize;
        // setNextRequest jumps honored this iteration (Postman caps loops;
        // a backward/self jump must not spin forever).
        let mut jumps = 0usize;

        while current_index < item_count {
            // Force-stop: stop walking the collection mid-iteration (backlog:
            // gracefulStop force-stop was advisory only — the item loop had
            // no stop check).
            if self.force_stop.load(Ordering::Acquire) {
                break;
            }
            // W2 #197: setNextRequest jumps are consumed at the LOOP BOTTOM
            // (below, after each item's scripts run) — the old top-of-loop
            // check dropped a jump set by the LAST item, so the standard poll
            // loop (`if (status !== 'done') setNextRequest('PollStatus')`)
            // ran once, green, and never exercised the re-poll path.

            let item = &self.execution_items[current_index];

            // Process leaf items: execute the request (if present), then run scripts.
            // Items without a request (e.g. transpiled TS/ES module scripts) still
            // execute their prerequest and test scripts.
            if item.items.is_empty()
                && (item.request.is_some() || !item.prerequest.is_empty() || !item.test.is_empty())
            {
                // Set request info in PM state
                {
                    let mut state = self.pm_state.lock().unwrap();
                    state.request = item.request.clone();
                    // W1-A: `pm.response` must never leak the PREVIOUS item's
                    // response. It is assigned only on request success, so a
                    // transport error / skipped-scheme / skipped item leaves it
                    // pointing at the prior request's 200 — `pm.test(...to.have
                    // .status(200))` then passes against stale data, and
                    // `pm.response.json().id` propagates a wrong id downstream.
                    // Reset here, at the top of every item, alongside `request`;
                    // the success path re-populates it after the request runs.
                    state.response = None;
                    state.skip_request = false;
                    state.current_request_name = item.name.clone();
                }

                // Run prerequest scripts — EACH in its own lexical scope
                // (backlog §4): Postman compiles every script separately, so
                // a `const baseUrl` at collection level and at request level
                // must NOT collide, a top-level `return` only exits its own
                // script, and each script hits the compiled-function cache
                // independently (the old single joined string shared one
                // scope and one cache entry). An error in one script stops
                // the rest of the chain (Postman behavior) but still counts
                // as a failure.
                //
                // Backlog line 101: pm.info.eventName must name the running
                // script phase, not a hardcoded "test". Set only when a
                // prerequest script actually runs (an empty vec must not
                // leave event_name stale while the request executes).
                if !item.prerequest.is_empty() {
                    self.pm_state.lock().unwrap().event_name = "prerequest".to_string();
                }
                for (script_idx, script) in item.prerequest.iter().enumerate() {
                    let source_url = Some(format!("{}.prerequest#{}.js", item.name, script_idx));
                    if let Err(e) = Self::run_script(&mut self.js_ctx, script, source_url).await {
                        if self.force_stop.load(Ordering::Acquire) {
                            // A deliberate force-stop interrupted the eval —
                            // not a script failure; k6 ends such runs neutrally
                            // (backlog: gracefulStop force-stop was advisory
                            // only).
                            tracing::debug!(
                                "VU {} prerequest interrupted by force-stop",
                                iteration_index
                            );
                        } else {
                            tracing::warn!("VU {} prerequest script error: {}", iteration_index, e);
                            // Backlog line 98: script failures were swallowed —
                            // no failed check, no metric, exit 0. Record a
                            // failed check so the failure is visible in the
                            // summary and drives a non-zero exit.
                            record_script_failure(
                                &mut result,
                                &format!("{}.prerequest", item.name),
                            );
                        }
                        break;
                    }
                }

                // Rebuild scope after prerequest script (may have changed env vars)
                let data_row_ref = data_row.as_ref();
                let scope = self.build_scope(data_row_ref.cloned(), env_vars);

                // Backlog line 146: pm.execution.skipRequest() may have been
                // called by the prerequest script. Postman semantics: skip the
                // CURRENT item only — no request send, no test script — and
                // move on to the next one. (The old shim routed it through
                // setNextRequest(null), which threw and stopped the whole run.)
                let skip_item = {
                    let mut state = self.pm_state.lock().unwrap();
                    let s = state.skip_request;
                    state.skip_request = false;
                    s
                };

                // Execute HTTP request only if this item has one.
                // Script-only items (transpiled TS/ES module scripts) don't have
                // a request — they handle HTTP via pm.sendRequest internally.
                if !skip_item && item.request.is_some() {
                    // Backlog line 145: the prerequest script may have MUTATED
                    // the outgoing request via pm.request.* (added an auth
                    // header, changed the URL/method/body). state.request was
                    // seeded from item.request before the prerequest ran — read
                    // THAT so the mutations actually go out on the wire instead
                    // of being discarded by rebuilding from the collection
                    // snapshot.
                    let request = {
                        let st = self.pm_state.lock().unwrap();
                        st.request.clone().unwrap_or_else(|| {
                            item.request
                                .clone()
                                .expect("guarded by item.request.is_some()")
                        })
                    };
                    // Resolve variables across the entire request. URL resolution
                    // uses EscapeMode::Url (currently a passthrough — the planned
                    // percent-encoding was deliberately removed; backlog line 96).
                    // The HTTP layer handles query-param encoding on the wire.
                    // headers/query_params keep raw substitution.
                    let resolved_url = resolver.resolve_url_deep(
                        &request.url,
                        &scope,
                        tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                    );

                    // Resolve headers, query params, body. Headers keep
                    // declaration order + duplicates (W2 #203).
                    let resolved_headers: Vec<(String, String)> = request
                        .headers
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                resolver.resolve_deep(
                                    v,
                                    &scope,
                                    tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                                ),
                            )
                        })
                        .collect();
                    let resolved_query: HashMap<String, String> = request
                        .query_params
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                resolver.resolve_deep(
                                    v,
                                    &scope,
                                    tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                                ),
                            )
                        })
                        .collect();
                    let resolved_body = request
                        .body
                        .as_ref()
                        .map(|b| resolve_body(b, &resolver, &scope));

                    // Build the fully resolved request
                    let mut resolved_req = tropel_sdk::types::Request {
                        url: resolved_url.clone(),
                        method: request.method.clone(),
                        headers: resolved_headers,
                        query_params: resolved_query,
                        body: resolved_body,
                        auth: request
                            .auth
                            .as_ref()
                            .map(|a| resolve_auth(a, &resolver, &scope)),
                        certificate: request.certificate.clone(),
                        follow_redirects: request.follow_redirects,
                        timeout: request.timeout,
                        response_type: request.response_type,
                    };

                    // Backlog line 205: test scripts read pm.request from
                    // state.request, which was seeded from the UNRESOLVED
                    // item before the prerequest ran. Refresh it with the
                    // fully-resolved request so pm.request.url/headers/body
                    // show substituted values, not literal {{templates}}.
                    {
                        let mut state = self.pm_state.lock().unwrap();
                        state.request = Some(resolved_req.clone());
                    }

                    // ── gRPC protocol dispatch (grpc:// or grpcs://) ──
                    // When the URL uses the gRPC scheme, dispatch to the
                    // registered protocol instead of the HTTP client. The
                    // protocol resolves its proto source from request
                    // headers / config / env and returns both the metric
                    // samples and a Response for pm.response.
                    // Scheme-driven dispatch: ANY registered protocol (gRPC,
                    // WebSocket, or a third-party one) runs when its scheme
                    // matches the URL. TLS-suffixed schemes (grpcs, wss) map
                    // to the base registration (grpc, ws) when not registered
                    // verbatim. The protocol returns both the metric samples
                    // and a Response for pm.response.
                    let scheme = resolved_url.split("://").next().unwrap_or("");
                    // http/https are the built-in HTTP path — never dispatch
                    // them to a registered protocol (also closes the latent
                    // https→http strip fallback foot-gun).
                    let is_http_scheme = matches!(scheme, "http" | "https");
                    let protocol = if is_http_scheme {
                        None
                    } else {
                        self.protocols
                            .get(scheme)
                            .or_else(|| self.protocols.get(scheme.strip_suffix('s').unwrap_or("")))
                            .cloned()
                    };
                    // A clearly non-HTTP scheme with no registered protocol:
                    // warn and SKIP (parity with the old 'no gRPC protocol
                    // registered — skipping' behavior) instead of producing a
                    // confusing reqwest error.
                    if protocol.is_none() && !is_http_scheme {
                        tracing::warn!(
                            "VU {}: {}:// URL '{}' but no protocol registered for scheme '{}' — skipping",
                            iteration_index,
                            scheme,
                            resolved_url,
                            scheme
                        );
                    }
                    if let Some(proto) = protocol {
                        let exec_start = Instant::now();
                        match proto.execute(&resolved_req, None).await {
                            Ok(outcome) => {
                                let duration = exec_start.elapsed();
                                tracing::trace!(
                                    "VU runner: {}:// call to {} completed in {:?}",
                                    scheme,
                                    resolved_req.url,
                                    duration
                                );
                                if let Some(resp) = outcome.response {
                                    let mut state = self.pm_state.lock().unwrap();
                                    state.response = Some(resp);
                                }
                                result.samples.extend(outcome.samples);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "VU {} {}:// request '{}' failed: {}",
                                    iteration_index,
                                    scheme,
                                    item.name,
                                    e
                                );
                                let err_tags = Arc::new(TagMap::from_pairs([
                                    ("url", resolved_url.clone()),
                                    ("method", request.method.to_string()),
                                    ("name", item.name.clone()),
                                    ("error", e.to_string()),
                                ]));
                                let now = std::time::SystemTime::now();
                                result.samples.push(tropel_sdk::types::Sample {
                                    metric: "errors".into(),
                                    value: 1.0,
                                    tags: err_tags,
                                    timestamp: now,
                                    sample_type: SampleType::Counter,
                                });
                            }
                        }
                    } else if !is_http_scheme {
                        // Warned above; skip — never send a non-HTTP scheme to
                        // the HTTP client (reqwest would fail confusingly).
                    } else {
                        // Fold the scenario-level auth into the request as a
                        // fallback (request-level auth wins). The
                        // `DriverHttpClient` implementation builds the signer
                        // from `req.auth` internally — the executor no longer
                        // needs to reference auth signers at all (P4
                        // decoupling from the HTTP crate).
                        if resolved_req.auth.is_none() {
                            resolved_req.auth = self.scenario.auth.clone();
                            // Backlog line 205 (cont.): the scenario-auth fold
                            // happens AFTER the pre-dispatch refresh — mirror it
                            // so pm.request in test scripts shows the auth that
                            // actually went out on the wire.
                            {
                                let mut state = self.pm_state.lock().unwrap();
                                state.request = Some(resolved_req.clone());
                            }
                        }

                        // Execute the request directly via the per-VU HTTP client
                        tracing::trace!("VU runner: executing request to {}", resolved_req.url);

                        let exec_start = Instant::now();
                        let exec_result = self.client.execute(&resolved_req).await;
                        let duration = exec_start.elapsed();

                        tracing::trace!(
                            "VU runner: request to {} completed in {:?}",
                            resolved_req.url,
                            duration
                        );

                        match exec_result {
                            Ok(http_response) => {
                                // The DriverHttpClient impl already returns a
                                // core SDK Response — store it in PM state
                                // (clone: the redirect-chain loop below still
                                // borrows the original).
                                let pm_response = http_response.clone();
                                {
                                    let mut state = self.pm_state.lock().unwrap();
                                    state.response = Some(pm_response);
                                }

                                // Emit samples for EVERY redirect hop plus the final
                                // response (k6 parity: a 302 chain counts as hops + 1
                                // requests, not just the final — the earlier
                                // k6_sample_basic comparison showed 136 reqs for 68
                                // iterations while Tropel recorded 64). The final
                                // response's URL/status/body is what pm.response
                                // exposes; each hop gets its own sample set.
                                let chain = http_response
                                    .redirects
                                    .iter()
                                    .chain(std::iter::once(&http_response));
                                for resp in chain {
                                    // Build tags for all request-level metrics
                                    let mut tags = TagMap::with_capacity(5);
                                    tags.insert("url", resp.url.clone());
                                    tags.insert("method", resolved_req.method.to_string());
                                    tags.insert("status", resp.status_code.to_string());
                                    tags.insert("name", resp.url.clone());
                                    tags.insert("group", "http");
                                    // Share one Arc so all ~12 per-request samples bump a
                                    // refcount instead of copying the whole map.
                                    let tags = Arc::new(tags);

                                    let now = std::time::SystemTime::now();

                                    // http_req_duration (Trend) — this hop's own time
                                    result.samples.push(Sample {
                                        metric: "http_req_duration".into(),
                                        value: resp.response_time.as_secs_f64() * 1000.0,
                                        tags: tags.clone(),
                                        timestamp: now,
                                        sample_type: SampleType::Trend,
                                    });

                                    // http_reqs (Counter)
                                    result.samples.push(Sample {
                                        metric: "http_reqs".into(),
                                        value: 1.0,
                                        tags: tags.clone(),
                                        timestamp: now,
                                        sample_type: SampleType::Counter,
                                    });

                                    // http_req_failed (Rate) — true when status not in expected list
                                    let is_failed = !tropel_sdk::config::status_is_expected(
                                        resp.status_code,
                                        &self.expected_statuses,
                                    );
                                    result.samples.push(Sample {
                                        metric: "http_req_failed".into(),
                                        value: if is_failed { 1.0 } else { 0.0 },
                                        tags: tags.clone(),
                                        timestamp: now,
                                        sample_type: SampleType::Rate,
                                    });

                                    // data_received (Counter) — response body bytes
                                    result.samples.push(Sample {
                                        metric: "data_received".into(),
                                        value: resp.size as f64,
                                        tags: tags.clone(),
                                        timestamp: now,
                                        sample_type: SampleType::Counter,
                                    });

                                    // data_sent (Counter) — request body bytes
                                    result.samples.push(Sample {
                                        metric: "data_sent".into(),
                                        value: resp.request_body_size as f64,
                                        tags: tags.clone(),
                                        timestamp: now,
                                        sample_type: SampleType::Counter,
                                    });

                                    // ═══════════════════════════════════════
                                    // HTTP sub-timing metrics (Trend, all in μs)
                                    // ═══════════════════════════════════════
                                    // These match k6's http_req_* sub-timing
                                    // metrics. http_req_dns is a Tropel extra (k6
                                    // folds DNS into http_req_blocked).
                                    // blocked/dns/connecting are REAL (from
                                    // reqwest's dns_resolver + connector_layer
                                    // hooks); tls_handshaking/sending are always
                                    // ZERO (folded into connecting / waiting by
                                    // reqwest). waiting (TTFB) and receiving are
                                    // always measured. Note: on a pooled keep-alive
                                    // reuse no connector call happens, so
                                    // blocked/dns/connecting are 0.
                                    if let Some(timings) = &resp.timings {
                                        // Backlog line 459: omit tls_handshaking and sending
                                        // — always zero (folded into connecting/waiting by
                                        // reqwest). Saves 2 MetricKey builds per request.
                                        let sub_timing_metrics = [
                                            ("http_req_blocked", timings.blocked),
                                            ("http_req_dns", timings.dns),
                                            ("http_req_connecting", timings.connecting),
                                            ("http_req_waiting", timings.waiting),
                                            ("http_req_receiving", timings.receiving),
                                        ];
                                        let sub_tags = tags.clone();
                                        for (metric_name, dur) in &sub_timing_metrics {
                                            result.samples.push(Sample {
                                                metric: (*metric_name).into(),
                                                value: dur.as_secs_f64() * 1000.0,
                                                tags: sub_tags.clone(),
                                                timestamp: now,
                                                sample_type: SampleType::Trend,
                                            });
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "VU {} request '{}' failed: {}",
                                    iteration_index,
                                    item.name,
                                    e
                                );
                                let err_tags = Arc::new(TagMap::from_pairs([
                                    ("url", resolved_url.clone()),
                                    ("method", request.method.to_string()),
                                    ("name", item.name.clone()),
                                    ("error", e.to_string()),
                                ]));
                                let now = std::time::SystemTime::now();
                                // A failed request still counts as a request (k6
                                // parity): the k6 driver's push_http_failure emits
                                // http_reqs alongside http_req_failed. Without this,
                                // a transport error silently drops the request from
                                // the summary — the distributed merge undercounts
                                // http_reqs by exactly the number of failed hops.
                                result.samples.push(tropel_sdk::types::Sample {
                                    metric: "http_reqs".into(),
                                    value: 1.0,
                                    tags: err_tags.clone(),
                                    timestamp: now,
                                    sample_type: SampleType::Counter,
                                });
                                result.samples.push(tropel_sdk::types::Sample {
                                    metric: "errors".into(),
                                    value: 1.0,
                                    tags: err_tags.clone(),
                                    timestamp: now,
                                    sample_type: SampleType::Counter,
                                });
                                // Connection errors always count as failed requests
                                result.samples.push(tropel_sdk::types::Sample {
                                    metric: "http_req_failed".into(),
                                    value: 1.0,
                                    tags: err_tags,
                                    timestamp: now,
                                    sample_type: SampleType::Rate,
                                });
                            }
                        }
                    }
                }

                // Run test scripts (skipped when pm.execution.skipRequest()
                // ran) — EACH in its own lexical scope (backlog §4, same as
                // the prerequest chain).
                if !skip_item {
                    // Backlog line 101: pm.info.eventName names the running
                    // phase — "test" here, "prerequest" above. Set only when
                    // a test script actually runs.
                    if !item.test.is_empty() {
                        self.pm_state.lock().unwrap().event_name = "test".to_string();
                    }
                    for (script_idx, script) in item.test.iter().enumerate() {
                        let source_url = Some(format!("{}.test#{}.js", item.name, script_idx));
                        if let Err(e) = Self::run_script(&mut self.js_ctx, script, source_url).await
                        {
                            if self.force_stop.load(Ordering::Acquire) {
                                // Deliberate force-stop interrupted the eval —
                                // not a script failure (k6 parity: such runs end
                                // neutrally; backlog: gracefulStop force-stop
                                // was advisory only).
                                tracing::debug!(
                                    "VU {} test script interrupted by force-stop",
                                    iteration_index
                                );
                            } else {
                                tracing::warn!("VU {} test script error: {}", iteration_index, e);
                                // Backlog line 98: record a failed check so the
                                // failure is visible and drives a non-zero exit.
                                record_script_failure(&mut result, &format!("{}.test", item.name));
                            }
                            break;
                        }
                    }
                }

                // Collect samples from PM state (checks, custom metrics)
                {
                    let mut state = self.pm_state.lock().unwrap();
                    result.samples.append(&mut state.samples);
                }
            }

            // W2 #197: honor a setNextRequest set by THIS item's scripts.
            // Placed at the loop bottom so a jump from the LAST item is
            // re-consumed and the walk continues (Postman poll-loop
            // semantics). The END_ITERATION sentinel (usize::MAX) or any
            // out-of-range target ends the flow, like the old top check.
            {
                let mut state = self.pm_state.lock().unwrap();
                if let Some(next) = state.next_request.take() {
                    if next < item_count {
                        jumps += 1;
                        if jumps > MAX_SET_NEXT_REQUEST_JUMPS {
                            tracing::warn!(
                                "VU {}: setNextRequest loop exceeded {} jumps — aborting iteration",
                                iteration_index,
                                MAX_SET_NEXT_REQUEST_JUMPS
                            );
                            // Record a failed check so the runaway jump is
                            // visible in the summary and drives a non-zero
                            // exit, like any other script failure.
                            record_script_failure(
                                &mut result,
                                "setNextRequest (loop limit exceeded)",
                            );
                            break;
                        }
                        // Re-walk from the target WITHOUT the natural +1.
                        current_index = next;
                        continue;
                    }
                    break;
                }
            }

            current_index += 1;
        }

        result
    }

    /// Build a variable scope from the current PM state + iteration data + env.
    ///
    /// Deliberately synchronous (`&self`, no `.await`): the `#[async_trait]`
    /// `VuIterationSource` future must be `Send`, and an async `&self` method
    /// would hold `&ScenarioRunner` across an await — `&ScenarioRunner: Send` requires
    /// `ScenarioRunner: Sync`, which the now-`!Sync` `JsContext` can't satisfy.
    fn build_scope(
        &self,
        data_row: Option<HashMap<String, serde_json::Value>>,
        env_vars: &HashMap<String, String>,
    ) -> tropel_variables::VariableScope {
        let data = data_row.unwrap_or_default();
        let state = self.pm_state.lock().unwrap();
        // pm.environment.set() writes into PmState.environment — overlay it
        // on the static CLI/--env-file vars so {{var}} substitution sees
        // script-set values (request 1 saves a token → request 2 sends
        // Bearer {{token}}). Script-set values win over stale seeded ones.
        let mut env = env_vars.clone();
        for (k, v) in &state.environment {
            env.insert(k.clone(), v.clone());
        }
        tropel_variables::VariableScope {
            // pm.variables is the LOCAL scope — Postman's highest priority
            // (backlog line 137): script-set values must win over data/env
            // for {{var}} substitution in later requests.
            local: state.local_vars.clone(),
            data,
            env,
            collection: state.collection_vars.clone(),
            globals: state.globals.clone(),
        }
    }

    /// Run a JavaScript script via the tropel-js context.
    ///
    /// Uses the cached compilation path. The cached wrapper is an **async**
    /// function, so top-level `await` / `Promise` in user scripts is valid
    /// everywhere — there is no fragile substring sniffing to pick between
    /// sync and async paths. Any returned Promise is driven to completion and
    /// rejections surface as errors.
    ///
    /// `source_url` is an identifier shown in error messages and stack traces
    /// (e.g. `"prerequest.js"` or `"test.js"`). When omitted, errors show
    /// the raw source without a meaningful label.
    /// Run a script in the VU's JS context. Takes the context by itself (not
    /// `&mut self`) so callers holding an immutable borrow of another field
    /// (e.g. `&self.execution_items[i]`) don't trip the borrow checker — the
    /// js_ctx field is disjoint from the execution list.
    async fn run_script(
        js_ctx: &mut Option<Box<JsContext>>,
        code: &str,
        source_url: Option<String>,
    ) -> Result<()> {
        if let Some(ctx) = js_ctx {
            ctx.run_script_cached(code, source_url)
                .await
                .map_err(|e| tropel_sdk::TropelError::Other(format!("Script error: {}", e)))?;
        } else {
            tracing::trace!(
                "Script execution skipped (no JS context): {} chars",
                code.len()
            );
        }
        Ok(())
    }

    /// Get the current PM state (for the orchestrator to inject response data).
    pub fn state_handle(&self) -> SharedPmState {
        self.pm_state.clone()
    }
}

/// Record a failed `checks` sample for a script that errored (backlog line
/// 98: script failures were swallowed — a warn! with no failed check, no
/// metric, and exit 0). The check tag carries the script name so the failure
/// is attributable, and [`IterationResult::script_failures`] is incremented
/// so the count propagates to the engine and drives a non-zero exit.
fn record_script_failure(result: &mut IterationResult, script_name: &str) {
    result.script_failures = result.script_failures.saturating_add(1);
    let mut tags = TagMap::with_capacity(1);
    tags.insert("check", format!("script: {}", script_name));
    result.samples.push(Sample {
        metric: "checks".into(),
        value: 0.0,
        tags: Arc::new(tags),
        timestamp: std::time::SystemTime::now(),
        sample_type: SampleType::Rate,
    });
}

/// Depth-first flatten of the scenario item tree into the ordered execution
/// list. Folder items (children present) are containers — their leaf
/// children run in order. A leaf item (no children) runs only if it carries
/// something executable (a request or scripts); empty leaves are skipped.
///
/// This is what makes folder-organized Postman collections actually execute:
/// the parser nests children correctly, and the runner must descend into
/// them instead of walking only the top level.
///
/// `pub`: the engine pre-flattens ONCE per scenario (shared across all VUs
/// via `Arc`) so a large collection is not re-cloned per VU.
pub fn flatten_execution_items(items: &[ScenarioItem]) -> Vec<ScenarioItem> {
    let mut out = Vec::new();
    for item in items {
        if item.items.is_empty() {
            if item.request.is_some() || !item.prerequest.is_empty() || !item.test.is_empty() {
                out.push(item.clone());
            }
        } else {
            out.extend(flatten_execution_items(&item.items));
        }
    }
    out
}

/// Resolve variables inside an `AuthConfig`. Every string field that
/// could contain `{{var}}` placeholders (Bearer token, Basic username/
/// password, ApiKey value, etc.) is run through the variable resolver.
/// This ensures `Authorization: Bearer {{token}}` is resolved before
/// going on the wire (backlog line 218).
fn resolve_auth(
    auth: &tropel_sdk::types::AuthConfig,
    resolver: &tropel_variables::VariableResolver,
    scope: &tropel_variables::VariableScope,
) -> tropel_sdk::types::AuthConfig {
    use tropel_sdk::types::AuthConfig;
    let r =
        |s: &str| resolver.resolve_deep(s, scope, tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES);
    match auth {
        AuthConfig::NoAuth => AuthConfig::NoAuth,
        AuthConfig::Bearer { token } => AuthConfig::Bearer { token: r(token) },
        AuthConfig::Basic { username, password } => AuthConfig::Basic {
            username: r(username),
            password: r(password),
        },
        AuthConfig::ApiKey {
            key,
            value,
            location,
        } => AuthConfig::ApiKey {
            key: r(key),
            value: r(value),
            location: location.clone(),
        },
        AuthConfig::Digest { username, password } => AuthConfig::Digest {
            username: r(username),
            password: r(password),
        },
        AuthConfig::OAuth1 {
            consumer_key,
            consumer_secret,
            token,
            token_secret,
        } => AuthConfig::OAuth1 {
            consumer_key: r(consumer_key),
            consumer_secret: r(consumer_secret),
            token: token.as_deref().map(r),
            token_secret: token_secret.as_deref().map(r),
        },
        AuthConfig::OAuth2 {
            access_token,
            token_type,
        } => AuthConfig::OAuth2 {
            access_token: r(access_token),
            token_type: token_type.as_deref().map(r),
        },
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            region,
            service,
            session_token,
        } => AuthConfig::AwsSigV4 {
            access_key: r(access_key),
            secret_key: r(secret_key),
            region: region.as_deref().map(r),
            service: service.as_deref().map(r),
            session_token: session_token.as_deref().map(r),
        },
        AuthConfig::Hawk {
            auth_id,
            auth_key,
            algorithm,
        } => AuthConfig::Hawk {
            auth_id: r(auth_id),
            auth_key: r(auth_key),
            algorithm: algorithm.as_deref().map(r),
        },
    }
}

/// Resolve variables in a request body.
fn resolve_body(
    body: &tropel_sdk::types::Body,
    resolver: &tropel_variables::VariableResolver,
    scope: &tropel_variables::VariableScope,
) -> tropel_sdk::types::Body {
    match body {
        tropel_sdk::types::Body::Raw(s) => {
            // A Raw body that looks like JSON must resolve with JSON-string
            // escaping so a data value containing a quote/backslash/newline
            // does not produce a broken document (backlog line 96: the Json
            // arm guarded against this but the Raw arm the Postman parser
            // actually produces did not). Non-JSON raw bodies (XML, plain
            // text) stay literal — escaping would corrupt them.
            let trimmed = s.trim_start();
            let looks_like_json = trimmed.starts_with('{') || trimmed.starts_with('[');
            if looks_like_json {
                tropel_sdk::types::Body::Raw(resolver.resolve_json_deep(
                    s,
                    scope,
                    tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                ))
            } else {
                tropel_sdk::types::Body::Raw(resolver.resolve_deep(
                    s,
                    scope,
                    tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                ))
            }
        }
        tropel_sdk::types::Body::Json(val) => {
            // Resolve variables in JSON values by stringifying and re-parsing
            // with JSON-string escaping, so a substituted value cannot break
            // the document (previously a quote in the data fell back to the
            // UNRESOLVED value — the substitution silently never happened).
            let s = serde_json::to_string(val).unwrap_or_default();
            let resolved = resolver.resolve_json_deep(
                &s,
                scope,
                tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
            );
            tropel_sdk::types::Body::Json(
                serde_json::from_str(&resolved).unwrap_or_else(|_| val.clone()),
            )
        }
        tropel_sdk::types::Body::FormData(parts) => {
            // Line 198: form-data parts are text fields OR file uploads
            // (filename/mime/raw bytes). Variables resolve in text-field
            // values only — file bytes must never pass through the resolver.
            let resolved: Vec<tropel_sdk::types::FormDataPart> = parts
                .iter()
                .map(|p| {
                    let mut part = p.clone();
                    if let Some(v) = &p.value {
                        part.value = Some(resolver.resolve_deep(
                            v,
                            scope,
                            tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                        ));
                    }
                    part
                })
                .collect();
            tropel_sdk::types::Body::FormData(resolved)
        }
        tropel_sdk::types::Body::UrlEncoded(fields) => {
            // Duplicate keys preserved in order (W2 #203).
            let resolved: Vec<(String, String)> = fields
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        resolver.resolve_deep(
                            v,
                            scope,
                            tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                        ),
                    )
                })
                .collect();
            tropel_sdk::types::Body::UrlEncoded(resolved)
        }
        tropel_sdk::types::Body::Binary(data) => {
            // Binary bodies can't have variables — pass through
            tropel_sdk::types::Body::Binary(data.clone())
        }
        tropel_sdk::types::Body::GraphQL { query, variables } => {
            // GraphQL query text is not JSON — raw substitution; the
            // variables map IS JSON and gets the same quote-safe resolution
            // as the Json arm (backlog line 96).
            let resolved_query = resolver.resolve_deep(
                query,
                scope,
                tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
            );
            let resolved_vars = variables.as_ref().map(|vars| {
                let s = serde_json::to_string(vars).unwrap_or_default();
                let resolved = resolver.resolve_json_deep(
                    &s,
                    scope,
                    tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                );
                serde_json::from_str(&resolved).unwrap_or_else(|_| vars.clone())
            });
            tropel_sdk::types::Body::GraphQL {
                query: resolved_query,
                variables: resolved_vars,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tropel_http::client::HttpClient;
    use tropel_sdk::types::{Method, ResponseType};

    /// Test-only `DriverHttpClient`: wraps a real `HttpClient` behind the
    /// SDK trait (mirrors the engine's `DriverHttpClientImpl`), so runner
    /// tests exercise the same trait path production VUs use.
    struct TestHttpClient(HttpClient);

    #[async_trait]
    impl DriverHttpClient for TestHttpClient {
        async fn execute(
            &self,
            req: &tropel_sdk::types::Request,
        ) -> Result<tropel_sdk::types::Response> {
            let signer = req.auth.as_ref().and_then(|a| self.0.get_signer(a));
            let resp = self.0.execute(req, signer.as_deref()).await?;
            // Backlog line 312: use by-value conversion to avoid 16 clones.
            Ok(tropel_sdk::types::Response::from(resp))
        }
    }

    fn leaf(name: &str) -> ScenarioItem {
        ScenarioItem {
            name: name.to_string(),
            id: None,
            request: Some(tropel_sdk::types::Request {
                url: format!("http://example.com/{name}"),
                method: Method::GET,
                headers: Vec::new(),
                query_params: HashMap::new(),
                body: None,
                auth: None,
                certificate: None,
                follow_redirects: true,
                timeout: None,
                response_type: ResponseType::None,
            }),
            prerequest: vec![],
            test: vec![],
            assertions: vec![],
            items: vec![],
        }
    }

    fn folder(name: &str, items: Vec<ScenarioItem>) -> ScenarioItem {
        ScenarioItem {
            name: name.to_string(),
            id: None,
            request: None,
            prerequest: vec![],
            test: vec![],
            assertions: vec![],
            items,
        }
    }

    #[test]
    fn flatten_execution_items_descends_folders_in_order() {
        // Folder-organized collection: top-level request, folder with two
        // nested requests, a nested folder (depth 2), and an empty folder
        // that must be skipped. The runner previously walked only depth 1 and
        // ran 0 requests for anything inside a folder (P0).
        let items = vec![
            leaf("top"),
            folder(
                "f1",
                vec![
                    leaf("f1-a"),
                    folder("f1-sub", vec![leaf("f1-sub-1"), leaf("f1-sub-2")]),
                    leaf("f1-b"),
                ],
            ),
            folder("empty", vec![]),
        ];

        let flat = flatten_execution_items(&items);
        let names: Vec<&str> = flat.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["top", "f1-a", "f1-sub-1", "f1-sub-2", "f1-b"],
            "depth-first folder descent, empty folders skipped, got: {names:?}"
        );
    }

    #[test]
    fn resolve_body_raw_json_escapes_quoted_values() {
        // Backlog line 96: the Postman parser produces Raw bodies for JSON
        // request bodies. A data value with a quote used to produce broken
        // JSON (the Json arm guarded; the Raw arm did not).
        let resolver = tropel_variables::VariableResolver::new();
        let scope = tropel_variables::VariableScope {
            env: HashMap::from([("name".into(), "he said \"hi\"".into())]),
            ..Default::default()
        };

        let raw = tropel_sdk::types::Body::Raw(r#"{"s":"{{name}}"}"#.to_string());
        let resolved = resolve_body(&raw, &resolver, &scope);
        match resolved {
            tropel_sdk::types::Body::Raw(s) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&s).expect("resolved raw JSON body must stay valid");
                assert_eq!(parsed["s"], "he said \"hi\"");
            }
            other => panic!("Raw body must stay Raw, got {:?}", other),
        }
    }

    #[test]
    fn resolve_body_plain_raw_stays_literal() {
        // Non-JSON raw bodies (XML, plain text) must NOT be JSON-escaped —
        // escaping would corrupt them.
        let resolver = tropel_variables::VariableResolver::new();
        let scope = tropel_variables::VariableScope {
            env: HashMap::from([("msg".into(), "hi\"there".into())]),
            ..Default::default()
        };
        let raw = tropel_sdk::types::Body::Raw("<m>{{msg}}</m>".to_string());
        let resolved = resolve_body(&raw, &resolver, &scope);
        match resolved {
            tropel_sdk::types::Body::Raw(s) => assert_eq!(s, "<m>hi\"there</m>"),
            other => panic!("Raw body must stay Raw, got {:?}", other),
        }
    }

    #[test]
    fn resolve_auth_bearer_token() {
        // backlog line 218: auth fields must be variable-resolved so
        // `Authorization: Bearer {{token}}` is resolved before going on
        // the wire, not sent literally.
        let resolver = tropel_variables::VariableResolver::new();
        let scope = tropel_variables::VariableScope {
            env: HashMap::from([("token".into(), "my-secret-jwt".into())]),
            ..Default::default()
        };
        let auth = tropel_sdk::types::AuthConfig::Bearer {
            token: "{{token}}".to_string(),
        };
        let resolved = resolve_auth(&auth, &resolver, &scope);
        match resolved {
            tropel_sdk::types::AuthConfig::Bearer { token } => {
                assert_eq!(token, "my-secret-jwt");
            }
            other => panic!("expected Bearer, got {:?}", other),
        }
    }

    #[test]
    fn resolve_auth_basic_username_password() {
        let resolver = tropel_variables::VariableResolver::new();
        let scope = tropel_variables::VariableScope {
            env: HashMap::from([
                ("user".into(), "admin".into()),
                ("pass".into(), "s3cret".into()),
            ]),
            ..Default::default()
        };
        let auth = tropel_sdk::types::AuthConfig::Basic {
            username: "{{user}}".to_string(),
            password: "{{pass}}".to_string(),
        };
        let resolved = resolve_auth(&auth, &resolver, &scope);
        match resolved {
            tropel_sdk::types::AuthConfig::Basic { username, password } => {
                assert_eq!(username, "admin");
                assert_eq!(password, "s3cret");
            }
            other => panic!("expected Basic, got {:?}", other),
        }
    }

    #[test]
    fn flatten_execution_items_skips_scriptless_empty_leaves() {
        // A leaf with no request and no scripts is not executable; it must
        // not appear in the run order.
        let inert = ScenarioItem {
            name: "inert".into(),
            id: None,
            request: None,
            prerequest: vec![],
            test: vec![],
            assertions: vec![],
            items: vec![],
        };
        let flat = flatten_execution_items(&[leaf("a"), inert, leaf("b")]);
        let names: Vec<&str> = flat.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    /// Build a tiny runner over a single-request scenario for scope tests.
    fn runner_with_env_override(
        static_env: HashMap<String, String>,
        script_set: HashMap<String, String>,
    ) -> (ScenarioRunner, tropel_variables::VariableScope) {
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "scope-test".into(),
                description: None,
                schema: None,
            },
            items: vec![leaf("request-one"), leaf("request-two")],
            variables: HashMap::new(),
            auth: None,
        });
        let execution_items = Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let runner = ScenarioRunner::new(
            scenario,
            execution_items,
            names,
            client,
            0,
            "scope-test".into(),
        );
        // Simulate `pm.environment.set("token", ...)` from request 1's
        // prerequest script: it writes into PmState.environment.
        runner.pm_state().lock().unwrap().environment = script_set;
        let scope = runner.build_scope(None, &static_env);
        (runner, scope)
    }

    #[test]
    fn record_script_failure_emits_failed_check_and_counts() {
        // Backlog line 98: a script error used to be warn-only — no failed
        // check, no metric, exit 0. The helper must emit a failed checks
        // Rate sample (visible in the summary) AND increment the counter
        // that drives the non-zero exit.
        let mut result = IterationResult::default();
        record_script_failure(&mut result, "get-token.prerequest");

        assert_eq!(result.script_failures, 1);
        let failed = result
            .samples
            .iter()
            .find(|s| s.metric == "checks")
            .expect("a failed checks sample must be recorded");
        assert_eq!(failed.value, 0.0);
        assert_eq!(failed.sample_type, SampleType::Rate);
        assert_eq!(
            failed.tags.get("check"),
            Some("script: get-token.prerequest")
        );
    }

    #[test]
    fn build_scope_sees_pm_environment_set_values() {
        // P0: build_scope filled `env` only from the static CLI/--env-file
        // map, never from PmState.environment where pm.environment.set()
        // writes. The most common Postman pattern — request 1 saves a token,
        // request 2 sends `Bearer {{authToken}}` — sent the literal string.
        let mut static_env = HashMap::new();
        static_env.insert(
            "BASE_URL".to_string(),
            "https://api.example.com".to_string(),
        );
        let mut script_set = HashMap::new();
        script_set.insert("authToken".to_string(), "tok-abc-123".to_string());
        let (_, scope) = runner_with_env_override(static_env.clone(), script_set);

        // The script-set value must resolve inside {{var}} substitution.
        let resolver = tropel_variables::VariableResolver::new();
        assert_eq!(
            resolver.resolve("{{authToken}}", &scope),
            "tok-abc-123",
            "pm.environment.set() value must be visible to {{var}} substitution"
        );
        // Static CLI env vars still resolve too.
        assert_eq!(
            resolver.resolve("{{BASE_URL}}", &scope),
            "https://api.example.com"
        );

        // Script-set value must WIN over a stale seeded value with the same
        // name (the seeded value silently winning was the bug).
        let mut stale = HashMap::new();
        stale.insert("authToken".to_string(), "STALE".to_string());
        let mut fresh = HashMap::new();
        fresh.insert("authToken".to_string(), "fresh-token".to_string());
        let (_, scope2) = runner_with_env_override(stale, fresh);
        assert_eq!(
            resolver.resolve("{{authToken}}", &scope2),
            "fresh-token",
            "script-set env must override a stale seeded value"
        );
    }

    #[tokio::test]
    async fn pm_request_is_refreshed_with_resolved_url_after_iteration() {
        // Backlog line 205: state.request was seeded from the UNRESOLVED
        // item before the prerequest ran and never refreshed with the
        // resolved request, so test scripts reading pm.request saw literal
        // `{{templates}}` on the wire. After an iteration, pm.request must
        // expose the variable-substituted URL.
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "resolved".into(),
                description: None,
                schema: None,
            },
            variables: HashMap::new(),
            auth: None,
            items: vec![ScenarioItem {
                name: "resolved-item".into(),
                id: None,
                request: Some(tropel_sdk::types::Request {
                    url: "http://{{host}}/v1".into(),
                    method: Method::GET,
                    headers: Vec::new(),
                    query_params: HashMap::new(),
                    body: None,
                    auth: None,
                    certificate: None,
                    follow_redirects: true,
                    timeout: None,
                    response_type: Default::default(),
                }),
                prerequest: vec![],
                test: vec![],
                assertions: vec![],
                items: vec![],
            }],
        });
        let execution_items: Arc<Vec<ScenarioItem>> =
            Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let mut runner = ScenarioRunner::new(
            scenario,
            execution_items,
            names,
            client,
            0,
            "resolved".into(),
        );

        let mut env = HashMap::new();
        env.insert("host".to_string(), "api.example.com".to_string());
        runner.run_iteration(0, None, &env).await;

        let state = runner.pm_state().lock().unwrap();
        let seen_url = state.request.as_ref().map(|r| r.url.clone());
        assert_eq!(
            seen_url.as_deref(),
            Some("http://api.example.com/v1"),
            "pm.request.url must show the RESOLVED url, got {seen_url:?}"
        );
    }

    #[tokio::test]
    async fn force_stop_flag_breaks_item_loop() {
        // Backlog: gracefulStop force-stop was advisory only — the runner's
        // item loop had no stop check, so a force-stopped VU kept walking the
        // whole collection. The loop now breaks on the linked flag.
        let flag = Arc::new(AtomicBool::new(false));
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "fs".into(),
                description: None,
                schema: None,
            },
            variables: HashMap::new(),
            auth: None,
            items: vec![
                ScenarioItem {
                    name: "item-a".into(),
                    id: None,
                    request: Some(tropel_sdk::types::Request {
                        url: "http://127.0.0.1:1/a".into(),
                        method: Method::GET,
                        headers: Vec::new(),
                        query_params: HashMap::new(),
                        body: None,
                        auth: None,
                        certificate: None,
                        follow_redirects: true,
                        timeout: None,
                        response_type: Default::default(),
                    }),
                    prerequest: vec![],
                    test: vec![],
                    assertions: vec![],
                    items: vec![],
                },
                ScenarioItem {
                    name: "item-b".into(),
                    id: None,
                    request: Some(tropel_sdk::types::Request {
                        url: "http://127.0.0.1:1/b".into(),
                        method: Method::GET,
                        headers: Vec::new(),
                        query_params: HashMap::new(),
                        body: None,
                        auth: None,
                        certificate: None,
                        follow_redirects: true,
                        timeout: None,
                        response_type: Default::default(),
                    }),
                    prerequest: vec![],
                    test: vec![],
                    assertions: vec![],
                    items: vec![],
                },
            ],
        });
        let execution_items: Arc<Vec<ScenarioItem>> =
            Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let mut runner =
            ScenarioRunner::new(scenario, execution_items, names, client, 0, "fs".into())
                .with_force_stop_flag(flag.clone());

        // Control: without force-stop the items run (the dead-URL requests
        // still emit http_reqs/errors samples — k6 parity).
        let control = runner.run_iteration(0, None, &HashMap::new()).await;
        assert!(
            !control.samples.is_empty(),
            "control run must execute items (samples empty)"
        );

        // Force-stopped: the item loop must break before any item executes.
        flag.store(true, Ordering::Release);
        let stopped = runner.run_iteration(1, None, &HashMap::new()).await;
        assert!(
            stopped.samples.is_empty(),
            "force-stopped runner must not execute any items (got {} samples)",
            stopped.samples.len()
        );
    }

    #[tokio::test]
    async fn set_next_request_self_loop_terminates_with_failure() {
        // Backlog line 161: a prerequest script that jumps to an EARLIER
        // item (here: itself) used to spin forever inside ONE iteration — no
        // jump counter, and the JS interrupt doesn't apply to the Rust item
        // loop, so the run never terminated. The jump guard must abort the
        // iteration with a failed check instead of hanging.
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "loop".into(),
                description: None,
                schema: None,
            },
            // Item 0's prerequest jumps back to itself every iteration. The
            // jump is re-consumed at the loop bottom (W2 #197), so a single
            // item would spin too; item 1 just makes the runaway shape
            // explicit. Both items are script-only — no network traffic.
            items: vec![
                ScenarioItem {
                    name: "self".into(),
                    id: None,
                    request: None,
                    prerequest: vec!["postman.setNextRequest('self');".into()],
                    test: vec![],
                    assertions: vec![],
                    items: vec![],
                },
                ScenarioItem {
                    name: "after".into(),
                    id: None,
                    request: None,
                    prerequest: vec!["// inert".into()],
                    test: vec![],
                    assertions: vec![],
                    items: vec![],
                },
            ],
            variables: HashMap::new(),
            auth: None,
        });
        let execution_items = Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let mut runner =
            ScenarioRunner::new(scenario, execution_items, names, client, 0, "loop".into());

        // Wire a real JS context with the pm shim + bridge so the prerequest
        // script can actually call setNextRequest.
        let mut js_ctx = Box::new(
            JsContext::new(None, None)
                .await
                .expect("js context should construct"),
        );
        js_ctx
            .eval(include_str!("../../../js/shared/deep-equal.js"))
            .await
            .expect("shared deep-equal should eval");
        js_ctx
            .eval(include_str!("../../../js/scripting-api/pm.js"))
            .await
            .expect("pm shim should eval");
        let bridge_client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("bridge http client should construct"),
        ));
        tropel_sandbox::bindings::trp::TrpBridge::with_http_client(
            runner.pm_state().clone(),
            bridge_client,
        )
        .install(&mut js_ctx)
        .expect("pm bridge should install");
        runner = runner.with_js_context(js_ctx);

        let env = HashMap::new();
        // 30s outer guard: if the jump counter regresses, the loop spins and
        // this times out instead of hanging the whole test suite.
        let result =
            tokio::time::timeout(Duration::from_secs(30), runner.run_iteration(0, None, &env))
                .await
                .expect("setNextRequest self-loop must terminate (jump guard)");
        assert!(
            result.script_failures >= 1,
            "runaway jump must be recorded as a script failure"
        );
        assert!(
            result
                .samples
                .iter()
                .any(|s| s.metric == "checks" && s.value == 0.0),
            "a failed checks sample must be emitted for the loop limit"
        );
    }

    #[tokio::test]
    async fn folder_leaf_prerequest_runs_and_value_resolves() {
        // P0 (backlog): Postman collection/folder-level scripts never ran for
        // folder-organized collections. The parser fix folds the inherited
        // event chain (collection → folder → request, outer→inner) into each
        // leaf at convert time, so flatten_execution_items then executes it
        // as a normal leaf prerequest. This test pins the RUNTIME symptom:
        // a top-level prerequest that mints a token must actually run (once
        // the value is set, pm.environment.set writes into PmState, and the
        // scope used for {{var}} substitution sees it — no literal `Bearer
        // {{token}}` sent, no 401).
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "folder-scripts".into(),
                description: None,
                schema: None,
            },
            // The collection/folder prerequests were folded by the parser
            // into this leaf's prerequest (simulated here with the full
            // chain in order: COLLECTION then FOLDER then REQUEST).
            items: vec![folder(
                "Folder",
                vec![ScenarioItem {
                    name: "inner".into(),
                    id: None,
                    request: None,
                    prerequest: vec![
                        "pm.environment.set('token', 'tok-42'); // COLLECTION; FOLDER; REQUEST"
                            .into(),
                    ],
                    test: vec![],
                    assertions: vec![],
                    items: vec![],
                }],
            )],
            variables: HashMap::new(),
            auth: None,
        });
        let execution_items = Arc::new(flatten_execution_items(&scenario.items));
        assert_eq!(
            execution_items.len(),
            1,
            "folder must be descended into — the leaf with the folded scripts must execute"
        );
        assert!(
            execution_items[0]
                .prerequest
                .iter()
                .any(|s| s.contains("pm.environment.set")),
            "the folded collection/folder prerequest must ride on the leaf"
        );
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let mut runner = ScenarioRunner::new(
            scenario,
            execution_items,
            names,
            client,
            0,
            "folder-scripts".into(),
        );

        let mut js_ctx = Box::new(
            JsContext::new(None, None)
                .await
                .expect("js context should construct"),
        );
        js_ctx
            .eval(include_str!("../../../js/shared/deep-equal.js"))
            .await
            .expect("shared deep-equal should eval");
        js_ctx
            .eval(include_str!("../../../js/scripting-api/pm.js"))
            .await
            .expect("pm shim should eval");
        let bridge_client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("bridge http client should construct"),
        ));
        tropel_sandbox::bindings::trp::TrpBridge::with_http_client(
            runner.pm_state().clone(),
            bridge_client,
        )
        .install(&mut js_ctx)
        .expect("pm bridge should install");
        runner = runner.with_js_context(js_ctx);

        let env = HashMap::new();
        let result = runner.run_iteration(0, None, &env).await;
        assert_eq!(
            result.script_failures, 0,
            "the folded prerequest script must run without error"
        );

        // The runtime guarantee behind the backlog symptom: the token the
        // collection/folder prerequest minted is visible to {{var}}
        // substitution (no literal `Bearer {{token}}` sent).
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(
            state.environment.get("token").map(String::as_str),
            Some("tok-42"),
            "pm.environment.set from the folded script must persist"
        );
        drop(state);
        let scope = runner.build_scope(None, &env);
        let resolver = tropel_variables::VariableResolver::new();
        assert_eq!(
            resolver.resolve("{{token}}", &scope),
            "tok-42",
            "folded prerequest-set value must resolve in later requests"
        );
    }

    #[tokio::test]
    async fn each_script_runs_in_its_own_lexical_scope() {
        // Backlog §4: scripts were joined with "\n;\n" into ONE string that
        // compiled into a single `(async function ...)` — one lexical scope.
        // A `const baseUrl` at collection level AND at request level threw
        // `SyntaxError: Identifier 'baseUrl' has already been declared` and
        // killed the WHOLE chain (collection token-minting never ran); a
        // top-level `return` skipped every downstream script. Postman runs
        // each script as a separate compilation: this pins the runner-level
        // symptom — a redeclared const must not collide, and a `return` must
        // only exit its own script.
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "scopes".into(),
                description: None,
                schema: None,
            },
            // Three separate prerequest scripts on one leaf (the parser now
            // emits a LIST, one element per level):
            //   0: collection — declares `const baseUrl`
            //   1: folder — REDECLARES `const baseUrl` (the old joined
            //      string would throw here) AND returns early
            //   2: request — must STILL run (return only exits script 1)
            items: vec![ScenarioItem {
                name: "scoped".into(),
                id: None,
                request: None,
                prerequest: vec![
                    "const baseUrl = 'https://api.example.com'; // COLLECTION".into(),
                    "const baseUrl = 'https://api.example.com/v2'; if (true) { return; } // FOLDER"
                        .into(),
                    "pm.environment.set('token', 'tok-42'); // REQUEST".into(),
                ],
                test: vec![],
                assertions: vec![],
                items: vec![],
            }],
            variables: HashMap::new(),
            auth: None,
        });
        let execution_items = Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let mut runner =
            ScenarioRunner::new(scenario, execution_items, names, client, 0, "scopes".into());

        let mut js_ctx = Box::new(
            JsContext::new(None, None)
                .await
                .expect("js context should construct"),
        );
        js_ctx
            .eval(include_str!("../../../js/shared/deep-equal.js"))
            .await
            .expect("shared deep-equal should eval");
        js_ctx
            .eval(include_str!("../../../js/scripting-api/pm.js"))
            .await
            .expect("pm shim should eval");
        let bridge_client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("bridge http client should construct"),
        ));
        tropel_sandbox::bindings::trp::TrpBridge::with_http_client(
            runner.pm_state().clone(),
            bridge_client,
        )
        .install(&mut js_ctx)
        .expect("pm bridge should install");
        runner = runner.with_js_context(js_ctx);

        let env = HashMap::new();
        let result = runner.run_iteration(0, None, &env).await;
        assert_eq!(
            result.script_failures, 0,
            "redeclared const must not kill the chain; the request script must still run"
        );
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(
            state.environment.get("token").map(String::as_str),
            Some("tok-42"),
            "script 2 must still run after script 1's early return"
        );
    }

    /// Wire a runner over the given items with a REAL JS context (pm shim +
    /// bridge) so setNextRequest tests exercise the full flow-control path.
    async fn runner_with_scripts(items: Vec<ScenarioItem>) -> ScenarioRunner {
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "jumps".into(),
                description: None,
                schema: None,
            },
            items,
            variables: HashMap::new(),
            auth: None,
        });
        let execution_items = Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client should construct"),
        ));
        let runner =
            ScenarioRunner::new(scenario, execution_items, names, client, 0, "jumps".into());
        let mut js_ctx = Box::new(
            JsContext::new(None, None)
                .await
                .expect("js context should construct"),
        );
        js_ctx
            .eval(include_str!("../../../js/shared/deep-equal.js"))
            .await
            .expect("shared deep-equal should eval");
        js_ctx
            .eval(include_str!("../../../js/scripting-api/pm.js"))
            .await
            .expect("pm shim should eval");
        let bridge_client: Arc<dyn DriverHttpClient> = Arc::new(TestHttpClient(
            HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("bridge http client should construct"),
        ));
        tropel_sandbox::bindings::trp::TrpBridge::with_http_client(
            runner.pm_state().clone(),
            bridge_client,
        )
        .install(&mut js_ctx)
        .expect("pm bridge should install");
        runner.with_js_context(js_ctx)
    }

    fn script_item(name: &str, script: &str) -> ScenarioItem {
        ScenarioItem {
            id: None,
            name: name.into(),
            request: None,
            prerequest: vec![script.into()],
            test: vec![],
            assertions: vec![],
            items: vec![],
        }
    }

    #[tokio::test]
    async fn set_next_request_null_ends_the_iteration() {
        // Backlog §4: setNextRequest(null) was a NO-OP — the runner kept
        // walking the collection. Postman semantics: null ends the current
        // ITERATION (nothing after the jump runs).
        let mut runner = runner_with_scripts(vec![
            script_item(
                "first",
                "pm.environment.set('saw0', '1'); postman.setNextRequest(null);",
            ),
            script_item("second", "pm.environment.set('saw1', '1');"),
        ])
        .await;
        let result = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(state.environment.get("saw0").map(String::as_str), Some("1"));
        assert!(
            !state.environment.contains_key("saw1"),
            "setNextRequest(null) must end the iteration — item 1 must not run"
        );
    }

    #[tokio::test]
    async fn set_next_request_unknown_name_ends_the_iteration() {
        // Backlog §4: an unknown request name was a silent no-op (runner kept
        // walking). Postman stops the flow — the iteration ends.
        let mut runner = runner_with_scripts(vec![
            script_item(
                "first",
                "pm.environment.set('saw0', '1'); postman.setNextRequest('no-such-item');",
            ),
            script_item("second", "pm.environment.set('saw1', '1');"),
        ])
        .await;
        let result = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(state.environment.get("saw0").map(String::as_str), Some("1"));
        assert!(
            !state.environment.contains_key("saw1"),
            "unknown setNextRequest target must end the iteration"
        );
    }

    #[tokio::test]
    async fn set_next_request_jump_does_not_leak_into_next_iteration() {
        // Backlog §4 + W2 #197: a jump set by the LAST item is now HONORED
        // within the iteration (the loop re-checks after the final item) —
        // guarded here so the poll loop terminates. It must still be cleared
        // before the NEXT iteration: iteration 2 starts at item 0 again.
        let mut runner = runner_with_scripts(vec![
            script_item("a", "pm.environment.set('sawA', '1');"),
            script_item("b", "pm.environment.set('sawB', '1');"),
            script_item(
                "c",
                "let n = Number(pm.environment.get('cjumps') || '0'); if (n < 1) { pm.environment.set('cjumps', String(n + 1)); postman.setNextRequest('b'); }",
            ),
        ])
        .await;
        let result0 = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result0.script_failures, 0);
        {
            let state = runner.pm_state().lock().unwrap();
            assert_eq!(
                state.environment.get("cjumps").map(String::as_str),
                Some("1"),
                "the last item's jump must be honored within the iteration"
            );
        }
        // Iteration 2: the stale jump to 'b' must be cleared — item A runs.
        let result = runner.run_iteration(1, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        {
            let state = runner.pm_state().lock().unwrap();
            assert_eq!(
                state.environment.get("sawA").map(String::as_str),
                Some("1"),
                "iteration 2 must start at the first item — the previous jump leaked"
            );
        }
    }

    #[tokio::test]
    async fn set_next_request_from_last_item_keeps_poll_loop_running() {
        // W2 #197: the standard Postman poll loop — the LAST item
        // conditionally jumps back to itself until the poll target is
        // reached. Before the fix the jump was dropped (the loop only
        // checked at the top), so the poll item ran ONCE, green, and the
        // collection ended without ever re-polling. It must re-run within
        // the SAME iteration.
        let mut runner = runner_with_scripts(vec![
            script_item("seed", "pm.environment.set('sawSeed', '1');"),
            script_item(
                "poll",
                "let n = Number(pm.environment.get('polls') || '0') + 1; pm.environment.set('polls', String(n)); if (n < 3) { postman.setNextRequest('poll'); }",
            ),
        ])
        .await;
        let result = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(
            state.environment.get("polls").map(String::as_str),
            Some("3"),
            "the last-item poll jump must re-run the poll item 3 times in one iteration"
        );
        assert!(
            state.environment.contains_key("sawSeed"),
            "the seed item must run before the poll loop"
        );
    }

    #[tokio::test]
    async fn set_next_request_duplicate_name_is_last_wins() {
        // Backlog §4: the old first-wins position() jumped to the FIRST item
        // with a duplicate name. Postman is last-wins.
        let mut runner = runner_with_scripts(vec![
            script_item("start", "postman.setNextRequest('dup');"),
            script_item("dup", "pm.environment.set('saw1', '1');"),
            script_item("dup", "pm.environment.set('saw2', '1');"),
        ])
        .await;
        let result = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        let state = runner.pm_state().lock().unwrap();
        assert!(
            state.environment.contains_key("saw2"),
            "the LAST duplicate must win (saw2 set)"
        );
        assert!(
            !state.environment.contains_key("saw1"),
            "the FIRST duplicate must NOT run (saw1 unset)"
        );
    }

    #[tokio::test]
    async fn set_next_request_resolves_id_before_name() {
        // Backlog §4: Postman resolves item ids FIRST — a request id shadows
        // a same-named item. The named item sits BEFORE the id-carrying item:
        // if the name won, the jump would land on it first (and the id item
        // would still run after); if the id wins, execution jumps PAST the
        // named item directly to the id item (Postman continues from the
        // target to the end of the collection, so ordering isolates the win).
        let mut runner = runner_with_scripts(vec![
            script_item("start", "postman.setNextRequest('t1');"),
            script_item("t1", "pm.environment.set('sawName', '1');"),
            ScenarioItem {
                id: Some("t1".into()),
                name: "by-id".into(),
                request: None,
                prerequest: vec!["pm.environment.set('sawId', '1');".into()],
                test: vec![],
                assertions: vec![],
                items: vec![],
            },
        ])
        .await;
        let result = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(
            state.environment.get("sawId").map(String::as_str),
            Some("1"),
            "item id must win over the same-named item"
        );
        assert!(
            !state.environment.contains_key("sawName"),
            "the same-named item must NOT run (id resolved first)"
        );
    }

    #[tokio::test]
    async fn set_next_request_numeric_like_name_is_not_hijacked() {
        // Backlog §4: a request literally named "2" was hijacked by the
        // numeric-index parse (jumped to INDEX 2). The name lookup must
        // precede the legacy numeric fallback. (Postman continues from the
        // jump target to the end, so the later "third" item still runs — the
        // discriminator is that the item NAMED '2' runs at all.)
        let mut runner = runner_with_scripts(vec![
            script_item("start", "postman.setNextRequest('2');"),
            script_item("2", "pm.environment.set('sawName2', '1');"),
            script_item("third", "pm.environment.set('sawIdx2', '1');"),
        ])
        .await;
        let result = runner.run_iteration(0, None, &HashMap::new()).await;
        assert_eq!(result.script_failures, 0);
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(
            state.environment.get("sawName2").map(String::as_str),
            Some("1"),
            "a request named '2' must resolve by NAME, not index"
        );
        // Under the old numeric-first parse, '2' jumped to INDEX 2 ("third")
        // and the item named "2" never ran — sawIdx2 would be set and
        // sawName2 absent. The name lookup winning is what this pins.
        assert_eq!(
            state.environment.get("sawIdx2").map(String::as_str),
            Some("1"),
            "execution continues past the jump target (Postman flow)"
        );
    }

    #[tokio::test]
    async fn pm_variables_local_scope_shadows_iteration_data() {
        // Backlog line 137: pm.variables.set wrote to COLLECTION scope while
        // get read data > env > collection — set-then-get disagreed when
        // iteration data had the same key. pm.variables is Postman's LOCAL
        // (highest-priority) scope: the set value must win, both for
        // pm.variables.get and for {{var}} substitution in later requests.
        let mut runner = runner_with_scripts(vec![
            script_item("first", "pm.variables.set('token', 'local-tok');"),
            script_item(
                "second",
                "pm.environment.set('saw', pm.variables.get('token'));",
            ),
        ])
        .await;
        let data: HashMap<String, serde_json::Value> =
            HashMap::from([("token".into(), serde_json::Value::String("data-tok".into()))]);
        let _ = runner.run_iteration(0, Some(data), &HashMap::new()).await;
        let state = runner.pm_state().lock().unwrap();
        assert_eq!(
            state.environment.get("saw").map(String::as_str),
            Some("local-tok"),
            "pm.variables (local scope) must win over iteration data"
        );
    }

    /// Scripted mock: the first call succeeds (200), every later call fails
    /// with a transport error — the exact shape of "request 1 succeeds, then
    /// the server/target dies" that W1-A exercises.
    struct FlakyClient {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl DriverHttpClient for FlakyClient {
        async fn execute(
            &self,
            _req: &tropel_sdk::types::Request,
        ) -> Result<tropel_sdk::types::Response> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(tropel_sdk::types::Response {
                    url: "http://example.com/ok".into(),
                    status_code: 200,
                    status_text: "OK".into(),
                    headers: HashMap::new(),
                    body: br#"{"id":1}"#.to_vec(),
                    text_cache: std::sync::OnceLock::new(),
                    json_cache: std::sync::OnceLock::new(),
                    response_time: Duration::from_millis(1),
                    timings: None,
                    cookies: vec![],
                    size: 0,
                    request_body_size: 0,
                    redirects: vec![],
                })
            } else {
                Err(tropel_sdk::TropelError::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "connection refused (simulated transport failure)",
                )))
            }
        }
    }

    #[tokio::test]
    async fn failed_request_clears_stale_pm_response() {
        // W1-A: `pm.response` must never leak the PREVIOUS item's response.
        // The loop previously assigned only on success and never reset, so a
        // transport error on item 2 left `pm.response` pointing at item 1's
        // 200 — `pm.test(..., to.have.status(200))` PASSED against stale
        // data, and `pm.response.json().id` propagated a wrong id downstream.
        // Worst exactly at saturation, when http_req_failed climbs while
        // checks stay green.
        let scenario = Arc::new(Scenario {
            info: tropel_sdk::scenario::ScenarioInfo {
                name: "stale".into(),
                description: None,
                schema: None,
            },
            variables: HashMap::new(),
            auth: None,
            items: vec![leaf("ok-item"), leaf("dead-item")],
        });
        let execution_items: Arc<Vec<ScenarioItem>> =
            Arc::new(flatten_execution_items(&scenario.items));
        let names: Arc<Vec<String>> =
            Arc::new(execution_items.iter().map(|i| i.name.clone()).collect());
        let client: Arc<dyn DriverHttpClient> = Arc::new(FlakyClient {
            calls: Arc::new(AtomicU32::new(0)),
        });
        let mut runner =
            ScenarioRunner::new(scenario, execution_items, names, client, 0, "stale".into());

        let _ = runner.run_iteration(0, None, &HashMap::new()).await;

        let state = runner.pm_state().lock().unwrap();
        assert!(
            state.response.is_none(),
            "pm.response must be None after a failed request — got {:?} (stale from item 1)",
            state.response.as_ref().map(|r| r.status_code)
        );
    }
}
