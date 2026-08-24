//! k6-style summary data builder.
//!
//! Turns the aggregated results into the `handleSummary(data)` argument
//! object (per-metric values typed like k6, a top-level `thresholds` map,
//! and run state). Moved out of the former `engine.rs` god-file.

use serde_json::{json, Map};
use std::collections::HashMap;
use std::time::Instant;
use tropel_core::config::ThresholdConfig;
use tropel_metrics::collector::MetricSummary;
use tropel_metrics::collector::MetricType;
use tropel_metrics::collector::MetricsResult;
use tropel_metrics::thresholds::evaluate_thresholds;

/// Serialize one `MetricSummary` into the k6-style handleSummary entry:
/// `(metric_key, { "type", "contains", "values" })`.
fn metric_entry(m: &MetricSummary, run_secs: f64) -> (String, serde_json::Value) {
    // k6's `contains` reflects the metric's declared unit (Time/Data/
    // Default) — delegated to tropel-metrics `unit_of` (single source of
    // truth shared with json-stream/stdout, backlog line 32).
    let base = m.key.split('{').next().unwrap_or(&m.key);
    let contains = tropel_metrics::time_metrics::unit_of(base).as_str();
    let (typ, contains, values) = match m.metric_type {
        MetricType::Counter => {
            // k6 handleSummary Counter values: `count` (accumulated) +
            // `rate` = count / elapsed seconds (backlog line 154).
            let rate = if run_secs > 0.0 {
                m.count as f64 / run_secs
            } else {
                0.0
            };
            (
                "counter",
                contains,
                json!({ "count": m.count, "rate": rate }),
            )
        }
        MetricType::Gauge => (
            "gauge",
            contains,
            json!({ "value": m.last, "min": m.min, "max": m.max, "avg": m.mean }),
        ),
        MetricType::Rate => (
            "rate",
            contains,
            json!({ "rate": m.rate, "count": m.count }),
        ),
        MetricType::Trend => {
            // Values are already in ms end-to-end (backlog §0); k6's
            // `contains` reflects the metric's declared unit (Time/Data/
            // Default) — a custom byte-count Trend is NOT labelled time
            // (the old code hardcoded "time" for every Trend).
            (
                "trend",
                contains,
                json!({
                    "avg": m.mean,
                    "min": m.min,
                    "med": m.p50,
                    "max": m.max,
                    "p(90)": m.p90,
                    "p(95)": m.p95,
                    "p(99)": m.p99,
                    "count": m.count,
                }),
            )
        }
    };
    (
        m.key.clone(),
        json!({
            "type": typ,
            "contains": contains,
            "values": values,
        }),
    )
}

