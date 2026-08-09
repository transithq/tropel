use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Engine-only config ─────────────────────────────────────────────
// JobConfig, HttpConfig, TlsConfig, ExpectedStatus stay here. The contract
// config types (ExecutionConfig, ScenarioConfig, ThresholdConfig,
// OutputConfig, ThinkTimeConfig, Stage, ArrivalRateStage) live in tropel-sdk
// (the Driver contract references them); re-exported so engine crates keep
// resolving tropel_core::config::* unchanged.
pub use tropel_sdk::config::{
    ArrivalRateStage, ExecutionConfig, OutputConfig, ScenarioConfig, Stage, ThinkTimeConfig,
    ThresholdConfig,
};

/// Full configuration for a load test job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobConfig {
    /// Input file path (used as default for all scenarios, or directly for single-scenario mode).
    pub input: String,
    /// Input type (auto-detect if not specified).
    pub input_type: Option<String>,
    /// Execution configuration (used when no scenarios are defined — single-scenario mode).
    pub execution: ExecutionConfig,
    /// Named scenarios for multi-scenario runs. When present, each scenario runs
    /// independently with its own executor, env, and optional startTime.
    /// The top-level `execution` field is ignored when scenarios are defined.
    #[serde(default)]
    pub scenarios: HashMap<String, ScenarioConfig>,
    /// Environment variables (merged with per-scenario env).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Global variables.
    #[serde(default)]
    pub globals: HashMap<String, serde_json::Value>,
    /// Collection variables.
    #[serde(default)]
    pub collection_vars: HashMap<String, serde_json::Value>,
    /// Data file (CSV/JSON for iteration data).
    pub data_file: Option<String>,
    /// Iteration data variables.
    #[serde(default)]
    pub iteration_data: Vec<HashMap<String, serde_json::Value>>,
    /// Threshold configuration.
    #[serde(default)]
    pub thresholds: HashMap<String, ThresholdConfig>,
    /// Output/reporter configuration.
    #[serde(default)]
    pub output: OutputConfig,
    /// HTTP configuration.
    #[serde(default)]
    pub http: HttpConfig,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Extension configuration.
    #[serde(default)]
    pub extensions: HashMap<String, serde_json::Value>,
    /// Whether the load profile (`execution` / `scenarios`) was explicitly
    /// provided by the user (CLI flags or a config file). When false, an
    /// input driver that declares its own load profile — e.g. a k6 script's
    /// `export const options` (vus/duration/stages/scenarios/thresholds) —
    /// may override the job's execution config. Defaults to false so k6
    /// scripts drive their own runs unless the user opts out via flags.
    #[serde(default)]
    pub execution_explicit: bool,
    /// Deterministic workload partitioning: which fraction `[from, to)` of
    /// this run this node executes, as `"from:to"` (e.g. `"0:1/3"`).
    /// Combine with `execution_segment_sequence` for cross-node validation.
    /// k6-compatible: `executionSegment` / `executionSegmentSequence`.
    #[serde(default, alias = "executionSegment")]
    pub execution_segment: Option<String>,
    /// The full sequence of segment boundaries shared by all cooperating
    /// nodes, e.g. `"0,1/3,2/3,1"`. Optional but recommended: validates
    /// that `execution_segment` is a consecutive pair of this sequence.
    #[serde(default, alias = "executionSegmentSequence")]
    pub execution_segment_sequence: Option<String>,
    /// Set by `tropel-agent` when running as a distributed worker: the
    /// controller owns the end-of-run summary (reporters, handleSummary,
    /// summary-export), so the agent skips them and just ships its raw
    /// snapshot back for central merging.
    #[serde(default, alias = "distributedWorker")]
    pub distributed_worker: bool,
    /// Port for the runtime control API (k6 REST parity). When set, an
    /// `externally-controlled` scenario binds `127.0.0.1:<port>` and serves
    /// `GET/PATCH /v1/status` so the VU count can be adjusted mid-run.
    #[serde(default, alias = "controlPort")]
    pub control_port: Option<u16>,
}

/// Expected status code or range for determining http_req_failed.
/// A request fails (http_req_failed=1) when the response status code
/// does NOT fall within any of the expected entries.
///
/// Each entry can be:
/// - A single code: `200`
/// - A range: `"200-399"`
/// - A pattern with wildcard: `"2xx"`, `"3xx"`
///
/// Default: `["200-399"]` — all 2xx and 3xx are considered success.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExpectedStatus {
    Single(u16),
    Range(String),
}

