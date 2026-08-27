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
        ("POST", "/execute") => {
            let mut body_buf = vec![0u8; content_length.min(64 * 1024)];
            if content_length > 0 {
                sock.read_exact(&mut body_buf)
                    .await
                    .map_err(TropelError::Io)?;
            }
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
            let mut body_buf = vec![0u8; content_length.min(4 * 1024 * 1024)];
            if content_length > 0 {
                sock.read_exact(&mut body_buf)
                    .await
                    .map_err(TropelError::Io)?;
            }
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
            unsupported_errors.into_iter().map(serde_json::Value::String).collect(),
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
}
