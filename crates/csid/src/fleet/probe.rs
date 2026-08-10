//! The node-local health probe — what `csid fleet status` runs on each Pi.
//!
//! This is a **strict reader**. It opens the session spool, reads the tail of
//! the capture, and computes. It never touches the radio, never writes to the
//! session, and never opens the live socket (which only one consumer can bind,
//! and `csiscope` may already own it). Running it against a live capture is
//! safe by construction, which is the property that lets the operator poll it
//! every few seconds through a staged block.
//!
//! ## Reading a file that is being appended to
//!
//! `capture.raw` is the driver's bytes verbatim, framed as
//! `[be32 msg_len][be32 hdr_len][hdr][be32 csi_len][csi]`. Records are variable
//! length (the CSI payload follows the received frame's tone count), so there
//! is no arithmetic that finds the last N records from the file size.
//!
//! The frames are self-identifying instead: `hdr_len` is the constant 272 on
//! every record this driver emits. So the tail reader seeks back a bounded
//! number of bytes, scans forward for a position where `hdr_len == 272` *and*
//! the frame it implies is followed by another frame with `hdr_len == 272`, and
//! starts there. Two-frame confirmation makes a false sync astronomically
//! unlikely, and a false sync is self-limiting anyway: the frame lengths stop
//! agreeing within a record or two and the scan resumes.
//!
//! A partial trailing record — the writer is mid-`write` — is simply not
//! yielded. It arrives on the next poll.
//!
//! ## Which clock the rate and CV are computed on
//!
//! **`ftm`**, the 320 MHz baseband stamp, unwrapped. The house rule is stated
//! in the README: *analyse on `ftm`, anchor wallclock on `unix_ts_ns`*. The
//! baseband stamp is applied in the RF plane before any host software runs, so
//! it is immune to the host scheduling jitter that afflicts delivery timestamps
//! (measured: p50 19 µs, p95 57 µs, **p99.9 5.4 ms**). Computing an
//! inter-arrival CV on `unix_ts_ns` would therefore measure the Pi's scheduler
//! as much as the channel — and G2 exists to decide whether the *illumination*
//! is paced.
//!
//! `unix_ts_ns` is still read, and is what the wallclock span and the BLE join
//! are anchored on. Where the two disagree, the probe reports both.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::health::{BleHealth, DiskHealth, NodeHealth, NtpState, Scope, ThermalHealth};
use super::stats;
use crate::sidecar::Sidecar;

/// The fixed header length the iax raw stream declares on every record.
const HEADER_LEN: usize = csiq::raw::HEADER_LEN;
/// How much of the capture tail to read by default. At the measured ~440 KB/s
/// ceiling this is about 45 s of the busiest stream csid can produce.
pub const DEFAULT_TAIL_BYTES: u64 = 20 * 1024 * 1024;
/// A capture with no new bytes for this long is not producing records, whatever
/// the sidecar says.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(5);

/// The probe's options, so the same code serves the bench sweep and a manual
/// on-node check.
#[derive(Debug, Clone)]
pub struct ProbeOptions {
    pub spool: PathBuf,
    /// Analysis window, seconds. Records older than this are ignored.
    pub window_s: f64,
    /// Pin the scope instead of taking the window's dominant link.
    pub src_mac: Option<String>,
    pub class: Option<String>,
    /// Explicit session directory; otherwise the newest capturing session.
    pub session: Option<PathBuf>,
    pub tail_bytes: u64,
    pub stale_after: Duration,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        ProbeOptions {
            spool: PathBuf::from("/var/lib/csid"),
            window_s: 30.0,
            src_mac: None,
            class: None,
            session: None,
            tail_bytes: DEFAULT_TAIL_BYTES,
            stale_after: DEFAULT_STALE_AFTER,
        }
    }
}

/// The probe's wire form. `csid fleet status` parses exactly this off stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    pub schema: String,
    pub health: NodeHealth,
    /// The scoped arrival times, seconds on the `ftm` clock, relative to the
    /// window start. Shipped so the cockpit can run G1/G2 itself without a
    /// second round trip — the gate arithmetic lives in one place.
    #[serde(default)]
    pub arrival_s: Vec<f64>,
    /// Wallclock span of the same records, for the clock cross-check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_span_s: Option<f64>,
}

