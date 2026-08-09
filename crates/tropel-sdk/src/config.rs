use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Contract config types ─────────────────────────────────────────
// Referenced by the SDK's own extension contract (DriverDeclaredOptions,
// Output::configure) — they must live in the leaf. tropel-core re-exports
// them so engine crates keep resolving tropel_core::config::*.

///
/// Controls how long a VU waits before starting the next iteration.
/// - `delay`: fixed delay after each iteration (e.g. "2s")
/// - `min_delay` / `max_delay`: random delay in range [min, max]
/// - `iteration_pacing`: target iteration duration. If the iteration
///   finishes faster than this, the VU waits to hit the target.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ThinkTimeConfig {
    /// Fixed delay after each iteration (e.g., "2s", "500ms").
    /// If set, min/max_delay are ignored.
    pub delay: Option<String>,
    /// Minimum delay for random range (e.g., "1s").
    #[serde(alias = "minDelay")]
    pub min_delay: Option<String>,
    /// Maximum delay for random range (e.g., "3s").
    #[serde(alias = "maxDelay")]
    pub max_delay: Option<String>,
    /// Target iteration duration for pacing (e.g., "5s").
    /// If the iteration finishes faster than this, the VU waits
    /// to hit the target duration before starting the next iteration.
    #[serde(alias = "iterationPacing")]
    pub iteration_pacing: Option<String>,
}

/// Configuration for a single named scenario within a multi-scenario run.
/// Each scenario has its own executor, input, env, tags, and optional start time.
/// When only a single scenario is running, the top-level `execution` field is used
/// instead and no `ScenarioConfig` is needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
    /// Which executor to use.
    pub execution: ExecutionConfig,
    /// Optional input file override (defaults to the job-level `input`).
    pub input: Option<String>,
    /// Per-scenario environment variables (merged with job-level env).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-scenario tags applied to all metrics emitted by this scenario.
    #[serde(default)]
    pub tags: HashMap<String, String>,
    /// When to start this scenario (e.g. "5s", "30s").
    /// Defaults to "0s" — starts immediately alongside other scenarios.
    /// Use staggered values to sequence scenario start times.
    #[serde(default)]
    pub start_time: String,
    /// k6 `exec` selection — which exported function/flow this scenario runs.
    /// Drivers that support named entry points (e.g. the k6 driver) install
    /// this export as the iteration function; when absent, the script's
    /// `default` export runs. Ignored by declarative (adapter) scenarios.
    #[serde(default)]
    pub exec: Option<String>,
}

