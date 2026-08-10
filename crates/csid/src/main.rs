//! `csid` — a systemd-native Wi-Fi CSI capture daemon.
//!
//! One static binary owning the whole userspace path: nl80211 vendor-event
//! consumption over netlink, `iwlmvm` debugfs control, monitor tuning, TOML
//! configuration, the session sidecar, lossless raw spooling, best-effort live
//! streaming, and CSIQ export.
//!
//! See `docs/architecture.md` for the threading model and `docs/CSIQ-format-v1.md`
//! for the on-disk format.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use csid::commands;
use csid::config::{GlobalConfig, DEFAULT_CONFIG, DEFAULT_EXPERIMENT_DIR};

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
    /// Parse-check the node-global configuration file and exit.
    ///
    /// The `nginx -t` of csid: config management can validate a candidate file
    /// before installing it, so a typo never reaches a running node.
    CheckConfig,
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
    /// Report SoC temperature and the firmware throttle word.
    ///
    /// With no flags, a human-readable line. `--prometheus` emits
    /// node_exporter textfile-collector input, so a systemd timer can publish
    /// the throttle state the OS itself does not expose: above 80 °C the Pi 5
    /// firmware reduces the ARM clock while `cpufreq` keeps reporting the
    /// governor's requested 2.4 GHz.
    Thermal {
        /// Emit Prometheus text-format metrics.
        #[arg(long)]
        prometheus: bool,
        /// Write the metrics to this path, replacing it atomically. Implies
        /// `--prometheus`. Point this at the textfile collector's directory.
        #[arg(long, value_name = "PATH")]
        write: Option<PathBuf>,
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
    /// Stamp a coarse block marker into the running session.
    ///
    /// Stamped with this node's native `unix_ts_ns` — the same clock the CSI
    /// records and the BLE observations carry — so it needs no transform to
    /// land on the analysis timeline. That makes it the BOUNDARY TIMESTAMP OF
    /// RECORD; the phone application's marker for the same `block_id` rides on
    /// `mono_ns` through the gate-G4 affine fit and is the cross-check.
    Marker {
        /// Join key to the app's marker for the same block. Required.
        #[arg(long)]
        block_id: String,
        /// `ZONE-A` / `ZONE-B` / `ZONE-C`.
        #[arg(long)]
        zone: String,
        /// `staircase` / `cycling` / `empty` / `sync` / `calibration` / `drift`.
        #[arg(long)]
        kind: String,
        /// `start` | `end` | `mark`.
        #[arg(long, default_value = "start")]
        phase: String,
        /// Commanded staircase level (0,1,2,3,4,6,8,10).
        #[arg(long)]
        level: Option<u32>,
        /// `seated` | `walking` — the pre-registered motion sub-condition.
        #[arg(long)]
        sub_condition: Option<String>,
        /// Free-form operator note.
        #[arg(long)]
        note: Option<String>,
        /// Session spool root.
        #[arg(long, default_value = "/var/lib/csid")]
        spool: PathBuf,
        /// Emit the marker as JSON (what `csid fleet marker` parses).
        #[arg(long)]
        json: bool,
    },
    /// Time transfer over the illumination stream (`time_transfer.parquet`).
    ///
    /// Both of this lab's transmitters stamp their transmit time inside the
    /// payload — the injector's `CSID | seq | tx_unix_ns` and the phone's MNDP
    /// header. A node running `[timesync].enabled` records those beside its own
    /// receive stamp, which turns illumination into a time-transfer channel:
    /// `csid fleet skew` then measures inter-node skew to microseconds, for
    /// free, without the round trip that bounds `csid fleet clock`.
    Timesync {
        #[command(subcommand)]
        command: TimesyncCommand,
    },
    /// The bench cockpit — drive the whole fleet from the laptop.
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
}

