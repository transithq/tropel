//! # End-to-end test
//!
//! Exercises the FULL pipeline the way a user would drive it: a Postman
//! collection file is parsed by the postman adapter, executed by 2 VUs
//! against a real local HTTP server, with a `{{header}}` variable, a
//! prerequest script that sets a variable, a `pm.test`, and a threshold.
//!
//! Two historical regressions are covered elsewhere by dedicated unit
//! tests: N2 (the script interrupt timer keyed off context-creation time,
//! which killed every eval ~10s in) is pinned by
//! `tropel_js::context::reset_interrupt_keeps_evals_alive_past_original_deadline`;
//! N1 (a shared response slot letting one VU read another's response) is
//! covered by the per-VU response assertions. Keeping this e2e short (3s)
//! keeps `cargo test` fast.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tropel_core::config::{
    ExecutionConfig, JobConfig, OutputConfig, ThinkTimeConfig, ThresholdConfig,
};
use tropel_engine::Engine;
use tropel_ext::registry::ExtensionRegistry;
use tropel_metrics::thresholds::evaluate_thresholds;
use tropel_sdk::Result;

/// Minimal HTTP/1.1 server that records the `X-E2E` header value it sees on
/// each request, then answers `200 {"ok":true}`. Header values are pushed
/// into the shared `seen` list so the test can assert the resolved
/// `{{header}}` variable actually reached the wire.
///
/// Also tracks the PEAK number of simultaneously-open connections via an
/// `AtomicUsize` guard (returned alongside the address) so the test can
/// assert REAL concurrency — `http_reqs >= vus` only proves requests fired,
/// not that `vus` were actually concurrent.
async fn start_echo_server(
    seen: Arc<Mutex<Vec<String>>>,
) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let peak_out = peak.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let seen = seen.clone();
            let active = active.clone();
            let peak = peak.clone();
            tokio::spawn(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                // Keep-alive loop (like behavior_parity's echo server): the
                // client pools HTTP/1.1 connections, and a server that drops
                // the socket after one response races the client's reuse —
                // occasionally failing a pooled request and flaking
                // `checks_failed == 0` assertions. Run the per-connection
                // body in a block so the active-count decrement is guaranteed
                // on every exit path (clean close, read error, write error).
                let conn_result: std::io::Result<()> = async {
                    loop {
                        // Read until the request-head terminator (headers end
                        // at CRLF CRLF). A fresh head per request so a split
                        // packet never loses the header across requests.
                        let mut head = Vec::new();
                        loop {
                            let n = sock.read(&mut buf).await?;
                            if n == 0 {
                                return Ok(());
                            }
                            head.extend_from_slice(&buf[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let text = String::from_utf8_lossy(&head).to_string();
                        for line in text.lines() {
                            let lower = line.to_ascii_lowercase();
                            if let Some(v) = lower.strip_prefix("x-e2e:") {
                                // Poison-tolerant: a panicked test thread must
                                // not let this connection task die before the
                                // active-count decrement (which would inflate
                                // peak forever).
                                seen.lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .push(v.trim().to_string());
                            }
                        }
                        // Hold the connection open so concurrent VUs overlap —
                        // the same determinism trick as behavior_parity's
                        // peak server.
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        let body = r#"{"ok":true}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        sock.write_all(resp.as_bytes()).await?;
                    }
                }
                .await;
                let _ = conn_result;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (addr, peak_out)
}

/// Echo server with a LATENCY TAIL: every 6th request sleeps ~500ms while
/// the rest answer in ~10ms. Builds a distribution where the exact p75 is
/// fast (~10ms) but the tracked p90 bucket lands in the slow tail (~500ms) —
/// the setup that discriminates exact-percentile evaluation (backlog line
/// 59a) from the p90 fallback: `p75 < 100` passes exactly, but would abort
/// a healthy run if p75 silently became p90.
async fn start_echo_server_with_latency_tail() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req_no = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let req_no = req_no.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    let mut head = Vec::new();
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        head.extend_from_slice(&buf[..n]);
                        if head.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let n = req_no.fetch_add(1, Ordering::SeqCst);
                    if n.is_multiple_of(6) {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    let body = r#"{"ok":true}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// Server that records the `Authorization` header value it sees on each
/// request (used to prove prerequest-added pm.request headers reach the wire).
async fn start_auth_capture_server(seen: Arc<Mutex<Vec<String>>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                // Keep-alive loop (same shape as start_echo_server): the
                // client pools HTTP/1.1 connections, and a server that drops
                // the socket after ONE response races the client's reuse —
                // occasionally failing a pooled request. W1-A made those
                // transport failures COUNT as failed checks (the stale
                // pm.response bug used to mask them by passing the check
                // against the previous request's 200), so this server MUST
                // serve multiple requests per connection or the run reports
                // hundreds of real failures.
                loop {
                    // Read until the request-head terminator. A fresh head
                    // per request so a split packet never loses the header
                    // across requests.
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    let read_ok = loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break false,
                            Ok(n) => n,
                        };
                        head.extend_from_slice(&buf[..n]);
                        if head.windows(4).any(|w| w == b"\r\n\r\n") {
                            break true;
                        }
                    };
                    if !read_ok {
                        break;
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    for line in text.lines() {
                        // Match the header name case-insensitively but capture
                        // the VALUE in its ORIGINAL case (lowercasing the
                        // whole line would corrupt "Bearer s3cret" into
                        // "bearer s3cret").
                        if line.to_ascii_lowercase().starts_with("authorization:") {
                            if let Some(idx) = line.find(':') {
                                seen.lock()
                                    .unwrap()
                                    .push(line[idx + 1..].trim().to_string());
                            }
                        }
                    }
                    let body = r#"{"ok":true}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// Write a minimal Postman collection to a temp file and return its path.
///
/// The single request sends `X-E2E: {{header}}`; its prerequest script sets
/// a variable (`pm.variables.set`), and its test script asserts both the
/// HTTP status and that the prerequest-set variable is still visible — the
/// exact seam where a broken prerequest→test bridge or a broken response
/// slot would surface.
fn write_collection(base: &str, tag: &str) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tropel-e2e-{}-{}.json", std::process::id(), tag));
    let url = format!("{base}/");
    let collection = serde_json::json!({
        "info": {
            "name": "e2e",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": [{
            "name": "req1",
            "request": {
                "method": "GET",
                "url": url,
                "header": [{"key": "X-E2E", "value": "{{header}}"}]
            },
            "event": [
                {
                    "listen": "prerequest",
                    "script": {
                        "exec": ["pm.variables.set('pre_req_var', 'set-in-prerequest');"],
                        "type": "text/javascript"
                    }
                },
                {
                    "listen": "test",
                    "script": {
                        "exec": [
                            "pm.test('status is 200', function () { pm.response.to.have.status(200); });",
                            "pm.test('prereq var visible', function () { return pm.variables.get('pre_req_var') === 'set-in-prerequest'; });"
                        ],
                        "type": "text/javascript"
                    }
                }
            ]
        }]
    });
    let json = serde_json::to_string(&collection).unwrap();
    std::fs::write(&path, json).unwrap();
    path.to_string_lossy().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_two_vu_with_header_check_and_threshold() -> Result<()> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (srv, peak) = start_echo_server(seen.clone()).await;
    let coll = write_collection(&format!("http://{srv}"), "two-vu");

    // Threshold on http_req_duration (samples are MILLISECONDS): a generous
    // 5 s ceiling that still reflects REAL latency — a hardcoded pass or an
    // empty series (actual = 0) must fail the "> 0 real samples" asserts.
    let mut thresholds = HashMap::new();
    thresholds.insert(
        "http_req_duration".to_string(),
        ThresholdConfig {
            expression: "http_req_duration.p95 < 5000".to_string(),
            abort_on_fail: false,
            delay_abort_eval: None,
        },
    );

    let mut env = HashMap::new();
    env.insert("header".to_string(), "e2e-header-value".to_string());

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            // Short run to keep cargo test fast — the ~10s interrupt-timer
            // regression has dedicated unit coverage in tropel-js.
            duration: "3s".to_string(),
            graceful_stop: Some("5s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env,
        thresholds,
        // Keep the test output clean: no stdout summary stream.
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;
    let m = &result.metrics;

    // 1. The {{header}} variable was resolved and the header sent.
    {
        let seen_guard = seen.lock().unwrap();
        assert!(
            seen_guard.iter().any(|v| v == "e2e-header-value"),
            "server saw headers {:?}, expected the resolved 'e2e-header-value'",
            *seen_guard
        );
    }

    // 2. The pm.test checks ran (and the prerequest→test var bridge worked).
    //    checks_failed == 0 is essential: the test script has TWO pm.tests —
    //    a status check (passes regardless) and a prerequest-var check. If
    //    `pm.variables.get` were broken, only the second would fail, leaving
    //    checks_passed > 0 true. Zero failures pins the bridge down.
    assert!(
        m.checks_total > 0,
        "checks_total > 0, got {}",
        m.checks_total
    );
    assert_eq!(
        m.checks_failed, 0,
        "all checks passed, got {} failed of {} total",
        m.checks_failed, m.checks_total
    );

    // 3. The threshold reflected REAL latency: the http_req_duration series
    //    has samples with a measured max, and the evaluation passes.
    let dur = m
        .http_req_duration
        .as_ref()
        .expect("http_req_duration summary present");
    assert!(dur.count > 0, "http_req_duration has samples");
    assert!(
        dur.max > 0.0,
        "http_req_duration max > 0 (real latency measured)"
    );

    let threshold_results = evaluate_thresholds(&result.effective_thresholds, m);
    let t = threshold_results
        .iter()
        .find(|t| t.name == "http_req_duration")
        .expect("threshold evaluated");
    assert!(
        t.passed,
        "threshold '{}' passed (actual={} < {})",
        t.expression, t.actual, t.threshold
    );

    // Sanity: both VUs actually made requests.
    // Real concurrency: the server observed >= 2 simultaneously-open
    // connections. `http_reqs >= 2` only proves requests fired — a
    // sequential runner would pass it; two overlapping connections prove
    // the 2 configured VUs actually ran in parallel.
    let peak_obs = peak.load(Ordering::SeqCst);
    assert!(
        peak_obs >= 2,
        "peak concurrent connections >= 2, got {} (vus actually parallel)",
        peak_obs
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog line 145: a prerequest script using `pm.request.headers.add(...)`
/// to attach an Authorization header must ACTUALLY send that header — the
/// primary purpose of pm.request. The runner used to rebuild the wire request
/// from the static collection item, silently discarding every mutation; this
/// test pins the fix by asserting the server received the prerequest header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prerequest_pm_request_header_reaches_the_wire() -> Result<()> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let srv = start_auth_capture_server(seen.clone()).await;

    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "tropel-e2e-prereq-hdr-{}-prereq.json",
        std::process::id()
    ));
    let url = format!("http://{srv}/");
    let collection = serde_json::json!({
        "info": {
            "name": "prereq-hdr",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": [{
            "name": "req1",
            "request": {
                "method": "GET",
                "url": url,
                "header": []
            },
            "event": [
                {
                    "listen": "prerequest",
                    "script": {
                        "exec": [
                            // Literal token: the point of this test is that the
                            // prerequest pm.request mutation REACHES THE WIRE,
                            // not env resolution (pm.environment.get reads
                            // script-set state, not JobConfig env).
                            "pm.request.headers.add({ key: 'Authorization', value: 'Bearer fixed-token' });"
                        ],
                        "type": "text/javascript"
                    }
                },
                {
                    "listen": "test",
                    "script": {
                        "exec": [
                            "pm.test('status is 200', function () { pm.response.to.have.status(200); });"
                        ],
                        "type": "text/javascript"
                    }
                }
            ]
        }]
    });
    let coll = path.to_string_lossy().to_string();
    std::fs::write(&path, serde_json::to_string(&collection).unwrap()).unwrap();

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        // SharedIterations (not ConstantVus): a bounded iteration count
        // completes cleanly before the engine stops, so no in-flight request
        // is clipped by a wall-clock stop boundary. The old 3s ConstantVus
        // could abort exactly one in-flight request at t=3s on a loaded CI
        // runner (ubuntu) → 1 failed check of ~5473 → flaky.
        execution: ExecutionConfig::SharedIterations {
            iterations: 1000,
            // Safety net: fail fast if the capture server ever stalls
            // (matches the file's other SharedIterations tests).
            max_duration: Some("30s".to_string()),
            vus: 2,
            graceful_stop: Some("10s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env: HashMap::new(),
        thresholds: HashMap::new(),
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;

    {
        let seen_guard = seen.lock().unwrap();
        assert!(
            seen_guard.iter().any(|v| v == "Bearer fixed-token"),
            "server saw Authorization headers {:?}; the prerequest pm.request.headers.add was dropped by the runner",
            *seen_guard
        );
    }
    assert!(
        result.metrics.checks_failed == 0,
        "all checks passed, got {} failed of {} total",
        result.metrics.checks_failed,
        result.metrics.checks_total
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Regression (backlog line 59a): `set_summary_config` used to fire AFTER
/// all scenarios completed, so the abort coordinator's mid-run `results()`
/// evaluations had `retain_histograms = false`. A NON-tracked percentile
/// (here `p(75)`) then fell back to the nearest tracked bucket (p90). With
/// the latency-tail server, exact p75 ≈ 10ms but p90 ≈ 500ms, so a healthy
/// run under `p(75) < 100` with abortOnFail must run its FULL duration —
/// the old code aborted it at the first ~2s check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_tracked_percentile_threshold_completes_healthy_run() -> Result<()> {
    let srv = start_echo_server_with_latency_tail().await;
    let coll = write_collection(&format!("http://{srv}"), "p75-tail");

    let mut thresholds = HashMap::new();
    thresholds.insert(
        "http_req_duration".to_string(),
        ThresholdConfig {
            expression: "http_req_duration.p(75) < 100".to_string(),
            abort_on_fail: true,
            delay_abort_eval: None,
        },
    );

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            // 6s guarantees the abort coordinator's first ~2s check fires
            // mid-run; a correct run then still completes the full 6s.
            duration: "6s".to_string(),
            graceful_stop: Some("2s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env: HashMap::new(),
        thresholds,
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let start = std::time::Instant::now();
    let result = engine.run(&config).await?;
    let elapsed = start.elapsed();
    let m = &result.metrics;

    // THE regression: the run must NOT abort mid-run. A correct run takes
    // ~6s; an abort at the first ~2s check (p75 fell back to p90 ~= 500ms
    // > 100) finishes around 2-3s. The >= 5s bound discriminates: a
    // completed run is always >= 6s wall-clock, while an aborted run (2s
    // abort + in-flight drain, bounded by the 2s graceful stop) stays below
    // it — worst case ~4-4.5s, which is why the bound is 5s and not tighter.
    assert!(
        elapsed >= std::time::Duration::from_secs(5),
        "healthy p(75) run must complete its full duration; aborted after {elapsed:?}"
    );

    // Sanity: the tail actually produced samples, and the slow-tail
    // distribution is really in place (NON-VACUOUS guard). With 1/6 requests
    // ~500ms and the rest ~10ms, the tracked p95 lands in the slow region
    // (~500ms) while p75 stays fast. If the latency-tail server silently
    // broke (all-fast), this fails loudly instead of passing vacuously — the
    // old code would also have completed an all-fast run.
    assert!(m.http_reqs > 0, "requests fired, got {}", m.http_reqs);
    if let Some(hd) = &m.http_req_duration {
        assert!(
            hd.p95 >= 200.0,
            "latency tail must be present: p95 = {}, want >= 200ms",
            hd.p95
        );
    }

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog line 47: a malformed duration (`-d 30x`) used to produce a
/// zero-VU GREEN run — the engine swallowed the scheduler's parse error
/// (`executor.run(...).await.ok()`) and exited 0 with http_reqs: 0. The
/// scheduler error must surface as a VU-init failure so the run fails
/// loudly after reporting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_duration_fails_run_instead_of_zero_vu_green() -> Result<()> {
    let coll = write_collection("http://127.0.0.1:9", "bad-dur");

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 5,
            duration: "30x".to_string(), // unparseable
            graceful_stop: Some("1s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env: HashMap::new(),
        thresholds: HashMap::new(),
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;

    assert!(
        result.vu_init_failures > 0,
        "malformed duration must fail the run loudly (vu_init_failures={})",
        result.vu_init_failures
    );
    assert_eq!(
        result.metrics.http_reqs, 0,
        "no requests can run with a rejected execution config"
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog line 47, same class: a present-but-unparseable `maxDuration`
/// used to silently become the 10-minute k6 default, changing the profile
/// without any error. It must fail loudly too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_max_duration_fails_run_instead_of_silent_default() -> Result<()> {
    let coll = write_collection("http://127.0.0.1:9", "bad-maxdur");

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        execution: ExecutionConfig::SharedIterations {
            iterations: 1,
            max_duration: Some("10x".to_string()), // unparseable
            vus: 1,
            graceful_stop: Some("1s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env: HashMap::new(),
        thresholds: HashMap::new(),
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;

    assert!(
        result.vu_init_failures > 0,
        "unparseable max_duration must fail the run loudly (vu_init_failures={})",
        result.vu_init_failures
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog line 45: the mid-run abort coordinator fed thresholds a
/// `MetricsResult` whose `run_duration` was ZERO (the collector's `results()`
/// only gets the real elapsed stamped AFTER the run by the engine), so
/// counter `rate`/`avg` thresholds divided by 0 and evaluated to 0.0 — and
/// abortOnFail killed healthy runs at the coordinator's first ~2s check.
/// A 1-VU 3s run against the echo server with `http_reqs.rate > 0` +
/// abortOnFail must run to completion: run_duration must exceed the ~2s
/// window where the old code aborted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn midrun_rate_threshold_with_abort_on_fail_does_not_kill_healthy_run() -> Result<()> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (srv, _peak) = start_echo_server(seen.clone()).await;
    let coll = write_collection(&format!("http://{srv}"), "rate-abort");

    // The canonical k6 throughput gate: requests per second must be REAL.
    let mut thresholds = HashMap::new();
    thresholds.insert(
        "http_reqs".to_string(),
        ThresholdConfig {
            expression: "http_reqs.rate > 0".to_string(),
            abort_on_fail: true,
            delay_abort_eval: None,
        },
    );

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 1,
            duration: "3s".to_string(),
            graceful_stop: Some("2s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env: HashMap::new(),
        thresholds,
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;
    let m = &result.metrics;

    // The run must NOT have been cut at the ~2s first check: a healthy 3s
    // run leaves run_duration ≈ 3s, an abort at t≈2s leaves ≈ 2s. The 2.8s
    // bar leaves headroom against a late first check on a loaded CI (the
    // coordinator ticker uses MissedTickBehavior::Delay, and `results()` is
    // a full aggregate rebuild) without any red risk on the fixed path.
    assert!(
        m.run_duration >= Duration::from_secs(2) + Duration::from_millis(800),
        "run survived the mid-run abort check (run_duration={:?}, expected >= 2.8s)",
        m.run_duration
    );
    assert!(m.http_reqs > 0, "requests were made");

    // The threshold itself evaluates to a PASS post-run (rate = reqs/elapsed).
    let threshold_results = evaluate_thresholds(&result.effective_thresholds, m);
    let t = threshold_results
        .iter()
        .find(|t| t.name == "http_reqs")
        .expect("threshold evaluated");
    assert!(
        t.passed,
        "threshold '{}' passed (actual={})",
        t.expression, t.actual
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// TR-104: the mirror image of the two healthy-run tests above. A threshold
/// that genuinely FAILS with `abort_on_fail: true` must stop the run at the
/// failing request — the first ~2 s abort-coordinator check — NOT run to the
/// end and report green. The old code wired `pm.execution.stopOnError()` to a
/// flag nobody read; the abort coordinator here is the real stop mechanism,
/// and it must fire when a threshold is actually breached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_abort_on_fail_threshold_stops_run_at_the_failing_request() -> Result<()> {
    let srv = start_echo_server_with_latency_tail().await;
    let coll = write_collection(&format!("http://{srv}"), "abort-fail");

    // An impossible threshold: p(95) of the latency-tail server is ~500ms,
    // so `p(95) < 1` fails on every check. With abort_on_fail the run must
    // terminate at the first ~2s coordinator tick.
    let mut thresholds = HashMap::new();
    thresholds.insert(
        "http_req_duration".to_string(),
        ThresholdConfig {
            expression: "http_req_duration.p(95) < 1".to_string(),
            abort_on_fail: true,
            delay_abort_eval: None,
        },
    );

    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("postman".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            // Long enough that an abort at the first check is unambiguous
            // vs. the full run.
            duration: "30s".to_string(),
            graceful_stop: Some("2s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        env: HashMap::new(),
        thresholds,
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let start = std::time::Instant::now();
    let result = engine.run(&config).await?;
    let elapsed = start.elapsed();
    let m = &result.metrics;

    // The run must STOP, not run its full 30s. The abort fires at the first
    // ~2s check + in-flight drain (bounded by the 2s graceful stop). Any
    // completed run is >= 30s wall-clock, so a < 15s bound discriminates
    // sharply between "aborted at the failing request" and "ran to the end".
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "abort_on_fail run must stop at the failing request; ran {elapsed:?} instead"
    );

    // The threshold genuinely failed — non-vacuous guard.
    let threshold_results = evaluate_thresholds(&result.effective_thresholds, m);
    let t = threshold_results
        .iter()
        .find(|t| t.name == "http_req_duration")
        .expect("threshold evaluated");
    assert!(
        !t.passed,
        "threshold '{}' must have failed (actual={})",
        t.expression, t.actual
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}