/// How to execute the load test.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExecutionConfig {
    #[serde(rename = "constant-vus")]
    ConstantVus {
        vus: u32,
        duration: String,
        /// How long to wait for in-flight iterations to finish after the
        /// test duration expires. Defaults to 30s if not set.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
    #[serde(rename = "ramping-vus")]
    RampingVus {
        stages: Vec<Stage>,
        start_vus: u32,
        /// How long to wait for a VU to finish its current iteration during
        /// a ramp-down stage before moving on. Defaults to 30s.
        #[serde(default, alias = "gracefulRampDown")]
        graceful_ramp_down: Option<String>,
        /// How long to wait for in-flight iterations to finish after the
        /// final stage completes. Defaults to 30s.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
    #[serde(rename = "constant-arrival-rate")]
    ConstantArrivalRate {
        rate: f64,
        time_unit: String,
        duration: String,
        pre_alloc_vus: u32,
        max_vus: u32,
        /// How long to wait for in-flight iterations to finish after the
        /// test duration expires. Defaults to 30s if not set.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
    #[serde(rename = "shared-iterations")]
    SharedIterations {
        iterations: u64,
        max_duration: Option<String>,
        vus: u32,
        /// How long to wait for in-flight iterations to finish after the
        /// iteration budget is exhausted or max_duration is reached.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
    /// Ramping arrival rate — stages of target rate (iterations/sec).
    /// Similar to k6's `ramping-arrival-rate` executor.
    #[serde(rename = "ramping-arrival-rate")]
    RampingArrivalRate {
        /// Starting rate (iterations/sec).
        #[serde(default)]
        start_rate: f64,
        /// Stages defining how the rate changes over time.
        stages: Vec<ArrivalRateStage>,
        /// Time unit for the rate (e.g. "1s").
        #[serde(default = "default_time_unit")]
        time_unit: String,
        /// Pre-allocated VUs.
        #[serde(default = "default_pre_alloc")]
        pre_alloc_vus: u32,
        /// Maximum VUs.
        #[serde(default = "default_max_vus")]
        max_vus: u32,
        /// How long to wait for in-flight iterations to finish after the
        /// test duration expires. Defaults to 30s if not set.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
    /// Each VU runs exactly N iterations independently.
    /// Similar to k6's `per-vu-iterations` executor.
    #[serde(rename = "per-vu-iterations")]
    PerVUIterations {
        /// Number of VUs to spawn.
        vus: u32,
        /// Number of iterations per VU (each VU runs exactly this many).
        iterations: u64,
        /// Optional overall time limit for the test.
        #[serde(default, alias = "maxDuration")]
        max_duration: Option<String>,
        /// How long to wait for in-flight iterations to finish after the
        /// iteration budget is exhausted or max_duration is reached.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
    /// Externally-controlled VUs — the VU count can be adjusted AT RUNTIME
    /// via the control API (k6's `externally-controlled` executor / REST
    /// `/v1/status` parity). Starts with `vus`, may grow up to `max_vus` and
    /// shrink below `vus` as the controller commands. When `duration` is
    /// unset the run continues until the controller (or signal) stops it.
    #[serde(rename = "externally-controlled")]
    ExternallyControlled {
        /// Initial VU count.
        vus: u32,
        /// Maximum VU count the control API may scale up to.
        max_vus: u32,
        /// Optional wall-clock limit. When unset, the run continues until
        /// the control API requests a stop (or a signal / threshold aborts).
        #[serde(default, alias = "duration")]
        duration: Option<String>,
        /// How long to wait for in-flight iterations to finish after a
        /// stop / shrink command. Defaults to 30s.
        #[serde(default, alias = "gracefulStop")]
        graceful_stop: Option<String>,
        /// Think time / pacing configuration between iterations.
        #[serde(default, alias = "thinkTime")]
        think_time: ThinkTimeConfig,
    },
}

impl ExecutionConfig {
    /// k6-style executor type name (matches the serde tag used for this
    /// variant). Exposed to scripts via `exec.scenario.executor()`.
    pub fn executor_name(&self) -> &'static str {
        match self {
            ExecutionConfig::ConstantVus { .. } => "constant-vus",
            ExecutionConfig::RampingVus { .. } => "ramping-vus",
            ExecutionConfig::ConstantArrivalRate { .. } => "constant-arrival-rate",
            ExecutionConfig::SharedIterations { .. } => "shared-iterations",
            ExecutionConfig::RampingArrivalRate { .. } => "ramping-arrival-rate",
            ExecutionConfig::PerVUIterations { .. } => "per-vu-iterations",
            ExecutionConfig::ExternallyControlled { .. } => "externally-controlled",
        }
    }

    /// Estimated wall-clock duration of this executor's PLANNED phase — the
    /// target the live progress bar fills toward, reaching 100% when the
    /// planned phase ends (graceful-stop drain then holds the bar full, k6
    /// style). Grace is deliberately NOT included: an 8s run should show
    /// 8s as its target, not 8s + 30s grace. `None` when the run has no
    /// fixed duration: externally-controlled without a `duration`, or
    /// shared-/per-vu-iterations without a `max_duration`.
    pub fn total_duration(&self) -> Option<std::time::Duration> {
        use std::time::Duration;
        let parse = |s: &str| crate::parse_duration(s).ok();
        match self {
            ExecutionConfig::ConstantVus { duration, .. } => parse(duration),
            ExecutionConfig::RampingVus { stages, .. } => {
                let mut total = Duration::ZERO;
                for st in stages {
                    total += parse(&st.duration)?;
                }
                Some(total)
            }
            ExecutionConfig::ConstantArrivalRate { duration, .. } => parse(duration),
            ExecutionConfig::SharedIterations { max_duration, .. } => {
                max_duration.as_deref().and_then(parse)
            }
            ExecutionConfig::RampingArrivalRate { stages, .. } => {
                let mut total = Duration::ZERO;
                for st in stages {
                    total += parse(&st.duration)?;
                }
                Some(total)
            }
            ExecutionConfig::PerVUIterations { max_duration, .. } => {
                max_duration.as_deref().and_then(parse)
            }
            ExecutionConfig::ExternallyControlled { duration, .. } => {
                duration.as_deref().and_then(parse)
            }
        }
    }

    /// Build an `ExecutionConfig` from a k6-style executor `mode` plus the
    /// load-profile knobs (`vus` / `duration` / `iterations` / `stages`).
    ///
    /// Canonical mode→executor mapping, shared by the CLI (`cli.rs`) and the
    /// k6 env-file builder (`config_file.rs`) so the precedence rules live in
    /// exactly one place. `stages` is the raw JSON array string (if any);
    /// `duration` is the human duration string (if any).
    pub fn from_mode(
        mode: &str,
        vus: Option<u32>,
        duration: Option<String>,
        iterations: Option<u64>,
        stages: Option<String>,
    ) -> Self {
        let think_time = ThinkTimeConfig::default();
        match mode {
            "ramping-vus" => {
                let start_vus = vus.unwrap_or(1);
                let stages_list = stages
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<Stage>>(s).ok())
                    .unwrap_or_else(|| {
                        vec![Stage {
                            duration: duration.clone().unwrap_or_else(|| "30s".to_string()),
                            target: vus.unwrap_or(10),
                        }]
                    });
                ExecutionConfig::RampingVus {
                    stages: stages_list,
                    start_vus,
                    graceful_ramp_down: Some("30s".to_string()),
                    graceful_stop: Some("30s".to_string()),
                    think_time,
                }
            }
            "shared-iterations" => ExecutionConfig::SharedIterations {
                iterations: iterations.unwrap_or(100),
                max_duration: duration,
                vus: vus.unwrap_or(1),
                graceful_stop: Some("30s".to_string()),
                think_time,
            },
            "arrival-rate" | "constant-arrival-rate" => ExecutionConfig::ConstantArrivalRate {
                rate: vus.unwrap_or(1) as f64,
                time_unit: "1s".to_string(),
                duration: duration.unwrap_or_else(|| "30s".to_string()),
                pre_alloc_vus: 1,
                max_vus: vus.unwrap_or(10).max(10),
                graceful_stop: Some("30s".to_string()),
                think_time,
            },
            _ => ExecutionConfig::ConstantVus {
                vus: vus.unwrap_or(1),
                duration: duration.unwrap_or_else(|| "30s".to_string()),
                graceful_stop: Some("30s".to_string()),
                think_time,
            },
        }
    }
}

/// A ramping stage (for VU count — used by RampingVus).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub duration: String,
    pub target: u32,
}

/// A ramping arrival rate stage.
/// The rate linearly interpolates from the previous stage's target (or start_rate)
/// to this stage's target over the stage duration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrivalRateStage {
    pub duration: String,
    pub target: f64,
}

fn default_time_unit() -> String {
    "1s".to_string()
}

fn default_pre_alloc() -> u32 {
    1
}

fn default_max_vus() -> u32 {
    10
}

/// Threshold configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdConfig {
    /// Threshold expression.
    pub expression: String,
    /// Whether to abort the test on failure.
    #[serde(default)]
    pub abort_on_fail: bool,
    /// Grace period before abortOnFail activates (e.g. "30s").
    /// During this time metrics are collected but failures won't abort.
    #[serde(default, alias = "delayAbortEval")]
    pub delay_abort_eval: Option<String>,
}

