//! # Differential harness — native k6 vs wasm driver, one engine
//!
//! P6 (TROPEL_MODULARIZATION_TODO.md) — *"every fixture collection through
//! native-driven and wasm-driven tropel-exec, diffing the full
//! IterationOutcome. This is what makes the one-engine claim verifiable
//! rather than asserted."*
//!
//! `tropel-exec` (P5) is not extracted yet, but both the native k6 driver
//! (`tropel-input-k6`) and the WASM driver (`tropel-wasm`) are
//! inventory-registered drivers that the engine runs through the SAME
//! `Engine::run` → `run_driver_vus` path — same VU loop, same shared HTTP
//! client, same metrics collector, same scheduler. That is the engine whose
//! oneness this harness verifies.
//!
//! The WASM driver's `env.http_request` host function auto-records the
//! standard k6 sample set (`http_req_duration` / `http_reqs` /
//! `http_req_failed` / `data_received` / `data_sent`) with the SAME tags as
//! the native k6 driver (`url`, `method`, `status`, `name`, `group`). So a
//! single logical fixture — one GET against a local server — run as a k6
//! script and as a WAT driver must produce the same aggregated metrics.
//!
//! This test runs one deterministic iteration (`SharedIterations {1, 1}`) on
//! each side and diffs the shared surface: `http_reqs`, the
//! `http_req_duration` summary (count + status tag), `data_received`,
//! `data_sent`, `http_req_failed`, and `iterations`. When P5 lands, this
//! test upgrades to diff the full `IterationOutcome` from `tropel-exec`.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tropel_core::config::{ExecutionConfig, JobConfig, OutputConfig, ThinkTimeConfig};
use tropel_engine::Engine;
use tropel_ext::registry::ExtensionRegistry;
use tropel_metrics::collector::MetricsResult;
use tropel_sdk::Result;

/// Minimal HTTP/1.1 server answering `200 {"ok":true}` (keep-alive, so the
/// client's pooled connection is reused the same way on both driver paths).
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

/// `:` and `/` in the URL are invalid in filenames on Windows (Os error
/// 123), so the address is mangled into a filesystem-safe tag.
fn safe_tag(base: &str) -> String {
    base.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Native side: a k6 script with one `http.get` + a check, written to a
/// temp file (the k6 driver resolves the input by extension/content).
fn write_k6_script(base: &str) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "tropel-diff-native-{}-{}.js",
        std::process::id(),
        safe_tag(base)
    ));
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

/// Wasm side: a WAT driver module that makes ONE `env.http_request` against
/// the same URL (baked into the data segment, mirroring `DRIVER_WAT`), with
/// the request JSON's byte length computed for the call. `adapter_run_iteration`
/// returns 1 (failure) on a negative host return so the iteration must fail
/// if the request did not go out.
fn write_wasm_driver(base: &str) -> String {
    let req = format!(r#"{{"url":"{base}/","method":"GET"}}"#);
    let len = req.len();
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "tropel-diff-wasm-{}-{}.wat",
        std::process::id(),
        safe_tag(base)
    ));
    let wat = format!(
        r#"(module
  (import "env" "http_request" (func $http_request (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 64 256)
  (data (i32.const 4096) "{req}")
  (func (export "adapter_run_iteration") (param $in i32) (param $in_len i32) (result i32)
    (local $r i32)
    ;; http_request(req at 4096, {len} bytes, resp at 12288, cap 1024)
    (local.set $r (call $http_request (i32.const 4096) (i32.const {len}) (i32.const 12288) (i32.const 1024)))
    (if (i32.lt_s (local.get $r) (i32.const 0)) (then (return (i32.const 1))))
    (i32.const 0)))
"#,
        req = req.replace('"', "\\\""),
        len = len,
    );
    std::fs::write(&path, wat).unwrap();
    path.to_string_lossy().to_string()
}

/// Find the per-url breakdown summary for a metric + url tag. The collector
/// stores these with a tag-suffixed key (`http_req_duration{url=…}`) and a
/// `url` tag pair; `per_url` carries ONLY the url tag, so status/name/group
/// must come from `metrics` (see [`status_tag_for`]).
fn per_url_for<'a>(
    result: &'a MetricsResult,
    metric: &str,
    url: &str,
) -> Option<&'a tropel_metrics::collector::MetricSummary> {
    result.per_url.iter().find(|s| {
        s.key.starts_with(&format!("{metric}{{"))
            && s.tags.iter().any(|(k, v)| k == "url" && v == url)
    })
}

/// The `status` tag of the raw per-(url,method,status) `http_req_duration`
/// series in `result.metrics` (key `http_req_duration{url=…,method=…,status=…}`).
fn status_tag_for(result: &MetricsResult, metric: &str) -> Option<String> {
    result.metrics.iter().find_map(|s| {
        if s.key.starts_with(&format!("{metric}{{")) {
            s.tags
                .iter()
                .find(|(k, _)| k == "status")
                .map(|(_, v)| v.clone())
        } else {
            None
        }
    })
}

