//! # Tropel CLI
//!
//! The main entry point for the Tropel load testing tool.
//! Delegates all logic to `tropel_engine::cli::run_cli()`.
//! This ensures that custom binaries built with `tropel build` have
//! identical CLI behavior.

// Select the global allocator at compile time via feature flags.
#[cfg(feature = "alloc-mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "alloc-jemalloc")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Outer tokio runtime worker count.
///
/// VUs run on the thread-per-core `VUWorkerPool` (current-thread runtimes,
/// one per CPU core), so the outer orchestrator only needs minimal workers
/// for scenario coordination and final metric collection. Default 4;
/// override with `TROPEL_TOKIO_WORKERS` (clamped to [1, 128]).
///
/// P1 line 335: the old default of 2 was insufficient — the aggregator,
/// abort coordinator, VU sampler, control API, and 5+ output consumers
/// all share this runtime. Two workers caused flush_buffered() to park
/// a worker while build_results() had no .await inside, blocking the
/// entire runtime. Four workers give headroom for concurrent output
/// flushes without starving the aggregator.
fn outer_worker_threads() -> usize {
    std::env::var("TROPEL_TOKIO_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(1, 128)
}

fn main() -> tropel_sdk::Result<()> {
    // `tropel worker` — the distributed load-test worker (P6). Dispatched here
    // (the binary crate) rather than in tropel-engine's CLI because
    // tropel-distributed depends on tropel-engine; a CLI-level reference would
    // be a dependency cycle.
    //
    // TR-467: this answered to `tropel agent` until that was found to SHADOW
    // `Commands::Agent` in tropel-engine's CLI — the loopback HTTP agent
    // (`agent.rs`, TR-405) that the API client actually spawns. Two unrelated
    // features, one name, and this one won because it is matched before
    // run_cli() ever sees argv: `tropel agent --port 9876` died on
    // "unexpected argument '--port'", so the loopback agent had NO reachable
    // entry point from this binary at all.
    //
    // `agent` now belongs to the loopback agent — the meaning every knockport
    // doc, error message and `agent.rs` startup line already assumed. The
    // worker answers to `tropel worker` and to the standalone `tropel-agent`
    // binary, which is unchanged. (`tropel-cloud-run agent` is a different
    // binary and is unaffected.)
    match std::env::args().nth(1).as_deref() {
        Some("worker") => return worker_command(),
        // Invariant 8: an unsupported invocation names its reason. Without this
        // arm, the old worker command line would die on clap's "unexpected
        // argument '--controller'", which does not say that the subcommand was
        // renamed or where it went. Any OTHER `agent` invocation falls through
        // to run_cli() -> Commands::Agent, the loopback agent.
        Some("agent")
            if std::env::args().any(|a| a == "--controller" || a == "-C" || a == "--token-file") =>
        {
            eprintln!(
                "tropel: `agent` is the loopback HTTP agent \
                 (--port/--bind/--token/--allow-origin).\n\
                 The distributed worker moved to `tropel worker` (same flags); \
                 the standalone `tropel-agent` binary is unchanged."
            );
            std::process::exit(2);
        }
        _ => {}
    }

    let workers = outer_worker_threads();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("failed to build outer tokio runtime");
    rt.block_on(tropel_engine::cli::run_cli())
}

/// `tropel worker [--controller HOST:PORT] [--token TOKEN] [--token-file FILE]`
///
/// Connect to a `tropel-controller` and run this worker's segment of the job.
/// Same contract as the standalone `tropel-agent` binary, surfaced as a
/// subcommand of the main binary so a worker image needs nothing but `tropel`.
fn worker_command() -> tropel_sdk::Result<()> {
    use clap::Parser;
    use std::path::PathBuf;

    #[derive(Parser, Debug)]
    #[command(
        name = "tropel worker",
        about = "Distributed load-test worker (runs one segment of a job for a controller)",
        // P6 version handshake: `tropel worker --version` prints the binary's
        // version so the API client can feed it to checkVersionParity against
        // the loaded wasm's runtimeVersion. clap wires this from
        // CARGO_PKG_VERSION automatically.
        version
    )]
    struct AgentArgs {
        /// Controller address (host:port).
        #[arg(long, short = 'C', default_value = "127.0.0.1:17890")]
        controller: String,
        /// Shared auth token (or set TROPEL_TOKEN). Must match the controller's.
        #[arg(long)]
        token: Option<String>,
        /// Read the shared auth token from this file.
        #[arg(long)]
        token_file: Option<PathBuf>,
    }

    // run_cli() initializes tracing for the main commands; the worker path
    // bypasses it, so initialize here (mirrors the standalone `tropel-agent`
    // bin) or run_agent's tracing::info! calls would be silent no-ops.
    tracing_subscriber::fmt::init();

    // clap::parse() would treat the leading "worker" as an unknown positional;
    // parse_from over args[2..] scopes the subcommand's own argv correctly.
    //
    // IMPORTANT: parse_from treats its FIRST element as argv[0] (the program
    // name), so a lone `tropel worker --version` would have "--version" eaten
    // as the binary name and fall through to the auth check. Prepend a dummy
    // name so real flags (--version/--help included) land at argv[1..].
    let args = AgentArgs::parse_from(
        std::iter::once("tropel worker".to_string()).chain(std::env::args().skip(2)),
    );

    if args.controller.is_empty() {
        return Err(tropel_sdk::TropelError::Config(
            "--controller must not be empty".into(),
        ));
    }

    // An agent runs a FULL load-test engine, not just a socket loop — build a
    // runtime that scales with available parallelism (backlog line 119).
    let rt = tropel_distributed::build_runtime().map_err(tropel_sdk::TropelError::Io)?;
    rt.block_on(async {
        let token = tropel_distributed::resolve_token(args.token, args.token_file)?;
        tropel_distributed::run_agent(&args.controller, &token).await
    })
}