/// Output/reporter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// Reporters to use (e.g. ["stdout", "json"]).
    pub reporters: Vec<String>,
    /// Output file path (for json/csv reporters).
    pub output_file: Option<String>,
    /// Whether to show detailed summary.
    pub summary: bool,
    /// Whether to show trend statistics.
    pub trends: bool,
    /// Prometheus remote-write endpoint (e.g. `http://localhost:9090`).
    /// When set, samples are streamed to Prometheus via the remote-write API.
    #[serde(default)]
    pub prometheus_remote_write_url: Option<String>,
    /// OTLP/HTTP collector endpoint (e.g. `http://localhost:4318`).
    /// When set, samples are exported to the collector as OTLP metrics.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Path for the `--summary-export` JSON export (k6 semantics).
    ///
    /// When the script declares a `handleSummary(data)` function (k6), the
    /// script's returned file map governs output and this is ignored unless
    /// the script also prints to `stdout`; otherwise the aggregated summary
    /// data object is written here as JSON.
    #[serde(default)]
    pub summary_export: Option<String>,
    /// NDJSON streaming output file: every sample is appended as one JSON
    /// line while the run is in progress (k6 `--out json=file` equivalent).
    #[serde(default)]
    pub json_stream: Option<String>,
    /// StatsD / Datadog agent address (`host:port`, e.g. `localhost:8125`)
    /// for streaming datagram output with Datadog-style tags.
    #[serde(default)]
    pub statsd_addr: Option<String>,
    /// InfluxDB line-protocol UDP address (`host:port`, e.g. `localhost:8089`)
    /// for streaming line-protocol datagrams.
    #[serde(default)]
    pub influxdb_addr: Option<String>,
    /// Tag-key allowlist for network outputs (prometheus/otlp/statsd/
    /// influxdb). Only these tag keys are forwarded; empty (default) forwards
    /// all tags. Bounds label cardinality at the backend.
    #[serde(default)]
    pub tag_allowlist: Vec<String>,
    /// Max tag keys per sample forwarded to network outputs. When a sample
    /// carries more, tags are kept deterministically (sorted by key, first
    /// `cap` kept). `None` (default) = no cap.
    #[serde(default)]
    pub max_tags_per_sample: Option<usize>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            reporters: vec!["stdout".to_string()],
            output_file: None,
            summary: true,
            trends: true,
            prometheus_remote_write_url: None,
            otlp_endpoint: None,
            summary_export: None,
            json_stream: None,
            statsd_addr: None,
            influxdb_addr: None,
            tag_allowlist: Vec::new(),
            max_tags_per_sample: None,
        }
    }
}

