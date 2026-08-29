//! # OTLP/HTTP streaming output
//!
//! Exports samples to an OpenTelemetry Collector over OTLP/HTTP
//! (e.g. `http://localhost:4318/v1/metrics`).
//!
//! ## Encoding (TR-304)
//!
//! Two encodings ship, selected once at construction by
//! [`OtlpProtocol::from_env`]:
//!
//! - **protobuf** (`application/x-protobuf`) — the default. This is the
//!   encoding the OTLP spec requires every OTLP/HTTP receiver to support;
//!   JSON is optional there, so protobuf is the *more* interoperable of the
//!   two, and it is dramatically cheaper to produce (see below).
//! - **JSON** (`application/json`) — opt in with `TROPEL_OTLP_PROTOCOL=json`
//!   for a receiver that only speaks JSON.
//!
//! The JSON encoder builds a `serde_json::Value` tree — a `serde_json::Map`
//! per data point and per attribute, with an owned `String` for every tag
//! key and value — and then serialises it. At the budgeted rate that
//! dominates the flush: measured at **123.9 ms of CPU per 100 ms window**
//! (10 000 samples across 100 tagged series, encode + gzip, release, Apple
//! Silicon M-series), against a 20 ms budget. It could not keep pace with
//! its own window. The protobuf encoder writes borrowed `&str` straight onto
//! a `Vec<u8>` and allocates nothing per attribute.
//!
//! Both encodings are gzip-compressed (`Content-Encoding: gzip`) and carry
//! identical semantics — same DELTA Sum aggregation, same
//! `startTimeUnixNano`, same resource/scope attributes.
//!
//! Samples are buffered per metric name and flushed every `FLUSH_INTERVAL`
//! (or when the buffer exceeds `MAX_BUFFERED_SAMPLES`), with a final flush
//! on stream close. Metric type mapping follows the OTLP conventions:
//! - `Counter` samples → monotonic `Sum` with **DELTA** temporality: each
//!   data point is the increment since the last export (aggregated per
//!   tag-set within a flush window). CUMULATIVE would be wrong here — the
//!   raw values are per-event deltas, not running totals from process start.
//! - `Point` / `Gauge` samples → `Gauge`
//! - `Rate` samples → `Gauge` (a rate is a point-in-time ratio)
//! - `Trend` samples → each sample becomes a `Gauge` data point (raw
//!   observations; percentile summarization is left to the backend)
//!
//! Each data point carries the sample's tags as OTLP attributes, and a
//! `service.name` resource attribute identifies the exporter.

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::broadcast;
use tropel_sdk::types::{Sample, SampleType};
use tropel_sdk::{Result, TropelError};

use crate::otlp_proto::build_export_request_protobuf;
use crate::output::TagPolicy;
use crate::Output;

/// How often buffered samples are exported to the collector.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Max buffered samples before a forced export.
const MAX_BUFFERED_SAMPLES: usize = 10_000;

/// Environment variable selecting the OTLP/HTTP encoding.
///
/// This is an env var rather than an `OutputConfig` field because
/// `OutputConfig` lives in the pinned `tropel-sdk` submodule, which this
/// change must not move.
pub const OTLP_PROTOCOL_ENV: &str = "TROPEL_OTLP_PROTOCOL";

/// Wire encoding used for the `ExportMetricsServiceRequest` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtlpProtocol {
    /// Binary protobuf, `Content-Type: application/x-protobuf`. The default,
    /// and the encoding every conformant OTLP/HTTP receiver must accept.
    #[default]
    Protobuf,
    /// OTLP/JSON, `Content-Type: application/json`.
    Json,
}

impl OtlpProtocol {
    /// Read the protocol from [`OTLP_PROTOCOL_ENV`], defaulting to protobuf.
    ///
    /// Called **once**, at output construction — never per flush. A
    /// `std::env::var` on a hot path takes the process-global environment
    /// lock; TR-306 is the cautionary tale (two `env::var` calls per gRPC
    /// request capped the whole process).
    ///
    /// An unrecognised value falls back to the default and warns rather than
    /// failing the run: an output misconfiguration must not take down a load
    /// test, but it must not be silent either.
    pub fn from_env() -> Self {
        match std::env::var(OTLP_PROTOCOL_ENV) {
            Ok(v) => Self::parse(&v).unwrap_or_else(|| {
                tracing::warn!(
                    "{OTLP_PROTOCOL_ENV}={v:?} is not one of protobuf|json; \
                     using protobuf"
                );
                Self::Protobuf
            }),
            Err(_) => Self::Protobuf,
        }
    }

