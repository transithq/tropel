use crate::collector::{
    parse_percentile, percentile_value, MetricSummary, MetricType, MetricsResult,
};
use crate::histogram::LatencyHistogram;
use std::collections::HashMap;
use std::time::Duration;
use tropel_core::config::ThresholdConfig;

/// Result of threshold evaluation.
#[derive(Debug, Clone)]
pub struct ThresholdResult {
    pub name: String,
    pub expression: String,
    pub passed: bool,
    pub actual: f64,
    pub threshold: f64,
    pub abort_on_fail: bool,
    pub delay_abort_eval: Option<Duration>,
}

/// Evaluate thresholds against aggregated metrics.
pub fn evaluate_thresholds(
    thresholds: &HashMap<String, ThresholdConfig>,
    metrics: &MetricsResult,
) -> Vec<ThresholdResult> {
    let mut results = Vec::new();

    for (name, config) in thresholds {
        let result = evaluate_single_threshold(&config.expression, metrics);
        results.push(ThresholdResult {
            name: name.clone(),
            expression: config.expression.clone(),
            passed: result.0,
            actual: result.1,
            threshold: result.2,
            abort_on_fail: config.abort_on_fail,
            delay_abort_eval: config
                .delay_abort_eval
                .as_ref()
                .and_then(|s| parse_duration(s).ok()),
        });
    }

    // Sort by name so the summary output is deterministic — the input is a
    // HashMap whose iteration order is random per process, which made the
    // rendered Thresholds block (and the insta snapshot of it) flaky.
    results.sort_by(|a, b| a.name.cmp(&b.name));

    results
}

