//! `csid` — a systemd-native Wi-Fi CSI capture daemon.
//!
//! One static binary owning the whole userspace path: nl80211 vendor-event
//! consumption over netlink, `iwlmvm` debugfs control, monitor tuning, TOML
//! configuration, the session sidecar, lossless raw spooling, best-effort live
//! streaming, and CSIQ export.
//!
//! See `docs/architecture.md` for the threading model and `docs/CSIQ-format-v1.md`
//! for the on-disk format.

mod caps;
mod commands;
mod config;
mod debugfs;
mod engine;
mod export;
mod notify;
mod radio;
mod sidecar;
mod sinks;
mod source;
mod util;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use config::{GlobalConfig, DEFAULT_CONFIG, DEFAULT_EXPERIMENT_DIR};

/// Set by the signal handler; mirrored into the session stop flag.
static SIGNALLED: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(
    name = "csid",
    version,
    about = "Wi-Fi CSI capture daemon (Intel AX210 / iwlwifi-iax)",
    long_about = None
)]
struct Cli {
    /// Node-global configuration file.
    #[arg(long, global = true, default_value = DEFAULT_CONFIG)]
    config: PathBuf,

    /// Directory holding per-experiment configuration files.
    #[arg(long, global = true, default_value = DEFAULT_EXPERIMENT_DIR)]
    experiments: PathBuf,

    /// Increase log verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a capture session (the systemd ExecStart).
    Run {
        /// Experiment name (resolved under --experiments) or a config path.
        experiment: String,
        /// Override the configured session duration, e.g. `30s`, `10m`.
        #[arg(long, value_parser = parse_duration)]
        duration: Option<Duration>,
    },
    /// Validate a configuration without capturing.
    Validate {
        experiment: String,
        /// Also probe the hardware (interface presence, debugfs knobs).
        #[arg(long)]
        probe: bool,
    },
    /// Print the measured capability envelope and tuning tables.
    Caps {
        /// Emit JSON instead of a human-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Check whether this node can capture right now.
    Doctor {
        /// Interface to check.
        #[arg(long, default_value = "wlp1s0")]
        interface: String,
    },
    /// Convert a session's raw capture to a self-describing `.csiq`.
    Export {
        /// Session directory (containing capture.raw + metadata.json).
        session: PathBuf,
        /// Output path (defaults to <session>/capture.csiq).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Attach a debug subscriber to the live stream socket.
    Stream {
        #[arg(long, default_value = "/run/csid/live.sock")]
        socket: PathBuf,
        /// Stop after this many records.
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Timed capture(s) reporting achieved rate and CSI mix.
    Bench {
        experiment: String,
        /// Channels to sweep (defaults to the experiment's channel).
        #[arg(long, value_delimiter = ',')]
        channels: Vec<u32>,
        /// Duration per channel.
        #[arg(long, default_value = "30s", value_parser = parse_duration)]
        duration: Duration,
    },
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime_serde::re::humantime::parse_duration(s).map_err(|e| e.to_string())
}

fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    install_signal_handlers();

    if let Err(e) = dispatch(&cli) {
        // `{:#}` renders the whole anyhow context chain.
        tracing::error!("{e:#}");
        eprintln!("csid: {e:#}");
        std::process::exit(1);
    }
}

fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Run {
            experiment,
            duration,
        } => {
            let global = GlobalConfig::load(&cli.config)?;
            commands::run(
                &global,
                experiment,
                &cli.experiments,
                *duration,
                stop_flag(),
            )
        }
        Command::Validate { experiment, probe } => {
            commands::validate(experiment, &cli.experiments, *probe)
        }
        Command::Caps { json } => commands::caps_cmd(*json),
        Command::Doctor { interface } => {
            let global = GlobalConfig::load(&cli.config)?;
            commands::doctor(&global, interface)
        }
        Command::Export { session, out } => commands::export_cmd(session, out.clone()),
        Command::Stream { socket, limit } => commands::stream_cmd(socket, *limit),
        Command::Bench {
            experiment,
            channels,
            duration,
        } => {
            let global = GlobalConfig::load(&cli.config)?;
            commands::bench(
                &global,
                experiment,
                &cli.experiments,
                channels.clone(),
                *duration,
                stop_flag(),
            )
        }
    }
}

/// Logs go to journald under systemd, and to stderr otherwise.
fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("CSID_LOG").unwrap_or_else(|_| EnvFilter::new(default));

    #[cfg(target_os = "linux")]
    {
        // Under systemd, journald keeps structured fields; fall back to stderr
        // when the socket is absent (interactive runs, containers).
        if std::env::var_os("JOURNAL_STREAM").is_some() {
            if let Ok(layer) = tracing_journald::layer() {
                use tracing_subscriber::layer::SubscriberExt;
                use tracing_subscriber::util::SubscriberInitExt;
                tracing_subscriber::registry()
                    .with(filter)
                    .with(layer)
                    .init();
                return;
            }
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// A stop flag that mirrors SIGTERM/SIGINT into the session loop.
fn stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let mirror = stop.clone();
    std::thread::Builder::new()
        .name("csid-signals".into())
        .spawn(move || loop {
            if SIGNALLED.load(Ordering::Relaxed) {
                mirror.store(true, Ordering::Relaxed);
                return;
            }
            if Arc::strong_count(&mirror) == 1 {
                return; // session finished; nothing left to signal
            }
            std::thread::sleep(Duration::from_millis(100));
        })
        .ok();
    stop
}

extern "C" fn on_signal(_sig: libc::c_int) {
    // Async-signal-safe: a single relaxed atomic store.
    SIGNALLED.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}
