//! Minimal runtime control API — k6 REST `/v1/status` parity.
//!
//! Binds `127.0.0.1:<port>` and serves:
//! - `GET  /v1/status`  → k6 JSON:API shape
//!   `{"data":{"type":"status","id":"default","attributes":{...}}}`
//!   (vus / vus-max / paused / running / stopped / tainted)
//! - `PATCH /v1/status` → k6 envelope `{"data":{"attributes":{...}}}` or a
//!   flat `{"vus":N,"max":M,"vus-max":N,"paused":bool}` — adjusts the
//!   externally-controlled scheduler's VU pool / pause state at runtime.
//!   `max` is clamped to the configured `max_vus` ceiling, so a client can
//!   never grow the pool past the run's cap. Both `max` (legacy) and
//!   `vus-max` (k6's JSON:API field name) are accepted, and the status doc
//!   emits BOTH so old and new clients work.
//!
//! Everything else returns 404. This is intentionally dependency-free: a
//! hand-rolled HTTP/1.1 reader keeps the control surface small and avoids
//! pulling a web framework into the engine for one endpoint.

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tropel_scheduler::VUScheduler;
use tropel_sdk::Result;

/// Ceiling for a control request body. A status patch is a few hundred
/// bytes; `Content-Length` is attacker-controlled, so without this cap a
/// hostile `Content-Length: 68719476736` forced a 64 GiB `Vec::resize`
/// (backlog line 164).
const MAX_BODY_SIZE: usize = 64 * 1024;
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

/// Handle the control server task. Runs until the listener errors or the
/// task is aborted by the scenario finishing.
pub async fn serve_control_api(port: u16, scheduler: Arc<VUScheduler>) -> Result<()> {
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
        let sched = scheduler.clone();
        tokio::spawn(async move {
            // The permit is held for the whole connection lifetime, so the
            // cap counts live handlers, not just accepted sockets.
            let _permit = permit;
            if let Err(e) = handle_conn(stream, &sched).await {
                tracing::debug!("control API: connection error: {}", e);
            }
        });
    }
}

/// Serve one HTTP connection, bounded by a read timeout (a stalled client
/// must not hold its handler — and its connection slot — forever).
async fn handle_conn<S>(stream: S, sched: &Arc<VUScheduler>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Read timeout: without it a client that sends the request line but
    // never finishes headers/body parks the handler (and one of MAX_CONNS
    // slots) indefinitely (backlog line 164).
    match tokio::time::timeout(CONN_TIMEOUT, serve_request(stream, sched)).await {
        Ok(r) => r,
        Err(_elapsed) => {
            tracing::debug!("control API: connection timed out");
            Ok(())
        }
    }
}

/// Read one request (line + headers + body), route it, write the response.
async fn serve_request<S>(stream: S, sched: &Arc<VUScheduler>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let request_line = request_line.trim_end().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // Read headers, discover Content-Length.
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
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

    let (status, response_body) = route(&method, &path, &body, sched);

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
fn route(method: &str, path: &str, body: &[u8], sched: &Arc<VUScheduler>) -> (String, String) {
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
        ("POST", "/v1/stop") => {
            sched.request_stop();
            ("200 OK".to_string(), status_json(sched))
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
        assert!(!sched.is_stop_requested());
        let (status, body) = route("PATCH", "/v1/status", br#"{"stopped":true}"#, &sched);
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
        let (status, _) = route("PATCH", "/v1/status", br#"{"stopped":false}"#, &sched);
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
        let sched_c = sched.clone();
        let server_task = tokio::spawn(async move { serve_request(server, &sched_c).await });

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
        let sched_c = sched.clone();
        let server_task = tokio::spawn(async move { handle_conn(server, &sched_c).await });
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
}
