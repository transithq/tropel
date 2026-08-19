//! # JSON-stream streaming output
//!
//! Appends every sample as NDJSON to a file while the run is in progress —
//! the k6 `--out json=file` equivalent, emitting **byte-compatible k6
//! records**:
//!
//! - a `Metric` definition record (`{"type":"Metric","data":{"name",
//!   "type","contains","thresholds","submetrics"}}`) the first time each
//!   metric is seen, carrying the k6 metric type
//!   (`counter`/`gauge`/`rate`/`trend`) and `contains` (`time` for duration
//!   metrics, else `default`);
//! - a `Point` record (`{"type":"Point","data":{"time","metric","tags",
//!   "value"}}`) per sample with RFC 3339 (nanosecond) `time`, the metric
//!   name under **`metric`**, `tags`, and the value under **`value`** — k6's
//!   exact Point schema (not the InfluxDB point shape, which puts the name
//!   under `measurement` and the value under `fields.value`).
//!
//! Lines are buffered and written every `FLUSH_INTERVAL` (or when the
//! buffer exceeds `MAX_BUFFERED_SAMPLES`), with a final drain on stream
//! close. Write failures are logged, never fatal to the run.

use async_trait::async_trait;
use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;
use tropel_sdk::types::{Sample, SampleType};
use tropel_sdk::{Result, TropelError};

use crate::Output;

/// How often buffered samples are written to the file.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
/// Max buffered samples before a forced write.
const MAX_BUFFERED_SAMPLES: usize = 10_000;

/// NDJSON streaming output writing to `path`.
pub struct JsonStreamOutput {
    path: String,
    /// Buffered, serialized lines (plain UTF-8 strings — no shared state
    /// with the Sample type needed since we serialize immediately).
    buffer: Mutex<Vec<String>>,
    total_buffered: AtomicUsize,
    /// Metric names already emitted as a `Metric` definition record. Each
    /// metric's definition is written once, before its first `Point`.
    seen_metrics: Mutex<HashSet<String>>,
}

impl JsonStreamOutput {
    /// Create a JSON-stream output writing to `path`.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            buffer: Mutex::new(Vec::new()),
            total_buffered: AtomicUsize::new(0),
            seen_metrics: Mutex::new(HashSet::new()),
        }
    }

    /// Spawn a consumer task that appends samples as NDJSON lines.
    /// Returns a `JoinHandle` that completes when the stream closes.
    pub fn spawn(mut rx: broadcast::Receiver<Sample>, path: String) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let output = JsonStreamOutput::new(path);
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    res = rx.recv() => match res {
                        Ok(sample) => {
                            output.buffer(&sample);
                            if output.total_buffered.load(Ordering::Relaxed) >= MAX_BUFFERED_SAMPLES {
                                if let Err(e) = output.flush_buffered() {
                                    tracing::warn!("json-stream write failed: {e}");
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tropel_metrics::OUTPUT_SAMPLES_DROPPED.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!("json-stream dropped {n} samples (consumer lag)");
                        }
                    },
                    _ = tick.tick() => {
                        if output.total_buffered.load(Ordering::Relaxed) > 0 {
                            if let Err(e) = output.flush_buffered() {
                                tracing::warn!("json-stream write failed: {e}");
                            }
                        }
                    }
                }
            }

            if let Err(e) = output.flush_buffered() {
                tracing::warn!("json-stream final write failed: {e}");
            }
        })
    }

    /// Serialize a sample into k6-compatible NDJSON lines and buffer them.
    ///
    /// Emits the metric's `Metric` definition record the first time the
    /// metric is seen, then a `Point` record for this sample. The schema
    /// mirrors k6's `--out json` so consumers (e.g. k6's JSON parser, custom
    /// dashboards) can read the file unchanged.
    fn buffer(&self, sample: &Sample) {
        let metric_name = sample.metric.clone();
        let seen = {
            let mut seen = self.seen_metrics.lock().unwrap();
            !seen.insert(metric_name.to_string())
        };
        if !seen {
            // k6 Metric definition record — emitted once per metric.
            // k6 puts `metric` at the top level; tropel adds it for
            // jq/select compatibility (`jq 'select(.metric=="http_req_duration")'`).
            let def = serde_json::json!({
                "type": "Metric",
                "metric": metric_name,
                "data": {
                    "name": sample.metric,
                    "type": k6_metric_type(&sample.sample_type),
                    "contains": tropel_metrics::time_metrics::unit_of(&metric_name).as_str(),
                    "thresholds": [],
                    "submetrics": null,
                },
            })
            .to_string();
            self.buffer.lock().unwrap().push(def);
            self.total_buffered.fetch_add(1, Ordering::Relaxed);
        }

        // k6 Point record — one per sample. k6 puts `metric` at the
        // top level; `data.metric` is kept for backward compatibility.
        let point = serde_json::json!({
            "type": "Point",
            "metric": sample.metric,
            "data": {
                "time": k6_timestamp(sample.timestamp),
                "tags": sample.tags,
                "value": sample.value,
            },
        })
        .to_string();
        self.buffer.lock().unwrap().push(point);
        self.total_buffered.fetch_add(1, Ordering::Relaxed);
    }

    /// Drain the buffer and append the lines to the file.
    fn flush_buffered(&self) -> Result<()> {
        let lines = {
            let mut guard = self.buffer.lock().unwrap();
            let taken = std::mem::take(&mut *guard);
            self.total_buffered.store(0, Ordering::Relaxed);
            taken
        };
        if lines.is_empty() {
            return Ok(());
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| {
                TropelError::Report(format!("json-stream open '{}' failed: {e}", self.path))
            })?;
        for line in &lines {
            writeln!(file, "{line}")
                .map_err(|e| TropelError::Report(format!("json-stream write failed: {e}")))?;
        }
        Ok(())
    }
}

