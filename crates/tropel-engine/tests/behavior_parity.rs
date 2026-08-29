//! # Behavior-not-shape tests
//!
//! The "shape" tests (metrics present, struct fields correct) can pass while
//! the product silently does nothing — a hardcoded-zero histogram passes
//! "http_req_duration has samples" only if a *behavioral* assertion pins it.
//! These tests exercise real behavior:
//!
//! 1. **k6 end-to-end**: a real k6 script (`http.get` + `check`) is parsed by
//!    the k6 driver, executed by 2 VUs against a local HTTP server, and must
//!    produce `http_reqs > 0`, `checks_total > 0` with **zero failures**, and
//!    a `http_req_duration` series whose `max > 0` — i.e. real measured
//!    latency — that passes a real threshold evaluation. If the k6 shim's
//!    `check` never records, or the HTTP bridge never emits samples, or the
//!    threshold were hardcoded to pass, one of these assertions fails loudly.
//!
//! 2. **Ramping wall-clock**: a `RampingVus` run with staged targets must
//!    actually *span* wall-clock time (stages are not collapsed), actually
//!    reach the stage target (the server observes the peak number of
//!    simultaneously-open connections), and make real requests. A scheduler
//!    that skipped stages or finished instantly fails the elapsed-time and
//!    peak-concurrency assertions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tropel_core::config::{
    ExecutionConfig, HttpConfig, JobConfig, OutputConfig, Stage, ThinkTimeConfig, ThresholdConfig,
};
use tropel_engine::Engine;
use tropel_ext::registry::ExtensionRegistry;
use tropel_metrics::thresholds::evaluate_thresholds;
use tropel_sdk::Result;

/// Minimal HTTP/1.1 server that answers `200 {"ok":true}` while tracking
/// the PEAK number of simultaneously-open connections via an `AtomicUsize`
/// guard, and returns it alongside the address. The response is delayed
/// ~20 ms so concurrent VUs genuinely overlap — a fast local echo would
/// serialize connections and under-count the real concurrency. This is the
/// behavioral fix for the ramping test: `vus_max` is a pure function of
/// the config (`peak_vus()` = `stages.fold`), so only an on-the-wire peak
/// count proves the pool actually grew.
async fn start_peak_server() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
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
            let active = active.clone();
            let peak = peak.clone();
            tokio::spawn(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                // Hold the connection open so concurrent VUs overlap.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let body = r#"{"ok":true}"#;
                // `Connection: close` is load-bearing: this server measures
                // PEAK CONCURRENCY by counting open connections, so it must
                // stay one-shot — but a one-shot server without the header
                // races the client's HTTP/1.1 keep-alive pool (the client
                // reuses a socket the server is closing → occasional pooled
                // transport failures). W1-A made those failures COUNT as
                // failed checks instead of being masked by the stale
                // pm.response bug, so the header is what keeps the pool from
                // reusing this connection at all.
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (addr, peak_out)
}

/// Minimal HTTP/1.1 server that answers `200 {"ok":true}`.
///
/// Serves keep-alive (loops per connection): the client's HTTP/1.1
/// connection pool reuses sockets, and a server that dropped the socket
/// after one response raced the client's reuse — occasionally failing a
/// pooled request and flaking `checks_failed == 0` assertions (the k6
/// test observed 1 failed check of 4283).
async fn start_echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // Per-connection keep-alive loop: read a request head, answer,
                // repeat until the client closes. A fresh buffer per request
                // avoids stale head bytes from the previous pipelined read.
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

/// Server that answers `500 Internal Server Error` — for the error-path
/// tests that assert non-2xx drives `http_req_failed == 1.0`.
async fn start_500_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
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
                    let body = r#"{"error":"boom"}"#;
                    let resp = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
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

/// Server that ACCEPTS connections but never responds — for the timeout
/// test. The client's `request_timeout` must bound the hung request.
async fn start_hung_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Swallow the request, never answer — hold the connection
                // open until the client times out and closes it.
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }
    });
    addr
}

