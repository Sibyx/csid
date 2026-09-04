//! The time-transfer receive thread: `AF_PACKET` on the monitor interface.
//!
//! Same split as [`crate::inject`]: everything decidable is in
//! [`super::payload`] and unit-tested on a laptop; only the syscalls live here.
//!
//! ## Three details that would each silently ruin the measurement
//!
//! 1. **`PACKET_OUTGOING` must be dropped.** `AF_PACKET` loops locally
//!    transmitted frames back to *other* `AF_PACKET` sockets, and the injector
//!    is one of those transmitters on this very interface. Without the filter,
//!    an illuminating node "receives" its own frames with a delay of
//!    microseconds while its peers see real air delay — and the inter-node skew
//!    would read as a few hundred microseconds of pure artefact.
//!
//! 2. **`SO_TIMESTAMPNS` where the kernel offers it.** A stamp taken after
//!    `recvmsg` returns carries the scheduler's wake-up jitter, which is the
//!    same order as the quantity being measured (`collectord` learned this the
//!    hard way; its docs say kernel timestamps are "not optional"). The kernel
//!    stamp is taken in the receive path instead. Where the option cannot be
//!    set the userspace stamp is used and **recorded as such per row**, because
//!    a userspace-stamped session must not be pooled with kernel-stamped ones.
//!
//! 3. **This thread must never apply backpressure.** It owns its own socket and
//!    its own file. It shares nothing with the CSI receive path but the stop
//!    flag, so it cannot slow it down, and a failure here is recorded rather
//!    than propagated.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

use super::{TimesyncCounters, TimesyncHandle};
use crate::config::TimesyncConfig;

/// Start the receiver. See [`super::spawn`].
pub fn spawn(
    dir: &Path,
    monitor: &str,
    cfg: &TimesyncConfig,
    stop: Arc<AtomicBool>,
    counters: Arc<TimesyncCounters>,
) -> Result<TimesyncHandle> {
    imp::spawn(dir, monitor, cfg, stop, counters)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use super::super::{payload, Row, RowLog, StampSource, TimesyncCounters, TimesyncHandle};
    use crate::config::TimesyncConfig;
    use crate::util;

    use crate::rawsock::{RxSocket, PACKET_OUTGOING};

    /// Big enough for any 802.11 MPDU plus radiotap.
    const FRAME_BUF: usize = 4096;

    pub fn spawn(
        dir: &Path,
        monitor: &str,
        cfg: &TimesyncConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<TimesyncCounters>,
    ) -> Result<TimesyncHandle> {
        // Open on the caller's thread so a missing interface or capability
        // fails the session at setup rather than silently mid-run.
        let sock = RxSocket::open(monitor, "time transfer")?;
        if !sock.kernel_stamps {
            tracing::warn!(
                monitor,
                "SO_TIMESTAMPNS refused; time-transfer rows will carry USERSPACE stamps \
                 (scheduler jitter included). They are marked as such and must not be pooled \
                 with kernel-stamped sessions."
            );
        }
        let log = RowLog::create(dir, cfg.flush_every)?;
        let ndjson = dir.join(super::super::NDJSON_NAME);
        let monitor = monitor.to_string();

        let thread = std::thread::Builder::new()
            .name("csid-timesync".into())
            .spawn(move || run_loop(sock, log, &monitor, stop, counters))
            .context("spawning the time-transfer thread")?;
        Ok(TimesyncHandle::new(thread, ndjson))
    }

    fn run_loop(
        sock: RxSocket,
        mut log: RowLog,
        monitor: &str,
        stop: Arc<AtomicBool>,
        counters: Arc<TimesyncCounters>,
    ) {
        let stamp_src = if sock.kernel_stamps {
            StampSource::Kernel
        } else {
            StampSource::Userspace
        };
        tracing::info!(
            monitor,
            kernel_stamps = sock.kernel_stamps,
            "time transfer running"
        );

        let mut buf = vec![0u8; FRAME_BUF];
        let mut last_log = Instant::now();
        let mut last_rows: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            match sock.recv(&mut buf) {
                Ok(None) => {} // read timeout: loop and re-check the stop flag
                Err(e) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, "time-transfer read failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(Some((frame, pkttype, kernel_ns))) => {
                    // Our own injector, looped back. Counting it as a receipt
                    // would fabricate a sub-millisecond skew against every peer.
                    if pkttype == PACKET_OUTGOING {
                        counters.own_transmissions.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    counters.frames_seen.fetch_add(1, Ordering::Relaxed);

                    // Prefer the kernel's stamp; fall back to now.
                    let unix_ts_ns = kernel_ns.unwrap_or_else(util::now_unix_ns);
                    match payload::recognise(frame) {
                        Ok(stamp) => {
                            let row = Row::from_stamp(stamp, unix_ts_ns, stamp_src);
                            counters.note_row(row.tx_kind, unix_ts_ns);
                            if let Err(e) = log.append(&row) {
                                counters.errors.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(error = %e, "time-transfer log append failed");
                            }
                        }
                        Err(r) => counters.note_reject(r),
                    }
                }
            }

            if last_log.elapsed() >= Duration::from_secs(30) {
                let (rows, _) = counters.snapshot();
                let window = last_log.elapsed().as_secs_f64();
                tracing::info!(
                    rows,
                    rate_hz = (rows - last_rows) as f64 / window,
                    frames_seen = counters.frames_seen.load(Ordering::Relaxed),
                    protected = counters.protected.load(Ordering::Relaxed),
                    own_transmissions = counters.own_transmissions.load(Ordering::Relaxed),
                    "time transfer"
                );
                last_rows = rows;
                last_log = Instant::now();
            }
        }

        if let Err(e) = log.finish() {
            tracing::error!(error = %e, "closing the time-transfer log failed");
        }
        let (rows, rate) = counters.snapshot();
        tracing::info!(rows, rate_hz = rate, "time transfer stopped");
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use anyhow::Result;

    use super::super::{TimesyncCounters, TimesyncHandle};
    use crate::config::TimesyncConfig;

    pub fn spawn(
        _dir: &Path,
        _monitor: &str,
        _cfg: &TimesyncConfig,
        _stop: Arc<AtomicBool>,
        _counters: Arc<TimesyncCounters>,
    ) -> Result<TimesyncHandle> {
        anyhow::bail!(
            "[timesync].enabled requires Linux (AF_PACKET on the monitor interface); \
             this build is for development only"
        )
    }
}
