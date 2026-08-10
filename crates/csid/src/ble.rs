//! Fleet-side BLE co-capture: `ble_rssi.parquet` alongside `capture.csiq`.
//!
//! Why this lives in `csid` at all (IP-106 R5): the BLE-anchored recalibration
//! arm needs a BLE observation stream that shares a **clock** with the CSI
//! stream, not merely a wall-clock that was set by the same NTP server. Running
//! the scanner in the same process, on the same node, stamping with the same
//! [`crate::util::now_unix_ns`] the RX thread stamps CSI records with, makes the
//! two streams joinable by construction — there is no offset to estimate and no
//! second device to keep in sync. A separate BLE collector would have
//! reintroduced exactly the sync error the protocol budget cannot afford
//! (≤ one 6 s analysis window).
//!
//! ## What is recorded
//!
//! One row per received advertisement — not per device, not per second. The
//! scanner runs with duplicate filtering **off**, so a device that advertises at
//! 10 Hz contributes 10 rows a second and its RSSI series is a real time series.
//!
//! ## What is deliberately *not* recorded
//!
//! The Bluetooth device address, in any recoverable form. The project's posture
//! is count-without-identify, so an address is turned into a per-session
//! pseudonym the moment it is parsed:
//!
//! ```text
//! device_hash = hex( SHA-256( salt_32B ‖ addr_type ‖ addr_6B )[..hash_bytes] )
//! ```
//!
//! The salt is 32 bytes from the OS CSPRNG, generated at session open, held only
//! in memory, and dropped at session close. It is never written to the sidecar,
//! the log, or the parquet. Consequences, stated plainly:
//!
//! - **Stable within a session** — the same address yields the same pseudonym
//!   for the whole capture, so per-device RSSI series and device counts work.
//! - **Unlinkable across sessions** — the same phone in two sessions gets two
//!   unrelated pseudonyms. Cross-session device tracking is not possible, by
//!   construction rather than by policy.
//! - **Not reversible in practice** — the address space is only 2⁴⁸, so a
//!   pseudonym *without the salt* is still a 2²⁵⁶ search; with the salt it would
//!   be trivially enumerable, which is precisely why the salt never leaves RAM.
//!
//! ## Rotating addresses
//!
//! Modern phones advertise with Resolvable Private Addresses that rotate every
//! ~15 minutes, so "distinct pseudonyms" is an **upper bound** on devices, not a
//! device count. Rather than hide that, every row carries [`AddrKind`], derived
//! from the address type and the two most-significant address bits, so analysis
//! can separate the stable-identity population (`public`, `random_static`) from
//! the rotation-inflated one (`rpa_resolvable`, `rpa_non_resolvable`). The
//! reader-side counter reports both.
//!
//! ## Durability
//!
//! The scanner appends NDJSON to `ble_scan.jsonl` as it goes — crash-safe, the
//! same reasoning that makes `capture.raw` the durable CSI artefact. At session
//! close that log is streamed into `ble_rssi.parquet` (the contract artefact the
//! analysis side consumes) in row groups; the log is kept, exactly as
//! `capture.raw` is kept beside `capture.csiq`.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use parquet::basic::{Compression, LogicalType, Repetition, Type as PhysicalType};
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::BleConfig;

/// Crash-safe durable log, written as the scan runs.
pub const NDJSON_NAME: &str = "ble_scan.jsonl";
/// The contract artefact the analysis side consumes.
pub const PARQUET_NAME: &str = "ble_rssi.parquet";
/// Schema identifier, mirrored into the sidecar. Bump on any column change.
pub const PARQUET_SCHEMA: &str = "ble-rssi/1";
/// Rows per parquet row group. Small enough that a long session never has to
/// be held in memory, large enough that dictionary encoding still pays.
const ROW_GROUP_ROWS: usize = 65_536;
/// HCI's "RSSI not available" sentinel (Core spec: valid range −127…+20 dBm).
const RSSI_UNAVAILABLE: i8 = 127;

// -- observation model --------------------------------------------------------

/// Address class, derived from the HCI address type plus the two
/// most-significant address bits (Core spec vol 6, part B, §1.3).
///
/// This is the column that keeps a device count honest: `Public` and
/// `RandomStatic` identify one device for the whole session, while the two
/// `Rpa*` kinds rotate, so counting them as devices inflates the population.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddrKind {
    /// Registered public (OUI-bearing) address — stable, often an appliance.
    Public,
    /// Static random address — stable for the lifetime of the power cycle.
    RandomStatic,
    /// Resolvable private address — rotates (~15 min on iOS/Android).
    RpaResolvable,
    /// Non-resolvable private address — rotates, not resolvable by anyone.
    RpaNonResolvable,
    /// Random address with the reserved bit pattern.
    RandomReserved,
    /// Controller-resolved identity address (privacy + resolving list).
    PublicIdentity,
    /// Controller-resolved random identity address.
    RandomIdentity,
    /// Address type this build does not know.
    Unknown,
}