/// Build the k6-style summary data object (`handleSummary(data)` argument)
/// from the aggregated results: per-metric values typed like k6 plus a
/// top-level `thresholds` map (expression → pass/fail) and run state.
pub(crate) fn build_summary_data(
    results: &MetricsResult,
    thresholds: &HashMap<String, ThresholdConfig>,
    test_start: Instant,
) -> serde_json::Value {
    let run_secs = results.run_duration.as_secs_f64();
    let mut metrics = Map::new();
    for m in &results.metrics {
        let (key, entry) = metric_entry(m, run_secs);
        metrics.insert(key, entry);
    }

    // W1-B line 152: the merged headline summaries (`http_req_duration` /
    // `iteration_duration`) live in dedicated MetricsResult fields that the
    // collector DELIBERATELY keeps out of `results.metrics` (thresholds must
    // not double-count samples that already exist as raw per-(url,method,
    // status) series). Nothing re-injected them here, so the single most
    // common handleSummary line in existence —
    // `data.metrics['http_req_duration'].values['p(95)']` — THREW. Re-inject
    // them under their unscoped keys so handleSummary sees the same merged
    // headlines that stdout and thresholds evaluate.
    if let Some(m) = &results.http_req_duration {
        let (key, entry) = metric_entry(m, run_secs);
        metrics.insert(key, entry);
    }
    if let Some(m) = &results.iteration_duration {
        let (key, entry) = metric_entry(m, run_secs);
        metrics.insert(key, entry);
    }

    let mut thresholds_map = Map::new();
    for t in evaluate_thresholds(thresholds, results) {
        // P1 line 164: key by metric+expression to avoid duplicate expressions
        // erasing failures. The old code keyed by expression alone, so
        // {http_req_duration: ['p(95)<500'], iteration_duration: ['p(95)<500']}
        // produced one entry; if the passing one landed last, handleSummary
        // saw green while a threshold failed.
        let key = format!("{}.{}", t.name, t.expression);
        thresholds_map.insert(key, json!(t.passed));
    }

    json!({
        "metrics": metrics,
        "root_group": { "name": "", "path": "", "id": "", "groups": [], "checks": [] },
        "options": {},
        "thresholds": thresholds_map,
        "state": {
            "testRunDurationMs": test_start.elapsed().as_millis() as u64,
            "iterations": results.iterations,
            "vusMax": results.vus_max,
            "http_reqs": results.http_reqs,
            "checksTotal": results.checks_total,
            "checksPassed": results.checks_passed,
            "checksFailed": results.checks_failed,
            "seriesDropped": results.series_dropped,
            "samplesDropped": results.output_samples_dropped,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tropel_metrics::collector::MetricSummary;
    use tropel_metrics::collector::MetricType;

    fn trend(key: &str, p95: f64) -> MetricSummary {
        MetricSummary {
            key: key.to_string(),
            tags: vec![],
            metric_type: MetricType::Trend,
            count: 10,
            sum: 1000.0,
            mean: 100.0,
            min: 50.0,
            max: 900.0,
            p50: 90.0,
            p90: 800.0,
            p95,
            p99: 990.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        }
    }

    #[test]
    fn headline_http_req_duration_reinjected_into_handle_summary() {
        // W1-B line 152: the merged headline summaries live in DEDICATED
        // MetricsResult fields deliberately kept OUT of `metrics` (so
        // threshold evaluation can't double-count raw per-tag series).
        // build_summary_data never re-injected them, so the single most
        // common handleSummary line in existence —
        // `data.metrics['http_req_duration'].values['p(95)']` — THREW on a
        // healthy run. The headline must appear under the unscoped key with
        // the merged percentile.
        // The tag-scoped series are what `metrics` carries; the merged
        // headline is a SEPARATE field (collector keeps them apart).
        let results = MetricsResult {
            http_req_duration: Some(trend("http_req_duration", 321.5)),
            iteration_duration: Some(trend("iteration_duration", 7.25)),
            ..Default::default()
        };

        let data = build_summary_data(&results, &HashMap::new(), std::time::Instant::now());
        let metrics = &data["metrics"];
        assert_eq!(
            metrics["http_req_duration"]["values"]["p(95)"],
            json!(321.5),
            "unscoped http_req_duration headline must be visible to handleSummary"
        );
        assert_eq!(metrics["http_req_duration"]["type"], json!("trend"));
        assert_eq!(
            metrics["iteration_duration"]["values"]["p(95)"],
            json!(7.25)
        );
        assert_eq!(metrics["http_req_duration"]["values"]["count"], json!(10));
    }

    #[test]
    fn headline_reinjection_never_shadows_per_tag_series() {
        // A tag-scoped series with the same base name must keep its own
        // key; only the UNSCOPED merged headline is added under
        // `http_req_duration`.
        let results = MetricsResult {
            http_req_duration: Some(trend("http_req_duration", 300.0)),
            metrics: vec![MetricSummary {
                key: "http_req_duration{status=200,url=/a}".into(),
                tags: vec![],
                metric_type: MetricType::Trend,
                count: 5,
                sum: 250.0,
                mean: 50.0,
                min: 10.0,
                max: 100.0,
                p50: 45.0,
                p90: 90.0,
                p95: 95.0,
                p99: 99.0,
                last: 0.0,
                rate: 0.0,
                histogram: None,
            }],
            ..Default::default()
        };
        let data = build_summary_data(&results, &HashMap::new(), std::time::Instant::now());
        let metrics = &data["metrics"];
        // Both the tag-scoped series AND the unscoped headline exist.
        assert!(metrics
            .get("http_req_duration{status=200,url=/a}")
            .is_some());
        assert_eq!(
            metrics["http_req_duration"]["values"]["p(95)"],
            json!(300.0)
        );
    }
}
