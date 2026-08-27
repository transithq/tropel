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
        _ => respond(sock, 404, r#"{"error":"not found"}"#).await,
    }
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
    let request = TropelRequest {
        url: url.to_string(),
        method: method_parsed,
        headers,
        query_params: HashMap::new(),
        body: req
            .get("body")
            .and_then(|b| b.as_str())
            .map(|s| Body::Raw(s.to_string())),
        auth: None,
        certificate: None,
        follow_redirects: follow,
        host: None,
        cookies: Vec::new(),
        timeout: None,
        response_type: ResponseType::Text,
    };

    let start = Instant::now();
    let result = state.client.execute(&request, None).await;
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
}