    /// Parse a protocol name. `None` for anything unrecognised — never a
    /// silent fallback at this level (see the `unwrap_or` rule in
    /// `CONVENTIONS.md`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "protobuf" | "proto" | "pb" | "grpc" | "http/protobuf" => Some(Self::Protobuf),
            "json" | "http/json" => Some(Self::Json),
            _ => None,
        }
    }

    /// The `Content-Type` this encoding ships under.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Protobuf => "application/x-protobuf",
            Self::Json => "application/json",
        }
    }
}

/// Encode buffered metrics as an uncompressed OTLP
/// `ExportMetricsServiceRequest` body in `protocol`.
///
/// This is the function the flush path calls and the function the
/// `otlp_encode_100ms_window` benchmark measures — the benchmark exercises
/// production code, not a `#[cfg(test)]` twin.
pub fn encode_export_body(
    metrics: &HashMap<String, Vec<Sample>>,
    protocol: OtlpProtocol,
) -> Vec<u8> {
    match protocol {
        OtlpProtocol::Protobuf => build_export_request_protobuf(metrics),
        OtlpProtocol::Json => build_export_request(metrics).to_string().into_bytes(),
    }
}

/// gzip `body` at the flush path's compression level.
///
/// Shared with the benchmark so the measured window includes exactly the
/// compression the wire pays for.
pub fn gzip_body(body: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(body.len() / 4),
        flate2::Compression::fast(),
    );
    enc.write_all(body)
        .and_then(|_| enc.finish())
        .map_err(|e| TropelError::Other(format!("otlp gzip failed: {e}")))
}

/// OTLP/HTTP metrics output.
///
/// Create one with [`OtlpOutput::new`] and either drive it through the
/// [`Output`] trait or spawn the engine-facing consumer task with
/// [`OtlpOutput::spawn`].
pub struct OtlpOutput {
    /// Base endpoint (e.g. `http://localhost:4318`). `/v1/metrics` is
    /// appended when missing.
    endpoint: String,
    client: reqwest::Client,
    /// Buffered samples grouped by metric name.
    metrics: Mutex<HashMap<String, Vec<Sample>>>,
    total_buffered: AtomicUsize,
    /// Tag forwarding policy (allowlist + cardinality cap).
    tag_policy: TagPolicy,
    /// Wire encoding, resolved once at construction (never per flush).
    protocol: OtlpProtocol,
}

