use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Engine-only config ─────────────────────────────────────────────
// JobConfig stays here. The contract config types (ExecutionConfig,
// ScenarioConfig, ThresholdConfig, OutputConfig, ThinkTimeConfig, Stage,
// ArrivalRateStage, ExpectedStatus, status_is_expected) live in tropel-sdk;
// the HTTP config types (HttpConfig, TlsConfig) live in tropel-http (P3c:
// the runtime publish set must stop resolving tropel-core). All re-exported
// so engine crates keep resolving tropel_core::config::* unchanged.
pub use tropel_sdk::config::{
    status_is_expected, ArrivalRateStage, ExecutionConfig, ExpectedStatus, OutputConfig,
    ScenarioConfig, Stage, ThinkTimeConfig, ThresholdConfig,
};
pub use tropel_http::config::{HttpConfig, TlsConfig};

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
