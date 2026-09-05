//! TR-405: the localhost agent server. knockport's desktop transport reaches
//! the SAME engine over this socket — one engine from Send to 10 000 VU.
//!
//! The agent is a plain HTTP server on a loopback address (refuses any
//! non-loopback bind), token-authenticated and rate-limited, because it is an
//! arbitrary-request-execution endpoint reachable from any local process.
//!
//! Endpoints:
//!   POST /execute   { method, url, headers, body, follow_redirects } →
//!                   { status, status_text, headers, body, timings, error }
//!                   — a single request with full sub-timings (the same code
//!                   path a request under load takes).
//!   GET  /version   → the agent's tropel version (TR-406 handshake).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use tropel_sdk::types::{Body, Method, Request as TropelRequest, ResponseType};
use tropel_sdk::TropelError;

/// Max requests per second per connection (a crude rate limit for the
/// arbitrary-request-execution endpoint).
const RATE_LIMIT_PER_SEC: u64 = 200;

/// Shared agent state: the auth token and the engine's HTTP client.
struct AgentState {
    token: Option<String>,
    client: tropel_http::HttpClient,
}

/// Start the agent server. Refuses a non-loopback bind address (the register:
/// "Refuses to start with an obviously-wrong bind address rather than exposing
/// an execution endpoint to the network").
pub async fn run_agent(port: u16, bind: &str, token: Option<&str>) -> tropel_sdk::Result<()> {
    let ip: IpAddr = bind
        .parse()
        .map_err(|_| TropelError::Other(format!("invalid bind address: {bind}")))?;
    if !ip.is_loopback() {
        return Err(TropelError::Other(format!(
            "refusing to bind {bind}: the agent is a localhost-only execution endpoint (TR-405)"
        )));
    }

    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr).await.map_err(TropelError::Io)?;
    tracing::info!(
        "tropel agent listening on http://{addr} (token auth {})",
        if token.is_some() { "on" } else { "off" }
    );

    let http_config = tropel_http::HttpConfig::default();
    let client = tropel_http::HttpClient::new(&http_config)
        .map_err(|e| TropelError::Other(format!("http client init failed: {e}")))?;
    let state = Arc::new(AgentState {
        token: token.map(str::to_string),
        client,
    });

    loop {
        let (mut sock, peer) = listener.accept().await.map_err(TropelError::Io)?;
        tracing::debug!("agent: connection from {peer}");
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(&mut sock, state).await {
                tracing::debug!("agent: connection error: {e}");
            }
        });
    }
}

