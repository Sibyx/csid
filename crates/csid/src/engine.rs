//! Session orchestration: set up the radio, run the fan-out, tear down cleanly.
//!
//! Thread layout (see `docs/architecture.md`):
//!
//! ```text
//! [RX thread]  pinned, SCHED_RR — recv + stamp + hand off, nothing else
//!     ├─ unbounded channel ─→ [durable thread] → capture.raw (lossless)
//!     │                                            │ (capture.segment_duration)
//!     │                                            └─→ [sealer thread] → sidecar + .csiq
//!     └─ bounded channel   ─→ [live thread]    → CSIQ datagrams (best-effort)
//! [main thread] sd_notify watchdog, duration bound, stop flag
//! ```
//!
//! Nothing on the live side can apply backpressure to the RX thread: its
//! channel is bounded and the producer uses `try_send`.
//!
//! With `capture.segment_duration` set, the durable thread rolls its output
//! file on a wall-clock deadline and hands each sealed file to the sealer,
//! which writes that segment's sidecar and CSIQ export. The rotation itself is
//! only flush + fsync + create, so the expensive export never stalls the
//! writer. See [`crate::segment`] for why a segment is a sibling session
//! directory rather than a nested one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::ble::{self, BleCounters};
use crate::config::{ExperimentConfig, GlobalConfig};
use crate::debugfs::Knobs;
use crate::segment;
use crate::sidecar::{BleSummary, Sidecar, Status, SummaryMeta, TimesyncSummary};
use crate::sinks::{Counters, DurableSink, LiveSink};
use crate::source::{self, RawCsiMessage};
use crate::timesync::{self, TimesyncCounters};
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
/// `ble` carries `(observations, Hz)` when co-capture is enabled. It is appended
/// rather than interleaved so the CSI half of the line is byte-identical to
/// what it has always been, and absent entirely on a session without BLE.
fn capture_status(session_id: &str, records: u64, rate_hz: f64, ble: Option<(u64, f64)>) -> String {
    let base = format!("capturing {session_id} ({records} rec, {rate_hz:.1} Hz)");
    match ble {
        // A scanner that has stopped producing shows as `0.0 Hz` here, which is
        // the whole point: `active (running)` alone never proved BLE was alive.
        Some((obs, hz)) => format!("{base} · ble {obs} obs, {hz:.1} Hz"),
        None => base,
    }
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

    // The volatile counterpart of the sidecar (IP-132): what the capture is
    // doing right now, readable while it runs. Built here so it inherits the
    // sidecar's own run-id resolution rather than repeating it — two answers to
    // "which run is this" is exactly the drift the field exists to remove.
    let status_writer = (!global.node.status_path.as_os_str().is_empty())
        .then(|| crate::status::StatusWriter::new(global.node.status_path.clone()));
    let mut status_snap = crate::status::Snapshot {
        schema: crate::status::SCHEMA.to_string(),
        session_id: session_id.clone(),
        run_id: sidecar.run_id.clone(),
        run_id_generated: sidecar.run_id_generated,
        experiment: cfg.slug().to_string(),
        host: host.clone(),
        state: crate::status::STATE_STARTING.to_string(),
        started_unix_ns: util::now_unix_ns(),
        uptime_s: 0,
        channel: cfg.radio.channel,
        width: cfg.radio.width.iw_token().to_string(),
        band: crate::sidecar::band_label(tuning.band).to_string(),
        control_freq_mhz: tuning.freq,
        center_freq_mhz: tuning.center,
        interval_us: cfg.radio.interval_us,
        records: 0,
        frames_seen: 0,
        rate_hz: 0.0,
        capture_bytes: 0,
        live_sent: 0,
        live_dropped: 0,
        ble: cfg.ble.enabled.then(crate::status::BleStatus::default),
    };
    if let Some(w) = &status_writer {
        w.publish(&status_snap);
    }

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

    // -- time transfer over the illumination stream ------------------------
    // A sibling of the CSI path in exactly the sense the BLE scanner is: its
    // own socket, its own thread, its own file, sharing only the stop flag. It
    // cannot apply backpressure to the RX thread and its failure is recorded
    // rather than propagated (unless the profile declared it required, which a
    // session whose analysis pools nodes should).
    let ts_counters = Arc::new(TimesyncCounters::default());
    let mut ts_error: Option<String> = None;
    let timesync = if cfg.timesync.enabled {
        match timesync::spawn(
            &dir,
            &cfg.radio.monitor,
            &cfg.timesync,
            stop.clone(),
            ts_counters.clone(),
        ) {
            Ok(h) => Some(h),
            Err(e) if cfg.timesync.required => {
                knobs.set_best_effort(crate::debugfs::knob::CSI_ENABLED, "0");
                if let Some(h) = injector {
                    stop.store(true, Ordering::Relaxed);
                    let _ = h.join();
                }
                sidecar.close(Status::Failed, None);
                return Err(e)
                    .context("starting the time-transfer receiver (timesync.required = true)");
            }
            Err(e) => {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "time transfer failed to start; continuing WITHOUT inter-node skew data"
                );
                ts_error = Some(format!("{e:#}"));
                None
            }
        }
    } else {
        None
    };

    // -- BLE co-capture (IP-106 R5) ----------------------------------------
    // A sibling of the CSI path, never a dependency of it: the scanner shares
    // only the stop flag and the clock. Its failure mode is chosen by the
    // operator — `ble.required = true` on a calibration session (a capture
    // without BLE anchors is worthless), degrade-and-record otherwise.
    let ble_counters = Arc::new(BleCounters::default());
    let mut ble_error: Option<String> = None;
    let ble = if cfg.ble.enabled {
        match ble::spawn(&dir, &cfg.ble, stop.clone(), ble_counters.clone()) {
            Ok(h) => Some(h),
            Err(e) if cfg.ble.required => {
                knobs.set_best_effort(crate::debugfs::knob::CSI_ENABLED, "0");
                stop.store(true, Ordering::Relaxed);
                if let Some(h) = injector {
                    let _ = h.join();
                }
                // The time-transfer thread holds an AF_PACKET socket; `csid
                // bench` runs sessions back to back, so it must not be left
                // behind.
                if let Some(h) = timesync {
                    h.join();
                }
                sidecar.close(Status::Failed, None);
                return Err(e).context("starting the BLE scanner (ble.required = true)");
            }
            Err(e) => {
                // Loud, recorded, and non-fatal: the CSI capture is worth more
                // than the BLE channel, but a reader must never mistake an
                // absent BLE stream for an empty room.
                tracing::error!(
                    error = %format!("{e:#}"),
                    "BLE co-capture failed to start; continuing WITHOUT a BLE stream"
                );
                ble_error = Some(format!("{e:#}"));
                None
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
            // The scan thread holds an HCI socket and the time-transfer thread
            // an AF_PACKET one; `csid bench` runs sessions back to back, so
            // neither must be left behind.
            stop.store(true, Ordering::Relaxed);
            if let Some(h) = ble {
                h.join();
            }
            if let Some(h) = timesync {
                h.join();
            }
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

    // -- segment sealer ----------------------------------------------------
    // Only spawned when rotation is configured. It turns each sealed segment
    // into a session-shaped directory that `csid-sync` ships and `csid-prune`
    // reclaims *while this session is still running* — see `segment`.
    let segment_duration = cfg.capture.segment_duration;
    let sealer = match segment_duration {
        Some(d) => {
            tracing::info!(
                every = %humantime_serde::re::humantime::format_duration(d),
                "segment rotation enabled — sealed segments sync and prune during the run"
            );
            Some(segment::spawn(sidecar.clone(), cfg.clone())?)
        }
        None => None,
    };

    // -- durable writer thread --------------------------------------------
    let durable_counters = counters.clone();
    let durable_dir = dir.clone();
    let durable_session_id = session_id.clone();
    let durable = thread::Builder::new()
        .name("csid-durable".into())
        .spawn(move || -> Result<Vec<PathBuf>> {
            // With rotation on, the CSI stream never lands in the session root:
            // segment 0001 is a directory in its own right, so the root holds
            // only whole-session artefacts (the sidecar, time transfer, BLE).
            let spool = durable_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| durable_dir.clone());
            let mut index: u32 = 1;
            let mut write_dir = durable_dir.clone();
            if segment_duration.is_some() {
                write_dir = spool.join(segment::segment_dir_name(&durable_session_id, index));
                std::fs::create_dir_all(&write_dir)
                    .with_context(|| format!("creating segment dir {}", write_dir.display()))?;
            }

            let mut sink = DurableSink::create(&write_dir, durable_counters)?;
            let mut deadline = segment_duration.map(|d| Instant::now() + d);

            loop {
                // A short poll rather than a blocking `recv`: a segment must
                // roll on wall-clock time even on a channel so quiet that no
                // record arrives to trigger the check. Otherwise a silent hour
                // produces one oversized segment and nothing ships.
                match durable_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(msg) => sink.write(&msg)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }

                let Some(d) = segment_duration else { continue };
                if deadline.is_some_and(|dl| Instant::now() < dl) {
                    continue;
                }

                index += 1;
                let next_dir = spool.join(segment::segment_dir_name(&durable_session_id, index));
                if let Err(e) = std::fs::create_dir_all(&next_dir) {
                    // Keep capturing into the current segment rather than
                    // losing the stream over a full or read-only spool; the
                    // next tick retries.
                    tracing::error!(
                        error = %e,
                        dir = %next_dir.display(),
                        "creating the next segment dir failed; continuing in the current segment"
                    );
                    deadline = Some(Instant::now() + d);
                    index -= 1;
                    continue;
                }
                let sealed_raw = sink.rotate(&next_dir)?;
                let sealed_dir = sealed_raw
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| spool.clone());
                if let Some(s) = &sealer {
                    s.submit(segment::Sealed {
                        dir: sealed_dir,
                        raw: sealed_raw,
                        index: index - 1,
                    });
                }
                deadline = Some(Instant::now() + d);
            }

            let last = sink.finish()?;
            match sealer {
                // The final partial segment is sealed exactly like the others,
                // so a stopped run leaves no directory that sync would skip.
                // `finish` then drains the queue and hands back every sealed
                // raw in index order — which is the order the session-level
                // summary must walk them in for FTM unwrapping to be continuous.
                Some(s) => {
                    let last_dir = last
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| spool.clone());
                    s.submit(segment::Sealed {
                        dir: last_dir,
                        raw: last,
                        index,
                    });
                    Ok(s.finish())
                }
                None => Ok(vec![last]),
            }
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
    let ble_live = cfg.ble.enabled;
    crate::notify::ready();
    crate::notify::status(&capture_status(
        &session_id,
        0,
        0.0,
        ble_live.then_some((0, 0.0)),
    ));
    let deadline = cfg.capture.duration.map(|d| Instant::now() + d);
    let watchdog_every = crate::notify::watchdog_interval().unwrap_or(Duration::from_secs(10));
    let started = Instant::now();
    let mut last_log = Instant::now();
    let mut last_records: u64 = 0;
    let mut last_ble: u64 = 0;

    // The status file ticks ten times faster than the journal heartbeat. It is
    // read by a human watching a room, and ten seconds is long enough to walk
    // out of one; the journal line is read afterwards, where it is not.
    const STATUS_EVERY: Duration = Duration::from_secs(1);
    let mut last_status = Instant::now();
    let mut last_status_records: u64 = 0;
    let mut last_status_ble: u64 = 0;
    status_snap.state = crate::status::STATE_CAPTURING.to_string();

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

        if let Some(w) = &status_writer {
            if last_status.elapsed() >= STATUS_EVERY {
                let window = last_status.elapsed().as_secs_f64();
                let (records, bytes, sent, dropped) = counters.snapshot();
                status_snap.records = records;
                // `frames_seen` is the time-transfer receiver's count of frames
                // the radio delivered, CSI-bearing or not. Without it there is
                // no denominator, and a session with `timesync.enabled = false`
                // honestly reports 0 rather than inventing one.
                status_snap.frames_seen = ts_counters.frames_seen.load(Ordering::Relaxed);
                status_snap.rate_hz = if window > 0.0 {
                    records.saturating_sub(last_status_records) as f64 / window
                } else {
                    0.0
                };
                status_snap.capture_bytes = bytes;
                status_snap.live_sent = sent;
                status_snap.live_dropped = dropped;
                status_snap.uptime_s = started.elapsed().as_secs();
                if let Some(b) = status_snap.ble.as_mut() {
                    let obs = ble_counters.observations.load(Ordering::Relaxed);
                    b.rate_hz = if window > 0.0 {
                        obs.saturating_sub(last_status_ble) as f64 / window
                    } else {
                        0.0
                    };
                    b.observations = obs;
                    last_status_ble = obs;
                }
                w.publish(&status_snap);
                last_status_records = records;
                last_status = Instant::now();
            }
        }

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
            let ble_now = ble_live.then(|| {
                let obs = ble_counters.observations.load(Ordering::Relaxed);
                let hz = if window > 0.0 {
                    obs.saturating_sub(last_ble) as f64 / window
                } else {
                    0.0
                };
                last_ble = obs;
                (obs, hz)
            });
            tracing::info!(
                records,
                bytes,
                rate_hz,
                live_sent = sent,
                live_dropped = dropped,
                ble_observations = ble_now.map(|(o, _)| o),
                ble_rate_hz = ble_now.map(|(_, h)| h),
                "capturing"
            );
            crate::notify::status(&capture_status(&session_id, records, rate_hz, ble_now));
            last_records = records;
            last_log = Instant::now();
        }
    };

    // -- teardown ----------------------------------------------------------
    crate::notify::stopping();
    // Say so before the joins rather than after: sealing a segmented capture
    // can take seconds, and during them a console reading the last `capturing`
    // snapshot would report a session that has already stopped.
    if let Some(w) = &status_writer {
        status_snap.state = crate::status::STATE_STOPPING.to_string();
        status_snap.uptime_s = started.elapsed().as_secs();
        w.publish(&status_snap);
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = injector {
        let _ = h.join();
    }
    let _ = rx.join();
    // One entry for an unsegmented session; one per segment, in index order,
    // for a rotated one.
    let raw_paths: Vec<PathBuf> = match durable.join() {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "durable writer failed");
            Vec::new()
        }
        Err(_) => {
            tracing::error!("durable writer panicked");
            Vec::new()
        }
    };
    // The session root only owns a `capture.raw` when rotation is off; with
    // segments the root keeps just the whole-session artefacts.
    let raw_path: Option<PathBuf> = (cfg.capture.segment_duration.is_none())
        .then(|| raw_paths.first().cloned())
        .flatten();
    let _ = live.join();

    knobs.set_best_effort(crate::debugfs::knob::CSI_ENABLED, "0");

    // The time-transfer log has to be closed BEFORE the raw walk, because the
    // transmitters it saw are what scope that walk: `capture.raw` on a busy
    // channel is millions of records, and only the ones from a transmitter we
    // actually took a stamp from can ever be paired.
    let ts_rows = timesync.map(|h| {
        let ndjson = h.join();
        timesync::read_log(&ndjson).unwrap_or_else(|e| {
            tracing::error!(error = %format!("{e:#}"), "reading the time-transfer log failed");
            (Vec::new(), 0)
        })
    });
    let ts_macs: std::collections::HashSet<[u8; 6]> = ts_rows
        .as_ref()
        .map(|(rows, _)| {
            rows.iter()
                .filter_map(|r| crate::timesync::payload::parse_mac(&r.tx_mac))
                .collect()
        })
        .unwrap_or_default();

    // Close-time summary is best effort — it must never invalidate the capture.
    // The same single pass over `capture.raw` yields the `ftm` ticks, so time
    // transfer costs the teardown nothing extra and the hot path nothing at all.
    let (mut summary, ticks) = if raw_paths.is_empty() {
        (base_summary(&counters), Vec::new())
    } else {
        summarize(&raw_paths, cfg, &counters, &ts_macs)
    };

    if cfg.timesync.enabled {
        summary.timesync = Some(finish_timesync(
            &dir,
            &session_id,
            &host,
            cfg,
            ts_rows,
            &ticks,
            &ts_counters,
            ts_error,
        ));
    }

    if cfg.capture.mode == "inject" {
        let (sent, errors, skipped) = inject_counters.snapshot();
        summary.inject = Some(crate::sidecar::InjectSummary {
            sent,
            errors,
            skipped,
        });
    }

    if cfg.ble.enabled {
        summary.ble = Some(finish_ble(
            &dir,
            &session_id,
            &host,
            cfg,
            ble,
            &ble_counters,
            ble_error,
        ));
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

/// Join the scan thread, export `ble_rssi.parquet`, and grade the channel.
///
/// Every failure here is recorded rather than propagated: by this point the CSI
/// capture is on disk and closing it out is worth more than any BLE artefact.
/// The grade is what a reader looks at — `failed` and `degraded` both mean
/// "do not treat this BLE stream as a clean observation of the room".
#[allow(clippy::too_many_arguments)]
fn finish_ble(
    dir: &std::path::Path,
    session_id: &str,
    host: &str,
    cfg: &ExperimentConfig,
    handle: Option<crate::ble::BleHandle>,
    counters: &BleCounters,
    startup_error: Option<String>,
) -> BleSummary {
    let (observations, mean_rate_hz) = counters.snapshot();
    let mut s = BleSummary {
        status: "failed".to_string(),
        error: startup_error,
        observations,
        mean_rate_hz,
        max_gap_s: counters.max_gap_ms.load(Ordering::Relaxed) as f64 / 1000.0,
        gaps_over_alert: counters.gaps_over_alert.load(Ordering::Relaxed),
        gap_alert_s: cfg.ble.gap_alert_s,
        scan_restarts: counters.scan_restarts.load(Ordering::Relaxed),
        adapter_errors: counters.adapter_errors.load(Ordering::Relaxed),
        unparsed_events: counters.unparsed_events.load(Ordering::Relaxed),
        rssi_unavailable: counters.rssi_unavailable.load(Ordering::Relaxed),
        ..BleSummary::default()
    };

    let Some(handle) = handle else {
        return s; // never started; `error` already says why
    };
    let ndjson = handle.join();

    let ctx = crate::ble::ParquetContext {
        host: host.to_string(),
        session_id: session_id.to_string(),
        adapter: cfg.ble.adapter.clone(),
    };
    match crate::ble::export_parquet(&ndjson, &dir.join(crate::ble::PARQUET_NAME), &ctx) {
        Ok(stats) => {
            s.parquet_rows = stats.rows;
            s.distinct_device_hashes = stats.distinct_device_hashes;
            s.malformed_log_lines = stats.malformed_lines;
            tracing::info!(
                rows = stats.rows,
                devices = stats.distinct_device_hashes,
                "exported {}",
                crate::ble::PARQUET_NAME
            );
        }
        Err(e) => {
            tracing::error!(
                error = %format!("{e:#}"),
                "BLE parquet export failed ({} is intact)",
                crate::ble::NDJSON_NAME
            );
            s.error.get_or_insert_with(|| format!("{e:#}"));
        }
    }

    s.status = if s.observations == 0 {
        "failed"
    } else if s.scan_restarts > 0
        || s.adapter_errors > 0
        || s.gaps_over_alert > 0
        || s.parquet_rows != s.observations
    {
        "degraded"
    } else {
        "ok"
    }
    .to_string();
    s
}

/// Join the receive thread, attribute an `ftm` to each row, and export the
/// contract artefact.
///
/// Every failure here is recorded rather than propagated: by this point the CSI
/// capture is on disk, and closing it out is worth more than any time-transfer
/// artefact. `status` is what a reader looks at first.
#[allow(clippy::too_many_arguments)]
fn finish_timesync(
    dir: &std::path::Path,
    session_id: &str,
    host: &str,
    cfg: &ExperimentConfig,
    rows: Option<(Vec<timesync::Row>, u64)>,
    ticks: &[timesync::CsiTick],
    counters: &TimesyncCounters,
    startup_error: Option<String>,
) -> TimesyncSummary {
    let (n_rows, mean_rate_hz) = counters.snapshot();
    let mut s = TimesyncSummary {
        status: "failed".to_string(),
        error: startup_error,
        rx_stamp_source: "none".to_string(),
        mean_rate_hz,
        frames_seen: counters.frames_seen.load(Ordering::Relaxed),
        own_transmissions: counters.own_transmissions.load(Ordering::Relaxed),
        protected_frames: counters.protected.load(Ordering::Relaxed),
        unrecognised_frames: counters.no_stamp.load(Ordering::Relaxed),
        ..TimesyncSummary::default()
    };

    let Some((mut rows, malformed)) = rows else {
        return s; // never started; `error` already says why
    };
    s.malformed_log_lines = malformed;

    let paired = timesync::pair_ftm(&mut rows, ticks, cfg.timesync.ftm_tolerance_ns());
    s.ftm_paired = paired as u64;

    // A userspace-stamped session carries the scheduler's wake-up jitter in
    // every stamp; a MIXED one has two jitter causes in one file. Both are
    // reported rather than assumed away.
    let mut kernel = false;
    let mut userspace = false;
    for r in &rows {
        match r.rx_stamp_src {
            timesync::StampSource::Kernel => kernel = true,
            timesync::StampSource::Userspace => userspace = true,
        }
    }
    s.rx_stamp_source = match (kernel, userspace) {
        (true, true) => "mixed",
        (true, false) => "kernel",
        (false, true) => "userspace",
        (false, false) => "none",
    }
    .to_string();

    match timesync::write_parquet(
        &rows,
        &dir.join(timesync::PARQUET_NAME),
        &timesync::ParquetContext {
            host: host.to_string(),
            session_id: session_id.to_string(),
        },
    ) {
        Ok(stats) => {
            s.rows = stats.rows;
            s.rows_csid = stats.rows_csid;
            s.rows_app = stats.rows_app;
            s.distinct_transmitters = stats.distinct_transmitters;
            tracing::info!(
                rows = stats.rows,
                csid = stats.rows_csid,
                app = stats.rows_app,
                ftm_paired = paired,
                "exported {}",
                timesync::PARQUET_NAME
            );
        }
        Err(e) => {
            tracing::error!(
                error = %format!("{e:#}"),
                "time-transfer parquet export failed ({} is intact)",
                timesync::NDJSON_NAME
            );
            s.error.get_or_insert_with(|| format!("{e:#}"));
        }
    }

    // `degraded` is the honest grade for a receiver that ran and produced
    // nothing usable: an encrypted SSID, the wrong channel, or a silent
    // illuminator all land here, and the counters distinguish them.
    s.status = if n_rows == 0 {
        "failed"
    } else if s.rows_csid == 0 || s.malformed_log_lines > 0 || s.rx_stamp_source == "mixed" {
        "degraded"
    } else {
        "ok"
    }
    .to_string();
    s
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
        ble: None,
        timesync: None,
        // Session level: the root's own census would restate what the segments
        // already carry, and on an unsegmented session there is no mid-run
        // reader to serve. Left to the segments, which is where it answers a
        // question that could not otherwise be answered before teardown.
        transmitters: None,
    }
}

