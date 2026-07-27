//! Session orchestration: set up the radio, run the fan-out, tear down cleanly.
//!
//! Thread layout (see `docs/architecture.md`):
//!
//! ```text
//! [RX thread]  pinned, SCHED_RR — recv + stamp + hand off, nothing else
//!     ├─ unbounded channel ─→ [durable thread] → capture.raw (lossless)
//!     └─ bounded channel   ─→ [live thread]    → CSIQ datagrams (best-effort)
//! [main thread] sd_notify watchdog, duration bound, stop flag
//! ```
//!
//! Nothing on the live side can apply backpressure to the RX thread: its
//! channel is bounded and the producer uses `try_send`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{ExperimentConfig, GlobalConfig};
use crate::debugfs::Knobs;
use crate::sidecar::{Sidecar, Status, SummaryMeta};
use crate::sinks::{Counters, DurableSink, LiveSink};
use crate::source::{self, RawCsiMessage};
use crate::util;
use crate::{export, radio};

/// What a finished session produced.
#[derive(Debug)]
pub struct SessionOutcome {
    pub session_id: String,
    pub dir: PathBuf,
    pub status: Status,
    pub summary: SummaryMeta,
}

/// One-line capture status for `systemctl status`: how much has been captured,
/// and how fast it is arriving right now.
///
/// The rate matters as much as the total. A passive session yields nothing when
/// the monitored channel carries no CSI-bearing traffic, so `active (running)`
/// on its own says only that the process is alive — a starving capture pings the
/// watchdog exactly like a healthy one. Reporting `0 rec, 0.0 Hz` here makes the
/// difference visible without opening the spool.
fn capture_status(session_id: &str, records: u64, rate_hz: f64) -> String {
    format!("capturing {session_id} ({records} rec, {rate_hz:.1} Hz)")
}

