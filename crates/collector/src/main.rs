//! `collectord` — the UDP lab collector.
//!
//! It plays the **Collector** role of the MonadCount wireless-laboratory model: receive every
//! stream, stamp arrival with the reference clock, and be the single source of time for a session.
//!
//! Concretely it does three things, and deliberately nothing else:
//!
//! 1. Receives paced MNDP datagrams from mobile instruments and records a kernel arrival stamp for
//!    each — the delivered-rate timeline and inter-arrival statistics can only be produced here,
//!    at the receiving end.
//! 2. Answers the four-timestamp clock exchange, making it the phone's reference clock.
//! 3. Writes per-session records in the shape `csid`'s sync machinery already ships.
//!
//! It does not analyse, does not talk to a database, and does not decide anything about an
//! experiment. Everything it writes is raw enough to be re-reduced later.

mod config;
mod proto;
mod rx;
mod session;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::proto::{Hello, Packet};
use crate::rx::{now_unix_ns, RxSocket};
use crate::session::{Environment, SessionTable, Status};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "collectord", version = VERSION, about = "MonadCount UDP lab collector")]
struct Cli {
    /// Configuration file.
    #[arg(long, default_value = config::DEFAULT_PATH, global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Receive until stopped.
    Run,
    /// Parse and check the configuration without binding anything.
    Validate,
    /// Report what the runtime can actually do on this host.
    Doctor,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    match cli.command {
        Command::Validate => {
            let config = Config::load(&cli.config)?;
            println!(
                "ok: bind={} spool={} idle={}s",
                config.listen.bind,
                config.session.spool.display(),
                config.session.idle_timeout_seconds
            );
            Ok(())
        }
        Command::Doctor => doctor(&cli.config),
        Command::Run => run(&cli.config),
    }
}

fn doctor(path: &PathBuf) -> Result<()> {
    let config = Config::load(path)?;
    println!("collectord {VERSION}");
    println!("node:            {}", config.node_name());
    println!("config:          {}", path.display());
    println!("spool:           {}", config.session.spool.display());

    match RxSocket::bind(&config.listen.bind) {
        Ok(socket) => {
            println!("bind:            ok ({})", socket.local_addr()?);
            // The single most consequential runtime fact: without kernel stamps the inter-arrival
            // statistics measure this process's scheduler as much as the channel.
            println!(
                "kernel stamps:   {}",
                if socket.kernel_timestamps() {
                    "yes (SO_TIMESTAMPNS)"
                } else {
                    "NO — userspace fallback; arrival jitter will be inflated"
                }
            );
        }
        Err(e) => println!("bind:            FAILED: {e}"),
    }

    match std::fs::create_dir_all(&config.session.spool) {
        Ok(()) => println!("spool writable:  yes"),
        Err(e) => println!("spool writable:  NO: {e}"),
    }
    Ok(())
}

fn run(path: &PathBuf) -> Result<()> {
    let config = Config::load(path)?;
    std::fs::create_dir_all(&config.session.spool)
        .with_context(|| format!("creating spool {}", config.session.spool.display()))?;

    let socket = RxSocket::bind(&config.listen.bind)?;
    let environment = Environment {
        node: config.node_name(),
        collector_version: VERSION.to_string(),
        kernel_timestamps: socket.kernel_timestamps(),
        bind: config.listen.bind.clone(),
    };

    tracing::info!(
        bind = %config.listen.bind,
        node = %environment.node,
        kernel_timestamps = environment.kernel_timestamps,
        "collector listening"
    );
    csid::notify::ready();
    csid::notify::status(&format!("listening on {}", config.listen.bind));

    let running = Arc::new(AtomicBool::new(true));
    install_signal_handler(Arc::clone(&running))?;

    let mut sessions = SessionTable::default();
    let mut buf = vec![0u8; config.listen.buffer_bytes];
    let idle_timeout_ns = config.session.idle_timeout_seconds * 1_000_000_000;
    let heartbeat_ns = config.session.heartbeat_seconds.max(1) * 1_000_000_000;
    let mut last_maintenance = now_unix_ns();

    while running.load(Ordering::Relaxed) {
        match socket.recv(&mut buf) {
            Ok(Some(received)) => {
                if let Err(e) = handle(
                    &socket,
                    &config,
                    &environment,
                    &mut sessions,
                    &buf[..received.len],
                    &received,
                ) {
                    // Foreign traffic on the port is expected and must not be fatal; log at debug
                    // so a scanner cannot flood the journal.
                    tracing::debug!(peer = %received.peer, error = %e, "ignored datagram");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "receive failed"),
        }

        let now = now_unix_ns();
        if now.saturating_sub(last_maintenance) >= heartbeat_ns {
            last_maintenance = now;
            sessions.expire(now, idle_timeout_ns);
            csid::notify::watchdog();
            csid::notify::status(&format!("{} live session(s)", sessions.len()));
        }
    }

    tracing::info!("shutting down; closing {} session(s)", sessions.len());
    csid::notify::stopping();
    // Sessions cut short by a restart are `stopped`, not `complete` — they still ship, but the
    // status says plainly that the recording was interrupted rather than finished.
    sessions.close_all(Status::Stopped);
    Ok(())
}

fn handle(
    socket: &RxSocket,
    config: &Config,
    environment: &Environment,
    sessions: &mut SessionTable,
    bytes: &[u8],
    received: &rx::Received,
) -> Result<()> {
    let packet = Packet::decode(bytes)?;
    if packet.version != proto::VERSION {
        anyhow::bail!("unsupported protocol version {}", packet.version);
    }

    let uuid = packet.session_uuid();
    let session = sessions.get_or_create(
        &config.session.spool,
        &uuid,
        received.peer,
        environment,
    )?;

    match packet.kind {
        proto::TYPE_DATA => session.record_data(
            received.arrival_ns,
            packet.sequence,
            packet.t_mono_ns,
            packet.t_wall_ms,
            bytes.len(),
            received.kernel_stamped,
        )?,

        proto::TYPE_TIME_REQUEST => {
            if !config.listen.answer_time_requests {
                return Ok(());
            }
            // t2 is the kernel arrival stamp, not "now": the whole value of the exchange is that
            // t2 describes when the packet actually landed rather than when this code ran.
            let t2 = received.arrival_ns;
            let t3 = now_unix_ns();
            let reply = proto::encode_time_response(&packet.session, packet.sequence, t2, t3);
            socket.send_to(&reply, received.peer)?;
            session.record_exchange(packet.t_mono_ns, t2, t3, packet.sequence)?;
        }

        proto::TYPE_SESSION_HELLO => {
            let hello: Hello = serde_json::from_slice(packet.payload)
                .context("session hello payload is not the expected JSON")?;
            session.apply_hello(&hello);
            session.write_sidecar(Status::Receiving)?;
            tracing::info!(
                session = %uuid,
                participant = %hello.participant_id,
                site = %hello.site,
                "session identified"
            );
        }

        other => anyhow::bail!("unknown packet type {other}"),
    }
    Ok(())
}

fn install_signal_handler(running: Arc<AtomicBool>) -> Result<()> {
    // Deliberately a hand-rolled handler rather than a signal crate: the daemon has exactly one
    // shutdown path, and an extra dependency on the fleet's build is a cost with no return.
    static FLAG: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        FLAG.store(true, Ordering::SeqCst);
    }

    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    std::thread::spawn(move || loop {
        if FLAG.load(Ordering::SeqCst) {
            running.store(false, Ordering::Relaxed);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
