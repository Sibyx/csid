//! Frame census — **who is on the air**, read from the raw 802.11 header for
//! the whole session, beside the CSI capture.
//!
//! ## Why the CSI record's own address is not enough
//!
//! The 272-byte CSI header carries a source MAC at offset 68, and the
//! **firmware** writes it. When the firmware has no identity for a frame it
//! writes the fill `ef:be:ad:de:ad:de`. On this fleet, csid 0.2.0 sessions
//! show the eduroam BSSIDs in that field for the first ten to twelve minutes
//! after open and the fill for every AP frame after that, on every host
//! checked (2026-08-30 to 2026-09-01), while the frame rate holds. So a census
//! read from `src_mac` names nobody after the first minutes of a session, and
//! two published readings rested on it before anyone noticed.
//!
//! The driver has the real transmitter address on a side channel
//! (`flq_mvm_record` records `addr2` of each received MPDU), and its author
//! left the copy into the CSI header disabled because the CSI completion is
//! asynchronous to the MPDU path and the two are not frame-aligned. So the
//! identity cannot be repaired in the driver either. It has to be read from
//! the frame itself, in user space, which is what this module does.
//!
//! ## What it records
//!
//! One `AF_PACKET` socket on the monitor interface (the same socket type
//! [`crate::timesync`] uses, shared through [`crate::rawsock`]), one thread,
//! one file. Every received frame is classified by type and subtype, its
//! transmitter address (`addr2`, or the TA of a control frame that carries
//! one) and, where the frame has one, its BSSID. Frames are counted per
//! **minute** and per `(transmitter, bssid, kind, subtype, sounding)` and each
//! minute's rows are appended to `frame_census.jsonl` as it closes. Nothing
//! per frame is kept, so the log is small on any channel.
//!
//! Three subtypes carry a second label, `sounding`, because they are the
//! frames that make beamforming-feedback sensing possible without CSI access:
//!
//! | Frame | Label |
//! |---|---|
//! | Management action, category 21 (VHT), action 0 | `vht_bfi` — VHT compressed beamforming report |
//! | Management action, category 30 (HE), action 0 | `he_bfi` — HE compressed beamforming / CQI report |
//! | Control subtype 5 | `ndpa` — NDP announcement, the sounding trigger |
//!
//! A channel with `ndpa` and no `*_bfi` is one where the APs sound and no
//! client answers in the clear. A channel with neither is one where nobody
//! beamforms, and a BFI-sensing arm on it measures nothing.
//!
//! ## What it does not do
//!
//! It never decodes a payload, never keeps a frame, and never touches the CSI
//! hot path. It shares only the stop flag with the capture. A failure here is
//! recorded in the sidecar and the CSI capture continues.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::util::format_mac;

/// Crash-safe durable log, one JSON object per line, one line per
/// `(minute, transmitter, bssid, kind, subtype, sounding)` bucket.
pub const NDJSON_NAME: &str = "frame_census.jsonl";
/// Schema identifier, mirrored into the sidecar. Bump on any column change.
pub const SCHEMA: &str = "frame-census/1";

/// 802.11 frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Mgmt,
    Ctrl,
    Data,
    Ext,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Mgmt => "mgmt",
            Kind::Ctrl => "ctrl",
            Kind::Data => "data",
            Kind::Ext => "ext",
        }
    }
}

/// The sounding role of a frame, where it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sounding {
    /// VHT compressed beamforming report (action category 21, action 0).
    VhtBfi,
    /// HE compressed beamforming / CQI report (action category 30, action 0).
    HeBfi,
    /// NDP announcement (control subtype 5) — the sounding trigger.
    Ndpa,
}

impl Sounding {
    pub fn as_str(self) -> &'static str {
        match self {
            Sounding::VhtBfi => "vht_bfi",
            Sounding::HeBfi => "he_bfi",
            Sounding::Ndpa => "ndpa",
        }
    }
}