/// Run one capture session to completion.
pub fn run_session(
    global: &GlobalConfig,
    cfg: &ExperimentConfig,
    stop: Arc<AtomicBool>,
) -> Result<SessionOutcome> {
    cfg.validate().context("configuration is invalid")?;
    let tuning = radio::resolve(&cfg.radio)?;

    // -- session identity + directory -------------------------------------
    let host = global
        .node
        .hostname
        .clone()
        .or_else(|| util::run_opt("hostname", &[]))
        .unwrap_or_else(|| "unknown".to_string());
    let session_id = format!(
        "{host}_{}_{}",
        cfg.slug(),
        util::compact_stamp(util::now_unix())
    );
    let dir = global.node.spool.join(&session_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating session directory {}", dir.display()))?;
    tracing::info!(session_id, dir = %dir.display(), "session opening");

    // -- radio setup -------------------------------------------------------
    radio::ensure_monitor(&cfg.radio.interface, &cfg.radio.monitor)?;
    radio::tune(&cfg.radio.monitor, &tuning)?;

    let knobs = Knobs::for_interface(&cfg.radio.interface)?;
    knobs.set_interval(cfg.radio.interval_us)?;
    knobs.set_addresses(&cfg.radio.mac_filter)?;
    knobs.set_csi_enabled(true)?;

    // Sidecar is written *before* capture so a crashed session still has
    // complete provenance on disk.
    let mut sidecar = Sidecar::open(&dir, session_id.clone(), cfg, global, &tuning)?;

    // -- injector (capture.mode = "inject") --------------------------------
    // Spawned before the RX loop so a failed socket open fails the session at
    // setup. Runs on the tuned monitor interface; capture continues unchanged
    // (the driver reports no CSI for locally transmitted frames).
    let inject_counters = Arc::new(crate::inject::InjectCounters::default());
    let injector = if cfg.capture.mode == "inject" {
        match crate::inject::spawn(
            &cfg.radio.monitor,
            &cfg.inject,
            stop.clone(),
            inject_counters.clone(),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                knobs.set_best_effort(crate::debugfs::knob::CSI_ENABLED, "0");
                sidecar.close(Status::Failed, None);
                return Err(e).context("starting the injector");
            }
        }
    } else {
        None
    };

    // -- source ------------------------------------------------------------
    // The vendor registration must name the radio; CSI then arrives unicast to
    // this process's netlink portid.
    let wiphy = radio::phy_index(&cfg.radio.interface)?;
    let source = match source::open(&global.driver, wiphy) {
        Ok(s) => s,
        Err(e) => {
            knobs.set_best_effort(crate::debugfs::knob::CSI_ENABLED, "0");
            sidecar.close(Status::Failed, None);
            return Err(e).context("opening the CSI netlink source");
        }
    };

    let counters = Arc::new(Counters::default());
    let session_uid = util::now_unix();

    // -- channels ----------------------------------------------------------
    // Durable: unbounded (lossless). Live: bounded (best-effort).
    let (durable_tx, durable_rx) = mpsc::channel::<Arc<RawCsiMessage>>();
    let live_enabled = cfg.stream.enabled;
    let (live_tx, live_rx) = mpsc::sync_channel::<Arc<RawCsiMessage>>(if live_enabled {
        cfg.stream.max_queue
    } else {
        1
    });

    // -- durable writer thread --------------------------------------------
    let durable_counters = counters.clone();
    let durable_dir = dir.clone();
    let durable = thread::Builder::new()
        .name("csid-durable".into())
        .spawn(move || -> Result<PathBuf> {
            let mut sink = DurableSink::create(&durable_dir, durable_counters)?;
            while let Ok(msg) = durable_rx.recv() {
                sink.write(&msg)?;
            }
            sink.finish()
        })
        .context("spawning durable writer thread")?;

    // -- live publisher thread --------------------------------------------
    let live_counters = counters.clone();
    let width = cfg.radio.width.to_csiq();
    let stream_cfg = cfg.stream.clone();
    let live = thread::Builder::new()
        .name("csid-live".into())
        .spawn(move || {
            if !stream_cfg.enabled {
                // Drain so the sender never blocks if it was somehow used.
                while live_rx.recv().is_ok() {}
                return;
            }
            let sink = match stream_cfg.transport.as_str() {
                "udp" => LiveSink::udp(&stream_cfg.targets, session_uid, live_counters),
                _ => LiveSink::unix(&stream_cfg.unix_socket, session_uid, live_counters),
            };
            let mut sink = match sink {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "live sink unavailable; streaming disabled for this session");
                    while live_rx.recv().is_ok() {}
                    return;
                }
            };
            while let Ok(msg) = live_rx.recv() {
                match csiq::raw::parse_record(&msg.hdr, &msg.csi, width) {
                    Ok(mut rec) => {
                        // Prefer the host stamp taken at delivery.
                        if rec.unix_ts_ns == 0 {
                            rec.unix_ts_ns = msg.unix_ts_ns;
                        }
                        sink.publish(&rec);
                    }
                    Err(e) => tracing::debug!(error = %e, "live: unparseable record skipped"),
                }
            }
        })
        .context("spawning live publisher thread")?;

    // -- RX thread ---------------------------------------------------------
    let rx_stop = stop.clone();
    let rx_counters = counters.clone();
    let rx = thread::Builder::new()
        .name("csid-rx".into())
        .spawn(move || {
            apply_realtime_scheduling();
            let mut source = source;
            while !rx_stop.load(Ordering::Relaxed) {
                match source.recv() {
                    Ok(Some(msg)) => {
                        let msg = Arc::new(msg);
                        // Durable first, and it must never be skipped.
                        if durable_tx.send(msg.clone()).is_err() {
                            tracing::error!("durable writer went away; stopping capture");
                            break;
                        }
                        // Live is strictly best-effort.
                        if live_enabled {
                            if let Err(TrySendError::Full(_)) = live_tx.try_send(msg) {
                                rx_counters.live_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(None) => continue, // poll timeout; re-check the stop flag
                    Err(e) => {
                        tracing::error!(error = %e, "CSI source failed; stopping capture");
                        break;
                    }
                }
            }
            // Dropping the senders ends both consumer threads.
        })
        .context("spawning netlink RX thread")?;

    // -- supervise ---------------------------------------------------------
    crate::notify::ready();
    crate::notify::status(&capture_status(&session_id, 0, 0.0));
    let deadline = cfg.capture.duration.map(|d| Instant::now() + d);
    let watchdog_every = crate::notify::watchdog_interval().unwrap_or(Duration::from_secs(10));
    let mut last_log = Instant::now();
    let mut last_records: u64 = 0;

    let status = loop {
        thread::sleep(Duration::from_millis(200));

        if stop.load(Ordering::Relaxed) {
            break Status::Stopped;
        }
        if rx.is_finished() {
            break Status::Failed;
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break Status::Complete;
            }
        }

        crate::notify::watchdog();

        if last_log.elapsed() >= watchdog_every.max(Duration::from_secs(10)) {
            let (records, bytes, sent, dropped) = counters.snapshot();
            // Rate over the interval just elapsed, not since session start: a
            // capture that flowed for an hour and then stalled must not be
            // averaged back into looking healthy.
            let window = last_log.elapsed().as_secs_f64();
            let rate_hz = if window > 0.0 {
                records.saturating_sub(last_records) as f64 / window
            } else {
                0.0
            };
            tracing::info!(
                records,
                bytes,
                rate_hz,
                live_sent = sent,
                live_dropped = dropped,
                "capturing"
            );
            crate::notify::status(&capture_status(&session_id, records, rate_hz));
            last_records = records;
            last_log = Instant::now();
        }
    };

    // -- teardown ----------------------------------------------------------
    crate::notify::stopping();
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = injector {
        let _ = h.join();
    }
    let _ = rx.join();
    let raw_path = match durable.join() {
        Ok(Ok(p)) => Some(p),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "durable writer failed");
            None
        }
        Err(_) => {
            tracing::error!("durable writer panicked");
            None
        }
    };
    let _ = live.join();

    knobs.set_best_effort(crate::debugfs::knob::CSI_ENABLED, "0");

    // Close-time summary is best effort — it must never invalidate the capture.
    let mut summary = raw_path
        .as_ref()
        .map(|p| summarize(p, cfg, &counters))
        .unwrap_or_else(|| base_summary(&counters));

    if cfg.capture.mode == "inject" {
        let (sent, errors, skipped) = inject_counters.snapshot();
        summary.inject = Some(crate::sidecar::InjectSummary {
            sent,
            errors,
            skipped,
        });
    }

    sidecar.close(status, Some(summary.clone()));

    // Optional CSIQ export.
    if cfg.export.on_close {
        if let Some(p) = &raw_path {
            match export::raw_to_csiq(p, &dir.join("capture.csiq"), cfg, sidecar.path()) {
                Ok(n) => tracing::info!(records = n, "exported capture.csiq"),
                Err(e) => tracing::error!(error = %e, "CSIQ export failed (raw capture is intact)"),
            }
        }
    }

    tracing::info!(
        session_id,
        ?status,
        records = summary.records,
        bytes = summary.capture_bytes,
        "session closed"
    );

    Ok(SessionOutcome {
        session_id,
        dir,
        status,
        summary,
    })
}

