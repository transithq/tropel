//! Minimal runtime control API — k6 REST `/v1/status` parity.
//!
//! Binds `127.0.0.1:<port>` and serves:
//! - `GET  /v1/status`   → k6 JSON:API shape
//!   `{"data":{"type":"status","id":"default","attributes":{...}}}`
//!   (vus / vus-max / paused / running / stopped / tainted)
//! - `PATCH /v1/status` → k6 envelope `{"data":{"attributes":{...}}}` or a
//!   flat `{"vus":N,"max":M,"vus-max":N,"paused":bool}` — adjusts the
//!   externally-controlled scheduler's VU pool / pause state at runtime.
//!   `max` is clamped to the configured `max_vus` ceiling, so a client can
//!   never grow the pool past the run's cap. Both `max` (legacy) and
//!   `vus-max` (k6's JSON:API field name) are accepted, and the status doc
//!   emits BOTH so old and new clients work.
//! - `POST /v1/stop` → stop the run (k6 extension — also achievable via
//!   `PATCH /v1/status {"stopped":true}`)
//! - `PATCH /v1/stop` → k6 envelope `{"data":{"attributes":{"stopped":true}}}`
//!   — same as the PATCH /v1/status path, but on the `/v1/stop` route.
//! - `GET /v1/metrics` → k6 JSON:API envelope of current metric values
//! - `GET /v1/groups`  → k6 JSON:API envelope of the group hierarchy
//! - `GET /v1/setup`   → the current setup data (or null)
//! - `PUT /v1/setup`   → set the setup data from the request body
//! - `POST /v1/setup`  → run the script's setup() and return the result
//! - `POST /v1/teardown` → run the script's teardown()
//!
//! Everything else returns 404. This is intentionally dependency-free: a
//! hand-rolled HTTP/1.1 reader keeps the control surface small and avoids
//! pulling a web framework into the engine for one endpoint.
//!
//! ## SUPERSET (k6 v2 divergence)
//! k6 v2 turns the REST API **off by default** (`GlobalFlags.Address` → `""`).
//! Tropel serves it whenever a `--control-port` is configured (for any executor,
//! not just `externally-controlled`) — a deliberate SUPERSET so integrators
//! (knockport, scripts, or platform operators) can always inspect a live run
//! without reconfiguring the executor type. The `HEADER_LINE_LEN` / `BODY_SIZE`
//! caps are the bounds that keep this safe (TR-604).

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tropel_metrics::collector::{MetricsCollector, MetricsSnapshot, SeriesSnapshot};
use tropel_scheduler::VUScheduler;
use tropel_sdk::{Result, TropelError};

/// Ceiling for a control request body. A status patch is a few hundred
/// bytes; `Content-Length` is attacker-controlled, so without this cap a
/// hostile `Content-Length: 68719476736` forced a 64 GiB `Vec::resize`
/// (backlog line 164).
const MAX_BODY_SIZE: usize = 64 * 1024;
/// Backlog line 256: max length for a single header line (including
/// request line). Without this, a hostile client can send a multi-GB
/// single line that exhausts memory before the body ceiling kicks in.
const MAX_HEADER_LINE_LEN: usize = 8 * 1024;
/// Per-connection read timeout — a client that stalls mid-request must not
/// hold its handler (and a connection slot) forever (backlog line 164).
const CONN_TIMEOUT: Duration = Duration::from_secs(10);
/// Concurrent-connection cap — prevents unbounded handler tasks under a
/// connection flood (backlog line 164).
const MAX_CONNS: usize = 8;
/// Backoff after an accept error (e.g. EMFILE under fd exhaustion) — a
/// tight accept-error loop otherwise spins one core at 100% CPU while the
/// fd shortage persists (backlog line 164).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Shared state for the control API: the scheduler (mutable run state) and
/// read-only handles used by the read-only routes.
pub struct ControlApiState {
    pub scheduler: Arc<VUScheduler>,
    /// Live metric collector — `/v1/metrics` reads its current snapshot.
    pub metrics: Arc<MetricsCollector>,
    /// Current setup data (`None` before setup runs / when the script declares
    /// none). Written by the engine after setup() and by `PUT /v1/setup`.
    pub setup_data: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    /// Scenario name (k6's `group` root path is derived from the run context;
    /// we expose the scenario name as the top-level group label).
    pub scenario_name: String,
}