#[derive(Subcommand)]
enum TimesyncCommand {
    /// The node-local readout. `csid fleet skew` runs this over ssh; run it by
    /// hand when you are already on a node.
    Report {
        #[arg(long, default_value = "/var/lib/csid")]
        spool: PathBuf,
        /// Read a specific session directory instead of the newest.
        #[arg(long)]
        session: Option<PathBuf>,
        /// Analysis window, seconds. `0` reads the whole session.
        #[arg(long, default_value_t = 60.0)]
        window: f64,
        #[arg(long)]
        json: bool,
    },
    /// Rebuild `time_transfer.parquet` from the durable log.
    ///
    /// The recovery path for a session that lost power before its close-time
    /// export, and the way to re-pair `ftm` after restoring a `capture.raw`.
    Export {
        /// Session directory (containing time_transfer.jsonl + metadata.json).
        session: PathBuf,
        /// How near a CSI record must be to a received frame to be credited
        /// with its `ftm`, microseconds.
        #[arg(long, default_value_t = 2000)]
        ftm_tolerance_us: u64,
    },
}

/// Nodes are selected with `--nodes monad01,monad04` or `all` (the default,
/// meaning every node in `[fleet] nodes`).
#[derive(Subcommand)]
enum FleetCommand {
    /// Fleet status at a glance — the most-used command of a session.
    Status {
        #[arg(long)]
        nodes: Option<String>,
        /// Analysis window, seconds.
        #[arg(long, default_value_t = 30.0)]
        window: f64,
        /// Pin the scope to one transmitter (default: the window's dominant).
        #[arg(long)]
        src_mac: Option<String>,
        /// Pin the record class, e.g. `52:legacyofdm`.
        #[arg(long)]
        class: Option<String>,
        /// Skip the clock round trips (faster, but the clock column is blank).
        #[arg(long)]
        no_clock: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_color: bool,
    },
    /// Per-node clock offset and the fleet's worst mutual skew, over ssh.
    ///
    /// The FALLBACK instrument: bounded by round-trip time. Prefer
    /// `csid fleet skew` whenever a stamped transmitter is on the air.
    Clock {
        #[arg(long)]
        nodes: Option<String>,
        /// Round trips per node; the minimum-delay one decides.
        #[arg(long, default_value_t = 5)]
        samples: usize,
        #[arg(long)]
        json: bool,
    },
    /// Inter-node skew measured through the illumination stream — µs, not ms.
    ///
    /// One transmitted frame reaches every node at the same instant, so the
    /// difference of two nodes' receive stamps for the same sequence number IS
    /// their clock skew plus per-node RX jitter, and the jitter averages out
    /// over thousands of packets. Nothing round-trips, so nothing is bounded by
    /// round-trip asymmetry. Requires `[timesync].enabled` on the nodes.
    Skew {
        #[arg(long)]
        nodes: Option<String>,
        /// Analysis window, seconds.
        #[arg(long, default_value_t = 60.0)]
        window: f64,
        /// Fewest sequence numbers two nodes must share to be compared.
        #[arg(long, default_value_t = csid::timesync::skew::MIN_COMMON_DEFAULT)]
        min_common: usize,
        /// Pin the transmitter (default: the one the most nodes heard).
        #[arg(long)]
        tx: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Evaluate a pre-registered gate.
    Gate {
        #[command(subcommand)]
        which: GateCommand,
    },
    /// Start a named capture on every node — and confirm each one started.
    Start {
        experiment: String,
        #[arg(long)]
        nodes: Option<String>,
        /// How long to wait for each node to confirm it is producing records.
        #[arg(long, default_value_t = 30)]
        confirm_seconds: u64,
    },
    /// Stop the capture on every node.
    Stop {
        experiment: String,
        #[arg(long)]
        nodes: Option<String>,
    },
    /// Broadcast a block marker to every node.
    Marker {
        #[arg(long)]
        block_id: String,
        #[arg(long)]
        zone: String,
        #[arg(long)]
        kind: String,
        #[arg(long, default_value = "start")]
        phase: String,
        #[arg(long)]
        level: Option<u32>,
        #[arg(long)]
        sub_condition: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        nodes: Option<String>,
    },
    /// The node-local health readout. `fleet status` runs this over ssh; run it
    /// by hand when you are already on a node.
    Probe {
        #[arg(long, default_value = "/var/lib/csid")]
        spool: PathBuf,
        #[arg(long, default_value_t = 30.0)]
        window: f64,
        #[arg(long)]
        src_mac: Option<String>,
        #[arg(long)]
        class: Option<String>,
        /// Probe a specific session directory instead of the newest.
        #[arg(long)]
        session: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Walk a documented bring-up, stopping at the first failure.
    Runbook {
        #[command(subcommand)]
        which: RunbookCommand,
    },
}

#[derive(Subcommand)]
enum GateCommand {
    /// G1 — delivered rate >= 100 Hz on the 95% CI LOWER bound.
    G1 {
        #[arg(long)]
        nodes: Option<String>,
        #[arg(long, default_value_t = 60.0)]
        window: f64,
        #[arg(long)]
        src_mac: Option<String>,
        #[arg(long)]
        class: Option<String>,
    },
    /// G2 — inter-arrival CV < 0.5 on the 95% CI UPPER bound.
    G2 {
        #[arg(long)]
        nodes: Option<String>,
        #[arg(long, default_value_t = 60.0)]
        window: f64,
        #[arg(long)]
        src_mac: Option<String>,
        #[arg(long)]
        class: Option<String>,
    },
    /// G3 — the Day-0 5 GHz `iax` AP stability probe.
    G3 {
        /// Experiment to capture with during the probe.
        experiment: String,
        #[arg(long)]
        nodes: Option<String>,
        /// Probe window. The registered minimum is 30 min.
        #[arg(long, default_value = "30m", value_parser = parse_duration)]
        duration: Duration,
        /// Node hosting the 5 GHz AP.
        #[arg(long)]
        ap_node: Option<String>,
        /// systemd unit that brings the AP up (default `hostapd`).
        #[arg(long)]
        ap_unit: Option<String>,
        /// AP interface to read associated stations from (default `wlan1`).
        #[arg(long)]
        ap_interface: Option<String>,
        /// The AP is already running; do not try to start it.
        #[arg(long)]
        assume_ap_up: bool,
    },
}

#[derive(Subcommand)]
enum RunbookCommand {
    /// The Day-0 sequence: mask bluetoothd -> hciconfig up -> doctor ->
    /// validate --probe -> BLE sanity -> fleet clock -> delivered-rate gate.
    Day0 {
        /// Experiment to validate (the calibration profile, e.g. `lab-anchor`).
        #[arg(default_value = "lab-anchor")]
        experiment: String,
        #[arg(long)]
        nodes: Option<String>,
        #[arg(long, default_value = "hci0")]
        adapter: String,
        /// Length of the BLE sanity window.
        #[arg(long, default_value_t = 60)]
        ble_seconds: u64,
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
        Command::CheckConfig => {
            let global = GlobalConfig::load(&cli.config)?;
            println!(
                "{}: ok — spool={} sync={} otel={} driver oui=0x{:06x} subcmd=0x{:02x}",
                cli.config.display(),
                global.node.spool.display(),
                global.sync.enabled,
                global.otel.enabled,
                global.driver.vendor_oui,
                global.driver.csi_event_subcmd,
            );
            Ok(())
        }
        Command::Caps { json } => commands::caps_cmd(*json),
        Command::Doctor { interface } => {
            let global = GlobalConfig::load(&cli.config)?;
            commands::doctor(&global, interface)
        }
        Command::Export { session, out } => commands::export_cmd(session, out.clone()),
        Command::Stream { socket, limit } => commands::stream_cmd(socket, *limit),
        Command::Thermal { prometheus, write } => commands::thermal_cmd(*prometheus, write.clone()),
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
        Command::Marker {
            block_id,
            zone,
            kind,
            phase,
            level,
            sub_condition,
            note,
            spool,
            json,
        } => csid::fleet::cmd::marker_cmd(
            spool.clone(),
            block_id.clone(),
            zone.clone(),
            kind.clone(),
            phase.clone(),
            *level,
            sub_condition.clone(),
            note.clone(),
            *json,
        ),
        Command::Timesync { command } => match command {
            TimesyncCommand::Report {
                spool,
                session,
                window,
                json,
            } => csid::fleet::cmd::timesync_report_cmd(
                spool.clone(),
                session.clone(),
                *window,
                *json,
            ),
            TimesyncCommand::Export {
                session,
                ftm_tolerance_us,
            } => csid::fleet::cmd::timesync_export_cmd(session.clone(), *ftm_tolerance_us),
        },
        Command::Fleet { command } => dispatch_fleet(cli, command),
    }
}

fn dispatch_fleet(cli: &Cli, command: &FleetCommand) -> Result<()> {
    use csid::fleet::{self, cmd, StatusOptions};

    // `fleet probe` runs ON a node and must not need the cockpit's config.
    if let FleetCommand::Probe {
        spool,
        window,
        src_mac,
        class,
        session,
        json,
    } = command
    {
        return cmd::probe_cmd(
            spool.clone(),
            *window,
            src_mac.clone(),
            class.clone(),
            session.clone(),
            *json,
        );
    }

    let cfg = fleet::load(&cli.config);

    match command {
        FleetCommand::Probe { .. } => unreachable!("handled above"),
        FleetCommand::Status {
            nodes,
            window,
            src_mac,
            class,
            no_clock,
            json,
            no_color,
        } => cmd::status_cmd(
            &cfg,
            nodes.as_deref(),
            StatusOptions {
                window_s: *window,
                src_mac: src_mac.clone(),
                class: class.clone(),
                with_clock: !*no_clock,
                ..StatusOptions::default()
            },
            *json,
            *no_color,
        ),
        FleetCommand::Clock {
            nodes,
            samples,
            json,
        } => cmd::clock_cmd(&cfg, nodes.as_deref(), *samples, *json),
        FleetCommand::Skew {
            nodes,
            window,
            min_common,
            tx,
            json,
        } => cmd::fleet_skew_cmd(
            &cfg,
            nodes.as_deref(),
            *window,
            *min_common,
            tx.clone(),
            *json,
        ),
        FleetCommand::Gate { which } => match which {
            GateCommand::G1 {
                nodes,
                window,
                src_mac,
                class,
            } => cmd::gate_rate_cmd(
                &cfg,
                nodes.as_deref(),
                "g1",
                StatusOptions {
                    window_s: *window,
                    src_mac: src_mac.clone(),
                    class: class.clone(),
                    with_clock: false,
                    ..StatusOptions::default()
                },
            ),
            GateCommand::G2 {
                nodes,
                window,
                src_mac,
                class,
            } => cmd::gate_rate_cmd(
                &cfg,
                nodes.as_deref(),
                "g2",
                StatusOptions {
                    window_s: *window,
                    src_mac: src_mac.clone(),
                    class: class.clone(),
                    with_clock: false,
                    ..StatusOptions::default()
                },
            ),
            GateCommand::G3 {
                experiment,
                nodes,
                duration,
                ap_node,
                ap_unit,
                ap_interface,
                assume_ap_up,
            } => cmd::gate_g3_cmd(
                &cfg,
                nodes.as_deref(),
                experiment,
                *duration,
                ap_node.clone(),
                ap_unit.clone(),
                ap_interface.clone(),
                *assume_ap_up,
            ),
        },
        FleetCommand::Start {
            experiment,
            nodes,
            confirm_seconds,
        } => cmd::lifecycle_cmd(&cfg, nodes.as_deref(), experiment, false, *confirm_seconds),
        FleetCommand::Stop { experiment, nodes } => {
            cmd::lifecycle_cmd(&cfg, nodes.as_deref(), experiment, true, 0)
        }
        FleetCommand::Marker {
            block_id,
            zone,
            kind,
            phase,
            level,
            sub_condition,
            note,
            nodes,
        } => cmd::fleet_marker_cmd(
            &cfg,
            nodes.as_deref(),
            block_id,
            zone,
            kind,
            phase,
            *level,
            sub_condition.as_deref(),
            note.as_deref(),
        ),
        FleetCommand::Runbook { which } => match which {
            RunbookCommand::Day0 {
                experiment,
                nodes,
                adapter,
                ble_seconds,
            } => cmd::runbook_day0_cmd(&cfg, nodes.as_deref(), experiment, adapter, *ble_seconds),
        },
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