/// Walk the raw capture to produce the close-time summary.
///
/// `ftm_macs` scopes the second return value: the `(unix_ts_ns, ftm, src_mac)`
/// triples the time-transfer pairing needs. Passing an empty set collects
/// nothing, so a session without `[timesync]` walks the file exactly as it
/// always did and allocates nothing extra.
/// Session-level summary over the whole CSI stream.
///
/// Takes a *list* because a segmented session's stream is spread across
/// `<session_id>-segNNNN/capture.raw` files. They are walked in index order
/// with a single [`csiq::FtmUnwrapper`]: the FTM counter is a free-running
/// hardware value that wraps, so unwrapping it per segment and summing would
/// mis-measure the span at every boundary. One unwrapper across the
/// concatenation is the only correct reading, and it is why segment order is
/// part of the contract rather than an implementation detail.
fn summarize(
    raws: &[PathBuf],
    cfg: &ExperimentConfig,
    counters: &Counters,
    ftm_macs: &std::collections::HashSet<[u8; 6]>,
) -> (SummaryMeta, Vec<timesync::CsiTick>) {
    let mut summary = base_summary(counters);
    let mut ticks: Vec<timesync::CsiTick> = Vec::new();

    let mut tones: Vec<u16> = Vec::new();
    let mut count: u64 = 0;
    let mut first_ftm: Option<u32> = None;
    let mut unwrapper = csiq::FtmUnwrapper::new();
    let mut last_unwrapped: u64 = 0;

    for raw in raws {
        let Ok(file) = std::fs::File::open(raw) else {
            tracing::warn!(path = %raw.display(), "summary: segment unreadable; skipping");
            continue;
        };
        let reader = std::io::BufReader::new(file);
        let mut rr = csiq::raw::RawReader::new(reader, cfg.radio.width.to_csiq());

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

            // A record with no wallclock cannot be matched to a received frame,
            // and one from a transmitter we never stamped can never be needed.
            if rec.unix_ts_ns != 0 && ftm_macs.contains(&rec.src_mac) {
                ticks.push(timesync::CsiTick {
                    unix_ts_ns: rec.unix_ts_ns,
                    ftm: rec.ftm,
                    src_mac: rec.src_mac,
                });
            }
        }
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
    (summary, ticks)
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