/// Write a minimal k6 script to a temp file. Uses `http.get` + `check` —
/// the exact seam where a broken k6 shim, a broken HTTP bridge, or a broken
/// `checks` recording would surface as zero samples.
///
/// `tag` disambiguates the temp file per test: the tests in this binary run
/// in PARALLEL, so sharing one `{pid}`-keyed filename would race — one
/// test's `remove_file` could delete the script the other is about to read.
fn write_k6_script(base: &str, tag: &str) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tropel-k6-e2e-{}-{}.js", std::process::id(), tag));
    let script = format!(
        r#"import http from 'k6/http';
import {{ check }} from 'k6';

export default function () {{
  const res = http.get('{base}/');
  check(res, {{ 'status is 200': (r) => r.status === 200 }});
}}
"#
    );
    std::fs::write(&path, script).unwrap();
    path.to_string_lossy().to_string()
}

/// Variant with an explicit per-request `params.timeout` — k6's native way
/// to bound a single request shorter than the global ceiling. The k6 shim
/// only packs `timeoutMs` when the script EXPLICITLY sets `params.timeout`
/// (the old hardcoded `params.timeout || '30s'` default shadowed the global
/// `HttpConfig.request_timeout`, so the global never fired on the k6 path).
fn write_k6_timeout_script(base: &str, tag: &str, timeout: &str) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tropel-k6-e2e-{}-{}.js", std::process::id(), tag));
    let script = format!(
        r#"import http from 'k6/http';
import {{ check }} from 'k6';

export default function () {{
  const res = http.get('{base}/', {{ timeout: '{timeout}' }});
  check(res, {{ 'status is 200': (r) => r.status === 200 }});
}}
"#
    );
    std::fs::write(&path, script).unwrap();
    path.to_string_lossy().to_string()
}