impl OutputConfig {
    /// Build the output config dispatched to distributed worker agents.
    ///
    /// The controller owns ALL output — agents must not stream to the same
    /// endpoints/files the controller or other agents use (a shared NDJSON
    /// file written by N processes, or N parallel remote-write pushes).
    /// This constructor nulls every streaming/reporting field in one place,
    /// so adding a new output field can't silently leak into worker configs.
    pub fn into_worker(self) -> Self {
        Self {
            reporters: Vec::new(),
            output_file: None,
            prometheus_remote_write_url: None,
            otlp_endpoint: None,
            summary_export: None,
            json_stream: None,
            statsd_addr: None,
            influxdb_addr: None,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_names_match_serde_tags() {
        use ExecutionConfig::*;
        assert_eq!(
            ConstantVus {
                vus: 1,
                duration: "1s".into(),
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "constant-vus"
        );
        assert_eq!(
            RampingVus {
                stages: vec![],
                start_vus: 1,
                graceful_ramp_down: None,
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "ramping-vus"
        );
        assert_eq!(
            ConstantArrivalRate {
                rate: 1.0,
                time_unit: "1s".into(),
                duration: "1s".into(),
                pre_alloc_vus: 1,
                max_vus: 10,
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "constant-arrival-rate"
        );
        assert_eq!(
            SharedIterations {
                iterations: 10,
                max_duration: None,
                vus: 1,
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "shared-iterations"
        );
        assert_eq!(
            RampingArrivalRate {
                start_rate: 1.0,
                stages: vec![],
                time_unit: "1s".into(),
                pre_alloc_vus: 1,
                max_vus: 10,
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "ramping-arrival-rate"
        );
        assert_eq!(
            PerVUIterations {
                vus: 1,
                iterations: 10,
                max_duration: None,
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "per-vu-iterations"
        );
        assert_eq!(
            ExternallyControlled {
                vus: 1,
                max_vus: 10,
                duration: None,
                graceful_stop: None,
                think_time: Default::default()
            }
            .executor_name(),
            "externally-controlled"
        );
    }

    #[test]
    fn total_duration_sums_ramping_stages_and_handles_grace() {
        use std::time::Duration;
        // Grace is deliberately NOT included (progress bar target).
        let cv = ExecutionConfig::ConstantVus {
            vus: 1,
            duration: "8s".into(),
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        assert_eq!(cv.total_duration(), Some(Duration::from_secs(8)));

        // Ramping-vus sums stage durations.
        let rv = ExecutionConfig::RampingVus {
            stages: vec![
                Stage {
                    duration: "5s".into(),
                    target: 1,
                },
                Stage {
                    duration: "3s".into(),
                    target: 5,
                },
            ],
            start_vus: 1,
            graceful_ramp_down: Some("30s".into()),
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        assert_eq!(rv.total_duration(), Some(Duration::from_secs(8)));

        // Iteration-budget executors without max_duration → None.
        let si = ExecutionConfig::SharedIterations {
            iterations: 100,
            max_duration: None,
            vus: 1,
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        assert_eq!(si.total_duration(), None);

        // Externally-controlled without duration → None.
        let ec = ExecutionConfig::ExternallyControlled {
            vus: 1,
            max_vus: 10,
            duration: None,
            graceful_stop: None,
            think_time: Default::default(),
        };
        assert_eq!(ec.total_duration(), None);
    }

    #[test]
    fn from_mode_maps_k6_modes_with_defaults() {
        // constant-vus (unknown modes fall back here).
        let cv = ExecutionConfig::from_mode("constant-vus", Some(5), None, None, None);
        assert_eq!(cv.executor_name(), "constant-vus");
        assert_eq!(
            cv.total_duration(),
            Some(std::time::Duration::from_secs(30))
        );

        // ramping-vus: stages JSON wins when present; start_vus defaults to 1.
        let rv = ExecutionConfig::from_mode(
            "ramping-vus",
            Some(3),
            Some("1m".into()),
            None,
            Some(r#"[{"duration":"10s","target":50}]"#.into()),
        );
        match &rv {
            ExecutionConfig::RampingVus {
                stages, start_vus, ..
            } => {
                assert_eq!(stages.len(), 1);
                assert_eq!(stages[0].target, 50);
                assert_eq!(*start_vus, 3);
            }
            _ => panic!("expected ramping-vus"),
        }

        // arrival-rate: rate from vus, max_vus floors at 10.
        let ar = ExecutionConfig::from_mode("arrival-rate", Some(4), None, None, None);
        match &ar {
            ExecutionConfig::ConstantArrivalRate { rate, max_vus, .. } => {
                assert_eq!(*rate, 4.0);
                assert_eq!(*max_vus, 10);
            }
            _ => panic!("expected constant-arrival-rate"),
        }

        // shared-iterations: iterations default 100, vus default 1.
        let si = ExecutionConfig::from_mode("shared-iterations", None, None, Some(500), None);
        match &si {
            ExecutionConfig::SharedIterations {
                iterations, vus, ..
            } => {
                assert_eq!(*iterations, 500);
                assert_eq!(*vus, 1);
            }
            _ => panic!("expected shared-iterations"),
        }
    }

    #[test]
    fn output_into_worker_nulls_streaming_fields() {
        let cfg = OutputConfig {
            reporters: vec!["stdout".into(), "json".into()],
            output_file: Some("out.json".into()),
            summary_export: Some("summary.json".into()),
            json_stream: Some("stream.ndjson".into()),
            statsd_addr: Some("localhost:8125".into()),
            influxdb_addr: Some("localhost:8089".into()),
            ..Default::default()
        };
        let worker = cfg.into_worker();
        assert!(worker.reporters.is_empty());
        assert!(worker.output_file.is_none());
        assert!(worker.summary_export.is_none());
        assert!(worker.json_stream.is_none());
        assert!(worker.statsd_addr.is_none());
        assert!(worker.influxdb_addr.is_none());
        // Non-streaming fields survive (defaults copied by ..self).
        assert!(worker.summary);
    }
}