fn base_summary(counters: &Counters) -> SummaryMeta {
    let (records, bytes, _sent, dropped) = counters.snapshot();
    SummaryMeta {
        capture_bytes: bytes,
        records,
        mean_rate_hz: 0.0,
        live_dropped: dropped,
        tone_counts: Vec::new(),
        inject: None,
    }
}

/// Walk the raw capture to produce the close-time summary.
fn summarize(raw: &std::path::Path, cfg: &ExperimentConfig, counters: &Counters) -> SummaryMeta {
    let mut summary = base_summary(counters);

    let Ok(file) = std::fs::File::open(raw) else {
        return summary;
    };
    let reader = std::io::BufReader::new(file);
    let mut rr = csiq::raw::RawReader::new(reader, cfg.radio.width.to_csiq());

    let mut tones: Vec<u16> = Vec::new();
    let mut count: u64 = 0;
    let mut first_ftm: Option<u32> = None;
    let mut unwrapper = csiq::FtmUnwrapper::new();
    let mut last_unwrapped: u64 = 0;

    while let Ok(Some(rec)) = rr.next_record() {
        count += 1;
        if !tones.contains(&rec.ntone) {
            tones.push(rec.ntone);
        }
        let u = unwrapper.push(rec.ftm);
        if first_ftm.is_none() {
            first_ftm = Some(rec.ftm);
        }
        last_unwrapped = u;
    }

    tones.sort_unstable();
    summary.records = count;
    summary.tone_counts = tones;

    if let Some(first) = first_ftm {
        let span_ticks = last_unwrapped.saturating_sub(first as u64);
        let span_s = csiq::ftm_to_seconds(span_ticks);
        if span_s > 0.0 {
            summary.mean_rate_hz = count as f64 / span_s;
        }
    }
    summary
}

/// Give the RX thread realtime scheduling so a scheduler stall cannot push the
/// delivery-jitter tail out (measured baseline: p50 19 µs, p99.9 5.4 ms).
///
/// Failure is non-fatal — an unprivileged run simply keeps default scheduling.
#[cfg(target_os = "linux")]
fn apply_realtime_scheduling() {
    let param = libc::sched_param { sched_priority: 50 };
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_RR, &param) };
    if rc != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "could not set SCHED_RR on the RX thread; continuing with default scheduling"
        );
    } else {
        tracing::debug!("RX thread running SCHED_RR priority 50");
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_realtime_scheduling() {}