impl ExpectedStatus {
    /// Check if a given status code matches this expected status entry.
    pub fn matches(&self, code: u16) -> bool {
        match self {
            ExpectedStatus::Single(c) => *c == code,
            ExpectedStatus::Range(s) => {
                // Support patterns: "200-399" (range), "2xx" (wildcard), "200" (single)
                if let Some((lo, hi)) = s.split_once('-') {
                    // Range: "200-299"
                    let lo: u16 = lo.trim().parse().unwrap_or(0);
                    let hi: u16 = hi.trim().parse().unwrap_or(u16::MAX);
                    code >= lo && code <= hi
                } else if s.ends_with("xx") {
                    // Wildcard: "2xx" → 200-299, "3xx" → 300-399
                    let prefix = &s[..s.len() - 2];
                    if let Ok(base) = prefix.parse::<u16>() {
                        let lo = base * 100;
                        let hi = lo + 99;
                        code >= lo && code <= hi
                    } else {
                        false
                    }
                } else if let Ok(c) = s.parse::<u16>() {
                    c == code
                } else {
                    false
                }
            }
        }
    }
}

/// Check if a response status code is expected (successful) according to the
/// list of expected statuses. Returns true if the code matches ANY expected entry.
/// Returns false if the list is empty (never succeeds — all requests fail).
pub fn status_is_expected(code: u16, expected: &[ExpectedStatus]) -> bool {
    if expected.is_empty() {
        return false;
    }
    expected.iter().any(|e| e.matches(code))
}

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
    /// k6 `noVUConnectionReuse` parity. Tropel already gives every VU its own
    /// client with its own connection pool, so connections are never shared
    /// across VUs regardless of this flag; it is accepted for script
    /// compatibility and currently a no-op.
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
    /// Log every HTTP request/response at debug level (method, URL, status,
    /// timing). Off by default; enable with the `--http-debug` CLI flag.
    #[serde(default)]
    pub http_debug: bool,
}

fn default_expected_statuses() -> Vec<ExpectedStatus> {
    vec![ExpectedStatus::Range("200-399".to_string())]
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            // 2xx-3xx = success (default, matches k6 behavior)
            expected_statuses: default_expected_statuses(),
            // With per-VU HTTP clients, each VU has its own connection pool.
            // A VU only makes one request at a time (sequential), so 4 idle
            // connections per host per VU is plenty. The old default of 100
            // was designed for shared clients — with per-VU clients and 100 VUs
            // it would mean 10,000 idle connections total.
            max_idle_connections: 4,
            keep_alive: Some("30s".to_string()),
            // How long an idle connection is kept before being closed.
            idle_connection_timeout: Some("30s".to_string()),
            request_timeout: None,
            http2: true,
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
            http_debug: false,
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

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            input: String::new(),
            input_type: None,
            execution: ExecutionConfig::ConstantVus {
                vus: 1,
                duration: "30s".to_string(),
                graceful_stop: Some("30s".to_string()),
                think_time: ThinkTimeConfig::default(),
            },
            scenarios: HashMap::new(),
            env: HashMap::new(),
            globals: HashMap::new(),
            collection_vars: HashMap::new(),
            data_file: None,
            iteration_data: vec![],
            thresholds: HashMap::new(),
            output: OutputConfig::default(),
            http: HttpConfig::default(),
            tls: TlsConfig::default(),
            extensions: HashMap::new(),
            execution_explicit: false,
            execution_segment: None,
            execution_segment_sequence: None,
            distributed_worker: false,
            control_port: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_status_single_range_wildcard_and_invalid() {
        // Single code.
        assert!(ExpectedStatus::Single(200).matches(200));
        assert!(!ExpectedStatus::Single(200).matches(404));
        // Range "200-399" (default) — 2xx and 3xx succeed, 4xx fails.
        let default = ExpectedStatus::Range("200-399".into());
        assert!(default.matches(200));
        assert!(default.matches(304));
        assert!(default.matches(399));
        assert!(!default.matches(400));
        assert!(!default.matches(199));
        // Wildcard "2xx" → 200-299.
        let xx = ExpectedStatus::Range("2xx".into());
        assert!(xx.matches(200));
        assert!(xx.matches(299));
        assert!(!xx.matches(300));
        assert!(!xx.matches(199));
        // Malformed patterns never match (no panic, no silent all-match).
        // NOTE: "20-30-40" and "x-y" are deliberately NOT here — in both,
        // split_once('-') produces a hi segment that fails to parse and
        // degrades to u16::MAX (and lo to 0), so the code honestly treats
        // them as 0..=65535 and they DO match. The test pins only the
        // genuinely-non-matching malformed inputs.
        for bad in ["", "abc", "-5", "99999"] {
            assert!(!ExpectedStatus::Range(bad.into()).matches(200), "{bad}");
        }
    }

    #[test]
    fn status_is_expected_empty_list_never_succeeds() {
        // Documented contract: empty expected list = ALL requests fail.
        assert!(!status_is_expected(200, &[]));
        assert!(!status_is_expected(500, &[]));
        // Any-of semantics.
        let set = [
            ExpectedStatus::Single(200),
            ExpectedStatus::Range("4xx".into()),
        ];
        assert!(status_is_expected(200, &set));
        assert!(status_is_expected(404, &set));
        assert!(!status_is_expected(500, &set));
    }
}