impl AddrKind {
    /// Classify from the HCI address type and the address bytes as delivered
    /// (little-endian, so the most-significant octet is `addr[5]`).
    pub fn classify(addr_type: u8, addr: &[u8; 6]) -> Self {
        match addr_type {
            0x00 => AddrKind::Public,
            0x02 => AddrKind::PublicIdentity,
            0x03 => AddrKind::RandomIdentity,
            0x01 => match addr[5] >> 6 {
                0b11 => AddrKind::RandomStatic,
                0b01 => AddrKind::RpaResolvable,
                0b00 => AddrKind::RpaNonResolvable,
                _ => AddrKind::RandomReserved,
            },
            _ => AddrKind::Unknown,
        }
    }

    /// The string written to the parquet column.
    pub fn as_str(self) -> &'static str {
        match self {
            AddrKind::Public => "public",
            AddrKind::RandomStatic => "random_static",
            AddrKind::RpaResolvable => "rpa_resolvable",
            AddrKind::RpaNonResolvable => "rpa_non_resolvable",
            AddrKind::RandomReserved => "random_reserved",
            AddrKind::PublicIdentity => "public_identity",
            AddrKind::RandomIdentity => "random_identity",
            AddrKind::Unknown => "unknown",
        }
    }

    /// Whether this address class identifies one device for the whole session.
    /// Rotating classes do not, so counting them over-counts the population.
    pub fn is_stable(self) -> bool {
        matches!(
            self,
            AddrKind::Public
                | AddrKind::RandomStatic
                | AddrKind::PublicIdentity
                | AddrKind::RandomIdentity
        )
    }
}

/// Advertising PDU type from the legacy LE Advertising Report `Event_Type`.
///
/// The advertising *channel index* (37/38/39) is **not** exposed by the HCI
/// Advertising Report on any Bluetooth version, so it is not a column: the
/// controller reports what it heard, not where it heard it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PduType {
    AdvInd,
    AdvDirectInd,
    AdvScanInd,
    AdvNonconnInd,
    ScanRsp,
    Unknown,
}

impl PduType {
    pub fn from_event_type(t: u8) -> Self {
        match t {
            0x00 => PduType::AdvInd,
            0x01 => PduType::AdvDirectInd,
            0x02 => PduType::AdvScanInd,
            0x03 => PduType::AdvNonconnInd,
            0x04 => PduType::ScanRsp,
            _ => PduType::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PduType::AdvInd => "adv_ind",
            PduType::AdvDirectInd => "adv_direct_ind",
            PduType::AdvScanInd => "adv_scan_ind",
            PduType::AdvNonconnInd => "adv_nonconn_ind",
            PduType::ScanRsp => "scan_rsp",
            PduType::Unknown => "unknown",
        }
    }
}

/// One received advertisement, still carrying its address. Never leaves the
/// parser: [`DeviceHasher::observe`] is the only consumer and it returns an
/// [`Observation`], which has no address field to leak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAdv {
    pub event_type: u8,
    pub addr_type: u8,
    pub addr: [u8; 6],
    pub rssi: i8,
}

/// One row of the durable log — the address has already been replaced by its
/// per-session pseudonym.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Host wallclock in nanoseconds, from the same [`crate::util::now_unix_ns`]
    /// the CSI RX thread stamps records with. This is the join key.
    pub unix_ts_ns: u64,
    pub device_hash: String,
    pub addr_kind: AddrKind,
    pub pdu_type: PduType,
    /// `None` when the controller reported the "unavailable" sentinel.
    pub rssi_dbm: Option<i8>,
}

// -- hashing ------------------------------------------------------------------

/// Turns Bluetooth addresses into per-session pseudonyms.
///
/// See the module docs for the privacy argument. The salt is intentionally not
/// `Clone`-friendly to anywhere durable: nothing in this crate serialises it.
pub struct DeviceHasher {
    salt: [u8; 32],
    bytes: usize,
}

