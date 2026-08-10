//! The injector: paced monitor-mode frame transmission (`capture.mode = "inject"`).
//!
//! Replaces the hostapd soft-AP as the illumination source. The soft-AP path
//! was measured on the office pair (monad01→monad02, 2026-07-27) delivering
//! multicast in beacon-aligned bunches — inter-arrival p50 15.3 ms against a
//! commanded 40 ms, CV 2.6, 42% of frames <5 ms apart — because the AP queue,
//! not the pacing loop, owns the release schedule. Injection writes each frame
//! straight into the monitor interface, so the only jitter left between the
//! pacing loop and the air is CSMA itself. This is the published injector
//! pattern for CSI campaigns (OPERAnet's "injector mode"; AX-CSI's constant-
//! rate dummy traffic).
//!
//! Semantics:
//!
//! - Frames are 802.11 data frames sent to a (default broadcast, therefore
//!   unACKed) destination, from a configurable source MAC — the **sentinel**
//!   the receiving side's analysis keys on. The payload carries a sequence
//!   number and a TX wallclock stamp so delivery loss and latency are
//!   measurable offline from the receiver's capture alone.
//! - Pacing is absolute-deadline: each slot is `t0 + n·period`. A missed slot
//!   (scheduler stall, ENOBUFS backoff) is **skipped, never bunched** — the
//!   whole point of the injector is that frames do not arrive in bursts.
//! - The legacy OFDM bitrate is requested via the radiotap RATE field, so
//!   receivers see the 52-tone `legacy_ofdm` record class on both bands (the
//!   band-contrast invariant). The driver-side `monitor_tx_rate` debugfs knob
//!   is deliberately not driven here until its format is verified on hardware.
//!
//! Injection and capture are not exclusive: the radio is in monitor mode
//! either way, and the driver does not report CSI for locally transmitted
//! frames, so an inject session records ambient + peer traffic exactly like a
//! passive one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;

use crate::config::InjectConfig;

/// Injection counters, shared with the supervising loop for status lines.
#[derive(Debug, Default)]
pub struct InjectCounters {
    /// Frames handed to the kernel.
    pub sent: AtomicU64,
    /// Send failures (ENOBUFS/EAGAIN — carrier busy or queue full).
    pub errors: AtomicU64,
    /// Pacing slots skipped because the deadline had already passed.
    pub skipped: AtomicU64,
}

impl InjectCounters {
    /// Convenience snapshot: `(sent, errors, skipped)`.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.sent.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.skipped.load(Ordering::Relaxed),
        )
    }
}

/// Parse `aa:bb:cc:dd:ee:ff` into bytes. Validation has already run, so this
/// only fails on programmer error.
pub fn parse_mac(s: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    for (i, part) in s.split(':').take(6).enumerate() {
        out[i] = u8::from_str_radix(part, 16).unwrap_or(0);
    }
    out
}

/// Build one injectable frame: radiotap header + 802.11 data header + payload.
///
/// Radiotap carries exactly one field, RATE (bit 2), in 500 kbps units —
/// mac80211 honours it for legacy-rate injection. Everything else is left to
/// the driver. The 802.11 header is a plain data frame (ToDS=FromDS=0) with
/// `addr3 = addr2` (the sentinel doubles as the BSSID — there is no BSS).
///
/// Payload layout after the 24-byte header:
/// `b"CSID" | u64 LE seq | u64 LE tx unix ns | zero padding`.
pub fn build_frame(cfg: &InjectConfig, seq: u64, tx_unix_ns: u64) -> Vec<u8> {
    const RADIOTAP_LEN: usize = 9; // 8 fixed + 1 rate byte
    const HDR80211_LEN: usize = 24;

    let src = parse_mac(&cfg.src_mac);
    let dst = parse_mac(&cfg.dst_mac);
    let mpdu_len = cfg.frame_bytes.max(HDR80211_LEN + 20);
    let mut f = Vec::with_capacity(RADIOTAP_LEN + mpdu_len);

    // -- radiotap ---------------------------------------------------------
    f.push(0); // it_version
    f.push(0); // it_pad
    f.extend_from_slice(&(RADIOTAP_LEN as u16).to_le_bytes()); // it_len
    f.extend_from_slice(&(1u32 << 2).to_le_bytes()); // it_present: RATE
    f.push((cfg.bitrate_mbps * 2) as u8); // rate, 500 kbps units

    // -- 802.11 data header ----------------------------------------------
    f.extend_from_slice(&[0x08, 0x00]); // FC: type data, subtype data
    f.extend_from_slice(&[0x00, 0x00]); // duration
    f.extend_from_slice(&dst); // addr1: receiver
    f.extend_from_slice(&src); // addr2: the sentinel
    f.extend_from_slice(&src); // addr3: BSSID (= sentinel; no BSS exists)
    f.extend_from_slice(&(((seq as u16) & 0x0fff) << 4).to_le_bytes()); // seq ctrl

    // -- payload ----------------------------------------------------------
    f.extend_from_slice(b"CSID");
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend_from_slice(&tx_unix_ns.to_le_bytes());
    f.resize(RADIOTAP_LEN + mpdu_len, 0);
    f
}