pub const PROBE_SCHEMA: &str = "csid-probe/1";

/// One record's timing and identity — everything the probe needs, nothing else.
#[derive(Debug, Clone, Copy)]
struct Arrival {
    ftm: u32,
    unix_ts_ns: u64,
    ntone: u16,
    phy: Option<csiq::Modulation>,
    src_mac: [u8; 6],
}

fn fmt_mac(m: &[u8; 6]) -> String {
    let mut s = String::with_capacity(17);
    for (i, b) in m.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The record class label, in the same `<ntone>:<phy>` vocabulary `csiscope`
/// uses, so an operator can pin the same class in both tools.
fn class_label(ntone: u16, phy: Option<csiq::Modulation>) -> String {
    let p = match phy {
        None => "unlabelled".to_string(),
        Some(m) => format!("{m:?}").to_lowercase(),
    };
    format!("{ntone}:{p}")
}

/// Read the tail of a raw capture, resyncing on the fixed header length.
///
/// Returns records in file order. A partial trailing record is not yielded.
fn tail_arrivals(path: &Path, tail_bytes: u64, width: csiq::Width) -> Result<Vec<Arrival>> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(tail_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(len - start).read_to_end(&mut buf)?;

    let mut out = Vec::new();
    let mut i = if start == 0 { 0 } else { resync(&buf) };
    while i + 8 <= buf.len() {
        let msg_len = be32(&buf, i) as usize;
        let hdr_len = be32(&buf, i + 4) as usize;
        if hdr_len != HEADER_LEN || msg_len < 8 + hdr_len {
            // Lost the frame: rescan from the next byte.
            let next = resync(&buf[i + 1..]);
            if next == usize::MAX {
                break;
            }
            i = i + 1 + next;
            continue;
        }
        let end = i + 4 + msg_len;
        if end > buf.len() {
            break; // the writer is mid-record; it will be there next poll
        }
        let hdr = &buf[i + 8..i + 8 + hdr_len];
        let csi_off = i + 8 + hdr_len;
        let csi_len = be32(&buf, csi_off) as usize;
        let csi_start = csi_off + 4;
        if csi_start + csi_len <= buf.len() {
            if let Ok(rec) =
                csiq::raw::parse_record(hdr, &buf[csi_start..csi_start + csi_len], width)
            {
                out.push(Arrival {
                    ftm: rec.ftm,
                    unix_ts_ns: rec.unix_ts_ns,
                    ntone: rec.ntone,
                    phy: rec.phy.map(|p| p.modulation),
                    src_mac: rec.src_mac,
                });
            }
        }
        i = end;
    }
    Ok(out)
}

fn be32(b: &[u8], o: usize) -> u32 {
    if o + 4 > b.len() {
        return 0;
    }
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Find the first offset whose frame *and its successor* both declare
/// `hdr_len == 272`. Returns `usize::MAX` when no such offset exists.
fn resync(buf: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        if be32(buf, i + 4) as usize == HEADER_LEN {
            let msg_len = be32(buf, i) as usize;
            let next = i + 4 + msg_len;
            // Either the successor confirms, or this is the last whole frame in
            // the buffer and there is nothing left to confirm against.
            if msg_len >= 8 + HEADER_LEN {
                if next + 8 > buf.len() {
                    return i;
                }
                if be32(buf, next + 4) as usize == HEADER_LEN {
                    return i;
                }
            }
        }
        i += 1;
    }
    usize::MAX
}

/// Seconds on the unwrapped `ftm` clock, relative to the first record.
fn ftm_seconds(arrivals: &[Arrival]) -> Vec<f64> {
    let mut u = csiq::FtmUnwrapper::new();
    let mut base: Option<u64> = None;
    arrivals
        .iter()
        .map(|a| {
            let t = u.push(a.ftm);
            let b = *base.get_or_insert(t);
            csiq::ftm_to_seconds(t.saturating_sub(b))
        })
        .collect()
}

/// A transmitter and the record class it was heard on — the two things every
/// analytical view has to be scoped to before a number from it means anything.
type ScopeKey = ([u8; 6], u16, Option<csiq::Modulation>);

/// Pick the (src_mac, class) pair that dominates the window.
///
/// G1 and G2 are both scoped to one source MAC and one record class. On an
/// ambient channel the unscoped rate is a sum over interleaved transmitters and
/// PHY types and is not a measurement of any link — so the probe scopes, and
/// reports what it scoped to, rather than reporting a flattering total.
fn dominant_scope(arrivals: &[Arrival]) -> Option<ScopeKey> {
    let mut tally: Vec<(ScopeKey, u64)> = Vec::new();
    for a in arrivals {
        let key = (a.src_mac, a.ntone, a.phy);
        match tally.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => tally.push((key, 1)),
        }
    }
    tally
        .into_iter()
        .max_by(|a, b| {
            a.1.cmp(&b.1).then_with(|| {
                // Stable tie-break so a 50/50 channel does not oscillate the
                // scope between two polls.
                class_label(b.0 .1, b.0 .2).cmp(&class_label(a.0 .1, a.0 .2))
            })
        })
        .map(|(k, _)| k)
}