/// One frame, classified. Nothing of the payload survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub kind: Kind,
    pub subtype: u8,
    /// `addr2`, or the TA of a control frame that carries one. `None` for
    /// ACK, CTS and control wrappers, which name only their receiver.
    pub ta: Option<[u8; 6]>,
    /// The BSSID this frame belongs to, where the header says. Data frames
    /// with both DS bits set (WDS) and control frames carry none.
    pub bssid: Option<[u8; 6]>,
    pub sounding: Option<Sounding>,
    /// The Protected Frame bit. Recorded, never acted on.
    pub protected: bool,
}

/// Why a frame could not be classified. Both are counted; a high count of
/// either says the socket is not where it was meant to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Not a radiotap-prefixed frame — the socket is not on a monitor interface.
    NotRadiotap,
    /// Truncated below what its type requires.
    Short,
}

fn mac_at(b: &[u8], off: usize) -> Option<[u8; 6]> {
    b.get(off..off + 6).map(|s| {
        let mut m = [0u8; 6];
        m.copy_from_slice(s);
        m
    })
}

/// Classify one radiotap-prefixed 802.11 frame (pure; no I/O).
pub fn classify(frame: &[u8]) -> Result<Classified, Reject> {
    if frame.len() < 8 || frame[0] != 0 {
        return Err(Reject::NotRadiotap);
    }
    let rt_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if !(8..=1024).contains(&rt_len) || rt_len > frame.len() {
        return Err(Reject::NotRadiotap);
    }
    let mpdu = &frame[rt_len..];
    if mpdu.len() < 10 {
        return Err(Reject::Short);
    }
    let (fc0, fc1) = (mpdu[0], mpdu[1]);
    let ftype = (fc0 >> 2) & 0x03;
    let subtype = (fc0 >> 4) & 0x0F;
    let protected = fc1 & 0x40 != 0;

    match ftype {
        // -- control --------------------------------------------------------
        1 => {
            // CTS (12), ACK (13) and the control wrapper (7) name only a
            // receiver. Every other control subtype carries a TA at addr2.
            let ta = match subtype {
                7 | 12 | 13 => None,
                _ => {
                    if mpdu.len() < 16 {
                        return Err(Reject::Short);
                    }
                    mac_at(mpdu, 10)
                }
            };
            Ok(Classified {
                kind: Kind::Ctrl,
                subtype,
                ta,
                bssid: None,
                sounding: (subtype == 5).then_some(Sounding::Ndpa),
                protected: false,
            })
        }
        // -- management -----------------------------------------------------
        0 => {
            if mpdu.len() < 24 {
                return Err(Reject::Short);
            }
            let ta = mac_at(mpdu, 10);
            let bssid = mac_at(mpdu, 16);
            // Action (13) and Action No Ack (14). The body follows the 24-byte
            // header, plus HT Control when the Order bit is set. A protected
            // action frame carries a CCMP header first, and its category is
            // ciphertext — beamforming reports are never protected (VHT and HE
            // categories are not robust action frames), so a protected one is
            // simply not one of ours to label.
            let sounding = if matches!(subtype, 13 | 14) && !protected {
                let body_off = 24 + if fc1 & 0x80 != 0 { 4 } else { 0 };
                match (mpdu.get(body_off), mpdu.get(body_off + 1)) {
                    (Some(21), Some(0)) => Some(Sounding::VhtBfi),
                    (Some(30), Some(0)) => Some(Sounding::HeBfi),
                    _ => None,
                }
            } else {
                None
            };
            Ok(Classified {
                kind: Kind::Mgmt,
                subtype,
                ta,
                bssid,
                sounding,
                protected,
            })
        }
        // -- data -----------------------------------------------------------
        2 => {
            if mpdu.len() < 24 {
                return Err(Reject::Short);
            }
            let ta = mac_at(mpdu, 10);
            // Which address is the BSSID depends on the DS bits.
            let bssid = match fc1 & 0x03 {
                0x00 => mac_at(mpdu, 16), // addr3
                0x01 => mac_at(mpdu, 4),  // ToDS: addr1
                0x02 => mac_at(mpdu, 10), // FromDS: addr2
                _ => None,                // WDS
            };
            Ok(Classified {
                kind: Kind::Data,
                subtype,
                ta,
                bssid,
                sounding: None,
                protected,
            })
        }
        _ => Ok(Classified {
            kind: Kind::Ext,
            subtype,
            ta: None,
            bssid: None,
            sounding: None,
            protected,
        }),
    }
}