impl std::fmt::Debug for DeviceHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the salt, not even in a panic message.
        f.debug_struct("DeviceHasher")
            .field("salt", &"<redacted>")
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl DeviceHasher {
    /// Draw a fresh salt from the OS CSPRNG. One call per session.
    ///
    /// Via `getrandom` rather than a `/dev/urandom` read: this module is
    /// documented as building on any platform (the pseudonymisation and
    /// parquet paths are read back off a node), and a hardcoded device path
    /// made that false everywhere but Linux.
    pub fn new_random(bytes: usize) -> Result<Self> {
        let mut salt = [0u8; 32];
        getrandom::fill(&mut salt).context("drawing the BLE salt from the OS CSPRNG")?;
        Ok(DeviceHasher {
            salt,
            bytes: bytes.clamp(4, 32),
        })
    }

    /// Deterministic construction — tests only. Production always uses
    /// [`Self::new_random`], because a fixed salt would make pseudonyms
    /// linkable across sessions.
    pub fn with_salt(salt: [u8; 32], bytes: usize) -> Self {
        DeviceHasher {
            salt,
            bytes: bytes.clamp(4, 32),
        }
    }

    /// `hex(SHA-256(salt ‖ addr_type ‖ addr)[..bytes])`.
    pub fn hash(&self, addr_type: u8, addr: &[u8; 6]) -> String {
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update([addr_type]);
        h.update(addr);
        let digest = h.finalize();
        let mut out = String::with_capacity(self.bytes * 2);
        for b in &digest[..self.bytes] {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// Pseudonymise one advertisement. The address is consumed here and never
    /// travels further.
    pub fn observe(&self, adv: &RawAdv, unix_ts_ns: u64) -> Observation {
        Observation {
            unix_ts_ns,
            device_hash: self.hash(adv.addr_type, &adv.addr),
            addr_kind: AddrKind::classify(adv.addr_type, &adv.addr),
            pdu_type: PduType::from_event_type(adv.event_type),
            rssi_dbm: (adv.rssi != RSSI_UNAVAILABLE).then_some(adv.rssi),
        }
    }
}

// -- HCI event parsing (portable, so it is testable off-Linux) ----------------

/// Outcome of parsing one HCI event packet.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedEvent {
    pub advs: Vec<RawAdv>,
    /// The packet was an LE Advertising Report but ran out of bytes mid-report.
    pub truncated: bool,
    /// The packet was not an LE Advertising Report (command completion, an
    /// unrelated LE subevent, …). Counted so an adapter emitting nothing *but*
    /// these is visible.
    pub ignored: bool,
}

/// Parse one packet as read from an `AF_BLUETOOTH`/`BTPROTO_HCI` socket.
///
/// Wire layout (Core spec vol 4, part A §2 and vol 4, part E §7.7.65.2):
///
/// ```text
/// [0]      packet indicator  0x04 = HCI Event
/// [1]      event code        0x3E = LE Meta Event
/// [2]      parameter length
/// [3]      subevent code     0x02 = LE Advertising Report
/// [4]      num_reports
/// then, per report, packed consecutively:
///   event_type(1) addr_type(1) addr(6, little-endian) data_len(1) data(data_len) rssi(1, i8)
/// ```
///
/// Only the legacy report (0x02) is parsed. `csid` never enables extended
/// scanning, so subevent 0x0D cannot result from our own commands; if another
/// process on the adapter enabled it, those reports are counted as `ignored`
/// rather than parsed, so we never double-count the same air traffic.
pub fn parse_hci_event(buf: &[u8]) -> ParsedEvent {
    const HCI_EVENT_PKT: u8 = 0x04;
    const EVT_LE_META: u8 = 0x3E;
    const SUBEVT_LE_ADV_REPORT: u8 = 0x02;
    /// event_type + addr_type + addr(6) + data_len
    const REPORT_FIXED: usize = 9;

    let ignored = ParsedEvent {
        ignored: true,
        ..Default::default()
    };

    if buf.len() < 5 || buf[0] != HCI_EVENT_PKT || buf[1] != EVT_LE_META {
        return ignored;
    }
    let plen = buf[2] as usize;
    // Trust the declared length over the read size; a short read is truncation.
    let end = (3 + plen).min(buf.len());
    let params = &buf[3..end];
    if params.len() < 2 || params[0] != SUBEVT_LE_ADV_REPORT {
        return ignored;
    }

    let num = params[1] as usize;
    let d = &params[2..];
    let mut advs = Vec::with_capacity(num.min(8));
    let mut off = 0usize;
    for _ in 0..num {
        if off + REPORT_FIXED > d.len() {
            return ParsedEvent {
                advs,
                truncated: true,
                ignored: false,
            };
        }
        let event_type = d[off];
        let addr_type = d[off + 1];
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&d[off + 2..off + 8]);
        let data_len = d[off + 8] as usize;
        let rssi_at = off + REPORT_FIXED + data_len;
        if rssi_at >= d.len() {
            return ParsedEvent {
                advs,
                truncated: true,
                ignored: false,
            };
        }
        advs.push(RawAdv {
            event_type,
            addr_type,
            addr,
            rssi: d[rssi_at] as i8,
        });
        off = rssi_at + 1;
    }
    ParsedEvent {
        advs,
        truncated: false,
        ignored: false,
    }
}

// -- HCI command construction (portable, so it is testable off-Linux) ---------
//
// Same split as `inject`: the wire format is built and checked here, and only
// the syscalls live in the platform module. A byte-layout bug is then a unit
// test failure on any developer machine rather than a silent no-op on a Pi.

/// HCI packet indicators (Core spec vol 4, part A §2).
pub const HCI_COMMAND_PKT: u8 = 0x01;
pub const HCI_EVENT_PKT: u8 = 0x04;
/// Event codes we act on.
pub const EVT_CMD_COMPLETE: u8 = 0x0E;
pub const EVT_CMD_STATUS: u8 = 0x0F;

/// `opcode = (OGF << 10) | OCF`.
pub const fn hci_opcode(ogf: u16, ocf: u16) -> u16 {
    (ogf << 10) | ocf
}
/// OGF 0x08 = LE Controller commands.
pub const OP_LE_SET_SCAN_PARAMETERS: u16 = hci_opcode(0x08, 0x000B);
pub const OP_LE_SET_SCAN_ENABLE: u16 = hci_opcode(0x08, 0x000C);