/// k6 metric type name for a sample type.
fn k6_metric_type(sample_type: &SampleType) -> &'static str {
    match sample_type {
        SampleType::Counter => "counter",
        // Gauge metrics are emitted as Point samples (snapshots).
        SampleType::Point => "gauge",
        SampleType::Rate => "rate",
        SampleType::Trend => "trend",
    }
}

/// k6 RFC 3339 timestamp (nanosecond precision, UTC), e.g.
/// `2026-08-03T12:34:56.123456789Z`.
fn k6_timestamp(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// k6's `contains` unit for a metric ("time"/"data"/"default"). Delegates
/// to the tropel-metrics registry+heuristic — the SINGLE source of truth
/// (backlog §0/§32) — so json-stream, stdout, and handleSummary always
/// agree on which metric carries which unit.
pub(crate) fn metric_unit(metric: &str) -> tropel_metrics::MetricUnit {
    tropel_metrics::time_metrics::unit_of(metric)
}

#[async_trait]
impl Output for JsonStreamOutput {
    fn name(&self) -> &str {
        "json-stream"
    }

    async fn emit(&self, samples: &[Sample]) -> Result<()> {
        for sample in samples {
            self.buffer(sample);
        }
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        self.flush_buffered()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use tropel_sdk::types::{Sample, SampleType, TagMap};

    fn sample(metric: &str, value: f64) -> Sample {
        let mut tags = TagMap::new();
        tags.insert("status", "200");
        Sample {
            metric: std::borrow::Cow::Owned(metric.to_string()),
            value,
            tags: std::sync::Arc::new(tags),
            timestamp: SystemTime::now(),
            sample_type: if metric == "http_reqs" {
                SampleType::Counter
            } else {
                SampleType::Trend
            },
        }
    }

    #[test]
    fn flush_appends_k6_ndjson() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "tropel-json-stream-{}-flush.ndjson",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let output = JsonStreamOutput::new(path.to_string_lossy().to_string());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            output.emit(&[sample("http_reqs", 1.0)]).await.unwrap();
            output
                .emit(&[sample("http_req_duration", 12.5)])
                .await
                .unwrap();
            output
                .emit(&[sample("http_req_duration", 14.0)])
                .await
                .unwrap();
            output.flush().await.unwrap();
        });

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // 2 Metric definitions (http_reqs, http_req_duration) + 3 Points.
        assert_eq!(
            lines.len(),
            5,
            "defs once + one point per sample: {content}"
        );
        let mut metric_defs = 0;
        let mut points = 0;
        let mut duration_points = 0;
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            match v["type"].as_str().unwrap() {
                "Metric" => {
                    metric_defs += 1;
                    let data = &v["data"];
                    let name = data["name"].as_str().unwrap();
                    assert!(data["type"].is_string());
                    // k6's Metric record carries NO timestamp — RFC 3339
                    // time lives only on Point records.
                    assert!(data.get("time").is_none(), "Metric def has no time");
                    if name == "http_req_duration" {
                        assert_eq!(data["contains"], "time");
                    } else {
                        assert_eq!(data["contains"], "default");
                    }
                }
                "Point" => {
                    points += 1;
                    let data = &v["data"];
                    // k6 Point schema: `metric` at top level for jq compatibility.
                    assert!(v["metric"].is_string(), "metric at top level");
                    assert!(data["time"].is_string());
                    assert!(data["value"].is_number());
                    assert!(
                        data.get("measurement").is_none(),
                        "no InfluxDB measurement key"
                    );
                    assert!(data.get("fields").is_none(), "no InfluxDB fields wrapper");
                    assert!(data["tags"]["status"] == "200");
                    if v["metric"] == "http_req_duration" {
                        duration_points += 1;
                    }
                }
                other => panic!("unexpected record type {other}"),
            }
        }
        assert_eq!(metric_defs, 2);
        assert_eq!(points, 3);
        assert_eq!(duration_points, 2, "no duplicate Metric def per metric");
        let _ = std::fs::remove_file(&path);
    }

    /// Regression (backlog line 97): the Point record must match k6's
    /// `--out json` schema — metric name under `metric`, value directly under
    /// `value`, RFC 3339 `time`, `tags` — so `jq '.data.value'` pipelines and
    /// k6/Grafana JSON ingests read real numbers instead of null.
    #[test]
    fn point_record_matches_k6_schema() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "tropel-json-stream-{}-schema.ndjson",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let output = JsonStreamOutput::new(path.to_string_lossy().to_string());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            output
                .emit(&[sample("http_req_duration", 12.5)])
                .await
                .unwrap();
            output.flush().await.unwrap();
        });

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "Metric def + Point: {content}");

        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["type"], "Metric");
        // k6 puts `metric` at top level for jq compatibility.
        assert_eq!(v["metric"], "http_req_duration");
        // Exactly k6's Metric data keys — no InfluxDB-ish extras.
        let mut keys: Vec<&str> = v["data"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        let mut expected = vec!["contains", "name", "submetrics", "thresholds", "type"];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(v["data"]["name"], "http_req_duration");
        assert_eq!(v["data"]["type"], "trend");
        assert_eq!(v["data"]["contains"], "time");

        let v: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v["type"], "Point");
        // k6 puts `metric` at top level for jq compatibility.
        assert_eq!(v["metric"], "http_req_duration");
        let data = &v["data"];
        assert_eq!(data["value"], 12.5);
        assert!(
            data["time"].as_str().unwrap().ends_with('Z'),
            "RFC 3339 UTC"
        );
        assert_eq!(data["tags"]["status"], "200");
        // The InfluxDB shape must be gone — this is what every consumer reads.
        assert!(data.get("measurement").is_none());
        assert!(data.get("fields").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_flush_is_noop() {
        let output = JsonStreamOutput::new("/nonexistent-dir/x.ndjson");
        assert!(
            output.flush_buffered().is_ok(),
            "empty flush must not touch the file"
        );
    }
}