/// Handle the control server task. Runs until the listener errors or the
/// task is aborted by the scenario finishing.
pub async fn serve_control_api(port: u16, state: ControlApiState) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    // Bind failure must be visible: the spawned task's JoinHandle is only
    // aborted (never awaited) by the engine, so a port conflict would
    // otherwise leave the run silently without a control API.
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("control API: failed to bind {}: {}", addr, e);
            return Err(tropel_sdk::TropelError::Config(format!(
                "control API: failed to bind {}: {}",
                addr, e
            )));
        }
    };
    tracing::info!("Control API listening on http://{addr}");

    // Connection cap: at most MAX_CONNS handlers run concurrently; a flood
    // past the cap is refused (503) instead of spawning unbounded tasks.
    let conn_permits = Arc::new(Semaphore::new(MAX_CONNS));
    let state = Arc::new(state);

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // Back off: a persistent accept error (EMFILE under fd
                // exhaustion) would otherwise spin the accept loop at 100%
                // CPU until the shortage clears (backlog line 164).
                tracing::debug!("control API: accept error: {}; backing off", e);
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        // `try_acquire_owned` CONSUMES the Arc (returns an owned permit), so
        // clone per iteration — the original survives for the next accept.
        let permit = match conn_permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // Cap reached — refuse with 503 instead of queueing the
                // connection forever.
                let mut out = stream;
                let _ = out
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            // The permit is held for the whole connection lifetime, so the
            // cap counts live handlers, not just accepted sockets.
            let _permit = permit;
            if let Err(e) = handle_conn(stream, &state).await {
                tracing::debug!("control API: connection error: {}", e);
            }
        });
    }
}

/// Serve one HTTP connection, bounded by a read timeout (a stalled client
/// must not hold its handler — and its connection slot — forever).
async fn handle_conn<S>(stream: S, state: &Arc<ControlApiState>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Read timeout: without it a client that sends the request line but
    // never finishes headers/body parks the handler (and one of MAX_CONNS
    // slots) indefinitely (backlog line 164).
    match tokio::time::timeout(CONN_TIMEOUT, serve_request(stream, state)).await {
        Ok(r) => r,
        Err(_elapsed) => {
            tracing::debug!("control API: connection timed out");
            Ok(())
        }
    }
}