/// Spawn the injector thread on `monitor` (already up and tuned).
///
/// Returns immediately; the thread runs until `stop` is set and reports its
/// totals through `counters`. Send failures are counted, logged at a low duty
/// cycle, and never abort the session — a busy carrier is data, not an error.
pub fn spawn(
    monitor: &str,
    cfg: &InjectConfig,
    stop: Arc<AtomicBool>,
    counters: Arc<InjectCounters>,
) -> Result<JoinHandle<()>> {
    imp::spawn(monitor, cfg, stop, counters)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use super::{build_frame, InjectCounters};
    use crate::config::InjectConfig;
    use crate::util;

    /// AF_PACKET raw socket bound to one interface.
    struct TxSocket {
        fd: libc::c_int,
    }

    impl TxSocket {
        fn open(iface: &str) -> Result<Self> {
            let ifindex = {
                let name = std::ffi::CString::new(iface).context("interface name")?;
                let idx = unsafe { libc::if_nametoindex(name.as_ptr()) };
                if idx == 0 {
                    anyhow::bail!(
                        "monitor interface {iface} not found: {}",
                        std::io::Error::last_os_error()
                    );
                }
                idx
            };

            let fd = unsafe {
                libc::socket(
                    libc::AF_PACKET,
                    libc::SOCK_RAW,
                    (libc::ETH_P_ALL as u16).to_be() as libc::c_int,
                )
            };
            if fd < 0 {
                anyhow::bail!(
                    "opening AF_PACKET socket: {} (CAP_NET_RAW required)",
                    std::io::Error::last_os_error()
                );
            }

            let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            addr.sll_family = libc::AF_PACKET as u16;
            addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
            addr.sll_ifindex = ifindex as libc::c_int;
            let rc = unsafe {
                libc::bind(
                    fd,
                    &addr as *const libc::sockaddr_ll as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                anyhow::bail!("binding AF_PACKET socket to {iface}: {e}");
            }
            Ok(TxSocket { fd })
        }

        fn send(&self, frame: &[u8]) -> std::io::Result<()> {
            let n = unsafe {
                libc::send(
                    self.fd,
                    frame.as_ptr() as *const libc::c_void,
                    frame.len(),
                    0,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for TxSocket {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }

    pub fn spawn(
        monitor: &str,
        cfg: &InjectConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<InjectCounters>,
    ) -> Result<JoinHandle<()>> {
        // Open the socket on the caller's thread so a missing interface or
        // capability fails the session at setup, not silently mid-run.
        let sock = TxSocket::open(monitor)?;

        // Force the FW TX rate for injected monitor frames. The radiotap RATE
        // field is honoured on 5 GHz but ignored for broadcast injection on
        // 2.4 GHz (mac80211 falls back to 1 Mbps DSSS → no OFDM CSI at the
        // receiver). The `monitor_tx_rate` debugfs knob overrides the FW rate
        // on both bands. Set on the caller's thread so a bad value or a
        // read-only debugfs fails the session at setup rather than silently
        // capturing at the wrong rate. Zero = leave the driver's default.
        if cfg.monitor_tx_rate != 0 {
            let knobs = crate::debugfs::Knobs::for_interface(monitor)
                .context("resolving iwlmvm debugfs for monitor_tx_rate")?;
            knobs
                .set(
                    crate::debugfs::knob::MONITOR_TX_RATE,
                    &format!("0x{:x}", cfg.monitor_tx_rate),
                )
                .context("writing monitor_tx_rate knob")?;
            tracing::info!(
                monitor_tx_rate = format!("0x{:x}", cfg.monitor_tx_rate),
                "forced FW injection rate (bypasses mac80211 multicast-rate fallback)"
            );
        }

        let cfg = cfg.clone();
        let monitor = monitor.to_string();

        let handle = std::thread::Builder::new()
            .name("csid-inject".into())
            .spawn(move || run_loop(sock, &monitor, &cfg, stop, counters))
            .context("spawning injector thread")?;
        Ok(handle)
    }

    fn run_loop(
        sock: TxSocket,
        monitor: &str,
        cfg: &InjectConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<InjectCounters>,
    ) {
        let period = Duration::from_secs_f64(1.0 / cfg.rate_hz as f64);
        let t0 = Instant::now();
        let mut next = t0;
        let mut seq: u64 = 0;
        let mut last_log = t0;
        let mut last_sent: u64 = 0;

        tracing::info!(
            monitor,
            rate_hz = cfg.rate_hz,
            frame_bytes = cfg.frame_bytes,
            src_mac = %cfg.src_mac,
            dst_mac = %cfg.dst_mac,
            bitrate_mbps = cfg.bitrate_mbps,
            "injector running"
        );

        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if next > now {
                // Bounded sleep so the stop flag is observed promptly even at
                // very low rates.
                std::thread::sleep((next - now).min(Duration::from_millis(100)));
                continue;
            }

            let frame = build_frame(cfg, seq, util::now_unix_ns());
            match sock.send(&frame) {
                Ok(()) => {
                    counters.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    // ENOBUFS/EAGAIN = carrier busy or driver queue full.
                    // Count it, skip the slot, keep pacing.
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, seq, "inject send failed (slot skipped)");
                }
            }
            seq += 1;
            next += period;

            // Deadline already passed (stall, send backlog): skip the missed
            // slots instead of bursting to catch up.
            let now = Instant::now();
            if next < now {
                let missed = (now - next).as_nanos() / period.as_nanos().max(1);
                if missed > 0 {
                    counters.skipped.fetch_add(missed as u64, Ordering::Relaxed);
                    next += period * missed as u32;
                }
            }

            if last_log.elapsed() >= Duration::from_secs(10) {
                let (sent, errors, skipped) = counters.snapshot();
                let window = last_log.elapsed().as_secs_f64();
                let rate = (sent - last_sent) as f64 / window;
                tracing::info!(sent, errors, skipped, rate_hz = rate, "injecting");
                last_sent = sent;
                last_log = Instant::now();
            }
        }

        // Clear the forced FW rate so a later passive capture on this radio is
        // not silently pinned to the injection rate. Best-effort: teardown must
        // not fail a completed session.
        if cfg.monitor_tx_rate != 0 {
            if let Ok(knobs) = crate::debugfs::Knobs::for_interface(monitor) {
                knobs.set_best_effort(crate::debugfs::knob::MONITOR_TX_RATE, "0x0");
            }
        }

        let (sent, errors, skipped) = counters.snapshot();
        tracing::info!(sent, errors, skipped, "injector stopped");
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use anyhow::Result;

    use super::InjectCounters;
    use crate::config::InjectConfig;

    pub fn spawn(
        _monitor: &str,
        _cfg: &InjectConfig,
        _stop: Arc<AtomicBool>,
        _counters: Arc<InjectCounters>,
    ) -> Result<JoinHandle<()>> {
        anyhow::bail!("capture.mode = \"inject\" requires Linux (AF_PACKET injection)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InjectConfig;

    fn cfg() -> InjectConfig {
        InjectConfig::default()
    }

    #[test]
    fn frame_layout_is_stable() {
        let f = build_frame(&cfg(), 7, 123_456_789);
        // radiotap: version 0, len 9, present = RATE only.
        assert_eq!(f[0], 0);
        assert_eq!(u16::from_le_bytes([f[2], f[3]]), 9);
        assert_eq!(u32::from_le_bytes([f[4], f[5], f[6], f[7]]), 1 << 2);
        assert_eq!(f[8], 12); // 6 Mbps in 500 kbps units
                              // 802.11: data frame to broadcast from the sentinel.
        assert_eq!(&f[9..11], &[0x08, 0x00]);
        assert_eq!(&f[13..19], &[0xff; 6]);
        assert_eq!(&f[19..25], &parse_mac("ef:be:ad:de:ad:de"));
        // seq control carries the low 12 bits of seq, shifted.
        assert_eq!(u16::from_le_bytes([f[31], f[32]]), 7 << 4);
        // payload magic + seq + stamp.
        assert_eq!(&f[33..37], b"CSID");
        assert_eq!(u64::from_le_bytes(f[37..45].try_into().unwrap()), 7);
        assert_eq!(
            u64::from_le_bytes(f[45..53].try_into().unwrap()),
            123_456_789
        );
        // total = radiotap + configured MPDU size.
        assert_eq!(f.len(), 9 + cfg().frame_bytes);
    }

    #[test]
    fn tiny_frame_bytes_still_fits_header_and_stamp() {
        let mut c = cfg();
        c.frame_bytes = 10; // below the 24 B header — must be clamped, not panic
        let f = build_frame(&c, 0, 0);
        assert!(f.len() >= 9 + 44);
    }
}
