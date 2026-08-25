//! HTTP client configuration (P3c): `HttpConfig` and `TlsConfig` moved here
//! from `tropel-core` so the runtime publish set (and `tropel-http` itself)
//! stops resolving `tropel-core`. `tropel-core` re-exports these so engine
//! crates keep resolving `tropel_core::config::*` unchanged.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tropel_sdk::config::ExpectedStatus;

/// HTTP client configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    /// Expected response status codes/ranges that determine request success.
    /// Used to drive the http_req_failed Rate metric.
    /// Default: `["200-399"]` — 2xx and 3xx are success, everything else fails.
    #[serde(default = "default_expected_statuses", alias = "expectedStatuses")]
    pub expected_statuses: Vec<ExpectedStatus>,
    /// Connection pool max idle connections.
    pub max_idle_connections: usize,
    /// Keep-alive duration.
    pub keep_alive: Option<String>,
    /// Timeout for idle connections.
    pub idle_connection_timeout: Option<String>,
    /// Global per-request timeout (k6 `timeout`), e.g. `"30s"`. Applied as
    /// the client-level ceiling for every request; a per-request `timeout`
    /// overrides it with a shorter value. `None` (default) uses the engine
    /// default of 10 seconds. Bounds how long a hung server can stall a VU
    /// (which in turn bounds the engine's VU-drain loop).
    #[serde(default, alias = "requestTimeout")]
    pub request_timeout: Option<String>,
    /// Whether to enable HTTP/2.
    pub http2: bool,
    /// Number of HTTP/2 connection lanes (default 1). Each lane is an
    /// independent reqwest::Client with its own connection pool. VUs are
    /// assigned to lanes round-robin by vu_id % N. Spreading load across
    /// N h2 connections hides per-connection server limits and parallelizes
    /// the single-core frame demux. k6 cannot do this at all.
    #[serde(default = "default_http2_connections", alias = "http2Connections")]
    pub http2_connections: usize,
    /// User-agent header value.
    pub user_agent: String,
    /// Whether to decompress response bodies.
    pub decompress: bool,
    /// Whether to discard response bodies entirely (don't store bytes).
    /// Saves memory and bandwidth at the cost of not being able to inspect
    /// response content in scripts.
    #[serde(default)]
    pub discard_response_bodies: bool,
    /// Max redirects to follow.
    pub max_redirects: u32,
    /// Disable redirect following entirely (`--no-redirects`). When true the
    /// 3xx response is returned as-is and no redirect hops are captured.
    /// k6 always follows redirects; this flag lets Tropel opt out.
    #[serde(default)]
    pub no_redirects: bool,
    /// Optional fixed ceiling for the latency histogram, in MILLISECONDS.
    /// `None` (default) uses hdrhistogram auto-resize — no ceiling, so very
    /// slow requests are recorded exactly instead of being clipped at 60 s.
    /// Set this to bound memory for runs with pathological outliers.
    #[serde(default, alias = "histogramMaxMs")]
    pub histogram_max_ms: Option<u64>,
    /// DNS cache TTL (k6 `dns.ttl`), e.g. `"5m"`, `"inf"`. `None` (default)
    /// disables caching — every request resolves. `"0"` also disables it.
    #[serde(default, alias = "dnsTtl")]
    pub dns_ttl: Option<String>,
    /// DNS address selection policy (k6 `dns.select`): `"first"`,
    /// `"roundRobin"`, `"random"`. `None` (default) keeps all resolved
    /// addresses in lookup order (reqwest's default behavior).
    #[serde(default, alias = "dnsSelect")]
    pub dns_select: Option<String>,
    /// DNS address policy (k6 `dns.policy`): `"preferIPv4"`, `"preferIPv6"`,
    /// `"onlyIPv4"`, `"onlyIPv6"`, `"any"`. `None` (default) keeps the
    /// resolved address family order unchanged.
    #[serde(default, alias = "dnsPolicy")]
    pub dns_policy: Option<String>,
    /// Close the connection after every request (k6 `noConnectionReuse`).
    /// Disables connection pooling entirely — each request opens a fresh
    /// connection, which trades latency for isolation.
    #[serde(default, alias = "noConnectionReuse")]
    pub no_connection_reuse: bool,
    /// k6 `noVUConnectionReuse` parity. When true, forces a fresh client
    /// (own connection pool) per VU. Default false: every VU shares one
    /// pooled client via Arc clone, keeping connections warm and TLS
    /// sessions reusable.
    #[serde(default, alias = "noVUConnectionReuse")]
    pub no_vu_connection_reuse: bool,
    /// Global request-rate cap in requests/second (k6 `rps`). When set, the
    /// whole run is paced so no more than this many requests start per second,
    /// shared across all VUs. `None` (default) is unlimited.
    #[serde(default)]
    pub rps: Option<f64>,
    /// Static hostname → IP mapping (k6 `hosts`), e.g.
    /// `{"api.example.com": "127.0.0.1"}`. Lookups for these hosts are served
    /// from the map without hitting DNS. Values may be comma-separated to
    /// provide several addresses; keys may be wildcards (`"*.example.com"`).
    #[serde(default)]
    pub hosts: HashMap<String, String>,
    /// IP addresses / CIDRs that requests may never connect to (k6
    /// `blacklistIPs`), e.g. `["10.0.0.0/8", "192.168.1.5"]`. When every
    /// resolved address is blacklisted the request fails with a clear error.
    #[serde(default, alias = "blacklistIPs")]
    pub blacklist_ips: Vec<String>,
    /// Hard ceiling on the response body size in BYTES, enforced while the
    /// body is streamed (final response AND redirect-hop bodies). `None`
    /// (default) is unlimited — k6 semantics. Proxy-style consumers
    /// (KnockPort relay) set this so a runaway upstream can't fill memory.
    #[serde(default, alias = "maxResponseBytes")]
    pub max_response_bytes: Option<u64>,
    /// Log every HTTP request/response at debug level (method, URL, status,
    /// timing). Off by default; enable with the `--http-debug` CLI flag.
    /// k6's `--http-debug=full` also prints request/response bodies — that
    /// extra mode is `http_debug_full`.
    #[serde(default)]
    pub http_debug: bool,
    /// `--http-debug=full` (k6 parity): also print the request/response
    /// bodies, not just the head lines. Only meaningful with `http_debug`.
    #[serde(default)]
    pub http_debug_full: bool,
}