/// The newest session directory, preferring one whose sidecar says `capturing`.
pub fn newest_session(spool: &Path) -> Option<(PathBuf, Sidecar)> {
    let mut candidates: Vec<(std::time::SystemTime, PathBuf, Sidecar)> = Vec::new();
    for entry in std::fs::read_dir(spool).ok()? {
        let Ok(entry) = entry else { continue };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let meta_path = dir.join("metadata.json");
        let Ok(text) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(sc) = serde_json::from_str::<Sidecar>(&text) else {
            continue;
        };
        let mtime = meta_path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((mtime, dir, sc));
    }
    // A capturing session always wins, however old its sidecar: the sidecar is
    // written once at open and not touched again until close.
    candidates.sort_by(|a, b| {
        let live = |s: &Sidecar| matches!(s.status, crate::sidecar::Status::Capturing);
        live(&a.2).cmp(&live(&b.2)).then_with(|| a.0.cmp(&b.0))
    });
    candidates.pop().map(|(_, d, s)| (d, s))
}

/// Tail the BLE durable log for the window's observation rate.
fn ble_health(dir: &Path, sidecar: &Sidecar, window_s: f64, now_ns: u64) -> Option<BleHealth> {
    let meta = sidecar.ble.as_ref()?;
    let log = dir.join(&meta.durable_log);
    // A closed session's sidecar already carries the authoritative summary.
    if let Some(s) = sidecar.summary.as_ref().and_then(|s| s.ble.as_ref()) {
        if !matches!(sidecar.status, crate::sidecar::Status::Capturing) {
            return Some(BleHealth {
                status: s.status.clone(),
                observations: s.observations,
                rate_hz: s.mean_rate_hz,
                max_gap_s: s.max_gap_s,
                scan_restarts: s.scan_restarts,
                unparsed_events: s.unparsed_events,
            });
        }
    }

    let Ok(f) = File::open(&log) else {
        return Some(BleHealth {
            status: "failed".into(),
            observations: 0,
            rate_hz: 0.0,
            max_gap_s: 0.0,
            scan_restarts: 0,
            unparsed_events: 0,
        });
    };
    let cutoff = now_ns.saturating_sub((window_s * 1e9) as u64);
    let mut stamps: Vec<u64> = Vec::new();
    for line in BufReader::new(f).lines().map_while(|l| l.ok()) {
        if let Ok(o) = serde_json::from_str::<crate::ble::Observation>(&line) {
            if o.unix_ts_ns >= cutoff {
                stamps.push(o.unix_ts_ns);
            }
        }
    }
    let (rate, max_gap) = if stamps.len() >= 2 {
        let span = (stamps[stamps.len() - 1].saturating_sub(stamps[0])) as f64 / 1e9;
        let mut max_gap = 0.0f64;
        for w in stamps.windows(2) {
            max_gap = max_gap.max(w[1].saturating_sub(w[0]) as f64 / 1e9);
        }
        (
            if span > 0.0 {
                stamps.len() as f64 / span
            } else {
                0.0
            },
            max_gap,
        )
    } else {
        (0.0, window_s)
    };
    Some(BleHealth {
        status: if stamps.is_empty() {
            "failed".into()
        } else {
            "ok".into()
        },
        observations: stamps.len() as u64,
        rate_hz: rate,
        max_gap_s: max_gap,
        scan_restarts: 0,
        unparsed_events: 0,
    })
}