// -- aggregation ---------------------------------------------------------------

/// One bucket of the per-minute census.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub ta: Option<[u8; 6]>,
    pub bssid: Option<[u8; 6]>,
    pub kind: Kind,
    pub subtype: u8,
    pub sounding: Option<Sounding>,
}

/// One line of `frame_census.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Start of the minute, Unix seconds, on the receiver's wallclock.
    pub minute_unix: u64,
    pub host: String,
    pub session_id: String,
    /// Transmitter address, or `null` for a frame that names none.
    pub ta: Option<String>,
    pub bssid: Option<String>,
    pub kind: Kind,
    pub subtype: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sounding: Option<Sounding>,
    pub frames: u64,
    /// Of `frames`, how many carried the Protected bit.
    pub protected: u64,
}

/// The census of one minute, in memory until the minute closes.
#[derive(Debug, Default)]
pub struct Minute {
    pub minute_unix: u64,
    pub buckets: HashMap<BucketKey, (u64, u64)>,
}

impl Minute {
    pub fn new(minute_unix: u64) -> Self {
        Minute {
            minute_unix,
            buckets: HashMap::new(),
        }
    }

    pub fn note(&mut self, c: &Classified) {
        let key = BucketKey {
            ta: c.ta,
            bssid: c.bssid,
            kind: c.kind,
            subtype: c.subtype,
            sounding: c.sounding,
        };
        let e = self.buckets.entry(key).or_insert((0, 0));
        e.0 += 1;
        if c.protected {
            e.1 += 1;
        }
    }

    /// Render the minute as rows, busiest bucket first so a reader who only
    /// looks at the head of the file sees what mattered.
    pub fn rows(&self, host: &str, session_id: &str) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .buckets
            .iter()
            .map(|(k, (frames, protected))| Row {
                minute_unix: self.minute_unix,
                host: host.to_string(),
                session_id: session_id.to_string(),
                ta: k.ta.as_ref().map(format_mac),
                bssid: k.bssid.as_ref().map(format_mac),
                kind: k.kind,
                subtype: k.subtype,
                sounding: k.sounding,
                frames: *frames,
                protected: *protected,
            })
            .collect();
        rows.sort_by(|a, b| b.frames.cmp(&a.frames).then_with(|| a.ta.cmp(&b.ta)));
        rows
    }
}

/// Session-level totals, kept beside the minutes so the sidecar can name the
/// busiest transmitters without re-reading the log.
#[derive(Debug, Default)]
pub struct Totals {
    /// Frames per transmitter, capped at `max_transmitters` entries.
    pub per_ta: HashMap<[u8; 6], u64>,
    /// Beacons per BSSID — the access points, by their own announcement.
    pub beacons: HashMap<[u8; 6], u64>,
    pub capped: bool,
    max_transmitters: usize,
}

impl Totals {
    pub fn new(max_transmitters: usize) -> Self {
        Totals {
            max_transmitters: max_transmitters.max(1),
            ..Default::default()
        }
    }

    pub fn note(&mut self, c: &Classified) {
        if let Some(ta) = c.ta {
            if let Some(n) = self.per_ta.get_mut(&ta) {
                *n += 1;
            } else if self.per_ta.len() < self.max_transmitters {
                self.per_ta.insert(ta, 1);
            } else {
                self.capped = true;
            }
            // A beacon's addr2 IS the BSSID, so counting it under `ta` is
            // counting the AP radio.
            if c.kind == Kind::Mgmt && c.subtype == 8 {
                *self.beacons.entry(ta).or_insert(0) += 1;
            }
        }
    }

    fn top(map: &HashMap<[u8; 6], u64>, n: usize) -> Vec<crate::sidecar::TransmitterCount> {
        let mut v: Vec<(&[u8; 6], &u64)> = map.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        v.into_iter()
            .take(n)
            .map(|(mac, records)| crate::sidecar::TransmitterCount {
                mac: format_mac(mac),
                records: *records,
            })
            .collect()
    }