/// Validate every configured threshold expression BEFORE the run starts.
/// Returns an error naming the first malformed expression so the run aborts
/// at startup with a clear message — k6 rejects bad threshold syntax at init
/// rather than silently passing it at the end.
///
/// Rejects: fewer/greater than 3 whitespace tokens per CLAUSE, an unknown
/// operator, or a non-numeric RHS. Backlog line 154: compound `&&`/`||`
/// expressions are now ACCEPTED (k6 supports them) — each clause is validated
/// independently instead of the whole expression being rejected.
pub fn validate_thresholds(thresholds: &HashMap<String, ThresholdConfig>) -> Result<(), String> {
    for (name, config) in thresholds {
        let expr = config.expression.trim();
        // Compound AND/OR: validate every clause the same way a single
        // threshold is validated.
        for clause in compound_clauses(expr) {
            let parts: Vec<&str> = clause.split_whitespace().collect();
            if parts.len() != 3 {
                return Err(format!(
                    "threshold '{}': clause '{}' in '{}' — expected '<metric> <op> <value>' \
                     (3 tokens), got {}",
                    name,
                    clause,
                    expr,
                    parts.len()
                ));
            }
            if !matches!(parts[1], "<" | "<=" | ">" | ">=" | "==" | "!=") {
                return Err(format!(
                    "threshold '{}': unknown operator '{}' in clause '{}' of '{}'",
                    name, parts[1], clause, expr
                ));
            }
            if parts[2].parse::<f64>().is_err() {
                return Err(format!(
                    "threshold '{}': value '{}' is not a number",
                    name, parts[2]
                ));
            }
            // Backlog §1: an unknown/typo'd STAT must also abort at startup —
            // the old resolver never validated it and silently gated on the
            // mean, so `http_req_duration.p95th < 500` passed on a 1200 ms
            // p95. Checked here alongside the operator/RHS, like k6.
            let (_, _, stat) = parse_metric_ref(parts[0]);
            if let Some(s) = stat {
                if !is_known_stat(s) {
                    return Err(format!(
                        "threshold '{}': unknown statistic '{}' in clause '{}' of '{}'",
                        name, s, clause, expr
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The complete set of statistics the threshold resolver understands. Used
/// by [`validate_thresholds`] so a typo'd stat (e.g. `p95th`) aborts the run
/// at startup instead of silently gating on the mean at the end.
fn is_known_stat(stat: &str) -> bool {
    matches!(
        stat,
        "avg"
            | "min"
            | "max"
            | "p50"
            | "median"
            | "med"
            | "p90"
            | "p95"
            | "p99"
            | "count"
            | "rate"
            | "sum"
            | "last"
            | "value"
            | "passed"
            | "failed"
            | "pass_rate"
    ) || parse_percentile(stat).is_some()
}

/// Split a (possibly compound) threshold expression into its individual
/// clauses on `&&` and `||`. A simple expression returns itself.
fn compound_clauses(expression: &str) -> Vec<&str> {
    if !expression.contains("&&") && !expression.contains("||") {
        return vec![expression];
    }
    expression
        .split("||")
        .flat_map(|group| group.split("&&"))
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Check if any abort-on-fail threshold has been breached (mid-run evaluation).
/// Returns `true` if the test should be aborted immediately.
/// Respects `delay_abort_eval` — thresholds in their grace period won't abort.
pub fn check_abort_on_fail(
    thresholds: &HashMap<String, ThresholdConfig>,
    metrics: &MetricsResult,
    elapsed: Duration,
) -> bool {
    for (name, config) in thresholds {
        if !config.abort_on_fail {
            continue;
        }

        // Check if delayAbortEval grace period is still active
        if let Some(ref delay_str) = config.delay_abort_eval {
            if let Ok(delay) = parse_duration(delay_str) {
                if elapsed < delay {
                    continue; // Still in grace period — don't abort yet
                }
            }
        }

        // Use the `_opt` variant: a metric with no samples YET (mid-run)
        // returns None and must NOT abort — data may simply not have
        // arrived. Only a definite numerical breach aborts.
        if let Some((passed, _, _)) = evaluate_single_threshold_opt(&config.expression, metrics) {
            if !passed {
                tracing::error!(
                    "Threshold '{}' ({}) breached with abortOnFail — aborting test",
                    name,
                    config.expression
                );
                return true;
            }
        }
    }
    false
}

/// Parse a duration string like "30s", "1m", "500ms" into a Duration.
fn parse_duration(s: &str) -> std::result::Result<Duration, ()> {
    tropel_sdk::parse_duration(s).map_err(|_| ())
}

/// Parse a tag-scoped metric reference like `"http_req_duration{status=200}.p95"`
/// into its components: (metric_name, tags, stat).
///
/// Returns:
/// - `metric_name`: the base metric name before any `{...}` or `.stat` suffix
/// - `tags`: vector of `(key, value)` pairs extracted from `{key=value,...}` (empty if none)
/// - `stat`: the statistic part after `.` (None if absent)
///
/// Examples:
///   "http_req_duration{status=200}.p95" → ("http_req_duration", [(status, 200)], "p95")
///   "http_reqs"                        → ("http_reqs", [], None)
///   "checks.pass_rate"                 → ("checks", [], "pass_rate")
pub(crate) fn parse_metric_ref(metric_ref: &str) -> (&str, Vec<(&str, &str)>, Option<&str>) {
    // Step 1: Find the tag block boundaries
    let (brace_start, brace_close) = {
        let start = metric_ref.find('{');
        let end = start.and_then(|s| metric_ref[s..].find('}').map(|i| s + i));
        (start, end)
    };

    // Step 2: Extract tags from inside `{...}`
    let tags = if let (Some(bs), Some(bc)) = (brace_start, brace_close) {
        metric_ref[bs + 1..bc]
            .split(',')
            .filter_map(|pair| {
                let pair = pair.trim();
                if pair.is_empty() {
                    return None;
                }
                // Support both `:` and `=` as key=value separators. Pick the
                // one that occurs FIRST positionally — a naive
                // `contains(':') ? ':' : '='` makes the scheme colon win in
                // `url=https://…` and splits into key "url=https" (never
                // matches, silent `✗ 0.00 FAIL` — W1-B line 147).
                let eq = pair.find('=').unwrap_or(usize::MAX);
                let colon = pair.find(':').unwrap_or(usize::MAX);
                // Neither present → '=' yields split_once None → pair dropped
                // (pre-existing behavior preserved).
                let sep = if eq <= colon { '=' } else { ':' };
                pair.split_once(sep).map(|(k, v)| (k.trim(), v.trim()))
            })
            .collect()
    } else {
        vec![]
    };

    // Step 3: Extract metric name and stat suffix
    // The metric name is the text before `{` (or the whole string if no tags).
    // The stat suffix can be after `}` (e.g. `{status=200}.p95`) or
    // after the name but before `{` (e.g. `.p95{status=200}`).
    let before = &metric_ref[..brace_start.unwrap_or(metric_ref.len())];
    let after = &metric_ref[brace_close.map(|bc| bc + 1).unwrap_or(metric_ref.len())..];

    // Find stat: prefer after `}` (more common), fall back to before `{`.
    // Must be the last dot OUTSIDE a `(...)` pair — a decimal percentile
    // like `.p(99.9)` contains a dot inside the parens, and a naive
    // `rfind('.')` would split `http_req_duration.p(99.9)` at the decimal
    // point (name "http_req_duration.p(99", stat "9"), silently degrading
    // p(99.9) thresholds to the mean / a non-existent metric.
    let (name, stat) = if let Some(dot) = rfind_dot_outside_parens(after) {
        // Stat is after `}` — name is the part before `{`
        (before, Some(&after[dot + 1..]))
    } else if let Some(dot) = rfind_dot_outside_parens(before) {
        // Stat is before `{` — strip it from the name
        (&before[..dot], Some(&before[dot + 1..]))
    } else {
        // No stat suffix
        (before, None)
    };

    (name, tags, stat)
}

/// Index of the last `'.'` in `s` that is NOT inside a `(...)` pair, or
/// `None` when every dot is parenthesized. Backs [`parse_metric_ref`]: the
/// stat suffix is `.p(99.9)`, whose decimal point sits inside the parens —
/// a plain `rfind('.')` would treat that decimal as the stat separator and
/// mangle the metric reference.
fn rfind_dot_outside_parens(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, b) in s.bytes().enumerate().rev() {
        match b {
            b')' => depth += 1,
            b'(' => depth -= 1,
            b'.' if depth <= 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Evaluate a single threshold expression against real metrics.
/// Supports expressions like:
///   "http_req_duration.p95 < 500"
///   "http_req_duration{status=200}.p95 < 500"
///   "http_reqs > 100"
///   "checks.pass_rate > 0.99"
///   "errors < 10"
/// Evaluate a single threshold expression, returning `None` when the metric
/// has NO samples yet (mid-run this must NOT abort — data may simply not have
/// arrived). Parse errors and unknown operators fail closed (see
/// [`validate_thresholds`], which rejects them at startup).
fn evaluate_single_threshold_opt(
    expression: &str,
    metrics: &MetricsResult,
) -> Option<(bool, f64, f64)> {
    // Backlog line 154: compound AND/OR expressions (k6 supports them,
    // e.g. `p(95) < 500 && p(99) < 1000`). `&&` binds tighter than `||`:
    // split on `||` first, every group must contain only AND-passing
    // clauses, and any passing group passes the whole threshold. The
    // reported (actual, threshold) pair comes from the LAST clause that
    // determined the outcome (the group's final clause).
    if expression.contains("&&") || expression.contains("||") {
        let groups: Vec<Vec<&str>> = expression
            .split("||")
            .map(|g| {
                g.split("&&")
                    .map(|c| c.trim())
                    .filter(|c| !c.is_empty())
                    .collect()
            })
            .filter(|g: &Vec<&str>| !g.is_empty())
            .collect();
        let mut last_pair = (0.0f64, 0.0f64);
        let mut any_group_passed = false;
        for group in groups {
            let mut group_passed = true;
            for clause in group {
                let (passed, actual, threshold) = evaluate_single_threshold_opt(clause, metrics)?;
                last_pair = (actual, threshold);
                if !passed {
                    group_passed = false;
                }
            }
            if group_passed {
                any_group_passed = true;
            }
        }
        return Some((any_group_passed, last_pair.0, last_pair.1));
    }

    // Fail CLOSED: any parse error or unknown operator must FAIL the
    // threshold (and, via `validate_thresholds` at startup, abort the run).
    // The old code returned `(true, …)` on malformed input, so a typo'd
    // metric or a bogus operator silently reported green. k6 rejects bad
    // threshold syntax at startup instead.
    let parts: Vec<&str> = expression.split_whitespace().collect();
    if parts.len() != 3 {
        tracing::error!(
            "Invalid threshold expression '{}' — expected '<metric> <op> <value>' (3 tokens), got {}",
            expression,
            parts.len()
        );
        return Some((false, 0.0, 0.0));
    }

    // Format: "metric_ref operator value"
    // metric_ref can be "http_req_duration.p95", "http_req_duration{status=200}.p95",
    // or just "http_reqs"
    let metric_ref = parts[0];
    let operator = parts[1];
    let threshold: f64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => {
            tracing::error!(
                "Invalid threshold value in '{}': '{}'",
                expression,
                parts[2]
            );
            return Some((false, 0.0, 0.0));
        }
    };

    // Parse metric reference into (metric_name, tags, stat)
    let (metric_name, tag_filters, stat) = parse_metric_ref(metric_ref);

    // Look up the actual metric value. `None` means the metric has NO
    // samples at all — distinguish that from a real measured 0.0: a missing
    // metric must fail the threshold (k6 marks no-data thresholds as failed),
    // never pass a `<` comparison against an invented 0.0.
    let actual = if !tag_filters.is_empty() {
        // Tag-scoped threshold: search metrics.metrics for matching entries
        get_tag_scoped_metric_value(metrics, metric_name, &tag_filters, stat)
    } else {
        // No tag filter — use the existing top-level lookup
        get_metric_value(metrics, metric_name, stat)
    };

    let actual = actual?; // None → metric has no samples yet — no data.

    let passed = match operator {
        "<" => actual < threshold,
        "<=" => actual <= threshold,
        ">" => actual > threshold,
        ">=" => actual >= threshold,
        "==" => (actual - threshold).abs() < f64::EPSILON,
        "!=" => (actual - threshold).abs() > f64::EPSILON,
        _ => {
            tracing::error!(
                "Unknown operator '{}' in threshold '{}'",
                operator,
                expression
            );
            return Some((false, 0.0, threshold));
        }
    };

    Some((passed, actual, threshold))
}

/// Fail-closed wrapper used by the final summary: no data → FAILED (k6 marks
/// no-data thresholds as failed).
fn evaluate_single_threshold(expression: &str, metrics: &MetricsResult) -> (bool, f64, f64) {
    evaluate_single_threshold_opt(expression, metrics).unwrap_or((false, 0.0, 0.0))
}

/// Extract the base metric name from a series key string, stripping any
/// `{tag=value}` suffix: `"http_req_duration{status=200}"` → `"http_req_duration"`.
/// Exact base-name matching (not prefix) keeps a threshold on `login` from
/// aggregating `login_errors` + `login_duration`.
fn metric_base_name(key: &str) -> &str {
    match key.find('{') {
        Some(i) => &key[..i],
        None => key,
    }
}

/// Can this series supply a truthful value for the given percentile-style
/// stat (`p50`/`p90`/`p95`/`p99`/arbitrary `pNN`, plus `min`/`max` which the
/// tracked buckets fold in)? Only Trend tracks a real distribution; Gauge
/// tracks real min/max from its samples. Counter and Rate summaries hardcode
/// p50..p99 (and Rate also min/max) to 0 — reading them would let a
/// `p(95)<0.01` gate pass on a 100%-failure run (backlog line 60).
fn series_supports_percentile(m: &MetricSummary, stat: &str) -> bool {
    match stat {
        "min" | "max" => matches!(m.metric_type, MetricType::Trend | MetricType::Gauge),
        _ => m.metric_type == MetricType::Trend,
    }
}

/// Does a percentile-style stat on this match set require fail-closed
/// handling? `percentile_stat` is true for `min`/`max`/`p50`/`p90`/`p95`/
/// `p99`/`median`/`med` and any arbitrary `pNN`/`p(NN)`; the guard then
/// returns `true` (caller must fail closed) unless EVERY matched series can
/// supply a truthful value for that stat. Shared by the tag-scoped and
/// unscoped custom-metric paths so the two can never drift (backlog line 60).
fn percentile_guard_fails(matched: &[&MetricSummary], stat: Option<&str>) -> bool {
    let percentile_stat = matches!(
        stat,
        Some("min" | "max" | "p50" | "median" | "med" | "p90" | "p95" | "p99")
    ) || stat.is_some_and(|s| parse_percentile(s).is_some());
    if !percentile_stat {
        return false;
    }
    !matched
        .iter()
        .all(|m| series_supports_percentile(m, stat.expect("percentile_stat implies a stat")))
}

/// Get a metric value for a tag-scoped threshold by searching the metrics list.
/// Looks for entries whose BASE name equals the metric name and that carry
/// all the specified tag key=value pairs (matched STRUCTURALLY against each
/// series' parsed `tags`, not against the rendered key string — the collector
/// joins all tags into one brace group `http_req_duration{status=200,method=GET}`,
/// so the old `{status=200}` substring never matched).
///
/// When MULTIPLE entries match (e.g. `http_req_duration{status=200,method=GET}`
/// and `http_req_duration{status=200,method=POST}` both match `{status=200}`),
/// the function aggregates:
/// - **Percentiles** (p50/p90/p95/p99): returns the WORST (highest) value
/// - **Avg/mean**: returns the WORST (highest) value across matches
/// - **Min**: returns the MINIMUM value across matches
/// - **Max**: returns the MAXIMUM value across matches
/// - **Count**: returns the SUM of counts
/// - **Sum**: returns the SUM of sums
/// - **Rate**: recomputes sum/count from totals
///
/// If no entry matches, returns `None` (no data for this tag set → the
/// evaluator fails the threshold closed, matching k6's "no data" behavior
/// instead of inventing a passing 0.0).
fn get_tag_scoped_metric_value(
    metrics: &MetricsResult,
    metric_name: &str,
    tag_filters: &[(&str, &str)],
    stat: Option<&str>,
) -> Option<f64> {
    let mut matched = Vec::new();

    for m in &metrics.metrics {
        // Exact base-name match: `login` must not aggregate `login_errors`.
        if metric_base_name(&m.key) != metric_name {
            continue;
        }
        // Match structurally against the parsed tag pairs — NOT the rendered
        // key. The collector emits ONE comma-joined brace group
        // (`{status=200,method=GET}`), so a substring like `{status=200}` can
        // never match; the k6-copied `http_req_duration{status:200}` threshold
        // therefore reported actual = 0.00 on a healthy run. `tags` is the
        // authoritative (key, value) source (backlog §1 P0).
        let all_tags_match = tag_filters
            .iter()
            .all(|(key, val)| m.tags.iter().any(|(k, v)| k == key && v == val));
        if !all_tags_match {
            continue;
        }
        matched.push(m);
    }

    if matched.is_empty() {
        return None;
    }

    // Backlog §1: percentile/min/max stats on a Counter are meaningless — the
    // accumulated value would masquerade as every percentile, so `errors.p95`
    // (or any custom counter like `data_received.p95`) must FAIL CLOSED, not
    // resolve to the counter's 1.0 bucket and pass a `> 0.5` gate.
    // Backlog §1 + line 60: percentile/min/max stats are only meaningful on
    // series that actually track a distribution or real extremes. The old
    // guard checked `all(Counter)` only, so a **Rate** summary (p50..p99
    // hardcoded 0) let `http_req_failed: ['p(95)<0.01']` PASS on a
    // 100%-failure run, and a **Gauge** (p50..p99 hardcoded 0) passed
    // `queue_depth.p95 < 10` trivially. Fail closed unless EVERY matched
    // series supports the stat (Trend for percentiles; Trend/Gauge for
    // min/max — Gauge tracks real extremes from its samples).
    if percentile_guard_fails(&matched, stat) {
        return None;
    }

    // Aggregate all matching entries
    Some(match stat {
        Some("avg") => {
            // Return the WORST (highest) mean across all matches
            matched.iter().map(|m| m.mean).fold(0.0_f64, f64::max)
        }
        Some("min") => {
            // Return the MINIMUM min across all matches
            matched.iter().map(|m| m.min).fold(f64::MAX, f64::min)
        }
        Some("max") => {
            // Return the MAXIMUM max across all matches
            matched.iter().map(|m| m.max).fold(0.0_f64, f64::max)
        }
        Some("p50") | Some("median") | Some("med") => merged_percentile(&matched, 50.0),
        Some("p90") => merged_percentile(&matched, 90.0),
        Some("p95") => merged_percentile(&matched, 95.0),
        Some("p99") => merged_percentile(&matched, 99.0),
        // Any other pNN / p(NN) percentile — exact from the retained
        // histogram of each matching series; merged-population when all
        // retained, worst (highest) fallback otherwise.
        Some(s) if parse_percentile(s).is_some() => {
            let pct = parse_percentile(s).expect("guarded");
            merged_percentile(&matched, pct)
        }
        Some("count") => matched.iter().map(|m| m.count as f64).sum(),
        Some("rate") => {
            if matched.iter().all(|m| m.metric_type == MetricType::Counter) {
                counter_rate(metrics, &matched)
            } else {
                let total_sum: f64 = matched.iter().map(|m| m.sum).sum();
                let total_count: f64 = matched.iter().map(|m| m.count as f64).sum();
                if total_count > 0.0 {
                    total_sum / total_count
                } else {
                    0.0
                }
            }
        }
        Some("sum") => matched.iter().map(|m| m.sum).sum(),
        // k6's `value`/`last` stat = the most recent sample. W1-A: the OLD
        // code read an ARBITRARY `matched.last()` (iteration-order-dependent
        // pick), and `last` was hardcoded 0.0 for Counter/Rate/Trend so the
        // tag-scoped value always PASSED a `<` gate trivially. Now that
        // Trend/Gauge track a real `last`, aggregate worst-of (max) across
        // matches — consistent with the avg/min/max arms — so a failure on
        // ANY matching series is visible.
        // k6's `value`/`last` stat = the most recent sample. W1-A: the OLD
        // code read an ARBITRARY `matched.last()` (iteration-order-dependent
        // pick), and `last` was hardcoded 0.0 for Counter/Rate/Trend so the
        // tag-scoped value always PASSED a `<` gate trivially. Trend/Gauge
        // now track a real `last` (worst-of max across matches, consistent
        // with the avg/min/max arms). Counter/Rate hardcode `last` 0.0, so
        // they FAIL CLOSED (same guard as the percentile path):
        // `http_req_failed.value < 0.01` must never pass against an invented
        // 0.0 on a 100%-failure run.
        Some("value") | Some("last") => {
            if !matched
                .iter()
                .all(|m| matches!(m.metric_type, MetricType::Trend | MetricType::Gauge))
            {
                return None;
            }
            matched.iter().map(|m| m.last).fold(0.0_f64, f64::max)
        }
        // Bare metric (no stat) → WORST mean across matches (documented
        // default). Backlog §1: an UNKNOWN/typo'd stat must FAIL CLOSED,
        // not silently gate on the mean like the old `_ =>` arm.
        None => matched.iter().map(|m| m.mean).fold(0.0_f64, f64::max),
        Some(_) => return None,
    })
}

/// Per-second rate for all-Counter matched series: the accumulated total
/// across series divided by the run duration (k6 counter `rate` semantics).
/// `count` IS the accumulated value on a counter, so sum/count would
/// degenerate to 1.0 — this is the real per-second number.
fn counter_rate(metrics: &MetricsResult, matched: &[&MetricSummary]) -> f64 {
    let total: f64 = matched.iter().map(|m| m.count as f64).sum();
    let secs = metrics.run_duration.as_secs_f64();
    if secs > 0.0 {
        total / secs
    } else {
        0.0
    }
}

/// Aggregate a statistic across ALL series in `metrics.metrics` whose BASE
/// name equals `name` (k6 merges tagged sub-series for the unscoped metric).
/// Exact match — never prefix: a threshold on `login` must not aggregate
/// `login_errors` / `login_duration`.
/// Returns `None` when no series match, so callers can fall back to a
/// top-level field or 0.0.
/// Compute a percentile over the MERGED population of every matching series
/// (k6 semantics: an unscoped or tag-scoped threshold like `http_req_waiting.p95`
/// is the p95 of ALL samples across the matching series, not the worst
/// sub-series). When every matched Trend retains its exact histogram, merge
/// them losslessly and read the percentile from the merged distribution;
/// otherwise (pre-config window / synthetic summaries with no retained
/// histograms) fall back to the worst (highest) value across matches — the
/// old fold behavior.
fn merged_percentile(matched: &[&MetricSummary], pct: f64) -> f64 {
    let all_retained = !matched.is_empty()
        && matched
            .iter()
            .all(|m| m.metric_type == MetricType::Trend && m.histogram.is_some());
    if all_retained {
        let mut merged = LatencyHistogram::new();
        for m in matched {
            if let Some(h) = &m.histogram {
                merged.merge(h);
            }
        }
        return merged.percentile(pct);
    }
    matched
        .iter()
        .map(|m| percentile_value(m, pct))
        .fold(0.0_f64, f64::max)
}

fn aggregate_series(metrics: &MetricsResult, name: &str, stat: Option<&str>) -> Option<f64> {
    let matched: Vec<&MetricSummary> = metrics
        .metrics
        .iter()
        .filter(|m| metric_base_name(&m.key) == name)
        .collect();
    if matched.is_empty() {
        return None;
    }
    // Backlog §1 + line 60: percentile/min/max stats are only meaningful on
    // series that actually track a distribution or real extremes. The old
    // guard checked `all(Counter)` only, so a **Rate** summary (p50..p99
    // hardcoded 0) let `http_req_failed: ['p(95)<0.01']` PASS on a
    // 100%-failure run, and a **Gauge** (p50..p99 hardcoded 0) passed
    // `queue_depth.p95 < 10` trivially. Fail closed unless EVERY matched
    // series supports the stat (Trend for percentiles; Trend/Gauge for
    // min/max — Gauge tracks real extremes from its samples).
    if percentile_guard_fails(&matched, stat) {
        return None;
    }
    Some(match stat {
        // Percentiles: merged-population (k6 semantics) when the exact
        // per-series histograms are retained; worst (highest) fallback.
        Some("min") => matched.iter().map(|m| m.min).fold(f64::MAX, f64::min),
        Some("max") => matched.iter().map(|m| m.max).fold(0.0_f64, f64::max),
        Some("p50") | Some("median") | Some("med") => merged_percentile(&matched, 50.0),
        Some("p90") => merged_percentile(&matched, 90.0),
        Some("p95") => merged_percentile(&matched, 95.0),
        Some("p99") => merged_percentile(&matched, 99.0),
        Some(s) if parse_percentile(s).is_some() => {
            let pct = parse_percentile(s).expect("guarded");
            merged_percentile(&matched, pct)
        }
        // Rate = total sum / total count across ALL series (k6 merges tagged
        // sub-series for the unscoped metric).
        Some("rate") => {
            if matched.iter().all(|m| m.metric_type == MetricType::Counter) {
                // Counter `count` IS the accumulated value (k6 semantics), so
                // sum/count would degenerate to 1.0. The per-second rate is
                // the accumulated total across series / run duration — the
                // backlog's `data_received: ['rate>1000000']` case, which the
                // old per-series-mean fallback made permanently red.
                counter_rate(metrics, &matched)
            } else {
                let total_sum: f64 = matched.iter().map(|m| m.sum).sum();
                let total_count: f64 = matched.iter().map(|m| m.count as f64).sum();
                if total_count > 0.0 {
                    total_sum / total_count
                } else {
                    0.0
                }
            }
        }
        Some("count") => matched.iter().map(|m| m.count as f64).sum(),
        Some("sum") => matched.iter().map(|m| m.sum).sum(),
        Some("avg") => {
            if matched.iter().all(|m| m.metric_type == MetricType::Counter) {
                // Counter: count == accumulated value, so sum/count is always
                // 1.0 — avg on a counter IS the per-second rate (k6).
                counter_rate(metrics, &matched)
            } else {
                let total_sum: f64 = matched.iter().map(|m| m.sum).sum();
                let total_count: f64 = matched.iter().map(|m| m.count as f64).sum();
                if total_count > 0.0 {
                    total_sum / total_count
                } else {
                    0.0
                }
            }
        }
        // k6's `value`/`last` stat = the most recent sample. W1-A: the OLD
        // code had NO value arm here — `Some(_) => return None` made every
        // `.value` threshold on a custom series FAIL CLOSED forever (the
        // `vus: ['value>10']` sub-item: the translator emitted `vus.value >
        // 10` and the evaluator never resolved it). With Trend/Gauge now
        // tracking a real `last`, worst-of (max) across matches.
        // k6's `value`/`last` stat = the most recent sample. W1-A: the OLD
        // code had NO value arm here — `Some(_) => return None` made every
        // `.value` threshold on a custom series FAIL CLOSED forever (the
        // `vus: ['value>10']` sub-item: the translator emitted `vus.value >
        // 10` and the evaluator never resolved it). Trend/Gauge now track a
        // real `last` (worst-of max across matches). Counter/Rate hardcode
        // `last` 0.0, so they FAIL CLOSED (same guard as the tag-scoped
        // path) — resolving them would re-open the always-pass hole on
        // `http_req_failed.value`.
        Some("value") | Some("last") => {
            if !matched
                .iter()
                .all(|m| matches!(m.metric_type, MetricType::Trend | MetricType::Gauge))
            {
                return None;
            }
            matched.iter().map(|m| m.last).fold(0.0_f64, f64::max)
        }
        // Bare metric (no stat) → worst mean across matches.
        None => matched.iter().map(|m| m.mean).fold(0.0_f64, f64::max),
        // Backlog §1: an UNKNOWN/typo'd stat must FAIL CLOSED, not silently
        // gate on the mean like the old `_ =>` arm.
        Some(_) => return None,
    })
}

/// Extract a metric value from the MetricsResult by name and optional
/// statistic. Returns `None` when the metric has NO samples at all, so the
/// evaluator can distinguish "no data" (fails closed, like k6) from a real
/// measured 0.0.
fn get_metric_value(metrics: &MetricsResult, name: &str, stat: Option<&str>) -> Option<f64> {
    match name {
        "http_reqs" => {
            // Backlog §1: the OLD code discarded the stat entirely —
            // `http_reqs: ['rate>100']` evaluated 5000 > 100 and PASSED where
            // k6 fails (83 req/s on a 60 s / 5000-request run). The per-second
            // rate is now real (count / run_duration); `avg` on a counter is
            // the same per-second rate (k6 semantics); any other stat
            // (percentiles, min/max) is meaningless on a Counter and FAILS
            // CLOSED instead of reading the summary's hardcoded zeros.
            let secs = metrics.run_duration.as_secs_f64();
            match stat {
                Some("rate") | Some("avg") => Some(if secs > 0.0 {
                    metrics.http_reqs as f64 / secs
                } else {
                    0.0
                }),
                Some("count") | None => Some(metrics.http_reqs as f64),
                _ => None,
            }
        }
        "errors" => {
            // Per-tag series (errors{url=…}) may exist; aggregate per stat
            // across them (k6 merges tagged sub-series for the unscoped
            // metric). Only when a stat is present though — for a bare
            // `errors < N` the top-level counter IS the merged total, while
            // the helper's default arm would return the worst per-series
            // mean (1.0 for value-1 Counter samples) and pass spuriously.
            if let Some(v) = stat.and_then(|_| aggregate_series(metrics, "errors", stat)) {
                Some(v)
            } else {
                let secs = metrics.run_duration.as_secs_f64();
                match stat {
                    Some("rate") | Some("avg") => Some(if secs > 0.0 {
                        metrics.errors as f64 / secs
                    } else {
                        0.0
                    }),
                    None | Some("count") => Some(metrics.errors as f64),
                    _ => None,
                }
            }
        }
        // NOTE: `parse_metric_ref` strips the stat at the last dot, so a
        // `checks.rate` / `checks.pass_rate` reference arrives here as
        // name="checks", stat=Some("rate"/"pass_rate") — the dotted-name
        // arms below would be unreachable. Match on (name, stat) instead of
        // on a dot-stripped name, otherwise `checks: ['rate>0.99']` (the
        // stock k6 gate) compares the check COUNT against the rate and
        // always passes.
        "checks" | "checks.total" => Some(match stat {
            Some("passed") => metrics.checks_passed as f64,
            Some("failed") => metrics.checks_failed as f64,
            Some("rate") | Some("pass_rate") => {
                if metrics.checks_total > 0 {
                    metrics.checks_passed as f64 / metrics.checks_total as f64
                } else {
                    0.0
                }
            }
            // Bare metric or `count` → total checks run.
            None | Some("count") => metrics.checks_total as f64,
            // Backlog §1: an unknown stat on checks (e.g. `checks.bogus`) must
            // fail closed, not resolve to the total like the old `_` arm.
            Some(_) => return None,
        }),
        "http_req_duration" => metrics.http_req_duration.as_ref().and_then(|d| {
            match stat {
                Some("avg") => Some(d.mean),
                Some("min") => Some(d.min),
                Some("max") => Some(d.max),
                Some("p50") | Some("median") | Some("med") => Some(d.p50),
                Some("p90") => Some(d.p90),
                Some("p95") => Some(d.p95),
                Some("p99") => Some(d.p99),
                Some("count") => Some(d.count as f64),
                // k6's `value` stat on a trend = the most recent sample.
                Some("value") | Some("last") => Some(d.last),
                Some("sum") => Some(d.sum),
                // Rate = sum/count (mirrors the custom-metric loop).
                Some("rate") => {
                    if d.count > 0 {
                        Some(d.sum / d.count as f64)
                    } else {
                        Some(0.0)
                    }
                }
                // Any other pNN / p(NN) percentile — exact from the
                // retained histogram (not the mean, not a bucket guess).
                Some(s) if parse_percentile(s).is_some() => {
                    Some(percentile_value(d, parse_percentile(s).expect("guarded")))
                }
                // No stat → mean (documented default for a bare metric).
                None => Some(d.mean),
                // Backlog §1: an UNKNOWN/typo'd stat (e.g. `p95th`) must FAIL
                // CLOSED — the old `_ => d.mean` silently gated on the mean,
                // passing `http_req_duration.p95th < 500000` on a 1200 ms p95.
                Some(_) => None,
            }
        }),
        "http_req_failed" => {
            // W1-A (capped vs totals): `metrics.http_req_failed` is the
            // totals-repaired headline rate — `build_results` computes it
            // from incremental counters maintained on EVERY sample, including
            // cardinality-dropped series. The OLD code fell through to
            // `aggregate_series`, which iterates the CAPPED per-series list:
            // a series-dropped run evaluated a truncated total, and bare
            // `http_req_failed < 0.01` read the WORST per-series mean (1.0
            // when any URL all-failed) instead of the true merged ratio. The
            // summary prints the headline rate; the threshold must evaluate
            // the same number. `value`/`last` resolve the same merged ratio
            // (k6 rate semantics) rather than failing closed on the invented
            // `last: 0.0` (W1-A #3 tightened the invented-zero hole; the real
            // headline makes the stat meaningful).
            match stat {
                None | Some("rate") | Some("avg") | Some("value") | Some("last") => {
                    Some(metrics.http_req_failed)
                }
                // Percentiles/min/max on a Rate: no distribution (hardcoded
                // zeros) — fail closed (backlog line 60).
                _ => None,
            }
        }
        "data_received" | "data_sent" => {
            // W1-A (capped vs totals): the headline counters are repaired
            // from the totals map (`build_results` max()es them over EVERY
            // sample incl. cardinality-dropped series), so they are the exact
            // totals even when the per-series list is capped. k6 counter
            // semantics: bare/count/sum = accumulated bytes; rate/avg =
            // accumulated / run_duration (per-second). Percentiles on a
            // Counter: no distribution — fail closed (backlog line 60).
            let total = if name == "data_received" {
                metrics.data_received
            } else {
                metrics.data_sent
            };
            match stat {
                None | Some("count") | Some("sum") => Some(total),
                Some("rate") | Some("avg") => {
                    let secs = metrics.run_duration.as_secs_f64();
                    Some(if secs > 0.0 { total / secs } else { 0.0 })
                }
                _ => None,
            }
        }
        _ => {
            // Custom metric (e.g. user metrics): aggregate across ALL series
            // whose key starts with the name. The naive first-match returned
            // an arbitrary tagged series instead of the merged value k6
            // reports for the unscoped metric. `None` = no series at all → no
            // data.
            aggregate_series(metrics, name, stat)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::{MetricSummary, MetricType, MetricsResult};

    fn make_metrics() -> MetricsResult {
        MetricsResult {
            http_reqs: 100,
            errors: 2,
            checks_total: 50,
            checks_passed: 45,
            checks_failed: 5,
            http_req_duration: Some(MetricSummary {
                key: "http_req_duration".into(),
                tags: vec![],
                metric_type: MetricType::Trend,
                count: 100,
                sum: 50000.0,
                mean: 500.0,
                min: 50.0,
                max: 2000.0,
                p50: 450.0,
                p90: 900.0,
                p95: 1200.0,
                p99: 1800.0,
                // W1-A: a REAL last sample (2500 ms) — the old fixture carried
                // 0.0, which made `.value` thresholds pass trivially AND let
                // the bug-pinning test below assert the buggy behaviour.
                last: 2500.0,
                rate: 0.0,
                histogram: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_threshold_p95_under() {
        let metrics = make_metrics();
        let result = evaluate_single_threshold("http_req_duration.p95 < 1500", &metrics);
        assert!(result.0, "p95 1200 should be < 1500");
        assert_eq!(result.1, 1200.0);
        assert_eq!(result.2, 1500.0);
    }

    #[test]
    fn test_threshold_p95_over() {
        let metrics = make_metrics();
        let result = evaluate_single_threshold("http_req_duration.p95 < 1000", &metrics);
        assert!(!result.0, "p95 1200 should NOT be < 1000");
        assert_eq!(result.1, 1200.0);
        assert_eq!(result.2, 1000.0);
    }

    // ── merged-population percentiles (backlog line 58) ──

    /// Build a Trend series carrying an exact retained histogram: `pairs` of
    /// (ms value, count) recorded into a fresh LatencyHistogram.
    fn trend_series_with_hist(
        name: &str,
        tags: Vec<(&str, &str)>,
        pairs: &[(u64, usize)],
    ) -> MetricSummary {
        let mut h = LatencyHistogram::new();
        for (ms, n) in pairs {
            for _ in 0..*n {
                h.record_ms(*ms as f64);
            }
        }
        let stats = h.stats();
        MetricSummary {
            key: name.to_string(),
            tags: tags
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            metric_type: MetricType::Trend,
            count: stats.count,
            sum: pairs.iter().map(|(ms, n)| (*ms as f64) * (*n as f64)).sum(),
            mean: stats.mean,
            min: stats.min,
            max: stats.max,
            p50: stats.p50,
            p90: stats.p90,
            p95: stats.p95,
            p99: stats.p99,
            last: 0.0,
            rate: 0.0,
            histogram: Some(h),
        }
    }

    #[test]
    fn unscoped_percentile_is_merged_population_not_worst_series() {
        // Two http_req_waiting series: a fast majority (1000 × 10 ms) and a
        // slow minority (10 × 2000 ms). k6's unscoped p95 is the p95 of the
        // MERGED population (~10 ms — the fast majority dominates), NOT the
        // worst sub-series (2000 ms). The old fold(f64::max) reported 2000 ms.
        let metrics = MetricsResult {
            http_reqs: 0,
            errors: 0,
            series_dropped: 0,
            checks_total: 0,
            checks_passed: 0,
            checks_failed: 0,
            http_req_duration: None,
            iteration_duration: None,
            data_received: 0.0,
            data_sent: 0.0,
            dropped_iterations: 0,
            http_req_failed: 0.0,
            iterations: 0,
            vus_max: 0,
            run_duration: Duration::from_secs(10),
            metrics: vec![
                trend_series_with_hist(
                    "http_req_waiting{url=/fast}",
                    vec![("url", "/fast")],
                    &[(10, 1000)],
                ),
                trend_series_with_hist(
                    "http_req_waiting{url=/slow}",
                    vec![("url", "/slow")],
                    &[(2000, 10)],
                ),
            ],
            per_url: vec![],
            per_group: vec![],
            summary_trend_stats: vec![],
            effective_thresholds: HashMap::new(),
        };

        // Unscoped: merged population p95 ≈ 10 ms → threshold passes.
        let result = evaluate_single_threshold("http_req_waiting.p95 < 100", &metrics);
        assert!(
            result.0,
            "merged p95 (~10 ms) must pass < 100, got {}",
            result.1
        );
        assert!(
            result.1 < 100.0,
            "actual {} must be the merged p95, not the 2000 ms worst series",
            result.1
        );

        // Sanity: tag-scoped to the SLOW series alone must still fail closed
        // to its own 2000 ms — merging must not leak across scopes.
        let slow = evaluate_single_threshold("http_req_waiting{url=/slow}.p95 < 100", &metrics);
        assert!(
            !slow.0,
            "slow series alone must stay at ~2000 ms and fail, got {}",
            slow.1
        );
    }

    #[test]
    fn tag_scoped_percentile_merges_matching_population() {
        // Two series both matching {status=200} (distinct urls): 1000 × 10 ms
        // and 20 × 100 ms (the fast majority is 98% of the merged population,
        // so the merged p95 index 969 lands in the fast region → ~10 ms). k6
        // evaluates `{status=200}` over the MERGED population of every
        // matching series — p95 ≈ 10 ms, not the worst (100 ms). Tag-scoped
        // was *always* max-folded before this fix.
        let metrics = MetricsResult {
            metrics: vec![
                trend_series_with_hist(
                    "http_req_waiting{status=200,url=/a}",
                    vec![("status", "200"), ("url", "/a")],
                    &[(10, 1000)],
                ),
                trend_series_with_hist(
                    "http_req_waiting{status=200,url=/b}",
                    vec![("status", "200"), ("url", "/b")],
                    &[(100, 20)],
                ),
            ],
            ..make_metrics()
        };

        let result = evaluate_single_threshold("http_req_waiting{status=200}.p95 < 50", &metrics);
        assert!(
            result.0,
            "merged p95 (~10 ms) must pass < 50, got {} — the old max-fold gave 100 ms",
            result.1
        );

        // A more selective scope must still isolate its own population.
        let only_b =
            evaluate_single_threshold("http_req_waiting{status=200,url=/b}.p95 < 50", &metrics);
        assert!(
            !only_b.0,
            "url=/b alone is ~100 ms and must fail, got {}",
            only_b.1
        );
    }

    #[test]
    fn merged_percentile_falls_back_to_worst_without_histograms() {
        // No retained histograms (pre-config window / synthetic summaries):
        // the fallback keeps the old worst-of behavior so a missing histogram
        // can never false-PASS a threshold.
        let mut m1 =
            trend_series_with_hist("http_req_waiting{url=/a}", vec![("url", "/a")], &[(10, 10)]);
        let mut m2 = trend_series_with_hist(
            "http_req_waiting{url=/b}",
            vec![("url", "/b")],
            &[(2000, 10)],
        );
        m1.histogram = None;
        m2.histogram = None;
        let metrics = MetricsResult {
            metrics: vec![m1, m2],
            ..make_metrics()
        };
        let result = evaluate_single_threshold("http_req_waiting.p95 < 500", &metrics);
        assert!(
            !result.0,
            "no histograms → worst-of fallback (2000 ms) must fail < 500, got {}",
            result.1
        );
    }

    // ── compound && / || (backlog line 154) ──

    #[test]
    fn compound_and_requires_all_clauses() {
        // p95=1200 < 1500 AND p90=900 < 1000 → both pass.
        let metrics = make_metrics();
        let result = evaluate_single_threshold(
            "http_req_duration.p95 < 1500 && http_req_duration.p90 < 1000",
            &metrics,
        );
        assert!(result.0, "&& passes when ALL clauses pass");
        assert_eq!(result.1, 900.0, "reports the last clause's actual");
        assert_eq!(result.2, 1000.0, "reports the last clause's threshold");

        // p95=1200 < 1500 (true) AND p90=900 < 500 (false) → fails.
        let result = evaluate_single_threshold(
            "http_req_duration.p95 < 1500 && http_req_duration.p90 < 500",
            &metrics,
        );
        assert!(!result.0, "&& requires ALL clauses");
    }

    #[test]
    fn compound_or_passes_on_any_clause() {
        // p95=1200 > 1500 (false) OR p90=900 < 1000 (true) → passes.
        let metrics = make_metrics();
        let result = evaluate_single_threshold(
            "http_req_duration.p95 > 1500 || http_req_duration.p90 < 1000",
            &metrics,
        );
        assert!(result.0, "|| passes if ANY clause passes");

        // Both false → fails.
        let result = evaluate_single_threshold(
            "http_req_duration.p95 > 1500 || http_req_duration.p90 > 1000",
            &metrics,
        );
        assert!(!result.0, "|| fails when ALL clauses fail");
    }

    #[test]
    fn compound_and_binds_tighter_than_or() {
        // `a && b || c` — && binds tighter: (a AND b) OR c. With make_metrics
        // p95=1200, p90=900, p99=1800: a = (p95>1500)=false, b = (p90<1000)=true,
        // so the && group is false; c = (p99>1500)=true saves the || group.
        let metrics = make_metrics();
        let result = evaluate_single_threshold(
            "http_req_duration.p95 > 1500 && http_req_duration.p90 < 1000 || \
             http_req_duration.p99 > 1500",
            &metrics,
        );
        assert!(result.0, "c saves the OR group: {result:?}");

        // c=false too → fails.
        let result = evaluate_single_threshold(
            "http_req_duration.p95 > 1500 && http_req_duration.p90 < 1000 || \
             http_req_duration.p99 < 1500",
            &metrics,
        );
        assert!(!result.0, "&& group fails and || group fails: {result:?}");
    }

    #[test]
    fn validate_accepts_compound_rejects_bad_clause() {
        let mut ok = HashMap::new();
        ok.insert(
            "compound".to_string(),
            ThresholdConfig {
                expression: "p(95) < 500 && p(99) < 1000".into(),
                abort_on_fail: false,
                delay_abort_eval: None,
            },
        );
        assert!(validate_thresholds(&ok).is_ok(), "&& thresholds validate");

        let mut bad = HashMap::new();
        bad.insert(
            "bad".to_string(),
            ThresholdConfig {
                expression: "p(95) < 500 && p(99) < not-a-number".into(),
                abort_on_fail: false,
                delay_abort_eval: None,
            },
        );
        let err = validate_thresholds(&bad).unwrap_err();
        assert!(
            err.contains("not-a-number"),
            "bad clause names the offending value: {err}"
        );
    }

    #[test]
    fn test_threshold_errors() {
        let metrics = make_metrics();
        let result = evaluate_single_threshold("errors < 10", &metrics);
        assert!(result.0);
        assert_eq!(result.1, 2.0);
    }

    #[test]
    fn test_threshold_http_reqs() {
        let metrics = make_metrics();
        let result = evaluate_single_threshold("http_reqs > 50", &metrics);
        assert!(result.0);
        assert_eq!(result.1, 100.0);
    }

    #[test]
    fn test_threshold_pass_rate() {
        let metrics = make_metrics();
        let result = evaluate_single_threshold("checks.pass_rate > 0.8", &metrics);
        assert!(result.0, "pass rate 0.9 should be > 0.8");
    }

    // ── Backlog §1: stat resolver fails CLOSED, Counter rate is real ──

    #[test]
    fn counter_rate_is_per_second_not_raw_count() {
        // `http_reqs: ['rate>100']` on a 100-request run lasting 0.5 s →
        // 200 req/s. The OLD code discarded the stat and compared the raw
        // count (100 > 100 = false… barely); on a 60 s / 5000-request run it
        // compared 5000 > 100 and PASSED where k6 (83 req/s) fails.
        let mut metrics = make_metrics();
        metrics.run_duration = Duration::from_millis(500);
        let result = evaluate_single_threshold("http_reqs.rate > 100", &metrics);
        assert!(result.0, "200 req/s should be > 100");
        assert_eq!(result.1, 200.0);

        let result = evaluate_single_threshold("http_reqs.rate > 500", &metrics);
        assert!(!result.0, "200 req/s should NOT be > 500");

        // avg on a Counter is the same per-second rate (k6 semantics).
        let result = evaluate_single_threshold("http_reqs.avg < 500", &metrics);
        assert!(result.0, "200 req/s should be < 500");
    }

    #[test]
    fn counter_percentile_stat_fails_closed() {
        // Percentiles on a Counter are meaningless — must FAIL, not read a
        // hardcoded zero / summary placeholder.
        let metrics = make_metrics();
        let result = evaluate_single_threshold("http_reqs.p95 < 1", &metrics);
        assert!(!result.0, "p95 on a Counter must fail closed");
    }

    // ── Backlog line 60: Rate/Gauge percentile stats fail closed ──

    /// Build a MetricsResult whose `metrics` list carries the given custom
    /// series (used to simulate `http_req_failed` as a Rate and
    /// `queue_depth` as a Gauge without driving a full run).
    fn make_metrics_with(series: MetricSummary) -> MetricsResult {
        let mut m = make_metrics();
        m.metrics.push(series);
        m
    }

    #[test]
    fn rate_percentile_stat_fails_closed() {
        // THE regression (backlog line 60): `http_req_failed` is a **Rate**;
        // its summary hardcodes p50..p99 = 0. The OLD guard only checked
        // `all(Counter)`, so `http_req_failed: ['p(95)<0.01']` on a
        // 100%-failure run read p95 = 0 < 0.01 and PASSED. It must fail
        // closed instead — a Rate has no distribution to percentile.
        let series = MetricSummary {
            key: "http_req_failed".into(),
            tags: vec![],
            metric_type: MetricType::Rate,
            count: 100,
            sum: 100.0, // every request failed → rate = 1.0
            mean: 1.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 0.0,
            rate: 1.0,
            histogram: None,
        };
        // W1-A: `rate`/`avg`/bare on http_req_failed now read the FIRST-CLASS
        // headline (`metrics.http_req_failed`, totals-repaired) — set it to
        // what build_results would produce on an all-failed run (1.0). The
        // pushed series is no longer consulted for the unscoped metric.
        let mut metrics = make_metrics_with(series);
        metrics.http_req_failed = 1.0;
        // p95 on a Rate must NOT evaluate to the hardcoded 0 (which would
        // pass `p(95) < 0.01`) — it must fail closed.
        let result = evaluate_single_threshold("http_req_failed.p95 < 0.01", &metrics);
        assert!(
            !result.0,
            "p95 on a Rate must fail closed, not read the hardcoded 0"
        );
        let result = evaluate_single_threshold("http_req_failed.p(90) < 0.01", &metrics);
        assert!(!result.0, "p(90) on a Rate must fail closed too");
        let result = evaluate_single_threshold("http_req_failed.max < 0.01", &metrics);
        assert!(
            !result.0,
            "max on a Rate must fail closed (Rate hardcodes min/max = 0)"
        );
        // Sanity: a MEANINGFUL Rate stat still resolves — the headline rate
        // (1.0 on an all-failed run) must breach `> 0.5`.
        let result = evaluate_single_threshold("http_req_failed.rate > 0.5", &metrics);
        assert!(result.0, "rate 1.0 should be > 0.5");
        let result = evaluate_single_threshold("http_req_failed.avg > 0.5", &metrics);
        assert!(result.0, "avg 1.0 should be > 0.5");
        // And it must FAIL against a real 1.0 ratio when the gate is tighter.
        let result = evaluate_single_threshold("http_req_failed.rate < 0.5", &metrics);
        assert!(!result.0, "rate 1.0 should fail < 0.5");
    }

    #[test]
    fn gauge_percentile_stat_fails_closed_but_min_max_resolve() {
        // Same class (backlog line 60): a `new Gauge('queue_depth')` summary
        // hardcodes p50..p99 = 0, so `queue_depth: ['p(95)<10']` passed
        // trivially. Percentiles must fail closed — but Gauge tracks REAL
        // min/max from its samples, so those two stats must still resolve.
        let series = MetricSummary {
            key: "queue_depth".into(),
            tags: vec![],
            metric_type: MetricType::Gauge,
            count: 100,
            sum: 5000.0,
            mean: 50.0,
            min: 5.0,
            max: 200.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 75.0,
            rate: 0.0,
            histogram: None,
        };
        let metrics = make_metrics_with(series);
        // Percentiles must fail closed, not read the hardcoded 0.
        let result = evaluate_single_threshold("queue_depth.p95 < 10", &metrics);
        assert!(
            !result.0,
            "p95 on a Gauge must fail closed, not read the hardcoded 0"
        );
        let result = evaluate_single_threshold("queue_depth.p(99.9) < 10", &metrics);
        assert!(!result.0, "arbitrary p(99.9) on a Gauge must fail closed");
        // But the real extremes resolve: max 200 > 100, min 5 < 10.
        let result = evaluate_single_threshold("queue_depth.max > 100", &metrics);
        assert!(result.0, "Gauge max 200 should be > 100");
        let result = evaluate_single_threshold("queue_depth.min < 10", &metrics);
        assert!(result.0, "Gauge min 5 should be < 10");
        // And the tracked avg resolves too.
        let result = evaluate_single_threshold("queue_depth.avg > 40", &metrics);
        assert!(result.0, "Gauge avg 50 should be > 40");
    }

    #[test]
    fn trend_percentile_stats_still_resolve() {
        // Sanity: the broader guard must not break real Trend percentiles
        // (the ONLY type with a genuine distribution).
        let metrics = make_metrics();
        let result = evaluate_single_threshold("http_req_duration.p95 < 5000", &metrics);
        assert!(result.0, "Trend p95 1200 should be < 5000");
        let result = evaluate_single_threshold("http_req_duration.p(75) < 5000", &metrics);
        assert!(
            result.0,
            "arbitrary p(75) on a Trend (retained histogram) should resolve"
        );
        let result = evaluate_single_threshold("http_req_duration.min > 10", &metrics);
        assert!(result.0, "Trend min 50 should be > 10");
        let result = evaluate_single_threshold("http_req_duration.max > 1000", &metrics);
        assert!(result.0, "Trend max 2000 should be > 1000");
    }

    #[test]
    fn tag_scoped_rate_percentile_fails_closed() {
        // The tag-scoped path (get_tag_scoped_metric_value) had the same
        // `all(Counter)`-only guard — a tag-scoped Rate percentile must fail
        // closed there too (backlog line 60). The tag value is colon-free
        // (`/a`) because parse_metric_ref's `:`-vs-`=` separator heuristic
        // would mis-split a value containing `://`.
        let series = MetricSummary {
            key: "http_req_failed{url=/a}".into(),
            tags: vec![("url".into(), "/a".into())],
            metric_type: MetricType::Rate,
            count: 50,
            sum: 50.0,
            mean: 1.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 0.0,
            rate: 1.0,
            histogram: None,
        };
        let metrics = make_metrics_with(series);
        let result = evaluate_single_threshold("http_req_failed{url=/a}.p95 < 0.01", &metrics);
        assert!(
            !result.0,
            "tag-scoped p95 on a Rate must fail closed, not read the hardcoded 0"
        );
        // The real rate on that tag still resolves.
        let result = evaluate_single_threshold("http_req_failed{url=/a}.rate > 0.5", &metrics);
        assert!(result.0, "tag-scoped rate 1.0 should be > 0.5");
    }

    #[test]
    fn unknown_trend_stat_fails_closed() {
        // `http_req_duration.p95th` (typo): mean = 500, p95 = 1200. The OLD
        // `_ => d.mean` arm gated on the mean (500 < 900 → PASS); a threshold
        // between the two proves the value is not the mean.
        let metrics = make_metrics();
        let result = evaluate_single_threshold("http_req_duration.p95th < 900", &metrics);
        assert!(
            !result.0,
            "unknown stat p95th must fail closed, not gate on the mean (500 < 900 would pass)"
        );
    }

    #[test]
    fn unknown_custom_metric_stat_fails_closed() {
        // Custom-metric path (aggregate_series): an unknown stat must fail
        // closed there too — the OLD `_ =>` arm returned worst mean. Uses a
        // genuinely custom metric (`custom_counter`) — `data_received` is a
        // first-class headline metric since W1-A #5.
        let mut metrics = make_metrics();
        metrics.metrics.push(MetricSummary {
            key: "custom_counter".into(),
            tags: vec![],
            metric_type: MetricType::Counter,
            count: 1500,
            sum: 1500.0,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 1.0,
            rate: 0.0,
            histogram: None,
        });
        let result = evaluate_single_threshold("custom_counter.bogus > 0", &metrics);
        assert!(
            !result.0,
            "unknown stat on a custom metric must fail closed"
        );

        // Sanity: a VALID stat still resolves.
        let result = evaluate_single_threshold("custom_counter.count > 1000", &metrics);
        assert!(result.0, "count 1500 should be > 1000");
    }

    #[test]
    fn counter_rate_via_aggregate_is_per_second() {
        // `data_received: ['rate>1000000']` on a 60 s run receiving 3 MB →
        // 50 000 B/s. W1-A #5: `data_received` is now a FIRST-CLASS headline
        // arm reading `metrics.data_received` (the totals-repaired value)
        // — set it to what `build_results` produces (3 MB received). The OLD
        // path returned the per-series mean (~1500) → permanently red.
        let mut metrics = make_metrics();
        metrics.run_duration = Duration::from_secs(60);
        metrics.data_received = 3_000_000.0;
        metrics.metrics.push(MetricSummary {
            key: "data_received{url=/a}".into(),
            tags: vec![("url".into(), "/a".into())],
            metric_type: MetricType::Counter,
            count: 3_000_000,
            sum: 3_000_000.0,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 1.0,
            rate: 0.0,
            histogram: None,
        });
        let result = evaluate_single_threshold("data_received.rate > 10000", &metrics);
        assert!(result.0, "50000 B/s should be > 10000");
        let result = evaluate_single_threshold("data_received.avg > 10000", &metrics);
        assert!(result.0, "avg on a counter is the per-second rate too");
        // Tag-scoped still resolves via the per-series path.
        let result = evaluate_single_threshold("data_received{url=/a}.count > 1000", &metrics);
        assert!(result.0, "tag-scoped count 3000000 should be > 1000");
    }

    #[test]
    fn checks_unknown_stat_fails_closed() {
        // `checks.bogus > 0` must fail, not resolve to the total (the old
        // `_ => checks_total` arm passed any > 0 gate).
        let metrics = make_metrics();
        let result = evaluate_single_threshold("checks.bogus > 0", &metrics);
        assert!(!result.0, "unknown checks stat must fail closed");
        // Bare checks still resolves to the total.
        let result = evaluate_single_threshold("checks > 40", &metrics);
        assert!(result.0, "bare checks should resolve to the total (50)");
    }

    #[test]
    fn validate_rejects_unknown_stat_at_startup() {
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "typo".to_string(),
            abort_config("http_req_duration.p95th < 500", false, None),
        );
        assert!(
            validate_thresholds(&thresholds).is_err(),
            "a typo'd stat must abort at startup"
        );
        // Valid stats (incl. arbitrary percentiles) still pass.
        let mut ok = HashMap::new();
        ok.insert(
            "a".to_string(),
            abort_config("http_reqs.rate > 10", false, None),
        );
        ok.insert(
            "b".to_string(),
            abort_config("http_req_duration.p(99.9) < 1000", false, None),
        );
        ok.insert(
            "c".to_string(),
            abort_config("checks.pass_rate > 0.9", false, None),
        );
        ok.insert(
            "d".to_string(),
            abort_config("http_req_duration.value < 500", false, None),
        );
        assert!(validate_thresholds(&ok).is_ok());
    }

    #[test]
    fn percentile_on_aggregate_counter_fails_closed() {
        // Round-2 review catch: the top-level http_reqs arm fails closed on
        // percentiles, but the aggregate/tag-scoped paths didn't — a Counter's
        // p95 bucket is 1.0, so `data_received.p95 > 0.5` passed. Must fail.
        let mut metrics = make_metrics();
        metrics.run_duration = Duration::from_secs(60);
        metrics.metrics.push(MetricSummary {
            key: "data_received".into(),
            tags: vec![],
            metric_type: MetricType::Counter,
            count: 3_000_000,
            sum: 3_000_000.0,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 1.0,
            rate: 0.0,
            histogram: None,
        });
        let result = evaluate_single_threshold("data_received.p95 > 0.5", &metrics);
        assert!(!result.0, "p95 on an aggregated Counter must fail closed");

        // Same via the tag-scoped path.
        let result = evaluate_single_threshold("data_received{url=/x}.p95 > 0.5", &metrics);
        assert!(!result.0, "tag-scoped p95 on a Counter must fail closed");
    }

    // ── W1-A: `.value` / `.last` must resolve to the REAL last sample ──
    // The OLD behaviour had THREE wrong answers: (1) the unscoped Trend arm
    // read `d.last`, hardcoded 0.0 in trend_summary → always PASSED a `<`
    // gate; (2) the tag-scoped arm read an arbitrary `matched.last()` with
    // last=0.0 for Counter/Rate/Trend → always passed; (3) aggregate_series
    // had NO value arm → `Some(_) => return None` → always FAILED closed
    // (the `vus: ['value>10']` sub-item: translator built, evaluator never
    // written). This test was INVERTED — it used to assert the buggy
    // behaviour (`value maps to the last sample (0 < 500)`).

    #[test]
    fn trend_value_stat_resolves_to_last_sample() {
        let metrics = make_metrics(); // fixture last = 2500 ms
                                      // The old pinned test asserted `value < 500` PASSES because last was
                                      // hardcoded 0.0. With a real last, 2500 must FAIL a < 500 gate.
        let result = evaluate_single_threshold("http_req_duration.value < 500", &metrics);
        assert!(
            !result.0,
            "value must resolve to the REAL last sample (2500), got {} — the old hardcoded 0 passed trivially",
            result.1
        );
        assert_eq!(result.1, 2500.0, "actual must be the last sample");
        // And pass a gate the real value satisfies.
        let result = evaluate_single_threshold("http_req_duration.last > 2000", &metrics);
        assert!(result.0, "last 2500 should be > 2000");
    }

    #[test]
    fn tag_scoped_value_is_worst_of_not_arbitrary_or_hardcoded_zero() {
        // Two http_req_duration series matching {status=200}: last 100 ms and
        // last 2500 ms. The OLD tag-scoped arm read an ARBITRARY
        // `matched.last()` (iteration-order dependent) with a hardcoded 0 for
        // Trend — the value threshold could pass against 0. Now worst-of
        // (max) across matches, so a failure on ANY series is visible.
        let m1 = MetricSummary {
            key: "http_req_duration{status=200,url=/a}".into(),
            tags: vec![("status".into(), "200".into()), ("url".into(), "/a".into())],
            metric_type: MetricType::Trend,
            count: 1,
            sum: 100.0,
            mean: 100.0,
            min: 100.0,
            max: 100.0,
            p50: 100.0,
            p90: 100.0,
            p95: 100.0,
            p99: 100.0,
            last: 100.0,
            rate: 0.0,
            histogram: None,
        };
        let m2 = MetricSummary {
            key: "http_req_duration{status=200,url=/b}".into(),
            tags: vec![("status".into(), "200".into()), ("url".into(), "/b".into())],
            metric_type: MetricType::Trend,
            count: 1,
            sum: 2500.0,
            mean: 2500.0,
            min: 2500.0,
            max: 2500.0,
            p50: 2500.0,
            p90: 2500.0,
            p95: 2500.0,
            p99: 2500.0,
            last: 2500.0,
            rate: 0.0,
            histogram: None,
        };
        let mut metrics = make_metrics();
        metrics.metrics.push(m1);
        metrics.metrics.push(m2);
        let result =
            evaluate_single_threshold("http_req_duration{status=200}.value < 500", &metrics);
        assert!(
            !result.0,
            "worst-of value (2500) must fail < 500, got {} — the old arbitrary/hardcoded-0 passed",
            result.1
        );
        assert_eq!(
            result.1, 2500.0,
            "actual must be the worst last across matches"
        );
    }

    #[test]
    fn custom_series_value_stat_resolves() {
        // The `vus: ['value>10']` sub-item: the k6 translator emitted
        // `vus.value > 10` (options.rs) but aggregate_series had NO value arm
        // — `Some(_) => return None` made it FAIL CLOSED forever. A Gauge
        // series (vus is a Gauge) carrying a real last must now resolve.
        let series = MetricSummary {
            key: "vus".into(),
            tags: vec![],
            metric_type: MetricType::Gauge,
            count: 100,
            sum: 5000.0,
            mean: 50.0,
            min: 5.0,
            max: 200.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 42.0,
            rate: 0.0,
            histogram: None,
        };
        let metrics = make_metrics_with(series);
        let result = evaluate_single_threshold("vus.value > 10", &metrics);
        assert!(
            result.0,
            "vus.value (42) must resolve and pass > 10, got {} — aggregate_series had no value arm",
            result.1
        );
        assert_eq!(result.1, 42.0, "actual must be the Gauge's last sample");
        let result = evaluate_single_threshold("vus.value < 10", &metrics);
        assert!(!result.0, "vus.value 42 must fail < 10");
        let result = evaluate_single_threshold("vus.last > 10", &metrics);
        assert!(result.0, "vus.last must alias value");
    }

    #[test]
    fn rate_counter_value_stat_fails_closed() {
        // W1-A follow-up: Counter/Rate hardcode `last: 0.0`, so resolving
        // `.value`/`.last` on them would re-open the always-pass hole —
        // `http_req_failed.value < 0.01` must fail on a 100%-failure run,
        // not read the invented 0.0. Same guard philosophy as the percentile
        // path (backlog line 60).
        let rate_series = MetricSummary {
            key: "http_req_failed".into(),
            tags: vec![],
            metric_type: MetricType::Rate,
            count: 100,
            sum: 100.0, // every request failed → rate = 1.0
            mean: 1.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 0.0,
            rate: 1.0,
            histogram: None,
        };
        // W1-A #5: `http_req_failed.value`/`.last` resolve the FIRST-CLASS
        // headline (`metrics.http_req_failed`, totals-repaired) — on a real
        // 100%-failure run that is 1.0, so the gate must still FAIL (not pass
        // against an invented 0.0). Set the headline to the all-failed value.
        let mut metrics = make_metrics_with(rate_series);
        metrics.http_req_failed = 1.0;
        let result = evaluate_single_threshold("http_req_failed.value < 0.01", &metrics);
        assert!(
            !result.0,
            "value on a Rate must fail against the real 1.0 headline, not pass"
        );
        let result = evaluate_single_threshold("http_req_failed.last < 0.01", &metrics);
        assert!(!result.0, "last on a Rate must fail against 1.0 too");
        // And it must PASS against a healthy headline (bare `http_req_failed`
        // is the summary's own number now).
        metrics.http_req_failed = 0.0;
        let result = evaluate_single_threshold("http_req_failed.value < 0.01", &metrics);
        assert!(result.0, "value 0.0 (healthy) should pass < 0.01");

        // Tag-scoped variant: same guard.
        let tagged = MetricSummary {
            key: "http_req_failed{url=/a}".into(),
            tags: vec![("url".into(), "/a".into())],
            metric_type: MetricType::Rate,
            count: 50,
            sum: 50.0,
            mean: 1.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 0.0,
            rate: 1.0,
            histogram: None,
        };
        let metrics = make_metrics_with(tagged);
        let result = evaluate_single_threshold("http_req_failed{url=/a}.value < 0.01", &metrics);
        assert!(!result.0, "tag-scoped value on a Rate must fail closed");
    }

    // ── W1-A #5: capped series vs `totals` — thresholds read the totals ──
    // `build_results` repairs the headline counters from the COMPLETE totals
    // map (incl. cardinality-dropped series), but the OLD `aggregate_series`
    // iterated the CAPPED per-series list — summary printed 1.20 GB while a
    // threshold on the same quantity evaluated ~780 MB, and bare
    // `http_req_failed < 0.01` read the worst per-series mean (1.0 when any
    // URL all-failed). The first-class arms must make thresholds evaluate
    // exactly the numbers the summary prints.

    #[test]
    fn headline_http_req_failed_matches_summary_not_worst_series() {
        // Summary prints Failed: 1 (0.01 %); the OLD bare `http_req_failed`
        // read the worst per-series mean → 1.0 → FAIL and non-zero exit on a
        // healthy run. The threshold must evaluate `metrics.http_req_failed`.
        let mut metrics = make_metrics();
        metrics.http_req_failed = 0.01; // 1 failure per 100 requests
        metrics.metrics.push(MetricSummary {
            key: "http_req_failed{url=/a}".into(),
            tags: vec![("url".into(), "/a".into())],
            metric_type: MetricType::Rate,
            count: 100,
            sum: 100.0, // this one URL failed 100%
            mean: 1.0,
            min: 0.0,
            max: 0.0,
            p50: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            last: 0.0,
            rate: 1.0,
            histogram: None,
        });
        let result = evaluate_single_threshold("http_req_failed < 0.05", &metrics);
        assert!(
            result.0,
            "bare http_req_failed must read the headline 0.01, not the worst series 1.0, got {}",
            result.1
        );
        assert_eq!(result.1, 0.01, "actual must be the headline rate");
        let result = evaluate_single_threshold("http_req_failed.rate < 0.05", &metrics);
        assert!(result.0, ".rate must read the headline too");
        // A genuinely failing run (1.0 headline) still fails the gate.
        metrics.http_req_failed = 1.0;
        let result = evaluate_single_threshold("http_req_failed < 0.05", &metrics);
        assert!(!result.0, "headline 1.0 must fail < 0.05");
    }

    #[test]
    fn data_received_threshold_reads_totals_not_capped_series() {
        // `build_results` repairs `data_received` from the complete totals map
        // (incl. cardinality-dropped series) → 1.20 GB, while the per-series
        // list (what the OLD `aggregate_series` iterated) only saw 780 MB. A
        // threshold on the same quantity must evaluate the totals number.
        let mut metrics = make_metrics();
        metrics.data_received = 1_200_000_000.0; // 1.20 GB — totals-repaired
        metrics.metrics.push(MetricSummary {
            key: "data_received".into(),
            tags: vec![],
            metric_type: MetricType::Counter,
            count: 780_000_000,
            sum: 780_000_000.0, // capped series list only saw ~780 MB
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 1.0,
            rate: 0.0,
            histogram: None,
        });
        // The threshold on the same quantity the summary prints must PASS.
        // NOTE: the threshold expression parser does not accept underscore
        // digit separators, so the literals below are plain digits.
        let result = evaluate_single_threshold("data_received > 1000000000", &metrics);
        assert!(
            result.0,
            "data_received must read the totals-repaired 1.20 GB, not the capped 780 MB, got {}",
            result.1
        );
        let result = evaluate_single_threshold("data_received.count > 1000000000", &metrics);
        assert!(result.0, ".count must read the headline total too");
        // data_sent works the same way.
        metrics.data_sent = 600_000_000.0;
        let result = evaluate_single_threshold("data_sent > 500000000", &metrics);
        assert!(result.0, "data_sent must read its headline total");
        // And per-second rates come from the headline, not the capped list.
        metrics.run_duration = Duration::from_secs(60);
        let result = evaluate_single_threshold("data_received.rate > 1000000", &metrics);
        assert!(
            result.0,
            "rate 20000000 B/s must read the headline, not the capped list"
        );
    }

    // ── Arbitrary-percentile tests ──

    /// Build a MetricsResult whose http_req_duration carries a real retained
    /// histogram with values 100..=1000 ms (10 samples, mean 550 ms).
    /// p75 ≈ 775-800 ms — far above the mean — so a `.p75 < 600` threshold
    /// MUST fail. Before the histogram was retained, any non-{p50,p90,p95,p99}
    /// stat silently fell back to the mean (550) → false PASS.
    fn make_histogram_metrics() -> MetricsResult {
        use crate::histogram::LatencyHistogram;
        let mut h = LatencyHistogram::new();
        for i in 1..=10u64 {
            h.record_ms((i * 100) as f64);
        }
        MetricsResult {
            http_req_duration: Some(MetricSummary {
                key: "http_req_duration".into(),
                tags: vec![],
                metric_type: MetricType::Trend,
                count: 10,
                sum: 5500.0,
                mean: 550.0,
                min: 100.0,
                max: 1000.0,
                p50: 500.0,
                p90: 900.0,
                p95: 950.0,
                p99: 990.0,
                last: 0.0,
                rate: 0.0,
                histogram: Some(h),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_arbitrary_percentile_exact_not_mean() {
        let metrics = make_histogram_metrics();
        // p75 of 100..1000 (10 values) is ~775-800 µs. The mean is 550.
        let result = evaluate_single_threshold("http_req_duration.p75 < 600", &metrics);
        assert!(
            !result.0,
            "p75 must be > 600 (mean fallback would false-PASS)"
        );
        assert!(
            result.1 > 600.0 && result.1 < 850.0,
            "p75 should be an exact histogram percentile, got {}",
            result.1
        );
        assert_ne!(result.1, 550.0, "must not fall back to the mean");
    }

    #[test]
    fn test_arbitrary_percentile_paren_syntax() {
        let metrics = make_histogram_metrics();
        // k6-style p(90) syntax
        let result = evaluate_single_threshold("http_req_duration.p(90) < 2000", &metrics);
        assert!(result.0, "p90 of 100..1000 is ~900, should be < 2000");
        assert!(
            result.1 >= 850.0 && result.1 <= 1000.0,
            "p(90) should be an exact histogram percentile, got {}",
            result.1
        );
    }

    #[test]
    fn test_decimal_percentile_exact_from_histogram() {
        // Backlog line 135 regression: p(99.9) must resolve EXACTLY from the
        // retained histogram, never the mean. parse_metric_ref splits the
        // stat at the last dot OUTSIDE parens — a naive rfind('.') split
        // `.p(99.9)` at the decimal point (stat "9", name "…p(99"), which
        // would fall back to the mean / a non-existent metric.
        let metrics = make_histogram_metrics();
        let result = evaluate_single_threshold("http_req_duration.p(99.9) < 2000", &metrics);
        assert!(result.0, "p99.9 of 100..1000 is ~1000, should be < 2000");
        assert!(
            result.1 > 950.0,
            "p(99.9) must be an exact high percentile (~1000), got {}",
            result.1
        );
        assert_ne!(result.1, 550.0, "must not fall back to the mean");

        // Tag-scoped decimal percentile resolves through the after-`}` path.
        use crate::histogram::LatencyHistogram;
        let mut h = LatencyHistogram::new();
        for i in 1..=10u64 {
            h.record_ms((i * 100) as f64);
        }
        let mut m = MetricsResult::default();
        m.metrics.push(MetricSummary {
            key: "http_req_duration{status=200}".into(),
            tags: vec![("status".into(), "200".into())],
            metric_type: MetricType::Trend,
            count: 10,
            sum: 5500.0,
            mean: 550.0,
            min: 100.0,
            max: 1000.0,
            p50: 500.0,
            p90: 900.0,
            p95: 950.0,
            p99: 990.0,
            last: 0.0,
            rate: 0.0,
            histogram: Some(h),
        });
        let result = evaluate_single_threshold("http_req_duration{status=200}.p(99.9) < 2000", &m);
        assert!(result.0);
        assert!(
            result.1 > 950.0,
            "tag-scoped p(99.9) must be exact, got {}",
            result.1
        );
    }

    #[test]
    fn test_arbitrary_percentile_tag_scoped() {
        // Build a tag-scoped series carrying a histogram, then assert the
        // tag-scoped path also resolves non-standard percentiles exactly.
        use crate::histogram::LatencyHistogram;
        let mut h = LatencyHistogram::new();
        for i in 1..=10u64 {
            h.record_ms((i * 100) as f64);
        }
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "http_req_duration{status=200}".into(),
            tags: vec![("status".into(), "200".into())],
            metric_type: MetricType::Trend,
            count: 10,
            sum: 5500.0,
            mean: 550.0,
            min: 100.0,
            max: 1000.0,
            p50: 500.0,
            p90: 900.0,
            p95: 950.0,
            p99: 990.0,
            last: 0.0,
            rate: 0.0,
            histogram: Some(h),
        });
        let result = evaluate_single_threshold("http_req_duration{status=200}.p75 < 600", &metrics);
        assert!(!result.0, "tag-scoped p75 must also be exact, not the mean");
        assert_ne!(result.1, 550.0);
    }

    // ── Tag-scoped threshold tests ──

    fn make_tag_scoped_metrics() -> MetricsResult {
        // Build MetricsResult with per-tag http_req_duration entries
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "http_req_duration{status=200}".into(),
            tags: vec![("status".into(), "200".into())],
            metric_type: MetricType::Trend,
            count: 80,
            sum: 32000.0,
            mean: 400.0,
            min: 50.0,
            max: 1500.0,
            p50: 350.0,
            p90: 700.0,
            p95: 900.0,
            p99: 1400.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        metrics.metrics.push(MetricSummary {
            key: "http_req_duration{status=500}".into(),
            tags: vec![("status".into(), "500".into())],
            metric_type: MetricType::Trend,
            count: 10,
            sum: 15000.0,
            mean: 1500.0,
            min: 500.0,
            max: 3000.0,
            p50: 1200.0,
            p90: 2500.0,
            p95: 2800.0,
            p99: 3000.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        metrics
    }

    #[test]
    fn test_tag_scoped_p95_under() {
        let metrics = make_tag_scoped_metrics();
        // http_req_duration{status=200}.p95 < 1000
        let result =
            evaluate_single_threshold("http_req_duration{status=200}.p95 < 1000", &metrics);
        assert!(result.0, "p95 900 should be < 1000");
        assert_eq!(result.1, 900.0);
        assert_eq!(result.2, 1000.0);
    }

    #[test]
    fn test_tag_scoped_p95_over() {
        let metrics = make_tag_scoped_metrics();
        // http_req_duration{status=500}.p95 < 2000
        let result =
            evaluate_single_threshold("http_req_duration{status=500}.p95 < 2000", &metrics);
        assert!(!result.0, "p95 2800 should NOT be < 2000");
        assert_eq!(result.1, 2800.0);
        assert_eq!(result.2, 2000.0);
    }

    #[test]
    fn test_tag_scoped_mean() {
        let metrics = make_tag_scoped_metrics();
        // http_req_duration{status=200}.avg < 500
        let result = evaluate_single_threshold("http_req_duration{status=200}.avg < 500", &metrics);
        assert!(result.0, "mean 400 should be < 500");
        assert!((result.1 - 400.0).abs() < 0.01);
    }

    #[test]
    fn test_tag_scoped_no_stat_defaults_to_mean() {
        let metrics = make_tag_scoped_metrics();
        // http_req_duration{status=200} < 500 — no stat, defaults to mean
        let result = evaluate_single_threshold("http_req_duration{status=200} < 500", &metrics);
        assert!(result.0, "mean 400 should be < 500");
    }

    #[test]
    fn test_tag_scoped_colon_syntax() {
        let metrics = make_tag_scoped_metrics();
        // Use colon syntax: {status:200}
        let result =
            evaluate_single_threshold("http_req_duration{status:200}.p95 < 1000", &metrics);
        assert!(result.0, "colon syntax should work");
        assert_eq!(result.1, 900.0);
    }

    #[test]
    fn test_tag_scoped_nonexistent_tag() {
        let metrics = make_tag_scoped_metrics();
        // Tag that doesn't exist in the metrics = no samples for that series.
        // Fail CLOSED: a missing series must FAIL the threshold (k6 marks
        // no-data thresholds as failed), never pass a `<` against an invented
        // 0.0 (the old behavior silently reported green).
        let result = evaluate_single_threshold("http_req_duration{status=404}.p95 < 100", &metrics);
        assert!(!result.0, "missing tag must fail closed, not pass with 0.0");
        assert_eq!(result.1, 0.0);
    }

    #[test]
    fn test_unscoped_custom_metric_aggregates_all_series() {
        // http_req_failed is emitted as one series per (url,status) tag set;
        // the unscoped threshold must merge ALL of them (k6 semantics), not
        // return one arbitrary series' rate.
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "http_req_failed{url=a}".into(),
            tags: vec![("url".into(), "a".into())],
            metric_type: MetricType::Rate,
            count: 100,
            sum: 20.0, // 20% failed
            mean: 0.2,
            min: 0.0,
            max: 1.0,
            p50: 0.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 0.2,
            histogram: None,
        });
        metrics.metrics.push(MetricSummary {
            key: "http_req_failed{url=b}".into(),
            tags: vec![("url".into(), "b".into())],
            metric_type: MetricType::Rate,
            count: 100,
            sum: 100.0, // 100% failed
            mean: 1.0,
            min: 0.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 1.0,
            histogram: None,
        });
        // W1-A #5: the unscoped http_req_failed now reads the FIRST-CLASS
        // headline (`metrics.http_req_failed`), which `build_results` computes
        // from incremental counters on EVERY sample — the merged 120
        // failures / 200 samples = 0.60. The per-series list may be CAPPED
        // (cardinality drops), so it is no longer consulted for the unscoped
        // metric; the headline is the single source (the summary prints it
        // too, so thresholds and stdout always agree).
        metrics.http_req_failed = 0.6;
        let (passed, actual, _) = evaluate_single_threshold("http_req_failed.rate < 0.5", &metrics);
        assert!(!passed, "merged rate 0.60 must not pass < 0.5");
        assert!(
            (actual - 0.6).abs() < 1e-9,
            "headline rate should be 0.6, got {actual}"
        );

        let (passed, _, _) = evaluate_single_threshold("http_req_failed.rate < 0.7", &metrics);
        assert!(passed, "merged rate 0.60 should pass < 0.7");
    }

    #[test]
    fn test_unscoped_count_sums_all_series() {
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "errors{url=a}".into(),
            tags: vec![("url".into(), "a".into())],
            metric_type: MetricType::Counter,
            count: 7,
            sum: 7.0,
            mean: 1.0,
            min: 0.0,
            max: 1.0,
            p50: 0.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        metrics.metrics.push(MetricSummary {
            key: "errors{url=b}".into(),
            tags: vec![("url".into(), "b".into())],
            metric_type: MetricType::Counter,
            count: 3,
            sum: 3.0,
            mean: 1.0,
            min: 0.0,
            max: 1.0,
            p50: 0.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        let (_, actual, _) = evaluate_single_threshold("errors.count > 5", &metrics);
        assert_eq!(actual, 10.0, "count must sum across all series");
    }

    #[test]
    fn test_threshold_exact_name_not_prefix_aggregated() {
        // Regression (backlog line 80): the unscoped series lookup used
        // `starts_with`, so a threshold on `login` aggregated `login_errors`
        // and `login_duration` too. Series must match the base name exactly.
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "login".into(),
            tags: vec![],
            metric_type: MetricType::Trend,
            count: 5,
            sum: 1000.0,
            mean: 200.0,
            min: 50.0,
            max: 400.0,
            p50: 200.0,
            p90: 300.0,
            p95: 350.0,
            p99: 390.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        metrics.metrics.push(MetricSummary {
            key: "login_errors".into(),
            tags: vec![],
            metric_type: MetricType::Trend,
            count: 500, // high count — would swamp `login` if prefix-folded
            sum: 1.0,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        metrics.metrics.push(MetricSummary {
            key: "login_duration".into(),
            tags: vec![],
            metric_type: MetricType::Trend,
            count: 500,
            sum: 1.0,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });

        // `login.count < 10` must evaluate against ONLY the `login` series
        // (count 5). Prefix folding would have summed all three → 1005 → fail.
        let (passed, actual, _) = evaluate_single_threshold("login.count < 10", &metrics);
        assert!(
            passed,
            "login.count must be 5 (only the exact series), not 1005"
        );
        assert_eq!(actual, 5.0);

        // A threshold on login_errors itself still resolves its own series.
        let (passed, actual, _) = evaluate_single_threshold("login_errors.count < 1000", &metrics);
        assert!(passed);
        assert_eq!(actual, 500.0);
    }

    #[test]
    fn test_tag_scoped_exact_name_not_prefix() {
        // Tag-scoped lookup had the same starts_with bug: `{status=200}` on
        // `login` must not match `login_errors{status=200}`.
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "login{status=200}".into(),
            tags: vec![("status".into(), "200".into())],
            metric_type: MetricType::Trend,
            count: 2,
            sum: 400.0,
            mean: 200.0,
            min: 100.0,
            max: 300.0,
            p50: 200.0,
            p90: 250.0,
            p95: 280.0,
            p99: 290.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        metrics.metrics.push(MetricSummary {
            key: "login_errors{status=200}".into(),
            tags: vec![("status".into(), "200".into())],
            metric_type: MetricType::Trend,
            count: 900,
            sum: 1.0,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            p50: 1.0,
            p90: 1.0,
            p95: 1.0,
            p99: 1.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });

        let (passed, actual, _) =
            evaluate_single_threshold("login{status=200}.count < 10", &metrics);
        assert!(
            passed,
            "must match only the login{{status=200}} series (count 2)"
        );
        assert_eq!(actual, 2.0);
    }

    #[test]
    fn test_tag_scoped_matches_real_comma_joined_key() {
        // Regression (backlog §1): the collector renders ONE comma-joined tag
        // group (`http_req_duration{status=200,method=GET}`), but the old
        // matcher substring-matched `{status=200}` — which can never appear
        // because the closing brace follows the LAST tag. The most-copied k6
        // threshold `http_req_duration{status:200}` reported actual = 0.00 on
        // a healthy run. Matching is now structural against the parsed tags.
        let mut metrics = MetricsResult::default();
        metrics.metrics.push(MetricSummary {
            key: "http_req_duration{status=200,method=GET}".into(),
            tags: vec![
                ("status".into(), "200".into()),
                ("method".into(), "GET".into()),
            ],
            metric_type: MetricType::Trend,
            count: 80,
            sum: 32000.0,
            mean: 400.0,
            min: 50.0,
            max: 1500.0,
            p50: 350.0,
            p90: 700.0,
            p95: 900.0,
            p99: 1400.0,
            last: 0.0,
            rate: 0.0,
            histogram: None,
        });
        // The k6-copied form, with the `:` tag separator and a single filter.
        let (passed, actual, _) =
            evaluate_single_threshold("http_req_duration{status:200}.p95 < 1000", &metrics);
        assert!(passed, "status=200 filter must match the comma-joined key");
        assert_eq!(actual, 900.0);

        // A second filter on the same key must also match.
        let (passed, actual, _) = evaluate_single_threshold(
            "http_req_duration{status=200,method=GET}.p95 < 1000",
            &metrics,
        );
        assert!(passed, "both filters must match");
        assert_eq!(actual, 900.0);

        // A filter the key does NOT carry must not match.
        let (passed, _, _) =
            evaluate_single_threshold("http_req_duration{status=404}.p95 < 1000", &metrics);
        assert!(!passed, "status=404 must not match the status=200 series");
    }

    #[test]
    fn test_parse_metric_ref_no_tags() {
        let (name, tags, stat) = parse_metric_ref("http_req_duration.p95");
        assert_eq!(name, "http_req_duration");
        assert!(tags.is_empty());
        assert_eq!(stat, Some("p95"));
    }

    #[test]
    fn test_parse_metric_ref_with_tags() {
        let (name, tags, stat) = parse_metric_ref("http_req_duration{status=200}.p95");
        assert_eq!(name, "http_req_duration");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0], ("status", "200"));
        assert_eq!(stat, Some("p95"));
    }

    #[test]
    fn test_parse_metric_ref_colon_tags() {
        let (name, tags, stat) = parse_metric_ref("http_req_duration{status:200}.p95");
        assert_eq!(name, "http_req_duration");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0], ("status", "200"));
        assert_eq!(stat, Some("p95"));
    }

    #[test]
    fn test_parse_metric_ref_url_scoped_splits_on_first_separator() {
        // W1-B line 147: `url=https://…` — the scheme colon must NOT win the
        // split, or the key becomes "url=https" and the threshold silently
        // never matches. The separator that occurs FIRST positionally wins.
        let (name, tags, stat) =
            parse_metric_ref("http_req_duration{url=https://api.x.dev/a?b=1}.p95");
        assert_eq!(name, "http_req_duration");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0], ("url", "https://api.x.dev/a?b=1"));
        assert_eq!(stat, Some("p95"));

        // Colon-scoped tags still split on the colon (no `=` present).
        let (_, tags, _) = parse_metric_ref("http_req_duration{method:POST}.p95");
        assert_eq!(tags, vec![("method", "POST")]);

        // A key with an `=` BEFORE the scheme colon keeps the `=` split.
        let (_, tags, _) = parse_metric_ref("http_req_duration{name=a=b}.p95");
        assert_eq!(tags, vec![("name", "a=b")]);
    }

    #[test]
    fn test_parse_metric_ref_no_stat() {
        let (name, tags, stat) = parse_metric_ref("http_reqs");
        assert_eq!(name, "http_reqs");
        assert!(tags.is_empty());
        assert_eq!(stat, None);
    }

    #[test]
    fn test_parse_metric_ref_stat_only() {
        let (name, tags, stat) = parse_metric_ref("checks.pass_rate");
        assert_eq!(name, "checks");
        assert!(tags.is_empty());
        assert_eq!(stat, Some("pass_rate"));
    }

    // ── check_abort_on_fail (backlog §6 P1: zero coverage before) ──

    fn abort_config(expression: &str, abort_on_fail: bool, delay: Option<&str>) -> ThresholdConfig {
        ThresholdConfig {
            expression: expression.to_string(),
            abort_on_fail,
            delay_abort_eval: delay.map(|s| s.to_string()),
        }
    }

    #[test]
    fn abort_fires_on_breach_with_abort_on_fail() {
        // p95 = 1200, threshold 1000 → breached, abort_on_fail → true.
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "dur".to_string(),
            abort_config("http_req_duration.p95 < 1000", true, None),
        );
        assert!(check_abort_on_fail(
            &thresholds,
            &make_metrics(),
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn abort_ignores_non_abort_thresholds() {
        // Breached but abort_on_fail = false → must NOT abort (the run
        // continues and reports the failure at the end).
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "dur".to_string(),
            abort_config("http_req_duration.p95 < 1000", false, None),
        );
        assert!(!check_abort_on_fail(
            &thresholds,
            &make_metrics(),
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn abort_respects_delay_abort_eval_grace_period() {
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "dur".to_string(),
            abort_config("http_req_duration.p95 < 1000", true, Some("30s")),
        );
        // Inside the 30s grace period: breach must NOT abort yet.
        assert!(!check_abort_on_fail(
            &thresholds,
            &make_metrics(),
            Duration::from_secs(10)
        ));
        // After the grace period: breach aborts.
        assert!(check_abort_on_fail(
            &thresholds,
            &make_metrics(),
            Duration::from_secs(31)
        ));
    }

    #[test]
    fn abort_does_not_fire_when_metric_has_no_samples_yet() {
        // Mid-run, the metric series may not have arrived yet (the `_opt`
        // variant returns None). A threshold on it must NOT abort — data
        // may simply not have been recorded in the first instant.
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "dur".to_string(),
            abort_config("http_req_duration.p95 < 1000", true, None),
        );
        let empty = MetricsResult::default(); // no http_req_duration series
        assert!(!check_abort_on_fail(
            &thresholds,
            &empty,
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn abort_only_fires_for_actually_breached_thresholds() {
        // p95 = 1200 < 1500 → passes → no abort even with abort_on_fail.
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "dur".to_string(),
            abort_config("http_req_duration.p95 < 1500", true, None),
        );
        assert!(!check_abort_on_fail(
            &thresholds,
            &make_metrics(),
            Duration::from_secs(5)
        ));
    }
}