impl OtlpOutput {
    /// Create a new OTLP output pushing to `endpoint` (base URL or full
    /// `/v1/metrics` path).
    ///
    /// The encoding comes from [`OtlpProtocol::from_env`] — protobuf unless
    /// `TROPEL_OTLP_PROTOCOL=json` is set.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: normalize_metrics_url(&endpoint.into()),
            client: reqwest::Client::new(),
            metrics: Mutex::new(HashMap::new()),
            total_buffered: AtomicUsize::new(0),
            tag_policy: TagPolicy::default(),
            protocol: OtlpProtocol::from_env(),
        }
    }

    /// Set the tag forwarding policy (allowlist + cardinality cap).
    pub fn with_tag_policy(mut self, policy: TagPolicy) -> Self {
        self.tag_policy = policy;
        self
    }

    /// Override the wire encoding, ignoring the environment.
    pub fn with_protocol(mut self, protocol: OtlpProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// Spawn a consumer task that exports samples to the collector.
    pub fn spawn(
        mut rx: broadcast::Receiver<Sample>,
        endpoint: String,
        tag_policy: TagPolicy,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let output = OtlpOutput::new(endpoint).with_tag_policy(tag_policy);
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    res = rx.recv() => match res {
                        Ok(sample) => {
                            output.buffer(sample);
                            if output.total_buffered.load(Ordering::Relaxed) >= MAX_BUFFERED_SAMPLES {
                                if let Err(e) = output.flush_buffered().await {
                                    tracing::warn!("otlp export failed: {e}");
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tropel_metrics::OUTPUT_SAMPLES_DROPPED.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!("otlp output dropped {n} samples (consumer lag)");
                        }
                    },
                    _ = tick.tick() => {
                        if output.total_buffered.load(Ordering::Relaxed) > 0 {
                            if let Err(e) = output.flush_buffered().await {
                                tracing::warn!("otlp export failed: {e}");
                            }
                        }
                    }
                }
            }

            if let Err(e) = output.flush_buffered().await {
                tracing::warn!("otlp final export failed: {e}");
            }
        })
    }

    fn buffer(&self, mut sample: Sample) {
        sample.tags = std::sync::Arc::new(self.tag_policy.apply(&sample.tags));
        self.metrics
            .lock()
            .unwrap()
            .entry(sample.metric.to_string())
            .or_default()
            .push(sample);
        self.total_buffered.fetch_add(1, Ordering::Relaxed);
    }

    /// Drain the buffer, build an `ExportMetricsServiceRequest` JSON payload,
    /// and POST it to `/v1/metrics`. Non-2xx responses are logged, not fatal.
    ///
    /// P1 line 335: the CPU-heavy payload build (tag expansion, JSON
    /// serialization) runs on spawn_blocking to avoid parking the async
    /// worker while the aggregator needs to drain its channel.
    async fn flush_buffered(&self) -> Result<()> {
        let metrics = {
            let mut guard = self.metrics.lock().unwrap();
            let taken = std::mem::take(&mut *guard);
            // Reset the counter inside the lock (see prometheus.rs flush for
            // the race this closes).
            self.total_buffered.store(0, Ordering::Relaxed);
            taken
        };
        if metrics.is_empty() {
            return Ok(());
        }

        // Move the CPU-heavy payload build off the async worker.
        // TR-304: encode as protobuf (default) and gzip. The old code built a
        // `serde_json::Value` tree and serialised it — 123.9 ms of CPU per
        // 100 ms window at the budgeted rate, i.e. the output could not keep
        // pace with its own window. gzip stays on for both encodings.
        let protocol = self.protocol;
        let body_gz = tokio::task::spawn_blocking(move || {
            gzip_body(&encode_export_body(&metrics, protocol))
        })
        .await
        .map_err(|e| TropelError::Other(format!("otlp payload build panicked: {e}")))??;

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", self.protocol.content_type())
            .header("Content-Encoding", "gzip")
            .body(body_gz)
            .send()
            .await
            .map_err(|e| TropelError::Http(format!("otlp POST failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("otlp collector rejected ({status}): {body}");
        }
        Ok(())
    }
}

#[async_trait]
impl Output for OtlpOutput {
    fn name(&self) -> &str {
        "otlp"
    }

    async fn emit(&self, samples: &[Sample]) -> Result<()> {
        for sample in samples {
            self.buffer(sample.clone());
        }
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        self.flush_buffered().await
    }
}

/// Append `/v1/metrics` to a bare base endpoint.
fn normalize_metrics_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/v1/metrics") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/metrics")
    }
}