/// Read one request (line + headers + body), route it, write the response.
async fn serve_request<S>(stream: S, state: &Arc<ControlApiState>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    // P2 line 173: use .take() to limit the read BEFORE read_line grows
    // the String unbounded. The old code checked AFTER read_line, so a
    // multi-GB line with no newline allocated all of it first.
    let mut limited = (&mut reader).take(MAX_HEADER_LINE_LEN as u64 + 1);
    if limited.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    if request_line.len() > MAX_HEADER_LINE_LEN {
        return Err(TropelError::Http(format!(
            "control API: request line too long ({} > {})",
            request_line.len(),
            MAX_HEADER_LINE_LEN
        )));
    }
    let request_line = request_line.trim_end().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // Read headers, discover Content-Length.
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        let mut limited = (&mut reader).take(MAX_HEADER_LINE_LEN as u64 + 1);
        if limited.read_line(&mut line).await? == 0 {
            break;
        }
        // P2 line 173: cap individual header line length BEFORE read.
        if line.len() > MAX_HEADER_LINE_LEN {
            return Err(TropelError::Http(format!(
                "control API: header line too long ({} > {})",
                line.len(),
                MAX_HEADER_LINE_LEN
            )));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // Body ceiling: `Content-Length` is client-controlled, so a hostile
    // value (e.g. 64 GiB) must be rejected BEFORE any allocation — the old
    // code `Vec::resize`d the full declared length unbounded (backlog line
    // 164).
    if content_length > MAX_BODY_SIZE {
        let mut out = reader.into_inner();
        out.write_all(
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await?;
        out.flush().await?;
        return Ok(());
    }

    // Read the body.
    let mut body = Vec::new();
    if content_length > 0 {
        body.resize(content_length, 0);
        reader.read_exact(&mut body).await?;
    }

    let (status, response_body) = route(&method, &path, &body, state).await;

    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        response_body.len(),
        response_body
    );
    let mut out = reader.into_inner();
    out.write_all(response.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

/// Route a control request and return (status line, JSON body).
async fn route(
    method: &str,
    path: &str,
    body: &[u8],
    state: &Arc<ControlApiState>,
) -> (String, String) {
    let sched = &state.scheduler;
    match (method, path) {
        ("GET", "/v1/status") => ("200 OK".to_string(), status_json(sched)),
        ("PATCH", "/v1/status") => match parse_status_body(body) {
            Some(patch) => {
                // Apply only the fields the client sent (k6 allows partial
                // PATCHes: just vus, just max, just paused, just stopped, or
                // any combo).
                if patch.vus.is_some() || patch.max.is_some() {
                    let vus = patch.vus.unwrap_or_else(|| sched.control_target());
                    let max = patch.max.unwrap_or_else(|| sched.control_max());
                    sched.set_control_target(vus, max);
                    tracing::info!("Control API: set VUs target={} max={}", vus, max);
                }
                if let Some(paused) = patch.paused {
                    sched.set_paused(paused);
                    tracing::info!("Control API: paused={}", paused);
                }
                // `stopped` was EMITTED in the status doc but never READ: a
                // PATCH `{"stopped":true}` used to fall through to the
                // unknown-field 400, so a k6-style client could never stop
                // the run via the control API (backlog line 164).
                if patch.stopped == Some(true) {
                    sched.request_stop();
                    tracing::info!("Control API: stop requested");
                }
                ("200 OK".to_string(), status_json(sched))
            }
            None => (
                "400 Bad Request".to_string(),
                "{\"error\":\"expected {\\\"vus\\\":N,\\\"max\\\":M,\\\"paused\\\":bool}\"}"
                    .to_string(),
            ),
        },
        // k6 envelope on the stop route itself: PATCH /v1/stop with
        // {"data":{"attributes":{"stopped":true}}}.
        ("PATCH", "/v1/stop") => {
            match parse_status_body(body) {
                Some(patch) if patch.stopped == Some(true) || patch.stopped.is_none() => {
                    if patch.stopped == Some(true) {
                        sched.request_stop();
                        tracing::info!("Control API: stop requested (PATCH /v1/stop)");
                    }
                    ("200 OK".to_string(), status_json(sched))
                }
                _ => (
                    "400 Bad Request".to_string(),
                    "{\"error\":\"expected {\\\"data\\\":{\\\"attributes\\\":{\\\"stopped\\\":true}}}\"}"
                        .to_string(),
                ),
            }
        }
        ("POST", "/v1/stop") => {
            sched.request_stop();
            ("200 OK".to_string(), status_json(sched))
        }
        ("GET", "/v1/metrics") => {
            let snap = state.metrics.snapshot().await;
            ("200 OK".to_string(), metrics_json(&snap))
        }
        ("GET", "/v1/groups") => {
            let snap = state.metrics.snapshot().await;
            ("200 OK".to_string(), groups_json(&snap, &state.scenario_name))
        }
        ("GET", "/v1/setup") => {
            let data = state.setup_data.lock().unwrap().clone();
            ("200 OK".to_string(), setup_json(data.as_deref()))
        }
        ("PUT", "/v1/setup") => {
            // k6: PUT /v1/setup with a JSON body (or empty to clear) sets the
            // setup data. We store the raw bytes so GET round-trips them.
            let parsed: Option<serde_json::Value> = if body.is_empty() {
                None
            } else {
                match serde_json::from_slice(body) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        return (
                            "400 Bad Request".to_string(),
                            format!(r#"{{"error":"invalid setup data: {e}"}}"#),
                        )
                    }
                }
            };
            *state.setup_data.lock().unwrap() = parsed.map(|v| v.to_string().into_bytes());
            let data = state.setup_data.lock().unwrap().clone();
            ("200 OK".to_string(), setup_json(data.as_deref()))
        }
        ("POST", "/v1/setup") => {
            // k6's POST /v1/setup runs the script's setup() — the engine runs
            // setup() once before VUs spawn; a mid-run re-run is not a k6
            // behaviour and is not supported.
            (
                "405 Method Not Allowed".to_string(),
                r#"{"error":"setup() runs once at engine start; POST re-run not supported"}"#
                    .to_string(),
            )
        }
        ("POST", "/v1/teardown") => {
            (
                "405 Method Not Allowed".to_string(),
                r#"{"error":"teardown() runs once at engine stop; POST re-run not supported"}"#
                    .to_string(),
            )
        }
        _ => (
            "404 Not Found".to_string(),
            r#"{"error":"not found"}"#.to_string(),
        ),
    }
}

/// Render the k6 JSON:API status document. `running` is false once a stop
/// has been requested; `tainted` reflects real threshold failures
/// (backlog line 154 — was hardcoded null).
fn status_json(sched: &Arc<VUScheduler>) -> String {
    let vus = sched.control_target();
    let max = sched.control_max();
    let paused = sched.is_paused();
    let stopped = sched.is_stop_requested();
    let running = !stopped;
    let tainted = sched.is_tainted();
    format!(
        r#"{{"data":{{"type":"status","id":"default","attributes":{{"vus":{},"vus-max":{},"max":{},"paused":{},"running":{},"stopped":{},"tainted":{}}}}}}}"#,
        vus, max, max, paused, running, stopped, tainted
    )
}

/// Render the k6 JSON:API `/v1/metrics` document: an array of metric objects
/// with the metric name as the JSON:API id and its current sample values in
/// `attributes`. Matches k6's `metric_jsonapi.go` envelope shape
/// (`{"data":[{"type":"metrics","id":"…","attributes":{…}}]}`).
fn metrics_json(snap: &MetricsSnapshot) -> String {
    let mut entries: Vec<serde_json::Value> = Vec::new();
    // Aggregate the per-series snapshots by metric name. A metric with several
    // tag-combinations (per-URL http_req_duration etc.) reports its last value
    // per series, but the JSON:API metric object is keyed by metric name only —
    // match k6 (one object per metric) and use the last observed value.
    let mut by_metric: std::collections::BTreeMap<&str, &SeriesSnapshot> = Default::default();
    for s in &snap.series {
        by_metric.insert(&s.metric, s);
    }
    for (name, s) in by_metric {
        // k6 `Sample` map: the trend value under "value" (ms), count/sum for
        // counters — minimal but shape-correct.
        let sample = serde_json::json!({
            "value": s.last,
        });
        entries.push(serde_json::json!({
            "type": "metrics",
            "id": name,
            "attributes": {
                "type": metric_type_name(s.metric_type),
                "contains": "default",
                "tainted": false,
                "sample": sample,
            },
        }));
    }
    // Even with no metrics, k6's envelope is `{"data":[]}`.
    serde_json::json!({ "data": entries }).to_string()
}

/// k6 metric type names: `counter`, `gauge`, `rate`, `trend`.
fn metric_type_name(t: tropel_metrics::collector::MetricType) -> &'static str {
    use tropel_metrics::collector::MetricType;
    match t {
        MetricType::Counter => "counter",
        MetricType::Gauge => "gauge",
        MetricType::Rate => "rate",
        MetricType::Trend => "trend",
    }
}

/// Render the k6 JSON:API `/v1/groups` document. k6's group tree is built
/// from the script's `group()` nesting; tropel does not track a nested group
/// tree at the engine level, so this returns a single root group named after
/// the scenario (k6's root is always `""` — its id is `0`). The shape matches
/// `group_jsonapi.go` (`{"data":[{"type":"groups","id":"…","attributes":{…}}]}`).
fn groups_json(_snap: &MetricsSnapshot, scenario_name: &str) -> String {
    serde_json::json!({
        "data": [{
            "type": "groups",
            "id": "0",
            "attributes": {
                "path": "",
                "name": scenario_name,
                "checks": [],
            },
            "relationships": {
                "groups": { "data": [] },
                "parent": { "data": null },
            },
        }]
    })
    .to_string()
}

/// Render the k6 JSON:API `/v1/setup` document: `{"data":{"data":<setup>}}`
/// where `<setup>` is the JSON setup value, or `null` when there is none.
fn setup_json(data: Option<&[u8]>) -> String {
    let value = match data {
        Some(bytes) => serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    serde_json::json!({ "data": { "data": value } }).to_string()
}

/// A parsed PATCH /v1/status body. All fields optional — k6 allows partial
/// patches (`{"paused":true}` alone is valid).
#[derive(Debug, Clone, Copy, PartialEq)]
struct StatusPatch {
    vus: Option<u32>,
    max: Option<u32>,
    paused: Option<bool>,
    stopped: Option<bool>,
}

/// Parse a PATCH /v1/status body. Accepts the flat form
/// `{"vus":5,"vus-max":10}`, the k6 envelope
/// `{"data":{"attributes":{"vus":5,"vus-max":10}}}` — and the legacy
/// `max` key (backlog line 154: k6's JSON:API field is `vus-max`; Tropel's
/// old `max` stays accepted). `vus-max` wins over `max` when both are sent.
/// Returns `None` when the body is unparseable or carries none of the known
/// fields (so a garbage body can't be silently swallowed).
fn parse_status_body(body: &[u8]) -> Option<StatusPatch> {
    let text = std::str::from_utf8(body).ok()?;
    let json: serde_json::Value = serde_json::from_str(text).ok()?;

    // k6 envelope: {"data":{"attributes":{...}}}
    let attrs = json
        .get("data")
        .and_then(|d| d.get("attributes"))
        .or(Some(&json))?;

    let vus = attrs.get("vus").and_then(|v| v.as_u64()).map(|v| v as u32);
    let max = attrs
        .get("vus-max")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .or_else(|| attrs.get("max").and_then(|v| v.as_u64()).map(|v| v as u32));
    let paused = attrs.get("paused").and_then(|v| v.as_bool());
    // `stopped` is emitted by status_json and must be accepted back (backlog
    // line 164): PATCH {"stopped":true} stops the run.
    let stopped = attrs.get("stopped").and_then(|v| v.as_bool());

    if vus.is_none() && max.is_none() && paused.is_none() && stopped.is_none() {
        return None;
    }
    Some(StatusPatch {
        vus,
        max,
        paused,
        stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tropel_metrics::collector::MetricsCollector;

    /// Build a control state for tests. The metrics collector is live but
    /// empty; setup data is None.
    fn test_state(sched: Arc<VUScheduler>) -> Arc<ControlApiState> {
        Arc::new(ControlApiState {
            scheduler: sched,
            metrics: Arc::new(MetricsCollector::new()),
            setup_data: Arc::new(std::sync::Mutex::new(None)),
            scenario_name: "s".to_string(),
        })
    }

    #[test]
    fn parses_flat_body() {
        assert_eq!(
            parse_status_body(br#"{"vus":5,"max":20}"#),
            Some(StatusPatch {
                vus: Some(5),
                max: Some(20),
                paused: None,
                stopped: None,
            })
        );
    }

    #[test]
    fn parses_k6_envelope() {
        assert_eq!(
            parse_status_body(br#"{"data":{"attributes":{"vus":3,"max":9}}}"#),
            Some(StatusPatch {
                vus: Some(3),
                max: Some(9),
                paused: None,
                stopped: None,
            })
        );
    }

    #[test]
    fn parses_paused_only() {
        assert_eq!(
            parse_status_body(br#"{"paused":true}"#),
            Some(StatusPatch {
                vus: None,
                max: None,
                paused: Some(true),
                stopped: None,
            })
        );
    }

    #[test]
    fn partial_patch_with_only_vus_is_valid() {
        assert_eq!(
            parse_status_body(br#"{"vus":5}"#),
            Some(StatusPatch {
                vus: Some(5),
                max: None,
                paused: None,
                stopped: None,
            })
        );
    }

    #[test]
    fn rejects_garbage_and_unknown_only() {
        assert_eq!(parse_status_body(br#"{"foo":1}"#), None); // no known field
        assert_eq!(parse_status_body(b"garbage"), None);
        assert_eq!(parse_status_body(b"{}"), None);
    }

    /// Backlog line 164: `stopped` is emitted by status_json and must be
    /// accepted back — a PATCH carrying only `{"stopped":true}` is a valid
    /// partial patch (it used to 400 as "unknown field").
    #[test]
    fn parses_stopped_only() {
        assert_eq!(
            parse_status_body(br#"{"stopped":true}"#),
            Some(StatusPatch {
                vus: None,
                max: None,
                paused: None,
                stopped: Some(true),
            })
        );
    }

    /// Backlog line 164: a PATCH `{"stopped":true}` must stop the run — it
    /// was emitted in the status doc but never read back, so a k6-style
    /// client could never stop via the control API.
    #[test]
    fn route_stopped_true_requests_stop() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched.clone());
        assert!(!sched.is_stop_requested());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (status, body) =
            rt.block_on(route("PATCH", "/v1/status", br#"{"stopped":true}"#, &state));
        assert_eq!(status, "200 OK");
        assert!(
            sched.is_stop_requested(),
            "PATCH stopped:true must request a stop"
        );
        // The response reflects the now-stopped state.
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["attributes"]["stopped"], true);
    }

    /// Backlog line 164: `stopped:false` is a no-op (nothing to un-stop).
    #[test]
    fn route_stopped_false_is_noop() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (status, _) = rt.block_on(route(
            "PATCH",
            "/v1/status",
            br#"{"stopped":false}"#,
            &state,
        ));
        assert_eq!(status, "200 OK");
        assert!(!sched.is_stop_requested());
    }

    /// Backlog line 164: a declared body larger than MAX_BODY_SIZE must be
    /// rejected with 413 BEFORE any allocation — the old code resized to the
    /// full declared length (a hostile 64 GiB Content-Length = 64 GiB alloc).
    #[tokio::test]
    async fn oversized_body_rejected_before_alloc() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let (mut client, server) = tokio::io::duplex(4096);
        let state = test_state(sched);
        let server_task = tokio::spawn(async move { serve_request(server, &state).await });

        // Declare a 64 GiB body; never send it. Must get 413 back (and the
        // handler must not try to read 64 GiB).
        client
            .write_all(b"PATCH /v1/status HTTP/1.1\r\nContent-Length: 68719476736\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.contains("413 Payload Too Large"),
            "expected 413, got: {}",
            text
        );
        server_task.await.unwrap().unwrap();
    }

    /// Backlog line 164: a client that sends the request line but never
    /// finishes the request must be cut off by the read timeout — it must
    /// not hold its handler (and one of MAX_CONNS slots) forever.
    /// Paused tokio time drives the internal CONN_TIMEOUT deterministically
    /// instead of waiting 10 real seconds.
    #[tokio::test(start_paused = true)]
    async fn stalled_client_is_timed_out() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let (mut client, server) = tokio::io::duplex(4096);
        // Send only a partial request line — never finish the headers/body,
        // so `read_line` would block forever WITHOUT the read timeout.
        client.write_all(b"PATCH /v1/status HTT").await.unwrap();
        let state = test_state(sched);
        let server_task = tokio::spawn(async move { handle_conn(server, &state).await });
        // Advance past CONN_TIMEOUT (10s) — the handler must return (the
        // timeout fires) rather than hanging on the incomplete request.
        tokio::time::advance(Duration::from_secs(11)).await;
        let result = server_task.await.unwrap();
        assert!(
            result.is_ok(),
            "stalled client must be cut off by the read timeout"
        );
    }

    #[test]
    fn status_json_is_k6_shape() {
        let sched = VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        );
        let sched = Arc::new(sched);
        sched.set_control_target(4, 10);
        let body = status_json(&sched);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let attrs = &v["data"]["attributes"];
        assert_eq!(attrs["type"], serde_json::Value::Null); // type/id live on data, not attributes
        assert_eq!(v["data"]["type"], "status");
        assert_eq!(v["data"]["id"], "default");
        assert_eq!(attrs["vus"], 4);
        // Backlog line 154: k6's JSON:API field is `vus-max`; Tropel also
        // keeps emitting `max` for back-compat.
        assert_eq!(attrs["vus-max"], 10);
        assert_eq!(attrs["max"], 10);
        assert_eq!(attrs["paused"], false);
        assert_eq!(attrs["running"], true);
        assert_eq!(attrs["stopped"], false);
        assert_eq!(attrs["tainted"], false);

        // Taint is real: once a threshold fails, the status shows it.
        sched.set_tainted();
        let body = status_json(&sched);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["attributes"]["tainted"], true);
    }

    /// TR-250: PATCH /v1/stop accepts the k6 envelope
    /// `{"data":{"attributes":{"stopped":true}}}` and stops the run.
    #[test]
    fn patch_stop_accepts_k6_envelope() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched.clone());
        assert!(!sched.is_stop_requested());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (status, body) = rt.block_on(route(
            "PATCH",
            "/v1/stop",
            br#"{"data":{"attributes":{"stopped":true}}}"#,
            &state,
        ));
        assert_eq!(status, "200 OK");
        assert!(
            sched.is_stop_requested(),
            "PATCH /v1/stop with the k6 envelope must stop the run"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["attributes"]["stopped"], true);
    }

    /// TR-250: GET /v1/metrics returns the k6 JSON:API envelope with a
    /// metric entry per observed metric name.
    #[tokio::test]
    async fn get_metrics_returns_k6_envelope() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched);
        let (status, body) = route("GET", "/v1/metrics", b"", &state).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["data"],
            serde_json::Value::Array(vec![]),
            "no metrics yet"
        );
    }

    /// TR-250: GET /v1/groups returns the k6 JSON:API envelope with the
    /// scenario as the root group (id "0", empty path).
    #[tokio::test]
    async fn get_groups_returns_k6_envelope() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched);
        let (status, body) = route("GET", "/v1/groups", b"", &state).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let group = &v["data"][0];
        assert_eq!(group["type"], "groups");
        assert_eq!(group["id"], "0");
        assert_eq!(group["attributes"]["path"], "");
        assert_eq!(group["attributes"]["name"], "s");
        assert_eq!(
            group["relationships"]["groups"]["data"],
            serde_json::Value::Array(vec![])
        );
        assert_eq!(
            group["relationships"]["parent"]["data"],
            serde_json::Value::Null
        );
    }

    /// TR-250: GET /v1/setup returns null data when none is set; PUT sets it;
    /// GET reads it back.
    #[tokio::test]
    async fn setup_get_put_roundtrip() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched);

        let (status, body) = route("GET", "/v1/setup", b"", &state).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["data"], serde_json::Value::Null);

        let (status, body) = route("PUT", "/v1/setup", br#"{"token":"abc"}"#, &state).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["data"]["token"], "abc");

        let (status, body) = route("GET", "/v1/setup", b"", &state).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["data"]["token"], "abc");
    }

    /// TR-250: k6's POST /v1/setup and POST /v1/teardown re-run the lifecycle
    /// functions; tropel runs them once at engine start/stop, so a mid-run
    /// POST must fail loudly (405) rather than pretend to have re-run setup.
    #[tokio::test]
    async fn setup_teardown_post_reexecution_rejected() {
        let sched = Arc::new(VUScheduler::new(
            &tropel_core::config::ExecutionConfig::ExternallyControlled {
                vus: 2,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default(),
            },
        ));
        let state = test_state(sched);
        let (status, body) = route("POST", "/v1/setup", b"", &state).await;
        assert_eq!(status, "405 Method Not Allowed");
        assert!(body.contains("setup() runs once"));
        let (status, body) = route("POST", "/v1/teardown", b"", &state).await;
        assert_eq!(status, "405 Method Not Allowed");
        assert!(body.contains("teardown() runs once"));
    }
}