/// Free space on the spool filesystem, and how long it lasts at the observed
/// byte rate.
#[cfg(unix)]
pub fn disk_health(spool: &Path, bytes_per_s: Option<f64>) -> Option<DiskHealth> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(spool.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bs = if st.f_frsize > 0 {
        st.f_frsize as f64
    } else {
        st.f_bsize as f64
    };
    let free = st.f_bavail as f64 * bs;
    let total = st.f_blocks as f64 * bs;
    Some(DiskHealth {
        free_gb: free / 1e9,
        total_gb: total / 1e9,
        hours_left: bytes_per_s.filter(|r| *r > 0.0).map(|r| free / r / 3600.0),
    })
}

#[cfg(not(unix))]
pub fn disk_health(_spool: &Path, _bytes_per_s: Option<f64>) -> Option<DiskHealth> {
    None
}

/// The node's own thermal state, decoded once in [`crate::thermal`].
pub fn thermal_health() -> Option<ThermalHealth> {
    let temp = crate::thermal::read_temp_c();
    let throttle = crate::thermal::read_throttle();
    if temp.is_none() && throttle.is_none() {
        return None;
    }
    Some(ThermalHealth {
        temp_c: temp,
        headroom_c: temp.map(|c| crate::thermal::SOFT_LIMIT_C - c),
        throttled_now: throttle.map(|t| t.degraded_now()).unwrap_or(false),
        throttled_since_boot: throttle.map(|t| t.degraded_since_boot()).unwrap_or(false),
        detail: throttle
            .map(|t| t.describe())
            .unwrap_or_else(|| "throttle word unavailable".into()),
    })
}

/// Read the node's time-daemon state. `chronyd` first (it publishes an error
/// bound), `timedatectl` as the fallback (it publishes only yes/no).
pub fn ntp_state() -> Option<NtpState> {
    if let Some(out) = crate::util::run_opt("chronyc", &["-c", "tracking"]) {
        if let Some(s) = super::clock::parse_chrony_tracking(&out) {
            return Some(s);
        }
    }
    crate::util::run_opt("timedatectl", &["show", "-p", "NTPSynchronized", "--value"])
        .as_deref()
        .and_then(super::clock::parse_timedatectl)
}