/// `[0x01][opcode LE][param_len][params…]`
pub fn build_hci_command(op: u16, params: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + params.len());
    pkt.push(HCI_COMMAND_PKT);
    pkt.extend_from_slice(&op.to_le_bytes());
    pkt.push(params.len() as u8);
    pkt.extend_from_slice(params);
    pkt
}

/// `LE Set Scan Parameters` — always **passive**, accept-all, public own-address.
///
/// Interval and window are in HCI 0.625 ms units; see
/// [`BleConfig::hci_units`](crate::config::BleConfig::hci_units).
pub fn scan_parameters_command(interval_units: u16, window_units: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(7);
    p.push(0x00); // LE_Scan_Type: passive — the scanner never transmits
    p.extend_from_slice(&interval_units.to_le_bytes());
    p.extend_from_slice(&window_units.to_le_bytes());
    p.push(0x00); // Own_Address_Type: public (unused while passive)
    p.push(0x00); // Scanning_Filter_Policy: accept all advertisements
    build_hci_command(OP_LE_SET_SCAN_PARAMETERS, &p)
}

/// `LE Set Scan Enable`. Duplicate filtering is always off: one row per
/// received advertisement is the whole point of the artefact.
pub fn scan_enable_command(enable: bool) -> Vec<u8> {
    build_hci_command(OP_LE_SET_SCAN_ENABLE, &[u8::from(enable), 0x00])
}

/// Status byte of a Command Complete / Command Status event for `op`, or `None`
/// when this packet is about something else (an advertising report that
/// arrived first, another process's command, …).
pub fn command_status(buf: &[u8], op: u16) -> Option<u8> {
    if buf.len() < 3 || buf[0] != HCI_EVENT_PKT {
        return None;
    }
    let params = &buf[3..];
    match buf[1] {
        // num_hci_command_packets, opcode(2), status, …
        EVT_CMD_COMPLETE if params.len() >= 4 => {
            (u16::from_le_bytes([params[1], params[2]]) == op).then_some(params[3])
        }
        // status, num_hci_command_packets, opcode(2)
        EVT_CMD_STATUS if params.len() >= 4 => {
            (u16::from_le_bytes([params[2], params[3]]) == op).then_some(params[0])
        }
        _ => None,
    }
}

// -- health counters ----------------------------------------------------------

/// Liveness counters, shared with the supervising loop.
///
/// The readiness audit's R3 lesson is that a *silently* degraded channel is
/// worse than an absent one, so every one of these ends up in the sidecar and
/// the observation rate ends up in the `systemctl status` line: a dead BLE
/// scanner has to be as loud as a dead illuminator.
#[derive(Debug, Default)]
pub struct BleCounters {
    /// Advertisements written to the durable log.
    pub observations: AtomicU64,
    /// Times the scanner socket was torn down and re-opened.
    pub scan_restarts: AtomicU64,
    /// Adapter open / command failures.
    pub adapter_errors: AtomicU64,
    /// Event packets that were not usable advertising reports.
    pub unparsed_events: AtomicU64,
    /// Advertisements whose controller RSSI was the "unavailable" sentinel.
    pub rssi_unavailable: AtomicU64,
    /// Longest observed gap between consecutive advertisements, milliseconds.
    pub max_gap_ms: AtomicU64,
    /// Gaps longer than `ble.gap_alert_s`.
    pub gaps_over_alert: AtomicU64,
    /// Wallclock of the first / most recent observation (ns, 0 = none yet).
    pub first_obs_ns: AtomicU64,
    pub last_obs_ns: AtomicU64,
}

impl BleCounters {
    /// Record one observation's timing. Returns the gap in milliseconds.
    pub fn note_observation(&self, unix_ts_ns: u64, gap_alert_s: f64) -> u64 {
        self.observations.fetch_add(1, Ordering::Relaxed);
        let prev = self.last_obs_ns.swap(unix_ts_ns, Ordering::Relaxed);
        if prev == 0 {
            self.first_obs_ns.store(unix_ts_ns, Ordering::Relaxed);
            return 0;
        }
        let gap_ms = unix_ts_ns.saturating_sub(prev) / 1_000_000;
        self.max_gap_ms.fetch_max(gap_ms, Ordering::Relaxed);
        if gap_ms as f64 / 1000.0 > gap_alert_s {
            self.gaps_over_alert.fetch_add(1, Ordering::Relaxed);
        }
        gap_ms
    }

    /// Observations so far and the mean rate over the observed span.
    pub fn snapshot(&self) -> (u64, f64) {
        let n = self.observations.load(Ordering::Relaxed);
        let first = self.first_obs_ns.load(Ordering::Relaxed);
        let last = self.last_obs_ns.load(Ordering::Relaxed);
        let span_s = if last > first {
            (last - first) as f64 / 1e9
        } else {
            0.0
        };
        (n, if span_s > 0.0 { n as f64 / span_s } else { 0.0 })
    }