/// k6 script run through the full engine: driver parse → JS eval → HTTP →
/// metrics → thresholds. Asserts *behavior*: requests actually fired, checks
/// actually recorded (with zero failures), and the threshold reflects real
/// measured latency (max > 0), not a hardcoded pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k6_script_records_requests_checks_and_real_latency() -> Result<()> {
    let srv = start_echo_server().await;
    let script = write_k6_script(&format!("http://{srv}"), "check");

    // Real-latency threshold: p95 < 5 s. An empty series or a hardcoded-zero
    // histogram must fail the "> 0 real samples" asserts below.
    let mut thresholds = HashMap::new();
    thresholds.insert(
        "http_req_duration".to_string(),
        ThresholdConfig {
            expression: "http_req_duration.p95 < 5000".to_string(),
            abort_on_fail: false,
            delay_abort_eval: None,
        },
    );

    let config = JobConfig {
        input: script.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            duration: "3s".to_string(),
            graceful_stop: Some("5s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
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

    // 1. The k6 script's http.get actually fired requests.
    assert!(m.http_reqs > 0, "http_reqs > 0, got {}", m.http_reqs);

    // 2. The k6 shim's check() recorded — and everything passed (the server
    //    always answers 200, so a single failure means the bridge is broken).
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

    // 3. The http_req_duration series has REAL measured latency.
    let dur = m
        .http_req_duration
        .as_ref()
        .expect("http_req_duration summary present");
    assert!(dur.count > 0, "http_req_duration has samples");
    assert!(
        dur.max > 0.0,
        "http_req_duration max > 0 (real latency measured)"
    );

    // 4. The threshold evaluation passes on the real series.
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

    let _ = std::fs::remove_file(&script);
    Ok(())
}

/// Ramping must be a *real* wall-clock behavior: stages are not collapsed,
/// the pool actually grows toward the stage target, and requests fire.
/// A scheduler that skipped stages (or finished instantly) fails the
/// elapsed-time and peak-concurrency assertions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ramping_stages_span_wall_clock_and_reach_target() -> Result<()> {
    let (srv, peak) = start_peak_server().await;
    let coll = write_k6_script(&format!("http://{srv}"), "ramp");

    // 1s ramp to 3 VUs, 1s hold at 3, 1s ramp down to 1 → ~3s of stages.
    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::RampingVus {
            start_vus: 1,
            stages: vec![
                Stage {
                    duration: "1s".to_string(),
                    target: 3,
                },
                Stage {
                    duration: "1s".to_string(),
                    target: 3,
                },
                Stage {
                    duration: "1s".to_string(),
                    target: 1,
                },
            ],
            graceful_ramp_down: Some("5s".to_string()),
            graceful_stop: Some("5s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let start = Instant::now();
    let result = engine.run(&config).await?;
    let elapsed = start.elapsed();
    let m = &result.metrics;

    // 1. The run actually SPANNED the stage wall-clock (3s of stages, minus
    //    a 25% tolerance for scheduler/timer granularity — but never 0).
    assert!(
        elapsed >= std::time::Duration::from_millis(2250),
        "ramping run elapsed {elapsed:?}, expected >= 2.25s (stages not collapsed)"
    );

    // 2. The ramp REALLY reached 3 concurrent VUs: the server observed the
    //    peak number of simultaneously-open connections. `vus_max` is a
    //    pure function of the config (`peak_vus()` = stages.fold of the
    //    targets the test itself wrote), so it proves nothing — a scheduler
    //    that spawned 1 VU and slept(3s) passes all the old assertions.
    //    Three overlapping connections prove the pool actually grew.
    let peak_obs = peak.load(Ordering::SeqCst);
    assert!(
        peak_obs >= 3,
        "peak concurrent connections >= 3, got {} (pool actually grew)",
        peak_obs
    );

    // 3. Requests actually fired during the ramp.
    assert!(
        m.http_reqs > 0,
        "http_reqs > 0 during ramp, got {}",
        m.http_reqs
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog P1 · the failure-path trio. Every other test server returns 200;
/// nothing exercised `http_req_failed` — the metric that decides whether a
/// load test found errors — against a real non-2xx response. A 500 with the
/// default expected statuses (`200-399`) must drive `http_req_failed == 1.0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_2xx_drives_http_req_failed() -> Result<()> {
    let srv = start_500_server().await;
    let coll = write_k6_script(&format!("http://{srv}"), "err500");
    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            duration: "2s".to_string(),
            graceful_stop: Some("3s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;
    let m = &result.metrics;

    assert!(m.http_reqs > 0, "requests fired against the 500 server");
    assert_eq!(
        m.http_req_failed, 1.0,
        "every request got a non-expected 500 -> http_req_failed == 1.0, got {}",
        m.http_req_failed
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog P1 · a server that ACCEPTS the connection but never responds
/// must be bounded by the client's `request_timeout`, the failed request
/// recorded as `http_req_failed`, and the RUN must still terminate (a hung
/// VU must not hang the drain loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_server_is_bounded_by_request_timeout() -> Result<()> {
    let srv = start_hung_server().await;
    // k6's native per-request timeout: the k6 shim packs `params.timeout`
    // and the driver wires it through to the per-request ceiling. (The
    // global HttpConfig.request_timeout covers the absent-params case — see
    // hung_server_is_bounded_by_global_request_timeout below.)
    let coll = write_k6_timeout_script(&format!("http://{srv}"), "hung", "500ms");
    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            duration: "2s".to_string(),
            graceful_stop: Some("3s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let start = Instant::now();
    let result = engine.run(&config).await?;
    let elapsed = start.elapsed();
    let m = &result.metrics;

    // The run terminated (a 2s job plus graceful stop, well under 30s even
    // with a 500ms per-request ceiling — proves the hung server couldn't
    // stall the drain loop).
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "run terminated despite hung server, took {elapsed:?}"
    );
    assert!(m.http_reqs > 0, "requests fired against the hung server");
    assert_eq!(
        m.http_req_failed, 1.0,
        "every request timed out -> http_req_failed == 1.0, got {}",
        m.http_req_failed
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog P1 · the GLOBAL `HttpConfig.request_timeout` must bound the k6
/// driver path too — not just k6's per-request `params.timeout`. This uses a
/// plain script (no `params.timeout`) and relies solely on the config-level
/// ceiling, proving the shared client built from `http_cfg` carries it.
///
/// Regression for the shim bug: `k6-shim.js` used to hardcode
/// `params.timeout || '30s'` and pack timeoutMs=30000, which the driver
/// turned into `request.timeout = Some(30s)` — overriding the global 500ms
/// via `req_builder.timeout(30s)` and letting a hung request run to the
/// engine's 30s join bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_server_is_bounded_by_global_request_timeout() -> Result<()> {
    let srv = start_hung_server().await;
    let coll = write_k6_script(&format!("http://{srv}"), "hungglobal");
    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            duration: "2s".to_string(),
            graceful_stop: Some("3s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        http: HttpConfig {
            request_timeout: Some("500ms".to_string()),
            ..Default::default()
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let start = Instant::now();
    let result = engine.run(&config).await?;
    let elapsed = start.elapsed();
    let m = &result.metrics;

    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "global request_timeout bounded the run, took {elapsed:?}"
    );
    assert!(m.http_reqs > 0, "requests fired against the hung server");
    assert_eq!(
        m.http_req_failed, 1.0,
        "every request timed out -> http_req_failed == 1.0, got {}",
        m.http_req_failed
    );

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// Backlog P1 · a connection-refused endpoint (bound then dropped) must be
/// recorded as a failed request, not silently dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_refused_is_recorded_as_failure() -> Result<()> {
    // Bind a listener to claim a port, grab the address, then drop the
    // listener so connecting to the address is refused.
    let refused_addr = {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        drop(listener);
        addr
    };
    let coll = write_k6_script(&format!("http://{refused_addr}"), "refused");
    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 2,
            duration: "2s".to_string(),
            graceful_stop: Some("3s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;
    let m = &result.metrics;

    assert_eq!(
        m.http_req_failed, 1.0,
        "connection refused -> http_req_failed == 1.0, got {}",
        m.http_req_failed
    );
    // The k6 driver records transport failures as http_reqs + http_req_failed
    // (no k6 `errors` counter — that metric is postman-runner-only).
    assert!(m.http_reqs > 0, "requests fired against the refused port");

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

/// k6 script whose default function fires one request then sleeps — the
/// exact backlog verified path (`gracefulStop force-stop is advisory only`:
/// 30s duration + `sleep(60)` kept issuing HTTP until t≈90). The sleep must
/// be INTERRUPTIBLE by a force-stop, not a native thread-block the VU
/// ignores.
fn write_k6_sleep_script(base: &str, tag: &str, sleep_secs: u64) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tropel-k6-e2e-{}-{}.js", std::process::id(), tag));
    let script = format!(
        r#"import http from 'k6/http';
import {{ check, sleep }} from 'k6';

export default function () {{
  const res = http.get('{base}/');
  check(res, {{ 'status is 200': (r) => r.status === 200 }});
  sleep({sleep_secs});
}}
"#
    );
    std::fs::write(&path, script).unwrap();
    path.to_string_lossy().to_string()
}

/// Backlog P1 · gracefulStop force-stop was advisory only: a VU stuck in a
/// native `sleep(60)` kept issuing HTTP until the sleep returned (verified
/// path: 30s duration + sleep(60) → traffic until t≈90). The flag-aware JS
/// interrupt + interruptible sleep must stop the VU within the grace
/// deadline, so the run terminates in a few seconds — NOT after the 60s
/// sleep (or the 30s join bound). This is the end-to-end regression for the
/// four-part fix (runner item loop, JS interrupt, sleep, handle abort).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_stop_interrupts_sleeping_vu() -> Result<()> {
    let srv = start_echo_server().await;
    let coll = write_k6_sleep_script(&format!("http://{srv}"), "fsleep", 60);
    let config = JobConfig {
        input: coll.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::ConstantVus {
            vus: 1,
            duration: "2s".to_string(),
            graceful_stop: Some("1s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let start = Instant::now();
    let result = engine.run(&config).await?;
    let elapsed = start.elapsed();
    let m = &result.metrics;

    // 1. THE regression: the run terminates FAST. 2s duration + 1s grace +
    //    force-stop must interrupt the 60s sleep — before the fix this took
    //    ~60s (sleep returned first) or ~33s (30s join bound + detach).
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "force-stop must interrupt the sleeping VU; run took {elapsed:?}"
    );

    // 2. The script ran at all (sanity: at least the first request fired).
    assert!(m.http_reqs > 0, "requests fired, got {}", m.http_reqs);

    let _ = std::fs::remove_file(&coll);
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// TR-233 · `http.cookieJar()` and `options.batch` / `options.batchPerHost`
//
// Both halves were *declared but not forwarded* (CONTEXT invariant 3), and
// both were "covered" by shape tests that could not see it: one asserted the
// four jar methods EXIST, the other asserted the batch limiter exists with
// k6's hardcoded defaults. So these two tests assert only what a user can
// observe from outside the process — the bytes on the wire:
//
//   * cookies: the `Cookie:` header the SERVER receives, and the request path
//     the script builds out of what the jar told it.
//   * batch:   the PEAK number of simultaneously in-flight requests the
//     SERVER counts.
//
// Neither can pass against a jar that stores nothing or a limiter sized from
// a constant. See the PR body for the pre-fix failure output.
// ──────────────────────────────────────────────────────────────────────

/// One request the cookie server saw: its path, and the `Cookie:` header the
/// client actually sent (`None` when the client sent no `Cookie:` header at
/// all — which is precisely the pre-fix behaviour).
type SeenRequest = (String, Option<String>);

/// HTTP/1.1 server that records every request's path and `Cookie:` header,
/// and answers `/set-cookie` with a `Set-Cookie:` of its own.
///
/// Keep-alive, because the point is that the jar — not the connection —
/// carries the cookie: the requests must be indistinguishable to the server
/// apart from their headers.
async fn start_cookie_server() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<SeenRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<SeenRequest>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_out = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let seen = seen.clone();
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
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let path = text
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let cookie = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                        .map(|l| l[7..].trim().to_string());
                    seen.lock().unwrap().push((path.clone(), cookie));

                    let body = r#"{"ok":true}"#;
                    // Only `/set-cookie` hands a cookie back, so a cookie
                    // observed on any OTHER path can only have come from the
                    // jar the script wrote to.
                    let set = if path.starts_with("/set-cookie") {
                        "Set-Cookie: srv=from_server; Path=/\r\n"
                    } else {
                        ""
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\n{set}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
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
    (addr, seen_out)
}

/// k6 script that drives all four `http.cookieJar()` verbs and encodes what
/// the jar told it into a request PATH, so the script's own view of the jar
/// is observable on the wire rather than through an in-process assertion.
fn write_k6_cookie_jar_script(base: &str, tag: &str) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tropel-k6-e2e-{}-{}.js", std::process::id(), tag));
    let script = format!(
        r#"import http from 'k6/http';

export default function () {{
  const jar = http.cookieJar();

  // (1) A cookie the SCRIPT set must ride on the next request.
  jar.set('{base}/', 'scripted', 'from_jar', {{ path: '/' }});
  http.get('{base}/first');

  // (2) The server sets one of its own.
  http.get('{base}/set-cookie');

  // (3) …which the script must be able to READ back out of the same jar.
  //     Whatever it saw becomes the path, so the server records it.
  const seen = jar.cookiesForURL('{base}/');
  const srv = (seen && seen['srv'] && seen['srv'][0]) ? seen['srv'][0] : 'MISSING';
  http.get('{base}/probe-' + srv);

  // (4) delete() drops ONLY the named cookie — the server's must survive.
  jar.delete('{base}/', 'scripted');
  http.get('{base}/after-delete');
}}
"#
    );
    std::fs::write(&path, script).unwrap();
    path.to_string_lossy().to_string()
}

/// TR-233 · `http.cookieJar()` had a full JS surface and no jar behind it:
/// `jar.set()` changed nothing about the next request, and `cookiesForURL()`
/// returned `[]` no matter what the server had set. A declared capability
/// that forwards nothing is worse than a missing one — a script that logs in
/// by seeding a session cookie ran green against an unauthenticated server.
///
/// Everything asserted here is observed by the SERVER, so no in-process
/// stub can satisfy it: the jar must be the same jar `VuCookieClient` puts
/// on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookie_jar_set_reaches_the_wire_and_reads_back_server_cookies() -> Result<()> {
    let (srv, seen) = start_cookie_server().await;
    let script = write_k6_cookie_jar_script(&format!("http://{srv}"), "cookiejar");

    let config = JobConfig {
        input: script.clone(),
        input_type: Some("k6".to_string()),
        // Exactly one iteration: the four requests below are the whole run,
        // so their order and count are deterministic.
        execution: ExecutionConfig::SharedIterations {
            iterations: 1,
            max_duration: Some("30s".to_string()),
            vus: 1,
            graceful_stop: Some("5s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };

    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;
    assert!(
        result.metrics.http_reqs >= 4,
        "the script's four requests must all fire, got {}",
        result.metrics.http_reqs
    );

    let seen = seen.lock().unwrap().clone();
    let find = |p: &str| -> SeenRequest {
        seen.iter()
            .find(|(path, _)| path == p)
            .unwrap_or_else(|| {
                panic!(
                    "the server never received {p}; it saw: {:?}",
                    seen.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>()
                )
            })
            .clone()
    };

    // 1. THE regression: a cookie the script put in the jar is on the wire.
    //    Pre-fix the bridge did not exist, the shim's `set` silently no-oped,
    //    and this request carried no `Cookie:` header at all.
    let (_, first_cookie) = find("/first");
    let first_cookie = first_cookie.unwrap_or_else(|| {
        panic!("jar.set() must put a Cookie header on the next request; none was sent")
    });
    assert!(
        first_cookie.contains("scripted=from_jar"),
        "jar.set() must reach the wire; server saw Cookie: {first_cookie}"
    );

    // 2. The server's own Set-Cookie is readable through cookiesForURL() —
    //    the script built this path out of what the jar handed back, so a jar
    //    that cannot see server cookies produces `/probe-MISSING`.
    let (probe_path, _) = seen
        .iter()
        .find(|(p, _)| p.starts_with("/probe-"))
        .unwrap_or_else(|| {
            panic!(
                "the server never received a /probe- request; it saw: {:?}",
                seen.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>()
            )
        })
        .clone();
    assert_eq!(
        probe_path, "/probe-from_server",
        "cookiesForURL() must return the server's cookie VALUE keyed by name \
         (k6 returns map[string][]string, so cookies['srv'][0]); \
         `/probe-MISSING` means the jar read nothing back"
    );

    // 3. delete(url, name) drops only the named cookie. k6's `clear` takes a
    //    URL alone and drops everything; the old shim aliased the two, so
    //    this distinction had no test at all.
    let (_, after_delete) = find("/after-delete");
    let after_delete = after_delete.unwrap_or_else(|| {
        panic!("the server's own cookie must survive delete('scripted'); no Cookie header was sent")
    });
    assert!(
        !after_delete.contains("scripted="),
        "delete() must stop the named cookie being sent; server saw Cookie: {after_delete}"
    );
    assert!(
        after_delete.contains("srv=from_server"),
        "delete() must NOT drop other cookies; server saw Cookie: {after_delete}"
    );

    let _ = std::fs::remove_file(&script);
    Ok(())
}

/// HTTP/1.1 server that tracks the peak number of simultaneously IN-FLIGHT
/// requests (not connections — `http.batch` runs over a pooled client, so
/// counting sockets would measure the pool, not the batch limiter).
///
/// Each request is held `hold_ms` before answering so overlapping requests
/// genuinely overlap; a server that answered instantly would serialize the
/// batch and report a peak of 1 whatever the limit is.
async fn start_inflight_peak_server(
    hold_ms: u64,
) -> (std::net::SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let (peak_out, total_out) = (peak.clone(), total.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let (active, peak, total) = (active.clone(), peak.clone(), total.clone());
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
                    total.fetch_add(1, Ordering::SeqCst);
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                    let body = r#"{"ok":true}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let wrote = sock.write_all(resp.as_bytes()).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    if wrote.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, peak_out, total_out)
}

/// k6 script that issues `n` requests in ONE `http.batch()` under the given
/// declared limits. All requests go to the same host, so the effective
/// ceiling is `min(batch, batchPerHost)`.
fn write_k6_batch_script(base: &str, tag: &str, batch: usize, per_host: usize, n: usize) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tropel-k6-e2e-{}-{}.js", std::process::id(), tag));
    let script = format!(
        r#"import http from 'k6/http';

export const options = {{ batch: {batch}, batchPerHost: {per_host} }};

export default function () {{
  const reqs = [];
  for (let i = 0; i < {n}; i++) {{ reqs.push(['GET', '{base}/b' + i]); }}
  http.batch(reqs);
}}
"#
    );
    std::fs::write(&path, script).unwrap();
    path.to_string_lossy().to_string()
}

/// Run one `http.batch()` of `n` requests under the declared limits and
/// return `(observed peak in-flight, total requests served)`.
async fn observed_batch_concurrency(
    tag: &str,
    batch: usize,
    per_host: usize,
    n: usize,
) -> Result<(usize, usize)> {
    let (srv, peak, total) = start_inflight_peak_server(150).await;
    let script = write_k6_batch_script(&format!("http://{srv}"), tag, batch, per_host, n);
    let config = JobConfig {
        input: script.clone(),
        input_type: Some("k6".to_string()),
        execution: ExecutionConfig::SharedIterations {
            iterations: 1,
            max_duration: Some("60s".to_string()),
            vus: 1,
            graceful_stop: Some("10s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };
    let engine = Engine::new(ExtensionRegistry::new());
    engine.run(&config).await?;
    let _ = std::fs::remove_file(&script);
    Ok((peak.load(Ordering::SeqCst), total.load(Ordering::SeqCst)))
}

/// TR-233 · `options.batch` / `options.batchPerHost` were parsed into the
/// `unknown` bag, warned about, and dropped: the limiter was built from k6's
/// hardcoded 20/6 whatever the script declared. Both directions matter and
/// both are user-visible only as concurrency on the wire —
///
///   * RAISING it (`batch: 10` above the per-host default of 6) is the case
///     the user is denied throughput they asked for;
///   * LOWERING it (`batch: 2`) is the case the user asked to be gentle with
///     a fragile endpoint and was ignored — the dangerous one.
///
/// Pre-fix BOTH observe a peak of 6 (the per-host default), which is what
/// makes a single hardcoded expectation unable to hide here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_batch_limit_is_the_observed_concurrency() -> Result<()> {
    // 12 requests, cap 2 → the server must never see a third in flight.
    let (peak_low, total_low) = observed_batch_concurrency("batch-low", 2, 2, 12).await?;
    assert_eq!(total_low, 12, "all 12 batch requests must be served");
    assert_eq!(
        peak_low, 2,
        "declared `batch: 2` must be the observed ceiling; the server saw \
         {peak_low} requests in flight at once (6 = k6's per-host default \
         still hardcoded, i.e. the declared option was dropped)"
    );

    // 12 requests, cap 10 → above the per-host default of 6, so a dropped
    // option pins this at 6 and a forwarded one reaches 10.
    let (peak_high, total_high) = observed_batch_concurrency("batch-high", 10, 10, 12).await?;
    assert_eq!(total_high, 12, "all 12 batch requests must be served");
    assert_eq!(
        peak_high, 10,
        "declared `batch: 10` must raise the ceiling above k6's per-host \
         default of 6; the server saw {peak_high} in flight at once"
    );

    Ok(())
}

/// The per-host limiter runs UNDER the global one, so for a single-host
/// batch the ceiling is `min(batch, batchPerHost)`. Without this, `batch`
/// alone could be wired while `batchPerHost` was quietly ignored — the same
/// declared-but-not-forwarded shape one level down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_per_host_caps_below_the_global_batch_limit() -> Result<()> {
    let (peak, total) = observed_batch_concurrency("batch-perhost", 12, 3, 12).await?;
    assert_eq!(total, 12, "all 12 batch requests must be served");
    assert_eq!(
        peak, 3,
        "`batchPerHost: 3` must cap a single-host batch below `batch: 12`; \
         the server saw {peak} in flight at once"
    );
    Ok(())
}