/// Run one deterministic iteration of the given input through the real
/// engine (driver path), returning the aggregated run metrics.
async fn run_one_iteration(input: &str, input_type: &str) -> Result<MetricsResult> {
    let config = JobConfig {
        input: input.to_string(),
        input_type: Some(input_type.to_string()),
        execution: ExecutionConfig::SharedIterations {
            iterations: 1,
            max_duration: None,
            vus: 1,
            graceful_stop: Some("5s".to_string()),
            think_time: ThinkTimeConfig::default(),
        },
        // Keep the test output clean: no stdout summary stream.
        output: OutputConfig {
            reporters: vec![],
            ..Default::default()
        },
        ..Default::default()
    };
    let engine = Engine::new(ExtensionRegistry::new());
    let result = engine.run(&config).await?;
    Ok(result.metrics)
}

/// The one-engine claim, pinned: the same logical fixture produces the same
/// shared HTTP metrics whether driven natively (k6 script) or by the WASM
/// driver — both through the identical `Engine::run` path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_k6_and_wasm_driver_produce_identical_http_metrics() -> Result<()> {
    let addr = start_echo_server().await;
    let base = format!("http://{addr}");

    let native_input = write_k6_script(&base);
    let wasm_input = write_wasm_driver(&base);

    let native = run_one_iteration(&native_input, "k6").await?;
    let wasm = run_one_iteration(&wasm_input, "wasm").await?;

    // 1. Exactly one request on each side (the deterministic fixture).
    assert_eq!(
        native.http_reqs, wasm.http_reqs,
        "http_reqs must match: native={} wasm={}",
        native.http_reqs, wasm.http_reqs
    );
    assert_eq!(
        native.http_reqs, 1,
        "native side must have fired exactly one request, got {}",
        native.http_reqs
    );

    // 2. The http_req_duration series: same total sample count. The status
    //    and url tags live on the per-url breakdown (the merged
    //    `http_req_duration` summary aggregates across URLs and carries no
    //    tags), so the "same fixture" check diffs the per-url series.
    let n_dur = native
        .http_req_duration
        .as_ref()
        .expect("native http_req_duration present");
    let w_dur = wasm
        .http_req_duration
        .as_ref()
        .expect("wasm http_req_duration present");
    assert_eq!(
        n_dur.count, w_dur.count,
        "http_req_duration sample counts must match: native={} wasm={}",
        n_dur.count, w_dur.count
    );
    assert_eq!(n_dur.count, 1, "exactly one duration sample per side");
    // The per-url breakdown (key `http_req_duration{url=…}`, tags carry the
    // url) — the same fixture URL must appear on both drivers.
    let n_per_url = per_url_for(&native, "http_req_duration", &format!("{base}/"))
        .expect("native per-url http_req_duration series present");
    let w_per_url = per_url_for(&wasm, "http_req_duration", &format!("{base}/"))
        .expect("wasm per-url http_req_duration series present");
    assert_eq!(
        n_per_url.count, w_per_url.count,
        "per-url sample counts must match: native={} wasm={}",
        n_per_url.count, w_per_url.count
    );
    // The status tag lives on the raw per-(url,method,status) series in
    // `metrics` (per_url deliberately drops everything but the url).
    let n_status = status_tag_for(&native, "http_req_duration");
    let w_status = status_tag_for(&wasm, "http_req_duration");
    assert_eq!(
        n_status, w_status,
        "status tags must match: native={:?} wasm={:?}",
        n_status, w_status
    );
    assert_eq!(
        n_status.as_deref(),
        Some("200"),
        "fixture answers 200 on both drivers"
    );

    // 3. Byte accounting: the same response body was counted on both sides.
    assert_eq!(
        native.data_received, wasm.data_received,
        "data_received must match: native={} wasm={}",
        native.data_received, wasm.data_received
    );
    assert!(
        native.data_received > 0.0,
        "data_received must be non-zero on the native side"
    );
    assert_eq!(
        native.data_sent, wasm.data_sent,
        "data_sent must match: native={} wasm={}",
        native.data_sent, wasm.data_sent
    );

    // 4. Failure semantics: a 200 fixture is not a failure on either driver.
    assert_eq!(
        native.http_req_failed, wasm.http_req_failed,
        "http_req_failed must match: native={} wasm={}",
        native.http_req_failed, wasm.http_req_failed
    );
    assert_eq!(native.http_req_failed, 0.0, "200 is not a failure");

    // 5. The run itself did the same amount of work on both sides.
    assert_eq!(
        native.iterations, wasm.iterations,
        "iterations must match: native={} wasm={}",
        native.iterations, wasm.iterations
    );
    assert_eq!(native.iterations, 1, "exactly one iteration per side");

    let _ = std::fs::remove_file(&native_input);
    let _ = std::fs::remove_file(&wasm_input);
    Ok(())
}