    /// Nanoseconds since the last observation, or `None` if there never was one.
    pub fn silence_ns(&self, now_ns: u64) -> Option<u64> {
        let last = self.last_obs_ns.load(Ordering::Relaxed);
        (last != 0).then(|| now_ns.saturating_sub(last))
    }
}

// -- durable log --------------------------------------------------------------

/// Append-only NDJSON writer for the scan thread.
pub struct ObservationLog {
    writer: BufWriter<File>,
    path: PathBuf,
    since_flush: usize,
    flush_every: usize,
    last_flush: Instant,
}

impl ObservationLog {
    pub fn create(dir: &Path, flush_every: usize) -> Result<Self> {
        let path = dir.join(NDJSON_NAME);
        let file =
            File::create(&path).with_context(|| format!("creating BLE log {}", path.display()))?;
        Ok(ObservationLog {
            writer: BufWriter::with_capacity(64 * 1024, file),
            path,
            since_flush: 0,
            flush_every: flush_every.max(1),
            last_flush: Instant::now(),
        })
    }

    /// Append one observation, flushing on the record or time budget so a
    /// power-cut loses seconds, not the session.
    pub fn append(&mut self, obs: &Observation) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, obs)?;
        self.writer.write_all(b"\n")?;
        self.since_flush += 1;
        if self.since_flush >= self.flush_every
            || self.last_flush.elapsed() >= std::time::Duration::from_secs(2)
        {
            self.writer.flush()?;
            self.since_flush = 0;
            self.last_flush = Instant::now();
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        self.writer.flush().context("flushing the BLE log")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("fsyncing the BLE log")?;
        Ok(self.path)
    }
}

// -- parquet export -----------------------------------------------------------

/// Session-constant columns. They are repeated on every row on purpose:
/// dictionary encoding makes them nearly free, and it means a ten-node lab
/// session concatenates into one dataframe with no bookkeeping.
#[derive(Debug, Clone)]
pub struct ParquetContext {
    pub host: String,
    pub session_id: String,
    pub adapter: String,
}

/// What the export produced — folded into the sidecar summary.
#[derive(Debug, Clone, Default)]
pub struct ExportStats {
    pub rows: u64,
    pub distinct_device_hashes: u64,
    pub rssi_null: u64,
    pub malformed_lines: u64,
}

/// The `ble_rssi.parquet` schema. **This is a contract** — the analysis side
/// (`monad_knowledge.csi.ble`) asserts against it, so a column rename is a
/// schema-version bump, not a refactor.
fn parquet_schema() -> Result<Type> {
    let s = |name: &str| -> Result<Type> {
        Ok(Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()?)
    };
    let fields = vec![
        Arc::new(
            Type::primitive_type_builder("unix_ts_ns", PhysicalType::INT64)
                .with_repetition(Repetition::REQUIRED)
                .build()?,
        ),
        Arc::new(s("host")?),
        Arc::new(s("session_id")?),
        Arc::new(s("adapter")?),
        Arc::new(s("device_hash")?),
        Arc::new(s("addr_kind")?),
        Arc::new(s("pdu_type")?),
        // OPTIONAL: null encodes the controller's "RSSI unavailable" sentinel
        // rather than writing 127 dBm, which no analysis should ever average.
        Arc::new(
            Type::primitive_type_builder("rssi_dbm", PhysicalType::INT32)
                .with_repetition(Repetition::OPTIONAL)
                .build()?,
        ),
    ];
    Ok(Type::group_type_builder("ble_rssi")
        .with_fields(fields)
        .build()?)
}