    pub fn top_transmitters(&self, n: usize) -> Vec<crate::sidecar::TransmitterCount> {
        Self::top(&self.per_ta, n)
    }

    pub fn top_beacons(&self, n: usize) -> Vec<crate::sidecar::TransmitterCount> {
        Self::top(&self.beacons, n)
    }
}

// -- counters -----------------------------------------------------------------

/// Liveness and diagnosis counters for the census thread.
#[derive(Debug, Default)]
pub struct CensusCounters {
    pub frames_seen: AtomicU64,
    pub own_transmissions: AtomicU64,
    pub mgmt: AtomicU64,
    pub ctrl: AtomicU64,
    pub data: AtomicU64,
    pub ext: AtomicU64,
    pub protected: AtomicU64,
    pub vht_bfi: AtomicU64,
    pub he_bfi: AtomicU64,
    pub ndpa: AtomicU64,
    /// Not radiotap, or truncated.
    pub malformed: AtomicU64,
    pub errors: AtomicU64,
    pub rows_written: AtomicU64,
}

impl CensusCounters {
    pub fn note(&self, c: &Classified) {
        match c.kind {
            Kind::Mgmt => &self.mgmt,
            Kind::Ctrl => &self.ctrl,
            Kind::Data => &self.data,
            Kind::Ext => &self.ext,
        }
        .fetch_add(1, Ordering::Relaxed);
        if c.protected {
            self.protected.fetch_add(1, Ordering::Relaxed);
        }
        match c.sounding {
            Some(Sounding::VhtBfi) => self.vht_bfi.fetch_add(1, Ordering::Relaxed),
            Some(Sounding::HeBfi) => self.he_bfi.fetch_add(1, Ordering::Relaxed),
            Some(Sounding::Ndpa) => self.ndpa.fetch_add(1, Ordering::Relaxed),
            None => 0,
        };
    }
}

// -- durable log --------------------------------------------------------------

/// Append-only NDJSON writer, flushed at every minute close.
pub struct CensusLog {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl CensusLog {
    pub fn create(dir: &Path) -> Result<Self> {
        let path = dir.join(NDJSON_NAME);
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(CensusLog {
            writer: BufWriter::with_capacity(64 * 1024, file),
            path,
        })
    }

    pub fn append_minute(&mut self, rows: &[Row]) -> std::io::Result<()> {
        for row in rows {
            serde_json::to_writer(&mut self.writer, row)?;
            self.writer.write_all(b"\n")?;
        }
        self.writer.flush()
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        self.writer.flush().context("flushing the frame census")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("fsyncing the frame census")?;
        Ok(self.path)
    }
}

// -- outcome ------------------------------------------------------------------

/// What the census thread hands back at close.
#[derive(Debug, Default)]
pub struct CensusOutcome {
    pub distinct_transmitters: u64,
    pub distinct_capped: bool,
    pub top: Vec<crate::sidecar::TransmitterCount>,
    pub beacons: Vec<crate::sidecar::TransmitterCount>,
    pub error: Option<String>,
}

/// Handle to the running census thread.
pub struct CensusHandle {
    thread: JoinHandle<CensusOutcome>,
}

impl CensusHandle {
    pub fn new(thread: JoinHandle<CensusOutcome>) -> Self {
        CensusHandle { thread }
    }

