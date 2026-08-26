//! k6-style summary data builder.
//!
//! Turns the aggregated results into the `handleSummary(data)` argument
//! object (per-metric values typed like k6, a top-level `thresholds` map,
//! and run state). Moved out of the former `engine.rs` god-file.

use serde_json::{json, Map};
use std::collections::{BTreeMap, HashMap};
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
        "root_group": build_root_group(results),
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
            "aggregatorSamplesDropped": results.aggregator_samples_dropped,
            "unverified": results.is_unverified(),
            "verification": if results.is_unverified() { "unverified" } else { "verified" },
        },
    })
}

/// Build the k6-style `root_group` tree from the per-group metric summaries.
/// k6's handleSummary v2.1.0 shape nests groups with their checks — the
/// hardcoded empty stub (TR-113) was the only thing keeping handleSummary
/// from rendering group-level data.
fn build_root_group(results: &MetricsResult) -> serde_json::Value {
    // Distinct group paths from the per-group summaries (root "" included).
    let mut paths: Vec<String> = vec![String::new()];
    for m in &results.per_group {
        if let Some(group) = m
            .tags
            .iter()
            .find(|(k, _)| k == "group")
            .map(|(_, v)| v.clone())
        {
            if !paths.contains(&group) {
                paths.push(group);
            }
        }
    }
    // Sort so shallower paths come first (root, ::a, ::a::b).
    paths.sort_by_key(|p| (p.matches("::").count(), p.clone()));

    // Build one node per path keyed by the full path so children can be
    // attached. Root "" is the outer object.
    let mut nodes: std::collections::BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for path in &paths {
        if path.is_empty() {
            continue;
        }
        let leaf = path.rsplit("::").next().unwrap_or(path);
        let (passes, fails) = group_checks(results, path);
        let mut checks = Vec::new();
        if passes > 0 || fails > 0 {
            checks.push(json!({ "name": "all", "passes": passes, "fails": fails }));
        }
        nodes.insert(
            path.clone(),
            json!({
                "name": leaf,
                "path": path,
                "id": leaf,
                "groups": [],
                "checks": checks,
            }),
        );
    }

    // Attach each node to its parent's `groups` (parent path = strip the
    // final `::leaf`). Iterate sorted so a parent is processed before its
    // children.
    let parent_of = |path: &str| -> String {
        match path.rfind("::") {
            Some(i) => path[..i].to_string(),
            None => String::new(),
        }
    };
    // Root checks: per-group `checks` Rate for root (group="" and the
    // default runner tag "http").
    let (rpasses, rfails) = group_checks(results, "");
    let (hpasses, hfails) = group_checks(results, "http");
    let mut root_checks = Vec::new();
    let rp = rpasses + hpasses;
    let rf = rfails + hfails;
    if rp > 0 || rf > 0 {
        root_checks.push(json!({ "name": "all", "passes": rp, "fails": rf }));
    }

    // Build the tree: repeatedly move each node into its parent's `groups`
    // (parent = strip the final ::leaf). Because `nodes` is keyed by path
    // and paths are sorted shallow-first, every parent already has its
    // `groups` array populated before a child needs to attach to it.
    let mut tree: BTreeMap<String, serde_json::Value> = nodes;
    for _ in 0..tree.len() {
        // A single pass moves every top-level leftover; nested attachments
        // need repeats since each iteration can only move a node whose
        // parent is still a plain map (not yet wrapped). Iterate until the
        // map holds only the root's direct children.
        let keys: Vec<String> = tree.keys().cloned().collect();
        let mut changed = false;
        for path in &keys {
            if path.is_empty() {
                continue;
            }
            let parent = parent_of(path);
            if let Some(node) = tree.remove(path) {
                if let Some(parent_node) = tree.get_mut(&parent) {
                    if let Some(groups) = parent_node["groups"].as_array_mut() {
                        groups.push(node);
                    }
                    changed = true;
                } else {
                    tree.insert(path.clone(), node);
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Any path whose parent was never a node in the tree (e.g. the parent
    // didn't exist in per_group) attaches to the root.
    let mut root_groups: Vec<serde_json::Value> = Vec::new();
    for (_, node) in tree {
        root_groups.push(node);
    }
    root_groups.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));

    json!({
        "name": "",
        "path": "",
        "id": "",
        "groups": root_groups,
        "checks": root_checks,
    })
}

/// Pass/fail counts for one group path from the per-group `checks` Rate
/// summaries. `count` is the number of checks recorded in that group; `rate`
/// is the pass fraction (k6 Rate semantics).
fn group_checks(results: &MetricsResult, group: &str) -> (u64, u64) {
    let mut count: f64 = 0.0;
    let mut rate: f64 = 0.0;
    for m in &results.per_group {
        if m.key.starts_with("checks") && m.tags.iter().any(|(k, v)| k == "group" && v == group) {
            count += m.count as f64;
            rate += m.rate * m.count as f64;
        }
    }
    let passes = rate.round() as u64;
    let fails = (count - rate).round() as u64;
    (passes, fails)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// TR-113: `root_group` must be a real group tree built from the
    /// per-group summaries, not the hardcoded empty stub. handleSummary
    /// scripts reading `data.root_group.groups` get the nested groups and
    /// per-group checks.
    #[test]
    fn root_group_builds_tree_from_per_group_data() {
        let mk = |key: &str, group: &str, count: u64, rate: f64| MetricSummary {
            key: key.into(),
            tags: vec![("group".to_string(), group.to_string())],
            metric_type: MetricType::Rate,
            count,
            sum: count as f64,
            mean: rate,
            min: 0.0,
            max: 1.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 0.0,
            rate,
            histogram: None,
        };
        let results = MetricsResult {
            per_group: vec![
                // checks at root (k6 runner default group tag "http").
                mk("checks{group=http}", "http", 10, 0.9),
                // nested groups with their own checks.
                mk("checks{group=::checkout}", "::checkout", 5, 1.0),
                mk(
                    "checks{group=::checkout::payment}",
                    "::checkout::payment",
                    4,
                    0.5,
                ),
            ],
            ..Default::default()
        };
        let data = build_summary_data(&results, &HashMap::new(), std::time::Instant::now());
        let root = &data["root_group"];
        assert_eq!(root["name"], json!(""));
        assert_eq!(root["path"], json!(""));

        // Root checks: 10 @ 0.9 → 9 passes, 1 fail.
        assert_eq!(root["checks"][0]["passes"], json!(9));
        assert_eq!(root["checks"][0]["fails"], json!(1));

        // Top-level group "checkout" with path ::checkout.
        let checkout = root["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == "checkout")
            .expect("checkout group must be in the tree");
        assert_eq!(checkout["path"], json!("::checkout"));
        assert_eq!(checkout["checks"][0]["passes"], json!(5));

        // Nested "payment" inside checkout.
        let payment = checkout["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == "payment")
            .expect("payment group must nest under checkout");
        assert_eq!(payment["path"], json!("::checkout::payment"));
        assert_eq!(payment["checks"][0]["passes"], json!(2));
        assert_eq!(payment["checks"][0]["fails"], json!(2));
    }
}