/// Stream `ble_scan.jsonl` into `ble_rssi.parquet`.
///
/// Row-group at a time, so a multi-day session never has to fit in memory. A
/// malformed line is counted and skipped: a truncated last line from a power
/// cut must not cost the rest of the capture.
pub fn export_parquet(ndjson: &Path, out: &Path, ctx: &ParquetContext) -> Result<ExportStats> {
    let file =
        File::open(ndjson).with_context(|| format!("opening BLE log {}", ndjson.display()))?;
    let reader = BufReader::with_capacity(256 * 1024, file);

    let schema = Arc::new(parquet_schema()?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build(),
    );
    let sink = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut writer = SerializedFileWriter::new(sink, schema, props)
        .context("opening the ble_rssi.parquet writer")?;

    let mut stats = ExportStats::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut batch: Vec<Observation> = Vec::with_capacity(ROW_GROUP_ROWS);

    for line in reader.lines() {
        let line = line.context("reading the BLE log")?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Observation>(&line) {
            Ok(obs) => {
                seen.insert(obs.device_hash.clone());
                if obs.rssi_dbm.is_none() {
                    stats.rssi_null += 1;
                }
                batch.push(obs);
            }
            Err(_) => stats.malformed_lines += 1,
        }
        if batch.len() >= ROW_GROUP_ROWS {
            stats.rows += write_row_group(&mut writer, &batch, ctx)? as u64;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        stats.rows += write_row_group(&mut writer, &batch, ctx)? as u64;
    }
    writer.close().context("closing ble_rssi.parquet")?;
    stats.distinct_device_hashes = seen.len() as u64;
    Ok(stats)
}

fn write_row_group<W: std::io::Write + Send>(
    writer: &mut SerializedFileWriter<W>,
    batch: &[Observation],
    ctx: &ParquetContext,
) -> Result<usize> {
    let mut rg = writer.next_row_group()?;

    let mut col = rg.next_column()?.context("column unix_ts_ns missing")?;
    let ts: Vec<i64> = batch.iter().map(|o| o.unix_ts_ns as i64).collect();
    col.typed::<Int64Type>().write_batch(&ts, None, None)?;
    col.close()?;

    for constant in [&ctx.host, &ctx.session_id, &ctx.adapter] {
        let mut col = rg.next_column()?.context("constant column missing")?;
        let v: Vec<ByteArray> =
            std::iter::repeat_n(ByteArray::from(constant.as_str()), batch.len()).collect();
        col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
        col.close()?;
    }

    let mut col = rg.next_column()?.context("column device_hash missing")?;
    let v: Vec<ByteArray> = batch
        .iter()
        .map(|o| ByteArray::from(o.device_hash.as_str()))
        .collect();
    col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
    col.close()?;

    let mut col = rg.next_column()?.context("column addr_kind missing")?;
    let v: Vec<ByteArray> = batch
        .iter()
        .map(|o| ByteArray::from(o.addr_kind.as_str()))
        .collect();
    col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
    col.close()?;

    let mut col = rg.next_column()?.context("column pdu_type missing")?;
    let v: Vec<ByteArray> = batch
        .iter()
        .map(|o| ByteArray::from(o.pdu_type.as_str()))
        .collect();
    col.typed::<ByteArrayType>().write_batch(&v, None, None)?;
    col.close()?;

    // OPTIONAL column: definition level 1 = present, 0 = null; only present
    // values appear in the value buffer.
    let mut col = rg.next_column()?.context("column rssi_dbm missing")?;
    let def: Vec<i16> = batch
        .iter()
        .map(|o| if o.rssi_dbm.is_some() { 1 } else { 0 })
        .collect();
    let vals: Vec<i32> = batch
        .iter()
        .filter_map(|o| o.rssi_dbm.map(i32::from))
        .collect();
    col.typed::<Int32Type>()
        .write_batch(&vals, Some(&def), None)?;
    col.close()?;

    rg.close()?;
    Ok(batch.len())
}

// -- scanner lifecycle --------------------------------------------------------

/// A running BLE co-capture.
pub struct BleHandle {
    thread: JoinHandle<()>,
    /// Path of the durable log, needed by the close-time export.
    pub ndjson: PathBuf,
}

impl BleHandle {
    /// Only the Linux scanner constructs these; other builds still compile the
    /// portable half of the module.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn new(thread: JoinHandle<()>, ndjson: PathBuf) -> Self {
        BleHandle { thread, ndjson }
    }

    /// Join the scan thread. Errors inside it are already counted and logged;
    /// a panicking scanner must never fail the CSI session.
    pub fn join(self) -> PathBuf {
        if self.thread.join().is_err() {
            tracing::error!("BLE scan thread panicked; the durable log up to that point is intact");
        }
        self.ndjson
    }
}