    /// Join the thread. A panic becomes an error in the outcome, never a
    /// propagated one — the CSI capture is worth more than this artefact.
    pub fn join(self) -> CensusOutcome {
        match self.thread.join() {
            Ok(o) => o,
            Err(_) => CensusOutcome {
                error: Some("census thread panicked".to_string()),
                ..Default::default()
            },
        }
    }
}

/// Start the census thread. See the module docs.
pub fn spawn(
    dir: &Path,
    monitor: &str,
    host: &str,
    session_id: &str,
    cfg: &crate::config::CensusConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    counters: std::sync::Arc<CensusCounters>,
) -> Result<CensusHandle> {
    imp::spawn(dir, monitor, host, session_id, cfg, stop, counters)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use super::{classify, CensusCounters, CensusHandle, CensusLog, CensusOutcome, Minute, Totals};
    use crate::config::CensusConfig;
    use crate::rawsock::{RxSocket, PACKET_OUTGOING};
    use crate::util;

    const FRAME_BUF: usize = 4096;

    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        dir: &Path,
        monitor: &str,
        host: &str,
        session_id: &str,
        cfg: &CensusConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<CensusCounters>,
    ) -> Result<CensusHandle> {
        // Open on the caller's thread so a missing interface or capability
        // fails at setup, where the sidecar records it, rather than mid-run.
        let sock = RxSocket::open(monitor, "the frame census")?;
        let log = CensusLog::create(dir)?;
        let monitor = monitor.to_string();
        let host = host.to_string();
        let session_id = session_id.to_string();
        let cfg = cfg.clone();

        let thread = std::thread::Builder::new()
            .name("csid-census".into())
            .spawn(move || {
                run_loop(
                    sock,
                    log,
                    &monitor,
                    &host,
                    &session_id,
                    &cfg,
                    stop,
                    counters,
                )
            })
            .context("spawning the frame-census thread")?;
        Ok(CensusHandle::new(thread))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_loop(
        sock: RxSocket,
        mut log: CensusLog,
        monitor: &str,
        host: &str,
        session_id: &str,
        cfg: &CensusConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<CensusCounters>,
    ) -> CensusOutcome {
        tracing::info!(monitor, "frame census running");
        let mut buf = vec![0u8; FRAME_BUF];
        let mut totals = Totals::new(cfg.max_transmitters);
        let mut minute = Minute::new(util::now_unix() / 60 * 60);
        let mut last_log = Instant::now();
        let mut error: Option<String> = None;

        while !stop.load(Ordering::Relaxed) {
            match sock.recv(&mut buf) {
                Ok(None) => {}
                Err(e) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, "frame census read failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(Some((frame, pkttype, kernel_ns))) => {
                    if pkttype == PACKET_OUTGOING {
                        counters.own_transmissions.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    counters.frames_seen.fetch_add(1, Ordering::Relaxed);
                    let now_s = kernel_ns
                        .map(|ns| ns / 1_000_000_000)
                        .unwrap_or_else(util::now_unix);
                    match classify(frame) {
                        Ok(c) => {
                            counters.note(&c);
                            totals.note(&c);
                            minute.note(&c);
                        }
                        Err(_) => {
                            counters.malformed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let this_minute = now_s / 60 * 60;
                    if this_minute != minute.minute_unix {
                        flush_minute(&mut log, &minute, host, session_id, &counters, &mut error);
                        minute = Minute::new(this_minute);
                    }
                }
            }

            // A silent channel must still close its minutes on the clock, or
            // the last quiet hour lands in one bucket at teardown.
            let wall_minute = util::now_unix() / 60 * 60;
            if wall_minute > minute.minute_unix {
                flush_minute(&mut log, &minute, host, session_id, &counters, &mut error);
                minute = Minute::new(wall_minute);
            }

            if last_log.elapsed() >= Duration::from_secs(60) {
                tracing::info!(
                    frames_seen = counters.frames_seen.load(Ordering::Relaxed),
                    transmitters = totals.per_ta.len(),
                    beacons_from = totals.beacons.len(),
                    vht_bfi = counters.vht_bfi.load(Ordering::Relaxed),
                    he_bfi = counters.he_bfi.load(Ordering::Relaxed),
                    ndpa = counters.ndpa.load(Ordering::Relaxed),
                    "frame census"
                );
                last_log = Instant::now();
            }
        }

        flush_minute(&mut log, &minute, host, session_id, &counters, &mut error);
        if let Err(e) = log.finish() {
            error = Some(format!("closing the frame census failed: {e:#}"));
        }
        tracing::info!(
            frames_seen = counters.frames_seen.load(Ordering::Relaxed),
            transmitters = totals.per_ta.len(),
            "frame census stopped"
        );
        CensusOutcome {
            distinct_transmitters: totals.per_ta.len() as u64,
            distinct_capped: totals.capped,
            top: totals.top_transmitters(cfg.top_n),
            beacons: totals.top_beacons(cfg.top_n),
            error,
        }
    }

    fn flush_minute(
        log: &mut CensusLog,
        minute: &Minute,
        host: &str,
        session_id: &str,
        counters: &CensusCounters,
        error: &mut Option<String>,
    ) {
        if minute.buckets.is_empty() {
            return;
        }
        let rows = minute.rows(host, session_id);
        match log.append_minute(&rows) {
            Ok(()) => {
                counters
                    .rows_written
                    .fetch_add(rows.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                if error.is_none() {
                    *error = Some(format!("frame census append failed: {e}"));
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use anyhow::Result;

    use super::{CensusCounters, CensusHandle};
    use crate::config::CensusConfig;

    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        _dir: &Path,
        _monitor: &str,
        _host: &str,
        _session_id: &str,
        _cfg: &CensusConfig,
        _stop: Arc<AtomicBool>,
        _counters: Arc<CensusCounters>,
    ) -> Result<CensusHandle> {
        anyhow::bail!(
            "[census].enabled requires Linux (AF_PACKET on the monitor interface); \
             this build is for development only"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AP: [u8; 6] = [0x54, 0xd7, 0xe3, 0x2e, 0xa6, 0x91];
    const STA: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

    fn radiotap() -> Vec<u8> {
        let mut f = vec![0u8, 0];
        f.extend_from_slice(&9u16.to_le_bytes());
        f.extend_from_slice(&(1u32 << 2).to_le_bytes());
        f.push(12);
        f
    }

    fn mgmt(subtype: u8, fc1: u8, ta: [u8; 6], bssid: [u8; 6], body: &[u8]) -> Vec<u8> {
        let mut f = radiotap();
        f.push(subtype << 4); // type 0
        f.push(fc1);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&[0xff; 6]);
        f.extend_from_slice(&ta);
        f.extend_from_slice(&bssid);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(body);
        f
    }

    fn data(fc1: u8, a1: [u8; 6], a2: [u8; 6], a3: [u8; 6]) -> Vec<u8> {
        let mut f = radiotap();
        f.push(0x08 | (8 << 4)); // QoS data
        f.push(fc1);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&a1);
        f.extend_from_slice(&a2);
        f.extend_from_slice(&a3);
        f.extend_from_slice(&[0, 0, 0, 0]);
        f
    }

    fn ctrl(subtype: u8, ra: [u8; 6], ta: Option<[u8; 6]>) -> Vec<u8> {
        let mut f = radiotap();
        f.push(0x04 | (subtype << 4));
        f.push(0);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&ra);
        if let Some(t) = ta {
            f.extend_from_slice(&t);
        }
        f
    }

    #[test]
    fn a_beacon_names_its_bssid_as_transmitter() {
        let c = classify(&mgmt(8, 0, AP, AP, &[0; 12])).unwrap();
        assert_eq!(c.kind, Kind::Mgmt);
        assert_eq!(c.subtype, 8);
        assert_eq!(c.ta, Some(AP));
        assert_eq!(c.bssid, Some(AP));
        assert_eq!(c.sounding, None);
    }

    #[test]
    fn a_vht_beamforming_report_is_labelled() {
        let c = classify(&mgmt(13, 0, STA, AP, &[21, 0, 0xaa])).unwrap();
        assert_eq!(c.sounding, Some(Sounding::VhtBfi));
        let c = classify(&mgmt(13, 0, STA, AP, &[30, 0, 0xaa])).unwrap();
        assert_eq!(c.sounding, Some(Sounding::HeBfi));
        // Any other action category is an ordinary action frame.
        let c = classify(&mgmt(13, 0, STA, AP, &[3, 0, 0xaa])).unwrap();
        assert_eq!(c.sounding, None);
    }

    #[test]
    fn a_protected_action_frame_is_never_labelled() {
        // 0x40 = Protected Frame. The category byte is ciphertext.
        let c = classify(&mgmt(13, 0x40, STA, AP, &[21, 0, 0xaa])).unwrap();
        assert_eq!(c.sounding, None);
        assert!(c.protected);
    }

    #[test]
    fn ht_control_moves_the_action_body() {
        // Order bit (0x80) on fc1: four bytes of HT Control before the body.
        let c = classify(&mgmt(13, 0x80, STA, AP, &[0, 0, 0, 0, 21, 0])).unwrap();
        assert_eq!(c.sounding, Some(Sounding::VhtBfi));
    }

    #[test]
    fn an_ndp_announcement_is_the_sounding_trigger() {
        let c = classify(&ctrl(5, STA, Some(AP))).unwrap();
        assert_eq!(c.kind, Kind::Ctrl);
        assert_eq!(c.ta, Some(AP));
        assert_eq!(c.sounding, Some(Sounding::Ndpa));
    }

    #[test]
    fn an_ack_names_nobody() {
        let c = classify(&ctrl(13, AP, None)).unwrap();
        assert_eq!(c.ta, None);
        assert_eq!(c.bssid, None);
    }

    #[test]
    fn data_frames_pick_the_bssid_by_ds_bits() {
        // Neither DS bit: addr3 is the BSSID.
        let c = classify(&data(0x00, STA, AP, AP)).unwrap();
        assert_eq!((c.ta, c.bssid), (Some(AP), Some(AP)));
        // ToDS: addr1 is the BSSID, addr2 the client.
        let c = classify(&data(0x01, AP, STA, [1; 6])).unwrap();
        assert_eq!((c.ta, c.bssid), (Some(STA), Some(AP)));
        // FromDS: addr2 is the BSSID.
        let c = classify(&data(0x02, STA, AP, [1; 6])).unwrap();
        assert_eq!((c.ta, c.bssid), (Some(AP), Some(AP)));
        // WDS: no BSSID.
        let c = classify(&data(0x03, STA, AP, [1; 6])).unwrap();
        assert_eq!(c.bssid, None);
        // Protected data is counted, not decoded.
        let c = classify(&data(0x40, STA, AP, AP)).unwrap();
        assert!(c.protected);
    }

    #[test]
    fn non_radiotap_and_short_frames_are_rejected() {
        assert_eq!(classify(&[1, 2, 3]), Err(Reject::NotRadiotap));
        let mut f = radiotap();
        f.extend_from_slice(&[0x80, 0, 0, 0]); // a beacon with no addresses
        assert_eq!(classify(&f), Err(Reject::Short));
    }

    #[test]
    fn a_minute_aggregates_by_bucket_and_sorts_busiest_first() {
        let mut m = Minute::new(60);
        for _ in 0..3 {
            m.note(&classify(&mgmt(8, 0, AP, AP, &[0; 12])).unwrap());
        }
        m.note(&classify(&data(0x40, STA, AP, AP)).unwrap());
        let rows = m.rows("monad07", "s");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].frames, 3);
        assert_eq!(rows[0].ta.as_deref(), Some("54:d7:e3:2e:a6:91"));
        assert_eq!(rows[1].protected, 1);
        assert_eq!(rows[1].ta.as_deref(), Some("54:d7:e3:2e:a6:91"));
        assert_eq!(rows[1].kind, Kind::Data);
        // Round-trips through the schema it declares.
        let line = serde_json::to_string(&rows[0]).unwrap();
        let back: Row = serde_json::from_str(&line).unwrap();
        assert_eq!(back, rows[0]);
    }

    #[test]
    fn totals_cap_the_transmitter_table_and_say_so() {
        let mut t = Totals::new(2);
        for i in 0..5u8 {
            let mut ta = STA;
            ta[5] = i;
            t.note(&classify(&data(0x00, AP, ta, AP)).unwrap());
        }
        assert_eq!(t.per_ta.len(), 2);
        assert!(t.capped);
    }

    #[test]
    fn beacons_are_counted_per_bssid() {
        let mut t = Totals::new(16);
        t.note(&classify(&mgmt(8, 0, AP, AP, &[0; 12])).unwrap());
        t.note(&classify(&mgmt(8, 0, AP, AP, &[0; 12])).unwrap());
        t.note(&classify(&mgmt(4, 0, STA, [0xff; 6], &[0; 4])).unwrap()); // probe request
        let b = t.top_beacons(8);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].records, 2);
        assert_eq!(t.top_transmitters(8).len(), 2);
    }
}