/// Build an OTLP/HTTP JSON `ExportMetricsServiceRequest` from buffered metrics.
///
/// Counters are aggregated per (metric, tag-set) within the flush window and
/// reported with **DELTA** temporality (`aggregationTemporality: 1`): each
/// data point is the sum of events since the last export. CUMULATIVE (2)
/// would be wrong — raw counter samples are per-event increments, not
/// running totals from process start, and a collector would interpret each
/// as a cumulative total.
///
/// Kept reachable for receivers that only speak OTLP/JSON
/// (`TROPEL_OTLP_PROTOCOL=json`).
///
/// `pub` so the benchmark harness can measure the REAL encode path — this
/// exact function, against
/// [`crate::otlp_proto::build_export_request_protobuf`], rather than a
/// re-implementation of either. The `otlp_per_window_cpu` bench used to gzip
/// a synthetic string and never touch this function, which made its "18 ms
/// per 100 ms window" number unrelated to the code it claimed to measure
/// (TR-002).
pub fn build_export_request(metrics: &HashMap<String, Vec<Sample>>) -> serde_json::Value {
    let mut metric_values = Vec::with_capacity(metrics.len());

    for (name, samples) in metrics {
        let is_counter = samples.iter().any(|s| s.sample_type == SampleType::Counter);

        let data_points: Vec<serde_json::Value> = if is_counter {
            // Sum per (sorted tag-set). Keep the LAST timestamp seen for a
            // tag-set so the delta point carries the newest time.
            // P-D.2: Use HashMap for O(1) lookup instead of O(n²) linear scan.
            // Track the earliest timestamp for startTimeUnixNano.
            let mut earliest_ts: u64 = u64::MAX;
            let mut per_tags: std::collections::HashMap<Vec<(String, String)>, (f64, u64)> =
                std::collections::HashMap::new();
            for s in samples {
                let mut tags: Vec<(String, String)> = s
                    .tags
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                tags.sort();
                let ts_nanos = s
                    .timestamp
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                if ts_nanos < earliest_ts {
                    earliest_ts = ts_nanos;
                }
                match per_tags.get_mut(&tags) {
                    Some((sum, ts)) => {
                        *sum += s.value;
                        *ts = ts_nanos;
                    }
                    None => {
                        per_tags.insert(tags, (s.value, ts_nanos));
                    }
                }
            }
            per_tags
                .into_iter()
                .map(|(tags, (sum, ts))| {
                    let attrs: Vec<serde_json::Value> = tags
                        .iter()
                        .map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } }))
                        .collect();
                    // P2 line 175: include startTimeUnixNano for DELTA
                    // Sums. Without it, the Collector's deltatocumulative/
                    // Prometheus exporters drop the point silently.
                    json!({
                        "startTimeUnixNano": if earliest_ts != u64::MAX { earliest_ts.to_string() } else { ts.to_string() },
                        "timeUnixNano": ts.to_string(),
                        "asDouble": sum,
                        "attributes": attrs,
                    })
                })
                .collect()
        } else {
            // Gauge: one data point per raw observation (unchanged).
            samples
                .iter()
                .map(|s| {
                    let attrs: Vec<serde_json::Value> = s
                        .tags
                        .iter()
                        .map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } }))
                        .collect();
                    let ts_nanos = s
                        .timestamp
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                        .to_string();
                    json!({
                        "timeUnixNano": ts_nanos,
                        "asDouble": s.value,
                        "attributes": attrs,
                    })
                })
                .collect()
        };

        let value_field = if is_counter {
            json!({
                "sum": {
                    "dataPoints": data_points,
                    "aggregationTemporality": 1, // DELTA
                    "isMonotonic": true,
                }
            })
        } else {
            json!({ "gauge": { "dataPoints": data_points } })
        };

        let mut metric_obj = serde_json::Map::new();
        metric_obj.insert("name".into(), json!(name));
        metric_obj.insert("description".into(), json!(""));
        metric_obj.insert("unit".into(), json!(""));
        if let Some(obj) = value_field.as_object() {
            for (k, v) in obj {
                metric_obj.insert(k.clone(), v.clone());
            }
        }
        metric_values.push(serde_json::Value::Object(metric_obj));
    }

    json!({
        "resourceMetrics": [
            {
                "resource": {
                    "attributes": [
                        { "key": "service.name", "value": { "stringValue": "tropel" } }
                    ]
                },
                "scopeMetrics": [
                    {
                        "scope": { "name": "tropel" },
                        "metrics": metric_values,
                    }
                ],
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use tropel_sdk::types::{Sample, SampleType, TagMap};

    fn sample(metric: &str, value: f64, sample_type: SampleType) -> Sample {
        let mut tags = TagMap::new();
        tags.insert("status", "200");
        sample_typed(metric, value, sample_type, tags)
    }

    fn sample_typed(metric: &str, value: f64, sample_type: SampleType, tags: TagMap) -> Sample {
        Sample {
            metric: std::borrow::Cow::Owned(metric.to_string()),
            value,
            tags: std::sync::Arc::new(tags),
            timestamp: SystemTime::now(),
            sample_type,
        }
    }

    #[test]
    fn export_request_structure() {
        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_req_duration".to_string(),
            vec![sample("http_req_duration", 12.5, SampleType::Trend)],
        );
        metrics.insert(
            "http_reqs".to_string(),
            vec![sample("http_reqs", 1.0, SampleType::Counter)],
        );

        let req = build_export_request(&metrics);
        let s = req.to_string();

        // Resource attribute present.
        assert!(s.contains("service.name"));

        // Trend → gauge.
        assert!(s.contains("\"http_req_duration\""));
        assert!(s.contains("\"gauge\""));
        assert!(s.contains("\"asDouble\":12.5") || s.contains("\"asDouble\": 12.5"));

        // Counter → monotonic sum with DELTA temporality (per-event values
        // are increments since the last export, not cumulative totals).
        assert!(s.contains("\"http_reqs\""));
        assert!(s.contains("\"sum\""));
        assert!(s.contains("\"isMonotonic\":true") || s.contains("\"isMonotonic\": true"));
        assert!(s.contains("\"aggregationTemporality\":1"));

        // Tags → attributes.
        assert!(s.contains("status"));
    }

    #[test]
    fn counter_aggregates_per_tag_set_with_delta() {
        // Regression for the temporality bug: raw counter samples are
        // per-event deltas, so they must be SUMMED per tag-set and reported
        // with DELTA temporality — never CUMULATIVE with per-event values.
        let mut tags_a = TagMap::new();
        tags_a.insert("status", "200");
        let mut tags_b = TagMap::new();
        tags_b.insert("status", "500");

        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_reqs".to_string(),
            vec![
                sample_typed("http_reqs", 1.0, SampleType::Counter, tags_a.clone()),
                sample_typed("http_reqs", 1.0, SampleType::Counter, tags_a.clone()),
                sample_typed("http_reqs", 1.0, SampleType::Counter, tags_b.clone()),
            ],
        );

        let req = build_export_request(&metrics);
        let s = req.to_string();

        // DELTA temporality, monotonic sum.
        assert!(s.contains("\"aggregationTemporality\":1"));
        assert!(s.contains("\"isMonotonic\":true") || s.contains("\"isMonotonic\": true"));

        // Exactly two data points: status=200 summed to 2.0, status=500 to 1.0.
        assert_eq!(
            s.matches("\"timeUnixNano\":").count(),
            2,
            "one aggregated delta point per tag-set, got: {s}"
        );
        assert!(s.contains("\"asDouble\":2.0") || s.contains("\"asDouble\": 2.0"));
        assert!(s.contains("\"asDouble\":1.0") || s.contains("\"asDouble\": 1.0"));
        // Both tag keys and both values appear in the attribute sets.
        assert!(s.contains("\"status\""));
        assert!(s.contains("\"200\"") && s.contains("\"500\""));
    }

    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize_metrics_url("http://localhost:4318"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(
            normalize_metrics_url("http://localhost:4318/"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(
            normalize_metrics_url("http://host:4318/v1/metrics"),
            "http://host:4318/v1/metrics"
        );
    }

    /// Read one HTTP request off a fresh loopback listener and return
    /// `(head, body)`.
    async fn capture_one_request(
        listener: tokio::net::TcpListener,
    ) -> tokio::task::JoinHandle<(String, Vec<u8>)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 2048];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") && {
                    let head = String::from_utf8_lossy(&buf);
                    let content_length: usize = head
                        .lines()
                        .find_map(|l| {
                            let l = l.trim();
                            let lower = l.to_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    buf.len() >= head_end + content_length
                } {
                    break;
                }
            }
            let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let body = buf[head_end..].to_vec();
            let resp =
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK".to_string();
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            (head, body)
        })
    }

    fn gunzip(b: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(b);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        out
    }

    /// End-to-end on the DEFAULT path: buffer samples, export to a live TCP
    /// server, and verify what actually left the process is gzipped OTLP
    /// **protobuf** under `Content-Type: application/x-protobuf`.
    ///
    /// FAILS ON PRE-FIX CODE. This is the inversion of the old
    /// `flush_posts_to_endpoint`, which asserted the body parsed as
    /// `serde_json` under `application/json` — it pinned the JSON wire as
    /// correct, which is exactly what TR-304 changes. Run against the
    /// pre-fix encoder, the `application/x-protobuf` assertion fails on the
    /// header and the first body byte is `{` (0x7b), not a protobuf tag.
    #[tokio::test]
    async fn flush_posts_protobuf_by_default() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = capture_one_request(listener).await;

        // Explicit, not env-derived: cargo runs tests in one process, so a
        // test that mutated TROPEL_OTLP_PROTOCOL would race its siblings.
        let output = OtlpOutput::new(format!("http://{addr}")).with_protocol(OtlpProtocol::Protobuf);
        output
            .emit(&[sample("http_reqs", 1.0, SampleType::Counter)])
            .await
            .unwrap();
        output.flush().await.unwrap();

        let (head, received) = server.await.unwrap();
        let head_lower = head.to_lowercase();
        assert!(
            head_lower.contains("content-type: application/x-protobuf"),
            "default flush must declare protobuf, got head:\n{head}"
        );
        assert!(head_lower.contains("content-encoding: gzip"), "head:\n{head}");

        let body = gunzip(&received);
        assert_ne!(body.first(), Some(&b'{'), "body is still JSON text");
        // ExportMetricsServiceRequest.resource_metrics = field 1, wire type 2.
        assert_eq!(body[0], 0x0a, "first byte is not the resource_metrics tag");
        // The metric name survives the round trip on the wire.
        assert!(
            body.windows(9).any(|w| w == b"http_reqs"),
            "metric name missing from the protobuf body"
        );
        assert!(
            body.windows(12).any(|w| w == b"service.name"),
            "resource attribute missing from the protobuf body"
        );
    }

    /// The JSON encoding stays reachable for receivers that only speak it.
    ///
    /// FAILS ON PRE-FIX CODE: `OtlpProtocol` / `with_protocol` did not exist,
    /// so there was no way to *choose* JSON — it was the only wire.
    #[tokio::test]
    async fn json_protocol_still_reachable() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = capture_one_request(listener).await;

        let output = OtlpOutput::new(format!("http://{addr}")).with_protocol(OtlpProtocol::Json);
        output
            .emit(&[sample("http_reqs", 1.0, SampleType::Counter)])
            .await
            .unwrap();
        output.flush().await.unwrap();

        let (head, received) = server.await.unwrap();
        assert!(
            head.to_lowercase()
                .contains("content-type: application/json"),
            "head:\n{head}"
        );
        let text = String::from_utf8(gunzip(&received)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let metrics = &json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        assert!(metrics.is_array() && !metrics.as_array().unwrap().is_empty());
        assert!(json["resourceMetrics"][0]["resource"]["attributes"]
            .as_array()
            .is_some());
    }

    /// The env toggle resolves the documented spellings and refuses the rest.
    ///
    /// FAILS ON PRE-FIX CODE: `OtlpProtocol` did not exist.
    #[test]
    fn protocol_parse_and_content_type() {
        assert_eq!(OtlpProtocol::parse("json"), Some(OtlpProtocol::Json));
        assert_eq!(OtlpProtocol::parse("  JSON "), Some(OtlpProtocol::Json));
        assert_eq!(OtlpProtocol::parse("http/json"), Some(OtlpProtocol::Json));
        assert_eq!(
            OtlpProtocol::parse("protobuf"),
            Some(OtlpProtocol::Protobuf)
        );
        assert_eq!(
            OtlpProtocol::parse("http/protobuf"),
            Some(OtlpProtocol::Protobuf)
        );
        // A typo must NOT silently resolve to a valid-looking value — the
        // `ExpectedStatus` rule in CONVENTIONS.md.
        assert_eq!(OtlpProtocol::parse("jsonn"), None);
        assert_eq!(OtlpProtocol::parse(""), None);

        assert_eq!(
            OtlpProtocol::Protobuf.content_type(),
            "application/x-protobuf"
        );
        assert_eq!(OtlpProtocol::Json.content_type(), "application/json");
        assert_eq!(OtlpProtocol::default(), OtlpProtocol::Protobuf);
    }

    /// The two encodings must describe the same window. Protobuf is not a
    /// re-encoding of the JSON text, so this compares the *decoded* content
    /// of each — not `f(x)` against `f(x)`.
    ///
    /// FAILS ON PRE-FIX CODE: only one encoder existed.
    #[test]
    fn both_encodings_describe_the_same_window() {
        let mut tags_a = TagMap::new();
        tags_a.insert("status", "200");
        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_reqs".to_string(),
            vec![
                sample_typed("http_reqs", 1.0, SampleType::Counter, tags_a.clone()),
                sample_typed("http_reqs", 2.0, SampleType::Counter, tags_a.clone()),
            ],
        );

        let json = String::from_utf8(encode_export_body(&metrics, OtlpProtocol::Json)).unwrap();
        let pb = encode_export_body(&metrics, OtlpProtocol::Protobuf);

        // JSON: aggregated to 3.0, DELTA, monotonic.
        assert!(json.contains("\"asDouble\":3.0"));
        assert!(json.contains("\"aggregationTemporality\":1"));
        // Protobuf: the same 3.0 as an IEEE-754 double, the same DELTA=1 and
        // is_monotonic=true varints, reached by an independent encoder.
        assert!(
            pb.windows(8).any(|w| w == 3.0f64.to_bits().to_le_bytes()),
            "protobuf body is missing the aggregated 3.0 double"
        );
        assert!(
            pb.windows(4).any(|w| w == [0x10, 0x01, 0x18, 0x01]),
            "protobuf body is missing DELTA(field 2 = 1) + is_monotonic(field 3 = true)"
        );
        // And the protobuf wire is materially smaller than the JSON text.
        assert!(
            pb.len() < json.len(),
            "protobuf {} B should be smaller than JSON {} B",
            pb.len(),
            json.len()
        );
    }
}