/// Start the BLE co-capture for a session.
///
/// The adapter is opened on the **caller's** thread, so an absent or down
/// adapter is a setup error the session can act on (fail if `ble.required`,
/// degrade and record otherwise) rather than a silent nothing.
pub fn spawn(
    dir: &Path,
    cfg: &BleConfig,
    stop: Arc<AtomicBool>,
    counters: Arc<BleCounters>,
) -> Result<BleHandle> {
    crate::hci::spawn(dir, cfg, stop, counters)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One advertising report as the fixtures below spell it out:
    /// `(event_type, addr_type, addr, adv_data, rssi)`.
    type Report<'a> = (u8, u8, [u8; 6], &'a [u8], i8);

    fn adv_event(reports: &[Report<'_>]) -> Vec<u8> {
        let mut params = vec![0x02u8, reports.len() as u8];
        for (etype, atype, addr, data, rssi) in reports {
            params.push(*etype);
            params.push(*atype);
            params.extend_from_slice(addr);
            params.push(data.len() as u8);
            params.extend_from_slice(data);
            params.push(*rssi as u8);
        }
        let mut pkt = vec![0x04u8, 0x3E, params.len() as u8];
        pkt.extend_from_slice(&params);
        pkt
    }

    #[test]
    fn parses_single_advertising_report() {
        let addr = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let pkt = adv_event(&[(0x00, 0x01, addr, &[0x02, 0x01, 0x06], -63)]);
        let out = parse_hci_event(&pkt);
        assert!(!out.truncated && !out.ignored);
        assert_eq!(
            out.advs,
            vec![RawAdv {
                event_type: 0x00,
                addr_type: 0x01,
                addr,
                rssi: -63
            }]
        );
    }

    #[test]
    fn parses_multiple_reports_in_one_event() {
        let a = [1, 2, 3, 4, 5, 0x40];
        let b = [9, 9, 9, 9, 9, 0xC0];
        let pkt = adv_event(&[(0x00, 0x01, a, &[0xAA], -50), (0x04, 0x00, b, &[], -80)]);
        let out = parse_hci_event(&pkt);
        assert_eq!(out.advs.len(), 2);
        assert_eq!(out.advs[0].rssi, -50);
        assert_eq!(out.advs[1].rssi, -80);
        assert_eq!(out.advs[1].event_type, 0x04);
    }

    #[test]
    fn flags_truncated_reports_without_losing_the_earlier_ones() {
        let a = [1, 2, 3, 4, 5, 6];
        let mut pkt = adv_event(&[(0x00, 0x01, a, &[0xAA], -50), (0x00, 0x01, a, &[], -60)]);
        pkt.truncate(pkt.len() - 3); // eat the second report's tail
        let out = parse_hci_event(&pkt);
        assert!(out.truncated);
        assert_eq!(out.advs.len(), 1);
    }

    #[test]
    fn ignores_non_advertising_events() {
        // Command Complete, not an LE Meta event.
        assert!(parse_hci_event(&[0x04, 0x0E, 0x04, 0x01, 0x0C, 0x20, 0x00]).ignored);
        // LE Meta, but the extended-advertising subevent we never enable.
        assert!(parse_hci_event(&[0x04, 0x3E, 0x02, 0x0D, 0x01]).ignored);
        // ACL data, not an event at all.
        assert!(parse_hci_event(&[0x02, 0x3E, 0x02, 0x02, 0x01]).ignored);
        assert!(parse_hci_event(&[]).ignored);
    }

    #[test]
    fn opcodes_and_command_framing_match_the_core_spec() {
        assert_eq!(OP_LE_SET_SCAN_PARAMETERS, 0x200B);
        assert_eq!(OP_LE_SET_SCAN_ENABLE, 0x200C);

        // 100 ms = 160 units of 0.625 ms.
        let p = scan_parameters_command(160, 160);
        assert_eq!(p[0], HCI_COMMAND_PKT);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 0x200B);
        assert_eq!(p[3] as usize, 7, "LE Set Scan Parameters takes 7 bytes");
        assert_eq!(p[4], 0x00, "scan type must be passive");
        assert_eq!(u16::from_le_bytes([p[5], p[6]]), 160);
        assert_eq!(u16::from_le_bytes([p[7], p[8]]), 160);
        assert_eq!(p[9], 0x00);
        assert_eq!(p[10], 0x00);
        assert_eq!(p.len(), 11);

        let on = scan_enable_command(true);
        assert_eq!(on, vec![0x01, 0x0C, 0x20, 0x02, 0x01, 0x00]);
        assert_eq!(
            on[5], 0x00,
            "duplicate filtering must stay off: one row per advertisement"
        );
        assert_eq!(scan_enable_command(false)[4], 0x00);
    }

    #[test]
    fn reads_command_completion_status_for_the_right_opcode() {
        // 04 0E 04 | num_cmd=01 opcode=0C20 status=00
        let ok = [0x04, 0x0E, 0x04, 0x01, 0x0C, 0x20, 0x00];
        assert_eq!(command_status(&ok, OP_LE_SET_SCAN_ENABLE), Some(0));
        assert_eq!(command_status(&ok, OP_LE_SET_SCAN_PARAMETERS), None);

        let rejected = [0x04, 0x0E, 0x04, 0x01, 0x0B, 0x20, 0x12];
        assert_eq!(
            command_status(&rejected, OP_LE_SET_SCAN_PARAMETERS),
            Some(0x12)
        );

        // Command Status form: 04 0F 04 | status=00 num_cmd=01 opcode=0C20
        let status_ev = [0x04, 0x0F, 0x04, 0x00, 0x01, 0x0C, 0x20];
        assert_eq!(command_status(&status_ev, OP_LE_SET_SCAN_ENABLE), Some(0));

        // An advertising report must never be read as a completion.
        assert_eq!(
            command_status(&[0x04, 0x3E, 0x02, 0x02, 0x00], OP_LE_SET_SCAN_ENABLE),
            None
        );
    }

    #[test]
    fn classifies_address_kinds_from_the_top_two_bits() {
        let with_msb = |m: u8| [0, 0, 0, 0, 0, m];
        assert_eq!(AddrKind::classify(0x00, &with_msb(0x00)), AddrKind::Public);
        assert_eq!(
            AddrKind::classify(0x01, &with_msb(0xC3)),
            AddrKind::RandomStatic
        );
        assert_eq!(
            AddrKind::classify(0x01, &with_msb(0x7F)),
            AddrKind::RpaResolvable
        );
        assert_eq!(
            AddrKind::classify(0x01, &with_msb(0x3F)),
            AddrKind::RpaNonResolvable
        );
        assert_eq!(
            AddrKind::classify(0x01, &with_msb(0x80)),
            AddrKind::RandomReserved
        );
        assert!(AddrKind::Public.is_stable());
        assert!(AddrKind::RandomStatic.is_stable());
        assert!(!AddrKind::RpaResolvable.is_stable());
        assert!(!AddrKind::RpaNonResolvable.is_stable());
    }

    #[test]
    fn hash_is_stable_within_a_session() {
        let h = DeviceHasher::with_salt([7u8; 32], 8);
        let addr = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
        let a = h.hash(0x01, &addr);
        let b = h.hash(0x01, &addr);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16, "8 bytes render as 16 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_is_unlinkable_across_sessions() {
        let addr = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
        let s1 = DeviceHasher::with_salt([1u8; 32], 8).hash(0x01, &addr);
        let s2 = DeviceHasher::with_salt([2u8; 32], 8).hash(0x01, &addr);
        assert_ne!(
            s1, s2,
            "the same device must not be linkable across two sessions"
        );
    }

    #[test]
    fn hash_separates_distinct_devices_and_address_types() {
        let h = DeviceHasher::with_salt([7u8; 32], 8);
        let a = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
        let b = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x02];
        assert_ne!(h.hash(0x01, &a), h.hash(0x01, &b));
        // The address type is part of the preimage: the same 48 bits used as a
        // public and as a random address are different devices.
        assert_ne!(h.hash(0x00, &a), h.hash(0x01, &a));
    }

    #[test]
    fn random_salts_differ_between_hashers() {
        let addr = [1, 2, 3, 4, 5, 6];
        let a = DeviceHasher::new_random(8).unwrap().hash(0x01, &addr);
        let b = DeviceHasher::new_random(8).unwrap().hash(0x01, &addr);
        assert_ne!(a, b, "two sessions must draw different salts");
    }

    #[test]
    fn debug_never_renders_the_salt() {
        let h = DeviceHasher::with_salt([0xAB; 32], 8);
        let rendered = format!("{h:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("171") && !rendered.contains("ab"));
    }

    #[test]
    fn unavailable_rssi_becomes_null_not_127() {
        let h = DeviceHasher::with_salt([0u8; 32], 8);
        let adv = RawAdv {
            event_type: 0,
            addr_type: 1,
            addr: [1, 2, 3, 4, 5, 6],
            rssi: 127,
        };
        assert_eq!(h.observe(&adv, 42).rssi_dbm, None);
        let ok = RawAdv { rssi: -70, ..adv };
        assert_eq!(h.observe(&ok, 42).rssi_dbm, Some(-70));
    }

    #[test]
    fn counters_track_rate_and_gaps() {
        let c = BleCounters::default();
        let t0 = 1_800_000_000_000_000_000u64;
        c.note_observation(t0, 5.0);
        c.note_observation(t0 + 1_000_000_000, 5.0); // 1 s
        c.note_observation(t0 + 9_000_000_000, 5.0); // 8 s — over the alert
        let (n, rate) = c.snapshot();
        assert_eq!(n, 3);
        assert!((rate - 3.0 / 9.0).abs() < 1e-9);
        assert_eq!(c.max_gap_ms.load(Ordering::Relaxed), 8_000);
        assert_eq!(c.gaps_over_alert.load(Ordering::Relaxed), 1);
        assert_eq!(c.silence_ns(t0 + 10_000_000_000), Some(1_000_000_000));
    }

    #[test]
    fn parquet_roundtrips_through_the_log() {
        let dir = std::env::temp_dir().join(format!("csid-ble-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hasher = DeviceHasher::with_salt([3u8; 32], 8);

        let mut log = ObservationLog::create(&dir, 1).unwrap();
        let t0 = 1_800_000_000_000_000_000u64;
        for i in 0..5u64 {
            let adv = RawAdv {
                event_type: 0x00,
                addr_type: 0x01,
                addr: [1, 2, 3, 4, 5, 0x40 | (i as u8 & 1)],
                rssi: if i == 3 { 127 } else { -60 - i as i8 },
            };
            log.append(&hasher.observe(&adv, t0 + i * 100_000_000))
                .unwrap();
        }
        let ndjson = log.finish().unwrap();

        let out = dir.join(PARQUET_NAME);
        let stats = export_parquet(
            &ndjson,
            &out,
            &ParquetContext {
                host: "monad02".into(),
                session_id: "monad02_lab_20260808-100000".into(),
                adapter: "hci0".into(),
            },
        )
        .unwrap();
        assert_eq!(stats.rows, 5);
        assert_eq!(stats.distinct_device_hashes, 2);
        assert_eq!(stats.rssi_null, 1);
        assert_eq!(stats.malformed_lines, 0);
        assert!(out.metadata().unwrap().len() > 0);

        // A truncated trailing line (power cut) costs that line, nothing more.
        let mut text = std::fs::read_to_string(&ndjson).unwrap();
        text.push_str("{\"unix_ts_ns\":1,\"devi");
        std::fs::write(&ndjson, text).unwrap();
        let stats = export_parquet(
            &ndjson,
            &out,
            &ParquetContext {
                host: "monad02".into(),
                session_id: "s".into(),
                adapter: "hci0".into(),
            },
        )
        .unwrap();
        assert_eq!(stats.rows, 5);
        assert_eq!(stats.malformed_lines, 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