fn default_expected_statuses() -> Vec<ExpectedStatus> {
    vec![ExpectedStatus::Range("200-399".to_string())]
}

fn default_http2_connections() -> usize {
    1
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            // 2xx-3xx = success (default, matches k6 behavior)
            expected_statuses: default_expected_statuses(),
            // The HTTP client is shared across all VUs (Arc), so this caps
            // idle connections per host for the ENTIRE run, not per VU.
            // reqwest's own default is usize::MAX (unlimited); k6 gives 6 per
            // VU so 100 VUs = 600 idle connections per host. Using reqwest's
            // default matches that scaling behavior.
            max_idle_connections: usize::MAX,
            keep_alive: Some("30s".to_string()),
            // How long an idle connection is kept before being closed.
            idle_connection_timeout: Some("30s".to_string()),
            request_timeout: None,
            http2: true,
            http2_connections: 1,
            user_agent: "Tropel/0.1.0".to_string(),
            decompress: true,
            max_redirects: 10,
            no_redirects: false,
            discard_response_bodies: false,
            histogram_max_ms: None,
            dns_ttl: None,
            dns_select: None,
            dns_policy: None,
            no_connection_reuse: false,
            no_vu_connection_reuse: false,
            rps: None,
            hosts: HashMap::new(),
            blacklist_ips: Vec::new(),
            max_response_bytes: None,
            http_debug: false,
            http_debug_full: false,
        }
    }
}

/// TLS configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct TlsConfig {
    pub insecure_skip_verify: bool,
    pub min_version: Option<String>,
    pub max_version: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    pub client_passphrase: Option<String>,
    pub allowed_ciphers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_config_defaults_match_k6() {
        let cfg = HttpConfig::default();
        // Default expected list: 2xx + 3xx succeed, 4xx/5xx fail.
        assert!(cfg.expected_statuses.iter().any(|e| e.matches(200)));
        assert!(cfg.expected_statuses.iter().any(|e| e.matches(304)));
        assert!(!cfg.expected_statuses.iter().any(|e| e.matches(404)));
        assert_eq!(cfg.max_redirects, 10);
        assert!(cfg.http2);
        assert_eq!(cfg.http2_connections, 1);
    }

    #[test]
    fn http2_connections_camel_case_alias() {
        let json = r#"{"http2Connections": 4}"#;
        let cfg: HttpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.http2_connections, 4);
    }
}