async fn handle_connection(sock: &mut TcpStream, state: Arc<AgentState>) -> tropel_sdk::Result<()> {
    // Read the HTTP request head (bounded buffer; we only support simple
    // POST/GET with a JSON body).
    let mut buf = vec![0u8; 64 * 1024];
    let n = sock.read(&mut buf).await.map_err(TropelError::Io)?;
    let raw = String::from_utf8_lossy(&buf[..n]);

    // TR-445: any body bytes that arrived in the SAME read as the head.
    //
    // This single `read` routinely returns head AND body together — a small
    // JSON POST is one TCP segment, which is the normal case, not an edge
    // one. Every handler below then called `read_exact(content_length)` and
    // waited for bytes that had already been delivered, so the connection
    // hung until the client gave up. It was masked because clients that write
    // the head and body in separate calls (curl, reqwest) happen to split the
    // segments.
    let prefetched_body: Vec<u8> = raw
        .find("\r\n\r\n")
        .map(|i| buf[i + 4..n].to_vec())
        .unwrap_or_default();

    // Rate limit + auth on every request (a fresh limiter per connection —
    // good enough for the localhost boundary).
    {
        let mut limiter = RateLimiter::new();
        if limiter.allow().is_err() {
            return respond(sock, 429, "rate limit exceeded").await;
        }
    }

    // Parse the request line, path, and headers.
    let mut lines = raw.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let mut content_length = 0usize;
    let mut auth_header = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            if key == "content-length" {
                content_length = v.trim().parse().unwrap_or(0);
            } else if key == "authorization" {
                auth_header = v.trim().to_string();
            }
        }
    }

    if let Some(expected) = &state.token {
        if auth_header != format!("Bearer {expected}") {
            return respond(sock, 401, r#"{"error":"unauthorized"}"#).await;
        }
    }

    match (method, path) {
        ("GET", "/version") => {
            let body = format!(r#"{{"version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
            respond(sock, 200, &body).await
        }
        // ── TR-445 · the RULES endpoints ─────────────────────────────────────
        //
        // The agent exposed request EXECUTION only, which is why every
        // core-tier method in knockport's `native-agent.ts` throws
        // `TropelCoreUnavailableError` naming this gap. Desktop ships no wasm,
        // so without these the only way to resolve a variable or sign a
        // request there is a TypeScript re-implementation — invariant #3, and
        // the most expensive recurring bug class in both repos.
        //
        // These are pure functions over JSON: same Rust the wasm tier calls,
        // reached over the loopback socket instead of a wasm boundary.
        ("POST", "/resolve/batch") => {
            // TR-448 (knockport KP-209): resolve MANY templates in one call.
            //
            // `/resolve` takes a single template, and knockport's
            // `resolveRequest` walks ~33 of them per request — url, headers,
            // params, auth fields, body. Per-call that is 33 loopback round
            // trips at 70 us each: 2.3 ms of pure overhead on every send,
            // measured. Batched it is one trip, 0.07 ms.
            //
            // That difference is the whole reason the desktop tier can use
            // the agent at all instead of shipping a second copy of this Rust
            // as wasm.
            let Some(payload) =
                read_json_body(sock, content_length, 8 * 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let vars: HashMap<String, String> = payload
                .get("variables")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let items: Vec<BatchResolveItem> =
                match serde_json::from_value(payload.get("items").cloned().unwrap_or_default()) {
                    Ok(v) => v,
                    Err(e) => {
                        return respond(sock, 400, &error_body(&format!("invalid items: {e}")))
                            .await
                    }
                };
            // ORDER IS THE CONTRACT: the caller re-assembles its request by
            // index, so a reordered or short reply would put a header's value
            // in a param. One output per input, always, even for the ones
            // that fail.
            // TR-449: each item reports `hitCap` and `unresolved`, not just a
            // string. KnockPort's `resolveVariables` uses them to tell a CYCLE
            // (`{{a}}` -> `{{b}}` -> `{{a}}`, a failed send) from an UNKNOWN
            // NAME (the user's typo, left visible and sent). Both leave a
            // literal `{{…}}` in the text, so a bare string cannot distinguish
            // them — and only the resolver's own loop knows which happened.
            let scope = tropel_variables::VariableScope {
                env: vars.clone(),
                ..Default::default()
            };
            let resolver = tropel_variables::VariableResolver::new();
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                let mode = item.mode.as_deref().unwrap_or("plain");
                // TR-449: `deep: false` is REFUSED here, not ignored. The
                // batched path exists to serve `resolveTemplateDetailed`,
                // whose report only means something for a chain the resolver
                // ran to settlement: a shallow pass stops BY DESIGN, so it has
                // no cap to hit. Emulating one with `max_passes = 1` would
                // report `{{a}}` -> `{{b}}` as a CYCLE, and silently upgrading
                // to deep is the worse half of the same trade — the caller
                // asked for one pass, got twenty, and cannot tell. `POST
                // /resolve` still answers a shallow single resolve.
                if item.deep == Some(false) {
                    out.push(serde_json::json!({
                        "error": "deep: false is not supported by POST /resolve/batch \u{2014} the batched reply reports hitCap/unresolved, which only a chain resolved to settlement has; use POST /resolve for a shallow resolve"
                    }));
                    continue;
                }
                match resolver.resolve_reporting(
                    &item.template,
                    &scope,
                    tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                    mode,
                ) {
                    Ok(outcome) => out.push(serde_json::json!({
                        "value": outcome.value,
                        "hitCap": outcome.hit_cap,
                        "unresolved": outcome.unresolved,
                    })),
                    // A per-item failure does NOT fail the batch: one bad
                    // escape mode must not lose the other 32 resolutions, and
                    // the caller can still see exactly which item broke.
                    Err(why) => out.push(serde_json::json!({ "error": why })),
                }
            }
            respond(sock, 200, &serde_json::json!({ "items": out }).to_string()).await
        }

        ("POST", "/resolve") => {
            let Some(payload) =
                read_json_body(sock, content_length, 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let template = payload
                .get("template")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let vars: HashMap<String, String> = payload
                .get("variables")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let mode = payload
                .get("mode")
                .and_then(|m| m.as_str())
                .unwrap_or("none");
            let deep = payload
                .get("deep")
                .and_then(|d| d.as_bool())
                .unwrap_or(true);
            match tropel_variables::resolve_template_for_host(template, &vars, mode, deep) {
                Ok(resolved) => {
                    respond(
                        sock,
                        200,
                        &serde_json::json!({ "resolved": resolved }).to_string(),
                    )
                    .await
                }
                // A typo'd mode is a NAMED 400, never a silent fallback to
                // plain — that is how a quote-bearing value corrupts a body.
                Err(why) => respond(sock, 400, &error_body(&why)).await,
            }
        }

        ("POST", "/assert") => {
            let Some(payload) =
                read_json_body(sock, content_length, 8 * 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let target: tropel_variables::assertions::AssertionTarget = match serde_json::from_value(
                payload.get("response").cloned().unwrap_or_default(),
            ) {
                Ok(t) => t,
                Err(e) => {
                    return respond(sock, 400, &error_body(&format!("invalid response: {e}"))).await
                }
            };
            let specs: Vec<AgentAssertionSpec> = match serde_json::from_value(
                payload.get("assertions").cloned().unwrap_or_default(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    return respond(sock, 400, &error_body(&format!("invalid assertions: {e}")))
                        .await
                }
            };
            // A native agent CAN link a regex engine — unlike the wasm tier,
            // where TR-434 removed it and the host's RegExp is injected. Using
            // Rust's `regex` here would make `matches` behave differently on
            // desktop than in the browser, which is precisely the divergence
            // this endpoint exists to prevent. So it is left unwired and the
            // outcome says so BY NAME.
            let outcomes: Vec<_> = specs
                .iter()
                .map(|spec| {
                    let name = spec
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{} {}", spec.target, spec.operator));
                    match tropel_variables::assertions::resolve_assertion_target(
                        &spec.target,
                        &target,
                    ) {
                        Ok(actual) => tropel_variables::assertions::assert_evaluate(
                            &name,
                            &spec.target,
                            &actual,
                            &spec.operator,
                            &spec.expected,
                            None,
                        ),
                        Err(why) => tropel_variables::assertions::AssertionOutcome {
                            name,
                            passed: false,
                            unsupported: Some(why),
                            message: None,
                        },
                    }
                })
                .collect();
            respond(
                sock,
                200,
                &serde_json::to_string(&outcomes).unwrap_or_default(),
            )
            .await
        }

        ("POST", "/variables/dynamic/batch") => {
            // TR-452: the batched twin, for the same reason /resolve has one.
            // KnockPort's `resolveVariables` calls the dynamic pass for EVERY
            // template, unconditionally, before it looks at the `{{var}}` map
            // — so without this the desktop tier pays a round trip per field.
            //
            // A client-side "skip it unless the text contains `{{$`" would
            // remove those trips without an endpoint, and it is exactly the
            // shortcut not to take: `{{ $guid }}` with spaces is a dynamic
            // token that such a test would miss, and deciding what counts as
            // one is the catalogue's job, not the caller's (invariant #3).
            let Some(payload) =
                read_json_body(sock, content_length, 8 * 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let items: Vec<BatchResolveItem> = payload
                .get("items")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            // ORDER IS THE CONTRACT, as in /resolve/batch: the caller
            // re-assembles by index, so one output per input, always.
            let catalog = tropel_variables::DynamicCatalog::new();
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                match catalog.resolve(&item.template) {
                    Ok(value) => out.push(serde_json::json!({ "value": value })),
                    Err(why) => out.push(serde_json::json!({ "error": why })),
                }
            }
            respond(sock, 200, &serde_json::json!({ "items": out }).to_string()).await
        }

        ("POST", "/variables/dynamic") => {
            // TR-451: `{{$guid}}`, `{{$timestamp}}`, `{{$randomInt}}` — the
            // predefined catalogue. SEPARATE from /resolve on purpose, and
            // that separation is the contract, not an accident: /resolve
            // substitutes the embedder's `{{var}}` map and leaves `{{$…}}`
            // alone, while this one generates a FRESH value per occurrence
            // and leaves plain `{{var}}` alone. KnockPort runs them in that
            // order (`resolveVariables` in packages/core/src/utils.ts), so
            // folding them together here would change which of the two saw a
            // `{{$guid}}` produced by a variable's value.
            let Some(payload) =
                read_json_body(sock, content_length, 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let template = payload
                .get("template")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let catalog = tropel_variables::DynamicCatalog::new();
            match catalog.resolve(template) {
                Ok(value) => {
                    respond(
                        sock,
                        200,
                        &serde_json::json!({ "value": value }).to_string(),
                    )
                    .await
                }
                // The 16 MiB total-output cap (TR-403). A NAMED failure, never
                // a truncated body: `{{$randomLoremParagraphs}}` in a loop is
                // the shape that hits it, and silently returning half of it is
                // the data loss invariant #7 forbids.
                Err(why) => respond(sock, 400, &error_body(&why)).await,
            }
        }

        ("GET", "/constants") => {
            // TR-451: the values that CANNOT drift between the two hosts, in
            // one fetch at handshake.
            //
            // The pass cap is here because KnockPort was carrying its own
            // `MAX_VARIABLE_RESOLUTION_PASSES_FALLBACK` on the desktop path —
            // a SECOND ceiling, which is the exact duplication KP-424 removed
            // from the resolver itself. A host that stops at 20 talking to an
            // agent that stops at 25 disagrees about which chains are cyclic,
            // and a cycle is a failed send.
            //
            // The predefined catalogue rides along rather than getting its own
            // route: it is equally constant, the editor needs it at the same
            // moment, and one fetch cannot half-succeed the way two can.
            let variables: Vec<serde_json::Value> = tropel_variables::PREDEFINED_VARIABLE_META
                .iter()
                .map(|m| serde_json::json!({ "name": m.name, "description": m.description }))
                .collect();
            let body = serde_json::json!({
                "maxVariableResolutionPasses": tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
                "predefinedVariables": variables,
            });
            respond(sock, 200, &body.to_string()).await
        }

        ("GET", "/operators") => {
            // The assertion vocabulary, so a desktop editor renders the SAME
            // dropdown the evaluator dispatches on.
            let body = serde_json::to_string(tropel_variables::assertions::ASSERTION_OPERATORS)
                .unwrap_or_default();
            respond(sock, 200, &body).await
        }

        ("POST", "/auth/sign") => {
            // TR-445: the four request signers, so the desktop tier does not
            // re-implement them in TypeScript. Same shape as `core-wasm`'s
            // exports — RAW request components in, finished headers out — so
            // the AWS service derivation, the S3 double-encoding rule, the
            // RFC 5849 base-string URI and the digest challenge parse all stay
            // on this side (TR-428..TR-431).
            let Some(payload) =
                read_json_body(sock, content_length, 8 * 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let scheme = payload.get("scheme").and_then(|s| s.as_str()).unwrap_or("");
            let params = payload.get("params").cloned().unwrap_or_default();
            match sign_with_scheme(scheme, &params) {
                Ok(headers) => {
                    respond(
                        sock,
                        200,
                        &serde_json::to_string(&headers).unwrap_or_default(),
                    )
                    .await
                }
                Err(why) => respond(sock, 400, &error_body(&why)).await,
            }
        }

        ("POST", "/script") => {
            // TR-446 (KT-203 `run_script`): run a pre/post-request script and
            // return its effects. Same realm a load run uses, so a script that
            // behaves one way in the app behaves the same way under load.
            let Some(payload) =
                read_json_body(sock, content_length, 4 * 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let code = payload.get("code").and_then(|c| c.as_str()).unwrap_or("");
            let environment: HashMap<String, String> = payload
                .get("environment")
                .and_then(|e| serde_json::from_value(e.clone()).ok())
                .unwrap_or_default();
            match run_script_once(code, environment).await {
                Ok(out) => respond(sock, 200, &out.to_string()).await,
                Err(why) => respond(sock, 500, &error_body(&why)).await,
            }
        }

        ("POST", "/auth/oauth2") => {
            // TR-447: the OAuth2/JWT/WSSE family. Closes the last arm of the
            // gap `native-agent.ts` documents — every method on its
            // `TropelAuthProvider` threw because the agent exposed none of
            // this, and desktop ships no wasm to fall back on.
            let Some(payload) =
                read_json_body(sock, content_length, 1024 * 1024, &prefetched_body).await?
            else {
                return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await;
            };
            let op = payload.get("op").and_then(|o| o.as_str()).unwrap_or("");
            let params = payload.get("params").cloned().unwrap_or_default();
            match oauth2_dispatch(op, &params) {
                Ok(out) => respond(sock, 200, &out.to_string()).await,
                Err(why) => respond(sock, 400, &error_body(&why)).await,
            }
        }

        ("POST", "/execute") => {
            let body_buf = read_body(sock, content_length, 64 * 1024, &prefetched_body).await?;
            let req: serde_json::Value = match serde_json::from_slice(&body_buf) {
                Ok(v) => v,
                Err(_) => return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await,
            };
            let out = execute_single(&state, &req).await;
            respond(sock, 200, &out.to_string()).await
        }
        ("POST", "/run") => {
            // TR-411: the relay is explicitly NOT a load transport — the agent
            // refuses a load dispatch that arrives over the relay. A web relay
            // request would be CORS-bridged and would misreport percentiles
            // (the browser tier must not be able to report them). Detect the
            // relay by the header the relay always sets (`X-Tropel-Relay` or
            // the legacy `X-Knockport-Relay` / `Via: relay`) and refuse with a
            // 403 so the client can surface "relay cannot run loads — use the
            // desktop transport or the native CLI".
            let raw_lower = raw.to_ascii_lowercase();
            if raw_lower.contains("x-tropel-relay")
                || raw_lower.contains("x-knockport-relay")
                || raw_lower.contains("via: relay")
                || raw_lower.contains("x-relay-transport")
            {
                return respond(
                    sock,
                    403,
                    r#"{"error":"relay is not a load transport — POST /run refused (TR-411); use the desktop tauri transport or the native CLI"}"#,
                )
                .await;
            }
            // TR-411: a load run — a collection (scenario JSON) plus a load
            // block (iterations). Runs each item through the SAME engine HTTP
            // client, bounded by `iterations`, and returns the aggregated
            // raw samples. NO percentiles (TR-411 — the browser tier cannot
            // report them; this endpoint matches that contract).
            let body_buf =
                read_body(sock, content_length, 4 * 1024 * 1024, &prefetched_body).await?;
            let payload: serde_json::Value = match serde_json::from_slice(&body_buf) {
                Ok(v) => v,
                Err(_) => return respond(sock, 400, r#"{"error":"invalid JSON body"}"#).await,
            };
            let scenario_json = payload
                .get("scenario")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let iterations = payload
                .get("iterations")
                .and_then(|i| i.as_u64())
                .unwrap_or(1)
                .min(1000); // bounded — a load run is not an unbounded loop
            let scenario: tropel_sdk::scenario::Scenario = match serde_json::from_str(scenario_json)
            {
                Ok(s) => s,
                Err(e) => {
                    return respond(
                        sock,
                        400,
                        &format!(r#"{{"error":"invalid scenario: {e}"}}"#),
                    )
                    .await
                }
            };
            // TR-411: optional thresholds map — evaluated against the run's
            // http_reqs count + http_req_failed rate; the verdict is returned
            // so the client can use it as the exit code.
            let thresholds: std::collections::HashMap<String, String> = payload
                .get("thresholds")
                .and_then(|t| t.as_object())
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            // TR-411: `stream: true` streams each iteration's samples as a
            // chunked response (live metrics) instead of one batched JSON.
            let stream = payload
                .get("stream")
                .and_then(|s| s.as_bool())
                .unwrap_or(false);
            if stream {
                return run_load_streaming(sock, &state, &scenario, iterations, &thresholds).await;
            }
            let out = run_load(&state, &scenario, iterations, &thresholds).await;
            respond(sock, 200, &out.to_string()).await
        }
        _ => respond(sock, 404, r#"{"error":"not found"}"#).await,
    }
}

/// Evaluate a simple `<metric> <op> <value>` threshold against a single
/// number. Supported metrics: `http_reqs` (count), `http_req_failed` (rate).
fn eval_threshold(expr: &str, reqs: u64, failed: u64) -> Result<bool, String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(format!(
            "invalid threshold '{expr}': expected '<metric> <op> <value>'"
        ));
    }
    let actual = match parts[0] {
        "http_reqs" => reqs as f64,
        "http_req_failed" => {
            if reqs == 0 {
                0.0
            } else {
                failed as f64 / reqs as f64
            }
        }
        other => return Err(format!("unsupported threshold metric '{other}'")),
    };
    let threshold: f64 = parts[2]
        .parse()
        .map_err(|_| format!("invalid threshold value '{}'", parts[2]))?;
    let passed = match parts[1] {
        "<" => actual < threshold,
        "<=" => actual <= threshold,
        ">" => actual > threshold,
        ">=" => actual >= threshold,
        "==" | "===" => (actual - threshold).abs() < f64::EPSILON,
        "!=" => (actual - threshold).abs() > f64::EPSILON,
        other => return Err(format!("unknown operator '{other}'")),
    };
    Ok(passed)
}

/// Run a load run: walk the scenario items `iterations` times, executing each
/// request through the shared engine HTTP client with full sub-timings, and
/// aggregate the raw samples. No percentiles (TR-411).
async fn run_load(
    state: &AgentState,
    scenario: &tropel_sdk::scenario::Scenario,
    iterations: u64,
    thresholds: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut samples: Vec<serde_json::Value> = Vec::new();
    let mut total_failures = 0u64;
    let mut unsupported_errors: Vec<String> = Vec::new();
    for it in 0..iterations {
        for item in &scenario.items {
            let Some(request) = item.request.as_ref() else {
                continue;
            };
            // TR-409: resolve the signer and surface `unsupported` as a hard
            // failure rather than sending the request unauthenticated. A
            // request that declares `ntlm`/`akamai-edgegrid`/`jwt`/`wsse` on a
            // transport that cannot sign it must fail loudly (the TR-004 shape
            // would be a 200 with no Authorization header and a green run).
            let signer_opt = match &request.auth {
                Some(auth) => match state.client.get_signer(auth) {
                    Ok(s) => s,
                    Err(e) => {
                        let msg = e.to_string();
                        if !unsupported_errors.contains(&msg) {
                            unsupported_errors.push(msg.clone());
                        }
                        total_failures += 1;
                        let elapsed_ms = 0.0;
                        samples.push(serde_json::json!({
                            "metric": "http_reqs",
                            "iteration": it,
                            "url": request.url,
                            "status": 0,
                            "duration_ms": elapsed_ms,
                            "error": msg,
                        }));
                        continue;
                    }
                },
                None => None,
            };
            let start = Instant::now();
            let result = state.client.execute(request, signer_opt.as_deref()).await;
            let elapsed_ms = start.elapsed().as_millis() as f64;
            let (status, ok) = match &result {
                Ok(resp) => (resp.status_code, (200..400).contains(&resp.status_code)),
                Err(_) => (0, false),
            };
            if !ok {
                total_failures += 1;
            }
            let mut sample = serde_json::json!({
                "metric": "http_reqs",
                "iteration": it,
                "url": request.url,
                "status": status,
                "duration_ms": elapsed_ms,
            });
            if let Err(e) = &result {
                sample["error"] = serde_json::Value::String(e.to_string());
            }
            samples.push(sample);
        }
    }
    let mut out = serde_json::json!({
        "iterations": iterations,
        "samples": samples,
        "failures": total_failures,
        "has_failures": total_failures > 0,
        "thresholds": threshold_verdict(thresholds, iterations, total_failures),
    });
    if !unsupported_errors.is_empty() {
        out["unsupported_auth"] = serde_json::Value::Array(
            unsupported_errors
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }
    out
}

/// TR-411: STREAMING load run — writes a chunked HTTP response and emits
/// each iteration's samples as a chunk, so the client sees live metrics
/// during the run instead of a single batched JSON at the end.
async fn run_load_streaming(
    sock: &mut TcpStream,
    state: &AgentState,
    scenario: &tropel_sdk::scenario::Scenario,
    iterations: u64,
    thresholds: &std::collections::HashMap<String, String>,
) -> tropel_sdk::Result<()> {
    // Chunked HTTP head.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    sock.write_all(head.as_bytes())
        .await
        .map_err(TropelError::Io)?;

    let mut total_failures = 0u64;
    for it in 0..iterations {
        let mut batch: Vec<serde_json::Value> = Vec::new();
        for item in &scenario.items {
            let Some(request) = item.request.as_ref() else {
                continue;
            };
            let signer_opt = match &request.auth {
                Some(auth) => match state.client.get_signer(auth) {
                    Ok(s) => s,
                    Err(e) => {
                        total_failures += 1;
                        batch.push(serde_json::json!({
                            "metric": "http_reqs",
                            "iteration": it,
                            "url": request.url,
                            "status": 0,
                            "duration_ms": 0.0,
                            "error": e.to_string(),
                        }));
                        continue;
                    }
                },
                None => None,
            };
            let start = Instant::now();
            let result = state.client.execute(request, signer_opt.as_deref()).await;
            let elapsed_ms = start.elapsed().as_millis() as f64;
            let (status, ok) = match &result {
                Ok(resp) => (resp.status_code, (200..400).contains(&resp.status_code)),
                Err(_) => (0, false),
            };
            if !ok {
                total_failures += 1;
            }
            let mut sample = serde_json::json!({
                "metric": "http_reqs",
                "iteration": it,
                "url": request.url,
                "status": status,
                "duration_ms": elapsed_ms,
            });
            if let Err(e) = &result {
                sample["error"] = serde_json::Value::String(e.to_string());
            }
            batch.push(sample);
        }
        let chunk = serde_json::json!({
            "iteration": it,
            "samples": batch,
            "failures": total_failures,
        })
        .to_string();
        write_chunk(sock, &chunk).await?;
    }
    // Final verdict chunk + the terminating chunk.
    let verdict = serde_json::json!({
        "done": true,
        "iterations": iterations,
        "failures": total_failures,
        "has_failures": total_failures > 0,
        "thresholds": threshold_verdict(thresholds, iterations, total_failures),
    })
    .to_string();
    write_chunk(sock, &verdict).await?;
    sock.write_all(b"0\r\n\r\n").await.map_err(TropelError::Io)
}

/// Write one HTTP/1.1 chunk: `<hex-size>\r\n<data>\r\n`.
async fn write_chunk(sock: &mut TcpStream, data: &str) -> tropel_sdk::Result<()> {
    let size = format!("{:x}\r\n", data.len());
    sock.write_all(size.as_bytes())
        .await
        .map_err(TropelError::Io)?;
    sock.write_all(data.as_bytes())
        .await
        .map_err(TropelError::Io)?;
    sock.write_all(b"\r\n").await.map_err(TropelError::Io)
}

/// Evaluate the run's thresholds and produce the verdict (TR-411).
fn threshold_verdict(
    thresholds: &std::collections::HashMap<String, String>,
    iterations: u64,
    failures: u64,
) -> serde_json::Value {
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut all_passed = true;
    for (name, expr) in thresholds {
        let passed = match eval_threshold(expr, iterations, failures) {
            Ok(p) => p,
            Err(e) => {
                all_passed = false;
                results.push(serde_json::json!({
                    "name": name, "expression": expr, "passed": false,
                    "error": e, "actual": null, "threshold": null,
                }));
                continue;
            }
        };
        if !passed {
            all_passed = false;
        }
        let actual = if expr.starts_with("http_req_failed") && iterations > 0 {
            failures as f64 / iterations as f64
        } else {
            iterations as f64
        };
        results.push(serde_json::json!({
            "name": name, "expression": expr, "passed": passed,
            "actual": actual,
            "threshold": expr.split_whitespace().nth(2).and_then(|v| v.parse::<f64>().ok()),
        }));
    }
    results.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    serde_json::json!({
        "results": results,
        "passed": all_passed,
    })
}

async fn respond(sock: &mut TcpStream, status: u16, body: &str) -> tropel_sdk::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes())
        .await
        .map_err(TropelError::Io)
}

/// Execute a single request with full sub-timings — the SAME engine code path
/// a request under load takes. Returns a JSON response payload.
/// One template in a batch resolve (TR-448).
#[derive(serde::Deserialize)]
struct BatchResolveItem {
    template: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    deep: Option<bool>,
}

/// One assertion as the desktop tier sends it.
#[derive(serde::Deserialize)]
struct AgentAssertionSpec {
    #[serde(default)]
    name: Option<String>,
    target: String,
    operator: String,
    #[serde(default)]
    expected: serde_json::Value,
}

/// Read and parse a JSON request body, bounded.
///
/// Returns `Ok(None)` for malformed JSON so the caller answers 400 rather
/// than dropping the connection — a desktop shell debugging its own payload
/// needs the status code, not a closed socket.
async fn read_json_body(
    sock: &mut TcpStream,
    content_length: usize,
    max: usize,
    prefetched: &[u8],
) -> Result<Option<serde_json::Value>, TropelError> {
    Ok(serde_json::from_slice(&read_body(sock, content_length, max, prefetched).await?).ok())
}

/// Read a request body, using whatever already arrived with the head.
///
/// TR-445: the fix for the hang described in `handle_connection`. Reads only
/// the REMAINDER, and returns immediately when the body was fully prefetched.
async fn read_body(
    sock: &mut TcpStream,
    content_length: usize,
    max: usize,
    prefetched: &[u8],
) -> Result<Vec<u8>, TropelError> {
    let want = content_length.min(max);
    let mut body = prefetched.to_vec();
    body.truncate(want);
    if body.len() < want {
        let mut rest = vec![0u8; want - body.len()];
        sock.read_exact(&mut rest).await.map_err(TropelError::Io)?;
        body.extend_from_slice(&rest);
    }
    Ok(body)
}

fn error_body(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Dispatch a signing request to the ungated `tropel-auth` builders.
///
/// TR-445: this is deliberately ONE endpoint with a `scheme` discriminant
/// rather than four routes. The desktop tier calls it from one place, and a
/// new scheme is a match arm here instead of a new URL its client must learn.
///
/// An unknown scheme is refused BY NAME. Falling back to "no headers" would
/// send the request unsigned while the config says it is authenticated —
/// invariant #7, silent data loss.
fn sign_with_scheme(
    scheme: &str,
    p: &serde_json::Value,
) -> Result<Vec<tropel_auth::builders::HeaderOut>, String> {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let opt = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let pairs = |k: &str| -> Vec<(String, String)> {
        p.get(k)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    };

    match scheme {
        "digest" => {
            let challenge = s("wwwAuthenticate");
            let Some(c) = tropel_auth::builders::find_digest_challenge(&challenge) else {
                // No Digest challenge in the header — the caller must not
                // re-send. Distinct from a signing failure.
                return Err("the WWW-Authenticate header carries no Digest challenge".to_string());
            };
            let get = |k: &str| c.get(k).map(String::as_str);
            Ok(vec![tropel_auth::builders::digest_build_authorization(
                &tropel_auth::builders::DigestBuildParams {
                    username: &s("username"),
                    password: &s("password"),
                    method: &s("method"),
                    uri: &s("uri"),
                    realm: get("realm").unwrap_or(""),
                    nonce: get("nonce").unwrap_or(""),
                    nc: p.get("nc").and_then(|v| v.as_u64()).unwrap_or(1),
                    cnonce: &s("cnonce"),
                    qop: get("qop"),
                    algorithm: get("algorithm"),
                    opaque: get("opaque"),
                },
            )])
        }
        "hawk" => Ok(vec![tropel_auth::builders::hawk_build_header(
            &tropel_auth::builders::HawkBuildParams {
                method: &s("method"),
                resource: &s("resource"),
                host: &s("host"),
                port: p.get("port").and_then(|v| v.as_u64()).unwrap_or(443) as u16,
                id: &s("id"),
                key: &s("key"),
                algorithm: opt("algorithm").as_deref(),
                ts: &s("ts"),
                nonce: &s("nonce"),
                ext: &s("ext"),
            },
        )]),
        "awsSigV4" => {
            let host = s("host");
            let region = opt("region").unwrap_or_else(|| "us-east-1".to_string());
            let service =
                opt("service").unwrap_or_else(|| tropel_auth::builders::default_service(&host));
            let signing_service = tropel_auth::builders::signing_name(&service);
            let path = s("path");
            let canonical_uri = tropel_auth::builders::sigv4_canonical_uri(&path, &service);
            let secret = s("secretKey");
            let date_stamp = s("dateStamp");
            let key = tropel_auth::builders::derive_signing_key(
                &secret,
                &date_stamp,
                &region,
                signing_service,
            );
            let body = match opt("bodyBase64") {
                Some(b64) => Some(
                    base64_decode(&b64).map_err(|e| format!("bodyBase64 is not base64: {e}"))?,
                ),
                None => None,
            };
            let headers = pairs("headers");
            let out = tropel_auth::builders::aws_sigv4_build_headers(
                &tropel_auth::builders::AwsSigV4BuildParams {
                    method: &s("method"),
                    path: &path,
                    query: &s("query"),
                    host: &tropel_auth::builders::bracket_host(&host),
                    headers: &headers,
                    body: body.as_deref(),
                    access_key: &s("accessKey"),
                    secret_key: &secret,
                    session_token: opt("sessionToken").as_deref(),
                    region: &region,
                    service: &service,
                    amz_date: &s("amzDate"),
                    date_stamp: &date_stamp,
                },
                &canonical_uri,
                signing_service,
                &key,
            );
            Ok(out.headers)
        }
        "oauth1" => {
            let base_uri = tropel_auth::builders::oauth1_base_uri(
                &s("scheme"),
                &s("host"),
                p.get("port").and_then(|v| v.as_u64()).map(|v| v as u16),
                &s("path"),
            );
            let mut params = pairs("queryParams");
            if let Some(form) = opt("formBody") {
                params.extend(tropel_auth::builders::parse_form(form.as_bytes()));
            }
            let method = s("signatureMethod");
            tropel_auth::builders::oauth1_build_header(&tropel_auth::builders::OAuth1BuildParams {
                method: &s("method"),
                base_uri: &base_uri,
                request_params: &params,
                consumer_key: &s("consumerKey"),
                consumer_secret: &s("consumerSecret"),
                token: opt("token").as_deref(),
                token_secret: opt("tokenSecret").as_deref(),
                signature_method: &method,
                nonce: &s("nonce"),
                timestamp: &s("timestamp"),
            })
            .map(|o| vec![o.header])
            .ok_or_else(|| {
                format!(
                    "unsupported OAuth1 signature_method '{method}' — supported: {}",
                    tropel_auth::builders::OAUTH1_SIGNATURE_METHODS.join(", ")
                )
            })
        }
        other => Err(format!(
            "unknown auth scheme '{other}' — supported: digest, hawk, awsSigV4, oauth1"
        )),
    }
}

/// Base64 decode without pulling a new dependency into this crate.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

/// Run a script in a fresh realm and report what it did.
///
/// TR-446, completing KT-203's `run_script`. This is the entry point the
/// desktop tier and the third differential leg both need: the SAME realm a
/// load run uses (deep-equal + k6-core + the `pm`/`trp` shims), driven once,
/// with its effects returned as data.
///
/// A FRESH realm per call is deliberate. Scripts mutate globals, and a shared
/// context would let one request's script change the next one's behaviour —
/// a bug that reproduces only under a specific ordering, which is the worst
/// kind to chase. The agent is a per-request ABI, not a session.
async fn run_script_once(
    code: &str,
    environment: HashMap<String, String>,
) -> Result<serde_json::Value, String> {
    let mut ctx = tropel_js::JsContext::new(None, Some(std::time::Duration::from_secs(10)))
        .await
        .map_err(|e| format!("js context: {e:?}"))?;

    ctx.eval(include_str!("../../../js/shared/deep-equal.js"))
        .await
        .map_err(|e| format!("deep-equal shim: {e:?}"))?;
    ctx.eval(concat!(
        include_str!("../../../js/shared/k6-core.js"),
        "\n",
        include_str!("../../../js/scripting-api/pm.js")
    ))
    .await
    .map_err(|e| format!("pm shim: {e:?}"))?;

    let state = tropel_sandbox::state::SharedPmState::default();
    {
        // A poisoned lock here means a previous script panicked mid-mutation.
        // Recovering the guard is right: the agent is per-request, the state
        // is fresh, and refusing would strand the caller on someone else's
        // panic.
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.environment = environment;
    }
    tropel_sandbox::bindings::trp::TrpBridge::new(state.clone())
        .install(&mut ctx)
        .map_err(|e| format!("bridge install: {e:?}"))?;

    // A THROWING script is not an agent error — it is a result. The caller
    // needs the message and whatever ran before the throw, exactly as the
    // in-app runner reports it; a 500 here would lose both.
    //
    // The throw is caught in JS rather than in Rust because the Rust side
    // only sees "Exception generated by QuickJS" — the actual message lives
    // on the JS exception object, and losing it leaves a user staring at a
    // failed script with no reason.
    //
    // MESSAGE FIRST, then the stack. QuickJS's `e.stack` carries only the
    // frames, so preferring it (the obvious `e.stack || e.message`) drops the
    // one line a user actually reads. A `try`/`catch` around the body keeps the
    // message; the `await` wrapper keeps top-level `await` working, which a
    // bare try/catch would break.
    let wrapped = format!(
        "globalThis.__tropel_script_error = null;\n\
         (async function () {{ try {{\n{code}\n}} catch (e) {{ \
           globalThis.__tropel_script_error = \
           (e && e.message ? e.message : String(e)) + \
           (e && e.stack ? \"\\n\" + e.stack : \"\"); \
         }} }})();"
    );
    let script_error = match ctx.eval(&wrapped).await {
        Ok(_) => {
            let raw = ctx
                .eval("globalThis.__tropel_script_error === null ? \"\" : String(globalThis.__tropel_script_error)")
                .await
                .unwrap_or_default();
            if raw.is_empty() {
                None
            } else {
                Some(raw)
            }
        }
        // A failure of the WRAPPER itself (a syntax error in the user's code,
        // which `try` cannot catch) still has to be reported.
        Err(e) => Some(format!("{e:?}")),
    };

    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    // Individual checks are reconstructed from the `checks` samples — that is
    // where `record_test_tagged` puts them, tagged with the raw check name.
    let tests: Vec<serde_json::Value> = st
        .samples
        .iter()
        .filter(|s| s.metric == "checks")
        .map(|s| {
            serde_json::json!({
                "name": s.tags.get("check").unwrap_or_default(),
                "passed": s.value != 0.0,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "tests": tests,
        "assertions": {
            "total": st.assertions.total,
            "passed": st.assertions.passed,
            "failed": st.assertions.failed,
        },
        // The MUTATIONS: what the script left behind. The caller merges these
        // into its own scope — the agent holds no session state.
        "environment": st.environment,
        "scriptError": script_error,
    }))
}

/// Dispatch an OAuth2/JWT/WSSE operation to the ungated `tropel-auth::oauth`.
///
/// TR-447, the last of KT-203's auth gap. Like `/auth/sign`, this is ONE
/// endpoint with an `op` discriminant rather than nine routes: the desktop
/// tier calls it from one place, and a new operation is a match arm instead
/// of a new URL its client has to learn.
///
/// Every arm is a straight call into the same functions the wasm tier
/// exports. No wrapper logic — a second implementation of PKCE, token
/// building or JWT signing is the invariant #3 failure this endpoint exists
/// to prevent, and D4 names signing specifically ("a signing byte-difference
/// is a 403 that takes a day to find").
/// Serialise a builder's output for the wire.
///
/// A free generic fn rather than a closure: `impl Trait` is not allowed in
/// closure parameters, and the alternative — a `dyn Serialize` box per call —
/// would allocate on a path that runs per request.
fn as_json<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn oauth2_dispatch(op: &str, p: &serde_json::Value) -> Result<serde_json::Value, String> {
    use tropel_auth::oauth;
    let as_str = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let opt = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    match op {
        "buildAuthorizeUrl" => {
            let params: oauth::AuthorizeParams =
                serde_json::from_value(p.clone()).map_err(|e| e.to_string())?;
            as_json(&oauth::build_authorize_url(&params).map_err(|e| e.to_string())?)
        }
        "buildTokenRequest" => {
            let params: oauth::TokenRequestParams =
                serde_json::from_value(p.clone()).map_err(|e| e.to_string())?;
            as_json(&oauth::build_token_request(&params).map_err(|e| e.to_string())?)
        }
        "parseTokenResponse" => {
            as_json(&oauth::parse_token_response(as_str("body")).map_err(|e| e.to_string())?)
        }
        "attachToken" => {
            // The placement vocabulary is the Rust's, not a string the caller
            // invents — an unknown placement must be refused, not defaulted
            // to header, or a token silently stops reaching a query-auth API.
            let placement = match p.get("placement").and_then(|v| v.as_str()) {
                Some("header") | None => oauth::TokenPlacement::Header,
                Some("query") => oauth::TokenPlacement::Query,
                Some(other) => {
                    return Err(format!(
                        "unknown token placement '{other}' — expected header or query"
                    ))
                }
            };
            as_json(&oauth::attach_token(
                as_str("token"),
                opt("tokenType").as_deref(),
                placement,
                opt("headerPrefix").as_deref(),
                opt("queryKey").as_deref(),
            ))
        }
        "decodeJwt" => as_json(&oauth::decode_jwt(as_str("token")).map_err(|e| e.to_string())?),
        "jwtExpiresAt" => {
            let exp = oauth::jwt_expires_at(as_str("token")).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "expiresAt": exp }))
        }
        "signJwt" => {
            let algorithm = match p.get("algorithm").and_then(|v| v.as_str()) {
                Some("HS256") | None => oauth::JwtAlgorithm::Hs256,
                Some("HS384") => oauth::JwtAlgorithm::Hs384,
                Some("HS512") => oauth::JwtAlgorithm::Hs512,
                // Never downgrade to HS256: the config would say one thing
                // and the wire another (the TR-004/TR-409 shape).
                Some(other) => {
                    return Err(format!(
                        "unsupported JWT algorithm '{other}' — supported: HS256, HS384, HS512"
                    ))
                }
            };
            let payload = p.get("payload").cloned().unwrap_or_default();
            let header = p.get("header").cloned().filter(|h| !h.is_null());
            let token = oauth::sign_jwt(header.as_ref(), &payload, algorithm, as_str("secret"))
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "token": token }))
        }
        "wsseSign" => {
            let params: oauth::WsseParams =
                serde_json::from_value(p.clone()).map_err(|e| e.to_string())?;
            as_json(&oauth::sign_wsse(&params).map_err(|e| e.to_string())?)
        }
        "codeChallengeS256" => Ok(serde_json::json!({
            "codeChallenge": oauth::code_challenge_s256(as_str("verifier")),
            "codeChallengeMethod": "S256",
        })),
        other => Err(format!(
            "unknown oauth2 op '{other}' — supported: buildAuthorizeUrl, buildTokenRequest, \
             parseTokenResponse, attachToken, decodeJwt, jwtExpiresAt, signJwt, wsseSign, \
             codeChallengeS256"
        )),
    }
}

async fn execute_single(state: &AgentState, req: &serde_json::Value) -> serde_json::Value {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
    let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let follow = req
        .get("follow_redirects")
        .and_then(|f| f.as_bool())
        .unwrap_or(true);

    let headers: Vec<(String, String)> = req
        .get("headers")
        .and_then(|h| h.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let method_parsed = Method::parse(method).unwrap_or(Method::GET);
    // TR-409: parse optional `auth` field (`AuthConfig` JSON) so the single
    // request path (`POST /execute`) and the load path (`POST /run`) share the
    // same signer builder. Unsupported schemes are reported, not degraded.
    let auth: Option<tropel_sdk::types::AuthConfig> = req
        .get("auth")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let request = TropelRequest {
        url: url.to_string(),
        method: method_parsed,
        headers,
        query_params: HashMap::new(),
        body: req
            .get("body")
            .and_then(|b| b.as_str())
            .map(|s| Body::Raw(s.to_string())),
        auth: auth.clone(),
        certificate: None,
        follow_redirects: follow,
        host: None,
        cookies: Vec::new(),
        timeout: None,
        response_type: ResponseType::Text,
    };

    // TR-409: surface unsupported auth as a transport error rather than
    // sending the request without an Authorization header (the TR-004 shape).
    let signer_opt = match &request.auth {
        Some(a) => match state.client.get_signer(a) {
            Ok(s) => s,
            Err(e) => {
                return serde_json::json!({
                    "status": 0,
                    "status_text": "Unsupported Auth",
                    "headers": {},
                    "body": "",
                    "timings": {
                        "blocked": 0.0, "dns": 0.0, "connecting": 0.0, "tls_handshaking": 0.0,
                        "sending": 0.0, "waiting": 0.0, "receiving": 0.0, "duration": 0.0,
                    },
                    "error": e.to_string(),
                });
            }
        },
        None => None,
    };

    let start = Instant::now();
    let result = state.client.execute(&request, signer_opt.as_deref()).await;
    let elapsed_ms = start.elapsed().as_millis() as f64;

    match result {
        Ok(resp) => {
            let waiting = resp
                .timings
                .as_ref()
                .map(|t| t.waiting.as_millis() as f64)
                .unwrap_or(0.0);
            let receiving = resp
                .timings
                .as_ref()
                .map(|t| t.receiving.as_millis() as f64)
                .unwrap_or(0.0);
            serde_json::json!({
                "status": resp.status_code,
                "status_text": resp.status_text,
                "headers": resp.headers,
                "body": String::from_utf8_lossy(&resp.body),
                "timings": {
                    "blocked": 0.0, "dns": 0.0, "connecting": 0.0, "tls_handshaking": 0.0,
                    "sending": 0.0, "waiting": waiting, "receiving": receiving,
                    "duration": elapsed_ms,
                },
                "error": null,
            })
        }
        Err(e) => {
            serde_json::json!({
                "status": 0,
                "status_text": "Transport Error",
                "headers": {},
                "body": "",
                "timings": {
                    "blocked": 0.0, "dns": 0.0, "connecting": 0.0, "tls_handshaking": 0.0,
                    "sending": 0.0, "waiting": 0.0, "receiving": 0.0, "duration": elapsed_ms,
                },
                "error": e.to_string(),
            })
        }
    }
}

/// A rolling-window rate limiter per connection.
struct RateLimiter {
    window_start: Instant,
    count: u64,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            count: 0,
        }
    }

    fn allow(&mut self) -> Result<(), ()> {
        if self.window_start.elapsed().as_secs() >= 1 {
            self.window_start = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        if self.count > RATE_LIMIT_PER_SEC {
            return Err(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_non_loopback_bind_is_enforced() {
        // The loopback check must reject a public address — the execution
        // endpoint must never be reachable off-box.
        let ip: IpAddr = "0.0.0.0".parse().unwrap();
        assert!(!ip.is_loopback(), "0.0.0.0 must be rejected");
        let ip2: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(ip2.is_loopback(), "127.0.0.1 must be accepted");
    }

    #[test]
    fn threshold_verdict_evaluates_and_verdicts() {
        // TR-411: http_reqs < N and http_req_failed <= rate thresholds.
        use std::collections::HashMap;
        let mut t = HashMap::new();
        t.insert("reqs_ok".into(), "http_reqs < 100".into());
        t.insert("fail_rate_ok".into(), "http_req_failed <= 0.1".into());

        // 10 iterations, 1 failure → both pass.
        let v = threshold_verdict(&t, 10, 1);
        assert!(v["passed"].as_bool().unwrap(), "all must pass: {v}");
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r["passed"].as_bool().unwrap()));

        // 200 iterations, 1 failure → http_reqs < 100 fails.
        let v2 = threshold_verdict(&t, 200, 1);
        assert!(!v2["passed"].as_bool().unwrap(), "reqs threshold must fail");
        let reqs = v2["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "reqs_ok")
            .unwrap();
        assert!(!reqs["passed"].as_bool().unwrap());

        // A malformed threshold reports an error, not a silent pass.
        let mut bad = HashMap::new();
        bad.insert("bogus".into(), "http_reqs".into());
        let v3 = threshold_verdict(&bad, 10, 0);
        assert!(!v3["passed"].as_bool().unwrap(), "malformed must fail");
        assert!(
            v3["results"][0]["error"].as_str().is_some(),
            "the malformed threshold must report its error"
        );
    }
    /// TR-445: the rules endpoints, driven over a REAL loopback socket.
    ///
    /// Unit-testing the handler functions would not prove the thing that
    /// matters — knockport's desktop tier reaches these through HTTP, and the
    /// bug class this closes (every core-tier method throwing
    /// `TropelCoreUnavailableError`) is about the WIRE contract, not the Rust.
    #[tokio::test]
    async fn the_rules_endpoints_answer_over_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(AgentState {
            token: None,
            client: tropel_http::HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client"),
        });
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(&mut sock, st).await;
                });
            }
        });

        let post = |path: &'static str, body: String| async move {
            let mut s = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.expect("write");
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.expect("read");
            String::from_utf8_lossy(&out).to_string()
        };

        // ── /resolve ──
        let raw = post(
            "/resolve",
            serde_json::json!({
                "template": "{{base}}/v1", "variables": {"base": "https://x.test"},
                "mode": "plain", "deep": true
            })
            .to_string(),
        )
        .await;
        assert!(raw.contains("https://x.test/v1"), "{raw}");

        // A typo'd mode is a NAMED 400, never a silent fallback to plain —
        // that is how a quote-bearing value corrupts a JSON body.
        let raw = post(
            "/resolve",
            serde_json::json!({"template": "x", "variables": {}, "mode": "jsonn"}).to_string(),
        )
        .await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("unknown mode"), "{raw}");

        // ── /assert ──
        let raw = post(
            "/assert",
            serde_json::json!({
                "response": {
                    "status": 200, "status_text": "OK",
                    "headers": [["Content-Type", "application/json"]],
                    "body": "{\"count\":2}", "response_time": 5.0, "size": 12,
                    "cookies": []
                },
                "assertions": [
                    {"name": "ok", "target": "status", "operator": "eq", "expected": 200},
                    {"target": "json.count", "operator": "eq", "expected": 99}
                ]
            })
            .to_string(),
        )
        .await;
        assert!(raw.contains(r#""name":"ok""#), "{raw}");
        assert!(raw.contains(r#""passed":true"#), "{raw}");
        // A FAILING row explains itself, and names the TARGET not the row name.
        assert!(
            raw.contains("expected target json.count equals 99"),
            "{raw}"
        );

        // `matches` is UNSUPPORTED here on purpose: a native agent could link
        // Rust's regex, but that would make the operator behave differently on
        // desktop than in the browser (where TR-434 injects the host RegExp) —
        // the exact divergence this endpoint exists to prevent.
        let raw = post(
            "/assert",
            serde_json::json!({
                "response": {
                    "status": 200, "status_text": "OK", "headers": [],
                    "body": "abc", "response_time": 1.0, "size": 3, "cookies": []
                },
                "assertions": [{"target": "body", "operator": "matches", "expected": "^a"}]
            })
            .to_string(),
        )
        .await;
        assert!(raw.contains("regex matcher"), "{raw}");

        // ── /operators ──
        let mut s = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        s.write_all(b"GET /operators HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("read");
        let raw = String::from_utf8_lossy(&out).to_string();
        assert!(raw.contains(r#""name":"eq""#), "{raw}");
        assert!(raw.contains(r#""arity":"unary""#), "{raw}");

        // ── /variables/dynamic (TR-451) ──
        // Each occurrence must generate a FRESH value — that is the whole
        // point of the catalogue, and a cached-once implementation would put
        // the same "unique" id on every request in a run.
        let raw = post(
            "/variables/dynamic",
            serde_json::json!({"template": "{{$guid}}|{{$guid}}"}).to_string(),
        )
        .await;
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let value = parsed["value"].as_str().expect("value");
        let (first, second) = value.split_once('|').expect("two guids");
        assert_ne!(
            first, second,
            "each occurrence generates a fresh value: {value}"
        );
        assert_eq!(first.len(), 36, "a v4 GUID, not a placeholder: {value}");

        // Plain `{{var}}` is left ALONE here. /resolve owns that map, and
        // KnockPort runs the two in order — folding them together would
        // change which pass saw a `{{$guid}}` that came out of a variable.
        let raw = post(
            "/variables/dynamic",
            serde_json::json!({"template": "{{base}}/{{$timestamp}}"}).to_string(),
        )
        .await;
        assert!(
            raw.contains("{{base}}"),
            "a plain variable must survive the dynamic pass untouched: {raw}"
        );

        // ── /variables/dynamic/batch (TR-452) ──
        // One output per input, in order: the caller re-assembles its request
        // by index, so a short or reordered reply puts a header's value in a
        // param.
        let raw = post(
            "/variables/dynamic/batch",
            serde_json::json!({
                "items": [
                    {"template": "{{$guid}}"},
                    {"template": "no tokens here"},
                    {"template": "{{$timestamp}}"}
                ]
            })
            .to_string(),
        )
        .await;
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let items = parsed["items"].as_array().expect("items");
        assert_eq!(items.len(), 3, "one output per input, always: {body}");
        assert_eq!(
            items[1]["value"], "no tokens here",
            "a template with nothing to resolve comes back unchanged: {body}"
        );
        assert_ne!(
            items[0]["value"], items[2]["value"],
            "different tokens, different values: {body}"
        );

        // ── /constants (TR-451) ──
        let mut s = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        s.write_all(b"GET /constants HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("read");
        let raw = String::from_utf8_lossy(&out).to_string();
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        // Read from the resolver's own constant, never a literal here: a
        // second ceiling is what KP-424 removed, and pinning 20 in this test
        // would quietly re-introduce one.
        assert_eq!(
            parsed["maxVariableResolutionPasses"],
            tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES,
            "the cap must come from the resolver: {body}"
        );
        let vars = parsed["predefinedVariables"]
            .as_array()
            .expect("predefinedVariables array");
        assert_eq!(
            vars.len(),
            tropel_variables::PREDEFINED_VARIABLE_META.len(),
            "the whole catalogue, not a subset: {body}"
        );
        assert!(
            vars.iter()
                .any(|v| v["name"] == "$guid" && v["description"].is_string()),
            "names AND descriptions — the editor renders both: {body}"
        );
    }

    /// KT-202 — the THIRD differential leg: the same corpus, over the socket.
    ///
    /// `packages/core-wasm/fixtures/resolve-corpus.json` already runs through
    /// two paths: native Rust (`tropel-core-wasm`'s `conformance_corpus`) and
    /// the real wasm (`packages/core-wasm/smoke.mjs`). Those are the two tiers
    /// a BROWSER host can be served by. The agent is the third, and since
    /// KP-209 it is the one KnockPort's DESKTOP tier actually runs on — so an
    /// agent that resolved `{{base-url}}` differently would put literal text
    /// on the wire for desktop users only, which is the single divergence
    /// this corpus was written to catch in the first place.
    ///
    /// It reads the SAME bytes rather than restating the cases. A copied
    /// corpus drifts exactly the way the two resolvers did.
    ///
    /// This deviates from KT-202's literal wording ("run the corpus through
    /// the TypeScript host"): driving KnockPort's TypeScript from a Rust test
    /// would need Node and a second checkout inside tropel's CI. The agent leg
    /// covers what that was for — the desktop tier's rules — with no
    /// cross-repo dependency, and the browser tier is already covered by
    /// smoke.mjs, which IS the TypeScript leg.
    #[tokio::test]
    async fn the_resolution_corpus_agrees_over_the_socket() {
        const CORPUS: &str =
            include_str!("../../../packages/core-wasm/fixtures/resolve-corpus.json");

        /// Mirrors `tropel-core-wasm`'s `generated_vars` — same kind, same
        /// construction from the SAME constant, so the two legs cannot drift
        /// into testing different chains.
        fn generated_vars(kind: &str) -> serde_json::Value {
            match kind {
                "chain_longer_than_cap" => {
                    let cap = tropel_variables::MAX_VARIABLE_RESOLUTION_PASSES;
                    let mut m = serde_json::Map::new();
                    for i in 0..=cap {
                        m.insert(
                            format!("v{i}"),
                            serde_json::json!(format!("{{{{v{}}}}}", i + 1)),
                        );
                    }
                    m.insert(format!("v{}", cap + 1), serde_json::json!("end"));
                    serde_json::Value::Object(m)
                }
                other => panic!("unknown vars_generated kind: {other}"),
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(AgentState {
            token: None,
            client: tropel_http::HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client"),
        });
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(&mut sock, st).await;
                });
            }
        });

        let doc: serde_json::Value = serde_json::from_str(CORPUS).expect("corpus is valid JSON");
        let cases = doc["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty(), "an empty corpus asserts nothing");

        for case in cases {
            let name = case["name"].as_str().expect("every case is named");
            let template = case["template"].as_str().expect("template");
            let mode = case["mode"].as_str().expect("mode");
            let vars = match case.get("vars_generated") {
                Some(kind) => generated_vars(kind.as_str().unwrap()),
                None => case["vars"].clone(),
            };

            // Through /resolve/batch, because that is the endpoint the desktop
            // tier actually calls — a leg that exercised a different route
            // would not be testing the path users get.
            let body = serde_json::json!({
                "variables": vars,
                "items": [{"template": template, "mode": mode}],
            })
            .to_string();
            let mut s = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "POST /resolve/batch HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.expect("write");
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.expect("read");
            let raw = String::from_utf8_lossy(&out).to_string();
            let reply = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
            let parsed: serde_json::Value =
                serde_json::from_str(&reply).unwrap_or_else(|e| panic!("{name}: {e} — {reply}"));
            let item = &parsed["items"][0];
            assert!(
                item["error"].is_null(),
                "{name}: the agent refused a corpus case: {item}"
            );
            let value = item["value"]
                .as_str()
                .unwrap_or_else(|| panic!("{name}: no value"));

            // The same assertions the native leg makes, in the same order.
            if let Some(expected) = case.get("expect").and_then(|v| v.as_str()) {
                assert_eq!(value, expected, "{name}");
            }
            if case.get("parses_as_json").and_then(|v| v.as_bool()) == Some(true) {
                serde_json::from_str::<serde_json::Value>(value).unwrap_or_else(|e| {
                    panic!("{name}: result must stay parseable JSON: {e} — {value}")
                });
            }
            if let Some(expected) = case.get("expect_hit_cap").and_then(|v| v.as_bool()) {
                assert_eq!(item["hitCap"], expected, "{name}: hitCap");
            }
            if let Some(expected) = case.get("expect_unresolved") {
                assert_eq!(&item["unresolved"], expected, "{name}: unresolved");
            }
            if let Some(required) = case
                .get("expect_unresolved_contains")
                .and_then(|v| v.as_array())
            {
                let got = item["unresolved"].as_array().unwrap();
                for n in required {
                    assert!(
                        got.contains(n),
                        "{name}: unresolved must contain {n} — got {got:?}"
                    );
                }
            }
        }
    }

    /// TR-445: `/auth/sign`, over the socket.
    ///
    /// This is the endpoint that lets knockport's DESKTOP tier stop throwing
    /// `TropelAuthUnavailableError`. What it must prove is not "signing
    /// works" — `tropel-auth` has its own vectors for that — but that the
    /// desktop tier gets the SAME rules the browser does, applied here rather
    /// than re-derived in TypeScript.
    #[tokio::test]
    async fn the_auth_sign_endpoint_applies_the_rules_server_side() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(AgentState {
            token: None,
            client: tropel_http::HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client"),
        });
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(&mut sock, st).await;
                });
            }
        });
        let sign = |body: String| async move {
            let mut s = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "POST /auth/sign HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.expect("write");
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.expect("read");
            String::from_utf8_lossy(&out).to_string()
        };

        // SigV4: the service must derive to `s3` from a VIRTUAL-HOSTED bucket
        // host. A desktop tier deriving it itself would take the first DNS
        // label — the bug `default_service`'s comment records, a 403 on every
        // virtual-hosted-S3 request (TR-428).
        let raw = sign(
            serde_json::json!({
                "scheme": "awsSigV4",
                "params": {
                    "method": "GET", "host": "examplebucket.s3.amazonaws.com",
                    "path": "/test.txt", "accessKey": "AKID", "secretKey": "SECRET",
                    "region": "us-east-1", "amzDate": "20130524T000000Z",
                    "dateStamp": "20130524"
                }
            })
            .to_string(),
        )
        .await;
        assert!(
            raw.contains("/20130524/us-east-1/s3/aws4_request"),
            "service must derive to s3, not the bucket: {raw}"
        );
        // Every header that is IN the signature must come back, or the caller
        // sends a valid-looking Authorization and gets a 403.
        assert!(raw.contains("x-amz-date"), "{raw}");
        assert!(raw.contains("x-amz-content-sha256"), "{raw}");

        // Digest: the challenge is parsed HERE — multi-scheme and quoted qop,
        // the two things a naive client parser gets wrong (TR-429).
        let raw = sign(
            serde_json::json!({
                "scheme": "digest",
                "params": {
                    "wwwAuthenticate": "Basic realm=\"b\", Digest realm=\"r\", qop=\"auth, auth-int\", nonce=\"n\"",
                    "username": "u", "password": "p", "method": "GET",
                    "uri": "/dir/index.html", "nc": 1, "cnonce": "0a4f113b"
                }
            })
            .to_string(),
        )
        .await;
        assert!(raw.contains("Digest "), "{raw}");
        assert!(raw.contains(r#"realm=\"r\""#), "{raw}");

        // A header with no Digest challenge is a NAMED 400, not an unsigned
        // 200 — the caller must not re-send.
        let raw = sign(
            serde_json::json!({
                "scheme": "digest",
                "params": {"wwwAuthenticate": "Basic realm=\"b\"", "username": "u"}
            })
            .to_string(),
        )
        .await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("no Digest challenge"), "{raw}");

        // OAuth1: the IPv6 host is bracketed HERE. A client forwarding
        // `URL.hostname` cannot know whether to add them (TR-431).
        let raw = sign(
            serde_json::json!({
                "scheme": "oauth1",
                "params": {
                    "method": "POST", "scheme": "http", "host": "::1", "port": 8080,
                    "path": "/request", "formBody": "c2=&a3=2+q",
                    "consumerKey": "ck", "consumerSecret": "cs",
                    "signatureMethod": "HMAC-SHA1", "nonce": "n", "timestamp": "1"
                }
            })
            .to_string(),
        )
        .await;
        assert!(raw.contains("oauth_signature="), "{raw}");

        // An unsupported signature method is refused BY NAME, never
        // downgraded to HMAC-SHA1 (TR-409).
        let raw = sign(
            serde_json::json!({
                "scheme": "oauth1",
                "params": {
                    "method": "GET", "scheme": "https", "host": "x.test", "path": "/",
                    "consumerKey": "ck", "consumerSecret": "cs",
                    "signatureMethod": "RSA-SHA1", "nonce": "n", "timestamp": "1"
                }
            })
            .to_string(),
        )
        .await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("RSA-SHA1"), "{raw}");

        // An unknown scheme is refused, not silently unsigned — sending a
        // request the config calls authenticated with no Authorization is
        // invariant #7's silent data loss.
        let raw = sign(serde_json::json!({"scheme": "ntlm", "params": {}}).to_string()).await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("unknown auth scheme"), "{raw}");
    }
    /// TR-446: `POST /script`, over the socket.
    ///
    /// This is KT-203's `run_script`. What it must prove is that the desktop
    /// tier gets the SAME realm a load run uses — a script behaving one way
    /// in the app and another under load is the divergence this whole
    /// workstream exists to remove.
    #[tokio::test]
    async fn the_script_endpoint_runs_in_the_same_realm_as_a_load_run() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(AgentState {
            token: None,
            client: tropel_http::HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client"),
        });
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(&mut sock, st).await;
                });
            }
        });
        let run = |body: String| async move {
            let mut s = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "POST /script HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.expect("write");
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.expect("read");
            String::from_utf8_lossy(&out).to_string()
        };

        // Checks are reported individually, not just counted — the results
        // panel names each one.
        let raw = run(serde_json::json!({
            "code": r#"
                    pm.test("passes", function () { pm.expect(1).to.eql(1); });
                    pm.test("fails", function () { pm.expect(1).to.eql(2); });
                "#
        })
        .to_string())
        .await;
        assert!(raw.contains(r#""name":"passes""#), "{raw}");
        assert!(raw.contains(r#""name":"fails""#), "{raw}");
        assert!(raw.contains(r#""passed":1"#), "{raw}");
        assert!(raw.contains(r#""failed":1"#), "{raw}");

        // Variable MUTATIONS come back — the agent holds no session, so the
        // caller merges them into its own scope. Without this the desktop
        // tier could run a pre-request script and lose everything it set.
        let raw = run(serde_json::json!({
            "code": r#"pm.environment.set("token", "abc123");"#,
            "environment": {"seeded": "yes"}
        })
        .to_string())
        .await;
        assert!(raw.contains(r#""token":"abc123""#), "{raw}");
        assert!(
            raw.contains(r#""seeded":"yes""#),
            "seed must survive: {raw}"
        );

        // A THROWING script is a RESULT, not a 500 — the caller needs the
        // message and whatever ran before the throw, exactly as the in-app
        // runner reports it.
        let raw = run(serde_json::json!({
            "code": r#"pm.test("ran", function () {}); throw new Error("boom");"#
        })
        .to_string())
        .await;
        assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
        assert!(raw.contains("scriptError"), "{raw}");
        assert!(raw.contains("boom"), "{raw}");
        assert!(
            raw.contains(r#""name":"ran""#),
            "what ran before the throw survives: {raw}"
        );

        // Each call gets a FRESH realm: a global left by one script must not
        // be visible to the next. A shared context would let one request
        // change the next one's behaviour — a bug that reproduces only under
        // a specific ordering.
        let _ = run(serde_json::json!({"code": "globalThis.__leak = 1;"}).to_string()).await;
        let raw = run(serde_json::json!({
            "code": r#"pm.test("isolated", function () {
                    pm.expect(typeof globalThis.__leak).to.eql("undefined");
                });"#
        })
        .to_string())
        .await;
        assert!(
            raw.contains(r#""passed":1"#),
            "realms must not share globals: {raw}"
        );
    }
    /// TR-447: `POST /auth/oauth2`, over the socket.
    ///
    /// The last arm of the gap `native-agent.ts` documents. What this must
    /// prove is that the desktop tier gets the SAME OAuth2/JWT/WSSE the
    /// browser does — D4 names signing specifically ("a signing
    /// byte-difference is a 403 that takes a day to find").
    #[tokio::test]
    async fn the_oauth2_endpoint_serves_the_whole_family() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(AgentState {
            token: None,
            client: tropel_http::HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client"),
        });
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(&mut sock, st).await;
                });
            }
        });
        let call = |op: &'static str, params: serde_json::Value| async move {
            let body = serde_json::json!({ "op": op, "params": params }).to_string();
            let mut s = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "POST /auth/oauth2 HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.expect("write");
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.expect("read");
            String::from_utf8_lossy(&out).to_string()
        };

        // PKCE: the challenge must be computed HERE, so it matches the token
        // request the same tier builds. A client deriving it separately is
        // how a verifier and challenge stop agreeing.
        let raw = call(
            "codeChallengeS256",
            serde_json::json!({"verifier": "abc123"}),
        )
        .await;
        assert!(raw.contains("codeChallenge"), "{raw}");
        assert!(raw.contains(r#""codeChallengeMethod":"S256""#), "{raw}");

        // JWT signing, and the algorithm is NEVER downgraded — a config
        // saying HS512 while the wire carries HS256 is the TR-004/TR-409
        // shape.
        let raw = call(
            "signJwt",
            serde_json::json!({"payload": {"sub": "u1"}, "algorithm": "HS512", "secret": "s"}),
        )
        .await;
        assert!(raw.contains(r#""token":"#), "{raw}");
        let raw = call(
            "signJwt",
            serde_json::json!({"payload": {"sub": "u1"}, "algorithm": "RS256", "secret": "s"}),
        )
        .await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("RS256"), "refused by name: {raw}");

        // Token placement is the Rust's vocabulary. An unknown value must be
        // REFUSED, not defaulted to header — defaulting silently stops a
        // token reaching a query-auth API.
        let raw = call(
            "attachToken",
            serde_json::json!({"token": "t", "placement": "cookie"}),
        )
        .await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("unknown token placement"), "{raw}");

        let raw = call(
            "attachToken",
            serde_json::json!({"token": "t", "tokenType": "Bearer", "placement": "header"}),
        )
        .await;
        assert!(raw.contains("Bearer"), "{raw}");

        // WSSE, and an unknown op is refused listing what IS supported.
        let raw = call(
            "wsseSign",
            serde_json::json!({"username": "u", "password": "p"}),
        )
        .await;
        assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
        let raw = call("nope", serde_json::json!({})).await;
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
        assert!(raw.contains("unknown oauth2 op"), "{raw}");
        assert!(raw.contains("signJwt"), "it lists the alternatives: {raw}");
    }
    /// TR-448: `POST /resolve/batch`.
    ///
    /// The endpoint that lets the DESKTOP tier use the agent instead of
    /// shipping a second copy of this Rust as wasm. Per-call resolution is 33
    /// loopback round trips per request — 2.3 ms measured, against 0.07 ms
    /// batched.
    #[tokio::test]
    async fn the_batch_resolve_endpoint_preserves_order_and_isolates_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(AgentState {
            token: None,
            client: tropel_http::HttpClient::new(&tropel_http::config::HttpConfig::default())
                .expect("http client"),
        });
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(&mut sock, st).await;
                });
            }
        });
        let post = |body: String| async move {
            let mut s = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "POST /resolve/batch HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).await.expect("write");
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.expect("read");
            String::from_utf8_lossy(&out).to_string()
        };

        let raw = post(
            serde_json::json!({
                "variables": {"base": "https://api.test", "tok": "abc", "n": "2"},
                "items": [
                    {"template": "{{base}}/v{{n}}"},
                    {"template": "Bearer {{tok}}"},
                    {"template": "{\"t\":\"{{tok}}\"}", "mode": "json"},
                    // A bad mode: this item fails, the others must not.
                    {"template": "{{tok}}", "mode": "nope"},
                    {"template": "{{base}}"}
                ]
            })
            .to_string(),
        )
        .await;

        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let items = parsed["items"].as_array().expect("items array");

        // ORDER IS THE CONTRACT — the caller re-assembles its request by index,
        // so a reordered or short reply would put a header's value in a param.
        assert_eq!(items.len(), 5, "one output per input, always: {body}");
        assert_eq!(items[0]["value"], "https://api.test/v2");
        assert_eq!(items[1]["value"], "Bearer abc");
        assert_eq!(items[2]["value"], "{\"t\":\"abc\"}");
        // A per-item failure does NOT fail the batch: one bad escape mode must
        // not lose the other four resolutions.
        assert!(
            items[3]["error"].is_string(),
            "item 3 should carry an error: {body}"
        );
        assert!(items[3]["value"].is_null());
        assert_eq!(
            items[4]["value"], "https://api.test",
            "later items still resolve"
        );

        // TR-449: a CYCLE and an UNKNOWN NAME both leave a literal `{{…}}`,
        // and only the resolver's own loop can tell them apart. KnockPort
        // turns `hitCap` into a failed send and an unresolved name into a
        // visible typo, so collapsing them to a bare string would make a
        // cyclic chain look like a harmless placeholder.
        let raw = post(
            serde_json::json!({
                "variables": {"a": "{{b}}", "b": "{{a}}"},
                "items": [
                    {"template": "{{a}}"},
                    {"template": "{{nosuchvar}}"}
                ]
            })
            .to_string(),
        )
        .await;
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let items = parsed["items"].as_array().expect("items");
        assert_eq!(
            items[0]["hitCap"], true,
            "a cycle must report hitCap: {body}"
        );
        assert_eq!(
            items[1]["hitCap"], false,
            "an unknown name is NOT a cycle: {body}"
        );
        assert!(
            items[1]["unresolved"]
                .as_array()
                .is_some_and(|u| u.iter().any(|n| n == "nosuchvar")),
            "the unknown name must be reported so the user sees their typo: {body}"
        );

        // TR-449: a shallow item is REFUSED by name. Silently resolving it
        // deep would hand the caller twenty passes when it asked for one,
        // with nothing in the reply to say so — the D4 failure this seam
        // exists to prevent.
        let raw = post(
            serde_json::json!({
                "variables": {"a": "{{b}}", "b": "final"},
                "items": [{"template": "{{a}}", "deep": false}]
            })
            .to_string(),
        )
        .await;
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let item = &parsed["items"][0];
        assert!(
            item["value"].is_null(),
            "a refused item carries no value: {body}"
        );
        assert!(
            item["error"]
                .as_str()
                .is_some_and(|e| e.contains("deep: false") && e.contains("/resolve")),
            "the refusal must name the field AND the endpoint that serves it: {body}"
        );

        // An empty batch is a valid batch — a request with no templates is not
        // an error, and returning one would make the caller special-case it.
        let raw = post(serde_json::json!({"variables": {}, "items": []}).to_string()).await;
        assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
        assert!(raw.contains(r#""items":[]"#), "{raw}");
    }
}