/// Run the probe against this node's spool.
///
/// Never fails on a missing measurement: an absent sensor, an absent BLE log or
/// an absent session become `None` on the report, and the cockpit renders them
/// as unknown. The only errors are ones that mean the probe itself is
/// misconfigured.
pub fn probe(opts: &ProbeOptions) -> Result<ProbeReport> {
    let host = crate::util::run_opt("hostname", &[]).unwrap_or_else(|| "unknown".into());
    let now_ns = crate::util::now_unix_ns();
    // The sidecar's hostname is the one the capture is filed under (and the one
    // `[node] hostname` pins), so it wins over the system hostname on a node
    // mid-rename or on a dev box reading someone else's spool.

    let mut health = NodeHealth {
        host: host.clone(),
        unreachable: None,
        session_id: None,
        experiment: None,
        capture_alive: false,
        scope: None,
        delivered_hz: None,
        interarrival_cv: None,
        ble: None,
        disk: None,
        thermal: thermal_health(),
        clock: None,
        notes: Vec::new(),
    };

    let found = match &opts.session {
        Some(dir) => std::fs::read_to_string(dir.join("metadata.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Sidecar>(&t).ok())
            .map(|sc| (dir.clone(), sc)),
        None => newest_session(&opts.spool),
    };

    let Some((dir, sidecar)) = found else {
        health.disk = disk_health(&opts.spool, None);
        return Ok(ProbeReport {
            schema: PROBE_SCHEMA.into(),
            health,
            arrival_s: Vec::new(),
            wall_span_s: None,
        });
    };

    health.session_id = Some(sidecar.session_id.clone());
    health.experiment = Some(sidecar.experiment.clone());
    if let Some(h) = &sidecar.environment.hostname {
        health.host = h.clone();
    }

    let raw = dir.join("capture.raw");
    let raw_meta = raw.metadata().ok();
    let raw_len = raw_meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let since_write = raw_meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok());

    // "Alive" is a property of the bytes, not of the sidecar. A starving
    // capture pings the systemd watchdog exactly like a healthy one.
    health.capture_alive = matches!(sidecar.status, crate::sidecar::Status::Capturing)
        && since_write.is_some_and(|d| d <= opts.stale_after);

    let width = csiq::Width::from_code(width_code(&sidecar.radio.width));
    let arrivals = tail_arrivals(&raw, opts.tail_bytes, width).unwrap_or_default();

    // Trim to the window on the wallclock, which is the clock the operator's
    // "--window 30s" means.
    let cutoff = now_ns.saturating_sub((opts.window_s * 1e9) as u64);
    let windowed: Vec<Arrival> = arrivals
        .iter()
        .copied()
        .filter(|a| a.unix_ts_ns == 0 || a.unix_ts_ns >= cutoff)
        .collect();

    let scope_key = match (&opts.src_mac, &opts.class) {
        (None, None) => dominant_scope(&windowed),
        _ => windowed
            .iter()
            .map(|a| (a.src_mac, a.ntone, a.phy))
            .find(|(mac, ntone, phy)| {
                opts.src_mac
                    .as_deref()
                    .is_none_or(|m| fmt_mac(mac).eq_ignore_ascii_case(m))
                    && opts
                        .class
                        .as_deref()
                        .is_none_or(|c| class_label(*ntone, *phy) == c)
            }),
    };

    if let Some((mac, ntone, phy)) = scope_key {
        let scoped: Vec<Arrival> = windowed
            .iter()
            .copied()
            .filter(|a| a.src_mac == mac && a.ntone == ntone && a.phy == phy)
            .collect();

        health.scope = Some(Scope {
            class: class_label(ntone, phy),
            src_mac: fmt_mac(&mac),
            scoped_records: scoped.len() as u64,
            window_records: windowed.len() as u64,
        });

        let times = ftm_seconds(&scoped);
        let bins = stats::per_second_bins(&times);
        health.delivered_hz =
            stats::bootstrap_mean(&bins, stats::BOOTSTRAP_B, stats::BOOTSTRAP_SEED);
        let (gaps, _) = stats::gaps_s(&times);
        health.interarrival_cv =
            stats::bootstrap_cv(&gaps, stats::BOOTSTRAP_B, stats::BOOTSTRAP_SEED);

        let wall_span_s = (scoped.len() >= 2)
            .then(|| {
                let a = scoped.first().unwrap().unix_ts_ns;
                let b = scoped.last().unwrap().unix_ts_ns;
                (b.saturating_sub(a)) as f64 / 1e9
            })
            .filter(|s| *s > 0.0);

        let bytes_per_s = (opts.window_s > 0.0 && raw_len > 0)
            .then(|| windowed.len() as f64 * 300.0 / opts.window_s);
        health.disk = disk_health(&opts.spool, bytes_per_s);
        health.ble = ble_health(&dir, &sidecar, opts.window_s, now_ns);

        return Ok(ProbeReport {
            schema: PROBE_SCHEMA.into(),
            health,
            arrival_s: times,
            wall_span_s,
        });
    }

    health.disk = disk_health(&opts.spool, None);
    health.ble = ble_health(&dir, &sidecar, opts.window_s, now_ns);
    Ok(ProbeReport {
        schema: PROBE_SCHEMA.into(),
        health,
        arrival_s: Vec::new(),
        wall_span_s: None,
    })
}

/// Map the sidecar's `iw` width token back to a [`csiq::Width`] code.
fn width_code(token: &str) -> u16 {
    match token {
        "NOHT" => 0,
        "HT20" => 1,
        "HT40-" => 2,
        "HT40+" => 3,
        "80MHz" => 4,
        "160MHz" => 5,
        "320MHz" => 6,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw frame the way `DurableSink` writes one.
    fn frame(ftm: u32, unix_ts_ns: u64, ntone: u16, mac: [u8; 6]) -> Vec<u8> {
        let mut hdr = vec![0u8; HEADER_LEN];
        hdr[8..12].copy_from_slice(&ftm.to_le_bytes());
        hdr[46] = 1; // nrx
        hdr[47] = 1; // ntx
        hdr[52..54].copy_from_slice(&ntone.to_le_bytes());
        hdr[68..74].copy_from_slice(&mac);
        hdr[208..216].copy_from_slice(&unix_ts_ns.to_le_bytes());
        // ntone tones x (nrx=1 * ntx=1) chains x 2 int16 (I and Q) x 2 bytes.
        let csi = vec![0u8; ntone as usize * 2 * 2];

        let msg_len = (4 + hdr.len() + 4 + csi.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&msg_len.to_be_bytes());
        out.extend_from_slice(&(hdr.len() as u32).to_be_bytes());
        out.extend_from_slice(&hdr);
        out.extend_from_slice(&(csi.len() as u32).to_be_bytes());
        out.extend_from_slice(&csi);
        out
    }

    fn write_capture(dir: &Path, frames: &[Vec<u8>]) -> PathBuf {
        let p = dir.join("capture.raw");
        let mut bytes = Vec::new();
        for f in frames {
            bytes.extend_from_slice(f);
        }
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "csid-probe-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_tail_reader_recovers_records_from_a_mid_file_start() {
        let dir = tmpdir("tail");
        let mac = [0xef, 0xbe, 0xad, 0xde, 0xad, 0xde];
        let frames: Vec<Vec<u8>> = (0..40)
            .map(|i| {
                frame(
                    1000 + i * 3_200_000,
                    1_786_000_000_000_000_000 + i as u64 * 10_000_000,
                    52,
                    mac,
                )
            })
            .collect();
        let p = write_capture(&dir, &frames);

        // Read the whole file: every record comes back.
        let all = tail_arrivals(&p, 10 * 1024 * 1024, csiq::Width::Ht20).unwrap();
        assert_eq!(all.len(), 40);
        assert_eq!(all[0].ntone, 52);
        assert_eq!(all[0].src_mac, mac);

        // Read a tail that starts inside a record: resync must find the next
        // frame boundary and lose only the record it landed in.
        let one = frames[0].len() as u64;
        let tail = tail_arrivals(&p, one * 5 + 17, csiq::Width::Ht20).unwrap();
        assert!(
            (4..=5).contains(&tail.len()),
            "expected 4-5 recovered records, got {}",
            tail.len()
        );
        // And what it recovered is the *end* of the stream, not the start.
        assert_eq!(tail.last().unwrap().ftm, all.last().unwrap().ftm);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The writer is mid-`write`. The partial record must not be yielded, and
    /// the complete ones before it must be.
    #[test]
    fn a_partially_written_trailing_record_is_skipped_not_misparsed() {
        let dir = tmpdir("partial");
        let mac = [1, 2, 3, 4, 5, 6];
        let mut frames: Vec<Vec<u8>> = (0..10)
            .map(|i| frame(1000 + i * 3_200_000, 1_786_000_000_000_000_000, 52, mac))
            .collect();
        let mut truncated = frames.pop().unwrap();
        truncated.truncate(truncated.len() / 2);
        frames.push(truncated);
        let p = write_capture(&dir, &frames);

        let out = tail_arrivals(&p, 10 * 1024 * 1024, csiq::Width::Ht20).unwrap();
        assert_eq!(out.len(), 9, "the half-written record is not a record");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The scope is the point: an ambient channel carries several transmitters
    /// and PHY types at once, and a rate summed across them is a measurement of
    /// nothing.
    #[test]
    fn the_scope_picks_the_dominant_transmitter_and_class_not_the_total() {
        let sentinel = [0xef, 0xbe, 0xad, 0xde, 0xad, 0xde];
        let noise = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let mut arrivals = Vec::new();
        for i in 0..300 {
            arrivals.push(Arrival {
                ftm: i,
                unix_ts_ns: 0,
                ntone: 52,
                phy: Some(csiq::Modulation::LegacyOfdm),
                src_mac: sentinel,
            });
        }
        for i in 0..90 {
            arrivals.push(Arrival {
                ftm: i,
                unix_ts_ns: 0,
                ntone: 56,
                phy: Some(csiq::Modulation::Ht),
                src_mac: noise,
            });
        }
        let (mac, ntone, phy) = dominant_scope(&arrivals).unwrap();
        assert_eq!(mac, sentinel);
        assert_eq!(ntone, 52);
        assert_eq!(class_label(ntone, phy), "52:legacyofdm");
        assert_eq!(dominant_scope(&[]), None);
    }

    /// A 50/50 channel must not flip the scope between two consecutive polls —
    /// the operator would see the rate jump between two different links.
    #[test]
    fn a_tied_scope_resolves_the_same_way_whichever_order_records_arrived() {
        let a = [1u8, 1, 1, 1, 1, 1];
        let b = [2u8, 2, 2, 2, 2, 2];
        let mk = |mac: [u8; 6], ntone: u16, phy| Arrival {
            ftm: 0,
            unix_ts_ns: 0,
            ntone,
            phy,
            src_mac: mac,
        };
        let mut fwd = Vec::new();
        let mut rev = Vec::new();
        for _ in 0..50 {
            fwd.push(mk(a, 52, Some(csiq::Modulation::LegacyOfdm)));
            fwd.push(mk(b, 56, Some(csiq::Modulation::Ht)));
            rev.push(mk(b, 56, Some(csiq::Modulation::Ht)));
            rev.push(mk(a, 52, Some(csiq::Modulation::LegacyOfdm)));
        }
        assert_eq!(dominant_scope(&fwd), dominant_scope(&rev));
    }

    #[test]
    fn ftm_seconds_are_relative_and_monotone_across_a_wrap() {
        // The 320 MHz counter wraps every ~13.4 s; the unwrapper must carry.
        let mut arrivals = Vec::new();
        let step = 3_200_000u32; // 10 ms
        let mut ftm = u32::MAX - step * 3;
        for _ in 0..8 {
            arrivals.push(Arrival {
                ftm,
                unix_ts_ns: 0,
                ntone: 52,
                phy: None,
                src_mac: [0; 6],
            });
            ftm = ftm.wrapping_add(step);
        }
        let t = ftm_seconds(&arrivals);
        assert_eq!(t[0], 0.0);
        for w in t.windows(2) {
            let d = w[1] - w[0];
            assert!(
                (d - 0.01).abs() < 1e-6,
                "10 ms steps must survive the wrap, got {d}"
            );
        }
    }

    #[test]
    fn a_class_label_matches_the_console_vocabulary() {
        assert_eq!(
            class_label(52, Some(csiq::Modulation::LegacyOfdm)),
            "52:legacyofdm"
        );
        assert_eq!(class_label(56, Some(csiq::Modulation::Ht)), "56:ht");
        assert_eq!(class_label(242, Some(csiq::Modulation::He)), "242:he");
        assert_eq!(class_label(242, None), "242:unlabelled");
    }

    #[test]
    fn macs_render_the_way_the_config_spells_them() {
        assert_eq!(
            fmt_mac(&[0xef, 0xbe, 0xad, 0xde, 0xad, 0xde]),
            "ef:be:ad:de:ad:de"
        );
    }

    #[test]
    fn width_tokens_round_trip_through_the_sidecar_spelling() {
        for (token, code) in [
            ("HT20", 1),
            ("80MHz", 4),
            ("160MHz", 5),
            ("HT40+", 3),
            ("NOHT", 0),
        ] {
            assert_eq!(width_code(token), code, "{token}");
            assert_eq!(csiq::Width::from_code(code).as_str(), token);
        }
        // An unknown token falls back to HT20 rather than panicking a probe.
        assert_eq!(width_code("nonsense"), 1);
    }

    /// The end-to-end shape: a spool with one capturing session produces a
    /// report with a scope, a rate interval and a CV interval.
    #[test]
    fn probing_a_live_looking_session_produces_intervals_not_bare_numbers() {
        let spool = tmpdir("probe-e2e");
        let dir = spool.join("monad04_lab-anchor_20260810-101500");
        std::fs::create_dir_all(&dir).unwrap();

        let now = crate::util::now_unix_ns();
        let mac = [0xef, 0xbe, 0xad, 0xde, 0xad, 0xde];
        // 20 s of a clean 120 Hz stream, ending now.
        let n = 2400usize;
        let frames: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let dt_ns = ((n - i) as u64) * 8_333_333;
                frame((i as u32).wrapping_mul(2_666_666), now - dt_ns, 52, mac)
            })
            .collect();
        write_capture(&dir, &frames);

        let sidecar = serde_json::json!({
            "schema": "csid-session/1",
            "session_id": "monad04_lab-anchor_20260810-101500",
            "experiment": "lab-anchor",
            "tag": null,
            "radio": {
                "interface": "wlp1s0", "monitor": "wlp1s0mon0", "band": "2.4",
                "channel": 11, "control_freq_mhz": 2462, "center_freq_mhz": null,
                "width": "HT20", "interval_us": 0, "mac_filter": []
            },
            "environment": { "csid_version": "0.1.0" },
            "started_at": "2026-08-10T10:15:00Z",
            "ended_at": null,
            "status": "capturing",
            "summary": null
        });
        std::fs::write(
            dir.join("metadata.json"),
            serde_json::to_string_pretty(&sidecar).unwrap(),
        )
        .unwrap();

        let report = probe(&ProbeOptions {
            spool: spool.clone(),
            window_s: 30.0,
            ..ProbeOptions::default()
        })
        .unwrap();

        assert_eq!(
            report.health.session_id.as_deref(),
            Some("monad04_lab-anchor_20260810-101500")
        );
        let scope = report.health.scope.expect("a scope must be reported");
        assert_eq!(scope.src_mac, "ef:be:ad:de:ad:de");
        assert_eq!(scope.class, "52:unlabelled");
        assert_eq!(scope.scoped_records, n as u64);

        let rate = report.health.delivered_hz.expect("a rate interval");
        assert!(rate.lo < rate.point && rate.point < rate.hi, "{rate:?}");
        assert!(
            (rate.point - 120.0).abs() < 5.0,
            "expected ~120 Hz, got {rate:?}"
        );
        let cv = report.health.interarrival_cv.expect("a CV interval");
        assert!(cv.hi < 0.5, "a paced stream must be regular: {cv:?}");
        assert_eq!(report.arrival_s.len(), n);

        std::fs::remove_dir_all(&spool).ok();
    }

    /// An empty spool is UNKNOWN, not a healthy node with a 0 Hz rate.
    #[test]
    fn a_node_with_no_session_reports_no_session_rather_than_zero() {
        let spool = tmpdir("probe-empty");
        let report = probe(&ProbeOptions {
            spool: spool.clone(),
            ..ProbeOptions::default()
        })
        .unwrap();
        assert!(report.health.session_id.is_none());
        assert!(!report.health.capture_alive);
        assert!(report.health.delivered_hz.is_none());
        let mut h = report.health;
        assert_eq!(
            h.grade(&super::super::health::Budgets::default()),
            super::super::health::State::Unknown
        );
        std::fs::remove_dir_all(&spool).ok();
    }
}
