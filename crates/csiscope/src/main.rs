//! `csiscope` — a live oscilloscope and operator console for `csid`.
//!
//! One binary that subscribes to the CSIQ live stream, computes every
//! representation the Wi-Fi sensing literature actually uses (waterfall,
//! spectrum bundle, sanitised phase, impulse response, Doppler), and serves
//! them to a browser alongside the configuration surface for the node.
//!
//! It is a strict **consumer**. It never touches the radio, never writes to the
//! capture path, and holds nothing the daemon needs. If `csiscope` dies
//! mid-experiment the capture is unaffected — the whole point of `csid`'s
//! best-effort live path.
//!
//! ```console
//! $ csiscope                              # http://127.0.0.1:8088
//! $ csiscope --bind 0.0.0.0:8088          # reachable from a laptop
//! $ csiscope --udp-bind 0.0.0.0:5599      # off-node, via [stream] transport = "udp"
//! $ csiscope --read-only                  # views only, no config or unit control
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use csid::config::{DEFAULT_CONFIG, DEFAULT_EXPERIMENT_DIR};
use csiscope::{ingest, server, state, DEFAULT_COEFF_BUDGET, DEFAULT_HISTORY};

#[derive(Parser)]
#[command(
    name = "csiscope",
    version,
    about = "Live CSI oscilloscope and operator console for csid",
    long_about = None
)]
struct Cli {
    /// Address to serve the console on.
    ///
    /// Defaults to loopback: the console is unauthenticated, so exposing it is
    /// a decision the operator makes explicitly.
    #[arg(long, default_value = "127.0.0.1:8088")]
    bind: SocketAddr,

    /// Unix datagram socket to subscribe to (the `csid` v1 stream default).
    #[arg(long, default_value = "/run/csid/live.sock")]
    socket: PathBuf,

    /// Receive the live stream over UDP instead, for an off-node console.
    /// Pair with `[stream] transport = "udp"` in the experiment.
    #[arg(long, value_name = "ADDR")]
    udp_bind: Option<String>,

    /// Node-global configuration file.
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,

    /// Directory holding per-experiment configuration files.
    #[arg(long, default_value = DEFAULT_EXPERIMENT_DIR)]
    experiments: PathBuf,

    /// Serve the views but refuse every write: no config edits, no unit
    /// control, no exports. The safe mode on an untrusted network.
    #[arg(long)]
    read_only: bool,

    /// The `csid` binary used for `doctor` and `export`.
    #[arg(long, default_value = "csid")]
    csid_bin: String,

    /// Default interface for `csid doctor`.
    #[arg(long, default_value = "wlp1s0")]
    interface: String,

    /// Records retained for the windowed views.
    #[arg(long, default_value_t = DEFAULT_HISTORY)]
    history: usize,

    /// I/Q coefficient budget for the retained history.
    #[arg(long, default_value_t = DEFAULT_COEFF_BUDGET)]
    coeff_budget: usize,

    /// Increase log verbosity (repeatable).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if let Err(e) = run(cli) {
        tracing::error!("{e:#}");
        eprintln!("csiscope: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let source = match &cli.udp_bind {
        Some(addr) => ingest::Source::Udp(addr.clone()),
        None => ingest::Source::Unix(cli.socket.clone()),
    };

    let hub = state::Hub::new(
        source.label(),
        cli.history.max(64),
        cli.coeff_budget.max(100_000),
    );
    ingest::spawn(source, hub.clone())?;

    if cli.read_only {
        tracing::info!("read-only: configuration, unit control and export are disabled");
    } else if !cli.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %cli.bind,
            "serving an unauthenticated write-capable console on a non-loopback address"
        );
    }

    let app = Arc::new(server::App {
        pipeline: csiscope::pipeline::Pipeline::new(hub.clone()),
        hub,
        config_path: cli.config,
        experiment_dir: cli.experiments,
        read_only: cli.read_only,
        csid_bin: cli.csid_bin,
        interface: cli.interface,
    });

    // A small runtime: the async side serves HTTP and a handful of WebSockets,
    // while ingest and per-frame analysis both live on their own threads.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("csiscope-rt")
        .build()
        .context("building the tokio runtime")?
        .block_on(server::serve(app, cli.bind))
}

/// Verbosity mirrors `csid`; `CSISCOPE_LOG` overrides it with a full
/// `EnvFilter` directive.
fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_env("CSISCOPE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
