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
//! ## Lab identity frames (`ble-rssi/2`)
//!
//! The MonadCount app (and any lab emitter) advertises a single **128-bit
//! service UUID** whose first twelve bytes are the deployment's namespace and
//! whose last four carry a 16-bit participant key and a 16-bit session key.
//! When `ble.lab_namespace_uuid` is configured, the scanner matches that prefix
//! in the advertisement payload and the row carries `lab_uuid`,
//! `lab_participant_key` and `lab_session_key`. This is the only payload
//! inspection the scanner does, and it is match-or-forget:
//!
//! - **A matching frame is ours by construction** — the namespace is
//!   lab-chosen, so storing its identity bytes identifies a consented session,
//!   not a person, and the session sidecar on the phone side holds the mapping.
//! - **A non-matching payload is dropped unparsed.** No service UUID, local
//!   name, or manufacturer data of a bystander device is ever stored, so the
//!   count-without-identify posture is unchanged: bystanders remain salted
//!   pseudonyms exactly as in `ble-rssi/1`.
//!
//! Byte order, stated once because it is the classic bug: AD structures carry
//! 128-bit UUIDs **little-endian**, so the matcher reverses each 16-byte chunk
//! before comparing against the canonical (big-endian) namespace.
//!
//! ## Durability
//!
//! The scanner appends NDJSON to `ble_scan.jsonl` as it goes — crash-safe, the
//! same reasoning that makes `capture.raw` the durable CSI artefact. At session
//! close that log is streamed into `ble_rssi.parquet` (the contract artefact the
//! analysis side consumes) in row groups; the log is kept, exactly as
//! `capture.raw` is kept beside `capture.csiq`. The parquet is
//! **self-describing**: its footer key-value metadata carries the schema id,
//! the scan parameters, the pseudonymisation scheme, the lab namespace and the
//! identity-byte layout, so the file can be interpreted with nothing but the
//! file.

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
/// Schema identifier, mirrored into the sidecar and the parquet footer. Bump on
/// any column change. `/2` adds the nullable lab-identity columns (`lab_uuid`,
/// `lab_participant_key`, `lab_session_key`) and the self-describing footer.
pub const PARQUET_SCHEMA: &str = "ble-rssi/2";
/// How the last four bytes of a matched lab UUID are laid out. Written into the
/// parquet footer so the file explains its own join keys.
pub const LAB_UUID_LAYOUT: &str =
    "bytes 0-11 namespace, 12-13 participant_key (big-endian u16), 14-15 session_key (big-endian u16)";
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

/// One received advertisement, still carrying its address and raw payload.
/// Never leaves the parser's caller: [`DeviceHasher::observe`] consumes the
/// address into a pseudonym, [`LabMatcher::extract`] consumes the payload into
/// a lab identity or nothing, and the resulting [`Observation`] has neither an
/// address nor a payload field to leak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAdv {
    pub event_type: u8,
    pub addr_type: u8,
    pub addr: [u8; 6],
    pub rssi: i8,
    /// The AD payload as received. Inspected only by [`LabMatcher::extract`];
    /// never serialised.
    pub data: Vec<u8>,
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
    /// Canonical 128-bit service UUID of a matched lab identity frame
    /// (`ble-rssi/2`). `None` for every non-lab advertisement — and for every
    /// row of a `ble-rssi/1` log, which is why all three fields are
    /// serde-defaulted: the segment cursor and the export must keep reading
    /// logs written before this schema existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_participant_key: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_session_key: Option<u16>,
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
        // `map_err` rather than `.context()`: `getrandom::Error` only implements
        // `std::error::Error` when getrandom's `std` feature is on, and whether
        // it is depends on which packages the invocation selected. A workspace
        // build unified it on and a `-p csid` build did not, so `.context()`
        // compiled on the bench and failed on the Pi. `Display` is unconditional.
        getrandom::fill(&mut salt)
            .map_err(|e| anyhow::anyhow!("drawing the BLE salt from the OS CSPRNG: {e}"))?;
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
    /// travels further. Lab identity, when a matcher is configured and the
    /// payload carries the namespace, rides on the same row.
    pub fn observe(
        &self,
        adv: &RawAdv,
        unix_ts_ns: u64,
        matcher: Option<&LabMatcher>,
    ) -> Observation {
        let lab = matcher.and_then(|m| m.extract(&adv.data));
        Observation {
            unix_ts_ns,
            device_hash: self.hash(adv.addr_type, &adv.addr),
            addr_kind: AddrKind::classify(adv.addr_type, &adv.addr),
            pdu_type: PduType::from_event_type(adv.event_type),
            rssi_dbm: (adv.rssi != RSSI_UNAVAILABLE).then_some(adv.rssi),
            lab_uuid: lab.as_ref().map(|f| f.uuid.clone()),
            lab_participant_key: lab.as_ref().map(|f| f.participant_key),
            lab_session_key: lab.map(|f| f.session_key),
        }
    }
}

// -- lab identity matching (portable, so it is testable off-Linux) ------------

/// A matched lab identity frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabFrame {
    /// Canonical (big-endian, dashed, lowercase) form of the advertised UUID.
    pub uuid: String,
    pub participant_key: u16,
    pub session_key: u16,
}

/// Matches the lab identity frame in an advertisement payload.
///
/// The frame is a 128-bit service UUID: the first twelve bytes are the
/// deployment namespace, the last four the participant and session keys (see
/// [`LAB_UUID_LAYOUT`]). Three AD types can legitimately carry it — 0x06 and
/// 0x07 (incomplete / complete list of 128-bit service UUIDs, the shape both
/// mobile platforms emit) and 0x21 (128-bit service data, the shape a
/// firmware emitter may prefer).
///
/// Everything that does not carry the namespace is ignored *unparsed* — this
/// matcher is the privacy boundary for payloads, exactly as [`DeviceHasher`]
/// is for addresses.
#[derive(Debug, Clone)]
pub struct LabMatcher {
    prefix: [u8; 12],
    namespace: String,
}

impl LabMatcher {
    /// Build from the canonical namespace UUID. `Err` on a malformed string —
    /// a misconfigured namespace must fail the setup loudly, because "matching
    /// silently off" would look identical to "nobody broadcast".
    pub fn from_namespace(namespace: &str) -> Result<Self> {
        let hex: String = namespace
            .chars()
            .filter(|c| *c != '-')
            .collect::<String>()
            .to_lowercase();
        if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("malformed ble.lab_namespace_uuid: {namespace:?}");
        }
        let mut prefix = [0u8; 12];
        for (i, byte) in prefix.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("checked hex");
        }
        Ok(LabMatcher {
            prefix,
            namespace: canonical_uuid_from_hex(&hex),
        })
    }

    /// The configured namespace in canonical form — sidecar / footer provenance.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Walk the AD structures and return the first lab frame, if any.
    ///
    /// Malformed structures (zero length, declared length past the buffer) end
    /// the walk without panicking: radio payloads are attacker-adjacent input
    /// and a bad byte must cost one match attempt, never the scanner.
    pub fn extract(&self, data: &[u8]) -> Option<LabFrame> {
        let mut i = 0usize;
        while i < data.len() {
            let ad_len = data[i] as usize; // length of type byte + payload
            if ad_len == 0 || i + 1 + ad_len > data.len() {
                return None;
            }
            let ad_type = data[i + 1];
            let payload = &data[i + 2..i + 1 + ad_len];
            match ad_type {
                // Lists of 128-bit service UUIDs: consecutive 16-byte entries.
                0x06 | 0x07 => {
                    for chunk in payload.chunks_exact(16) {
                        if let Some(frame) = self.match_uuid_le(chunk) {
                            return Some(frame);
                        }
                    }
                }
                // 128-bit service data: the UUID is the first 16 bytes.
                0x21 if payload.len() >= 16 => {
                    if let Some(frame) = self.match_uuid_le(&payload[..16]) {
                        return Some(frame);
                    }
                }
                _ => {}
            }
            i += 1 + ad_len;
        }
        None
    }

    /// Match one 16-byte UUID as it appears **on air (little-endian)**.
    fn match_uuid_le(&self, le: &[u8]) -> Option<LabFrame> {
        debug_assert_eq!(le.len(), 16);
        let mut be = [0u8; 16];
        for (i, b) in le.iter().rev().enumerate() {
            be[i] = *b;
        }
        if be[..12] != self.prefix {
            return None;
        }
        Some(LabFrame {
            uuid: canonical_uuid(&be),
            participant_key: u16::from_be_bytes([be[12], be[13]]),
            session_key: u16::from_be_bytes([be[14], be[15]]),
        })
    }
}

fn canonical_uuid(bytes: &[u8; 16]) -> String {
    let mut hex = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    canonical_uuid_from_hex(&hex)
}

fn canonical_uuid_from_hex(hex: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
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
            data: d[off + REPORT_FIXED..rssi_at].to_vec(),
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
    /// Advertisements that matched the lab identity namespace (`ble-rssi/2`).
    pub lab_frames: AtomicU64,
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

/// Session-constant columns plus the provenance written into the parquet
/// footer. The columns are repeated on every row on purpose: dictionary
/// encoding makes them nearly free, and it means a ten-node lab session
/// concatenates into one dataframe with no bookkeeping. The footer is what
/// makes the file self-describing — a reader with nothing but `ble_rssi.parquet`
/// learns the schema id, the scan posture, the pseudonymisation scheme and the
/// lab-identity layout without opening any sidecar.
#[derive(Debug, Clone)]
pub struct ParquetContext {
    pub host: String,
    pub session_id: String,
    pub adapter: String,
    /// Canonical lab namespace when matching was configured; `None` = v1-style
    /// scan with no identity matching.
    pub lab_namespace_uuid: Option<String>,
    pub scan_interval_ms: f64,
    pub scan_window_ms: f64,
    pub hash_bytes: usize,
}

/// What the export produced — folded into the sidecar summary.
#[derive(Debug, Clone, Default)]
pub struct ExportStats {
    pub rows: u64,
    pub distinct_device_hashes: u64,
    pub rssi_null: u64,
    pub malformed_lines: u64,
    /// Rows carrying a matched lab identity frame.
    pub lab_frames: u64,
    /// Distinct `lab_participant_key` values seen — the number of consented
    /// handsets this node heard, exact rather than address-bracketed.
    pub distinct_lab_participants: u64,
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
        // ble-rssi/2 — lab identity, null on every non-lab advertisement.
        Arc::new(
            Type::primitive_type_builder("lab_uuid", PhysicalType::BYTE_ARRAY)
                .with_repetition(Repetition::OPTIONAL)
                .with_logical_type(Some(LogicalType::String))
                .build()?,
        ),
        Arc::new(
            Type::primitive_type_builder("lab_participant_key", PhysicalType::INT32)
                .with_repetition(Repetition::OPTIONAL)
                .build()?,
        ),
        Arc::new(
            Type::primitive_type_builder("lab_session_key", PhysicalType::INT32)
                .with_repetition(Repetition::OPTIONAL)
                .build()?,
        ),
    ];
    Ok(Type::group_type_builder("ble_rssi")
        .with_fields(fields)
        .build()?)
}

/// The footer key-value metadata that makes `ble_rssi.parquet` self-describing.
///
/// Everything a reader needs to interpret the file with nothing but the file:
/// the schema id, what stamped the clock, how `device_hash` was derived (and
/// that the salt is gone), and how to decode a matched lab UUID. Keys are
/// namespaced `csid.` so they survive concatenation with other tooling's
/// metadata.
fn footer_metadata(ctx: &ParquetContext) -> Vec<parquet::file::metadata::KeyValue> {
    let kv =
        |k: &str, v: String| parquet::file::metadata::KeyValue::new(format!("csid.{k}"), Some(v));
    let mut meta = vec![
        kv("schema", PARQUET_SCHEMA.to_string()),
        kv("artefact", PARQUET_NAME.to_string()),
        kv("durable_log", NDJSON_NAME.to_string()),
        kv("host", ctx.host.clone()),
        kv("session_id", ctx.session_id.clone()),
        kv("adapter", ctx.adapter.clone()),
        kv("scan_type", "passive".to_string()),
        kv("scan_interval_ms", format!("{}", ctx.scan_interval_ms)),
        kv("scan_window_ms", format!("{}", ctx.scan_window_ms)),
        kv(
            "clock",
            "unix_ts_ns = host wallclock (ns), same call site as the CSI stream's t_ns".to_string(),
        ),
        kv(
            "hash_algorithm",
            "sha256(salt || addr_type || addr)[:hash_bytes], hex".to_string(),
        ),
        kv("hash_bytes", format!("{}", ctx.hash_bytes)),
        kv("salt_persisted", "false".to_string()),
        kv(
            "pseudonym_scope",
            "per-session; unlinkable across sessions by construction".to_string(),
        ),
        kv(
            "rssi_null_means",
            "controller reported the RSSI-unavailable sentinel (127)".to_string(),
        ),
        kv("writer", format!("csid {}", env!("CARGO_PKG_VERSION"))),
        kv("created_unix_ns", format!("{}", crate::util::now_unix_ns())),
    ];
    match &ctx.lab_namespace_uuid {
        Some(ns) => {
            meta.push(kv("lab_namespace_uuid", ns.clone()));
            meta.push(kv("lab_uuid_layout", LAB_UUID_LAYOUT.to_string()));
            meta.push(kv(
                "lab_match_ad_types",
                "0x06, 0x07 (128-bit service UUID lists), 0x21 (128-bit service data)".to_string(),
            ));
        }
        None => meta.push(kv("lab_namespace_uuid", String::new())),
    }
    meta
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
            .set_key_value_metadata(Some(footer_metadata(ctx)))
            .build(),
    );
    let sink = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut writer = SerializedFileWriter::new(sink, schema, props)
        .context("opening the ble_rssi.parquet writer")?;

    let mut stats = ExportStats::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lab_participants: std::collections::HashSet<u16> = std::collections::HashSet::new();
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
                if let Some(key) = obs.lab_participant_key {
                    stats.lab_frames += 1;
                    lab_participants.insert(key);
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
    stats.distinct_lab_participants = lab_participants.len() as u64;
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

    // ble-rssi/2 lab identity columns, all OPTIONAL with the same encoding as
    // rssi_dbm: definition level 1 = present, 0 = null.
    let mut col = rg.next_column()?.context("column lab_uuid missing")?;
    let def: Vec<i16> = batch
        .iter()
        .map(|o| if o.lab_uuid.is_some() { 1 } else { 0 })
        .collect();
    let vals: Vec<ByteArray> = batch
        .iter()
        .filter_map(|o| o.lab_uuid.as_deref().map(ByteArray::from))
        .collect();
    col.typed::<ByteArrayType>()
        .write_batch(&vals, Some(&def), None)?;
    col.close()?;

    for key in [
        |o: &Observation| o.lab_participant_key,
        |o: &Observation| o.lab_session_key,
    ] {
        let mut col = rg.next_column()?.context("lab key column missing")?;
        let def: Vec<i16> = batch
            .iter()
            .map(|o| if key(o).is_some() { 1 } else { 0 })
            .collect();
        let vals: Vec<i32> = batch.iter().filter_map(|o| key(o).map(i32::from)).collect();
        col.typed::<Int32Type>()
            .write_batch(&vals, Some(&def), None)?;
        col.close()?;
    }

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
                rssi: -63,
                data: vec![0x02, 0x01, 0x06],
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
            data: vec![],
        };
        assert_eq!(h.observe(&adv, 42, None).rssi_dbm, None);
        let ok = RawAdv { rssi: -70, ..adv };
        assert_eq!(h.observe(&ok, 42, None).rssi_dbm, Some(-70));
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

    /// The canonical test namespace and its on-air (little-endian) rendering.
    const NAMESPACE: &str = "6d6f6e61-6461-4076-b100-000000000000";

    /// AD payload carrying the lab frame for `participant_key`/`session_key`,
    /// preceded by a Flags structure, exactly as a phone emits it.
    fn lab_adv_data(participant_key: u16, session_key: u16, ad_type: u8) -> Vec<u8> {
        let mut be = [
            0x6d, 0x6f, 0x6e, 0x61, 0x64, 0x61, 0x40, 0x76, 0xb1, 0x00, 0x00, 0x00, 0, 0, 0, 0,
        ];
        be[12..14].copy_from_slice(&participant_key.to_be_bytes());
        be[14..16].copy_from_slice(&session_key.to_be_bytes());
        let mut data = vec![0x02, 0x01, 0x06]; // Flags AD first
        data.push(17); // length = type byte + 16
        data.push(ad_type);
        data.extend(be.iter().rev()); // ON AIR: little-endian
        data
    }

    fn test_ctx() -> ParquetContext {
        ParquetContext {
            host: "monad02".into(),
            session_id: "monad02_lab_20260808-100000".into(),
            adapter: "hci0".into(),
            lab_namespace_uuid: Some(NAMESPACE.to_string()),
            scan_interval_ms: 100.0,
            scan_window_ms: 100.0,
            hash_bytes: 8,
        }
    }

    #[test]
    fn lab_matcher_reads_the_frame_off_the_air_byte_order() {
        let m = LabMatcher::from_namespace(NAMESPACE).unwrap();
        for ad_type in [0x06u8, 0x07, 0x21] {
            let frame = m
                .extract(&lab_adv_data(0xABCD, 0x1234, ad_type))
                .unwrap_or_else(|| panic!("AD type {ad_type:#x} must match"));
            assert_eq!(frame.participant_key, 0xABCD);
            assert_eq!(frame.session_key, 0x1234);
            assert_eq!(frame.uuid, "6d6f6e61-6461-4076-b100-0000abcd1234");
        }
    }

    #[test]
    fn lab_matcher_ignores_foreign_and_malformed_payloads() {
        let m = LabMatcher::from_namespace(NAMESPACE).unwrap();
        // A different namespace: same shape, different prefix.
        let mut foreign = lab_adv_data(1, 2, 0x07);
        *foreign.last_mut().unwrap() ^= 0xFF; // MSB on air = first namespace byte
        assert_eq!(m.extract(&foreign), None);
        // Manufacturer data (an iBeacon, a Continuity frame) is never inspected.
        let ibeacon = [0x1A, 0xFF, 0x4C, 0x00, 0x02, 0x15, 1, 2, 3];
        assert_eq!(m.extract(&ibeacon), None);
        // Malformed: zero-length AD, and a declared length past the buffer.
        assert_eq!(m.extract(&[0x00, 0x07]), None);
        assert_eq!(m.extract(&[0x20, 0x07, 0x01]), None);
        assert_eq!(m.extract(&[]), None);
        // A 16-byte UUID split short (list truncated mid-entry) must not match.
        let mut short = lab_adv_data(1, 2, 0x07);
        short.truncate(short.len() - 1);
        short[3] = 16; // re-declare the shorter length so the walk stays valid
        assert_eq!(m.extract(&short), None);
    }

    #[test]
    fn lab_matcher_refuses_a_malformed_namespace() {
        assert!(LabMatcher::from_namespace("").is_err());
        assert!(LabMatcher::from_namespace("not-a-uuid").is_err());
        assert!(LabMatcher::from_namespace("6d6f6e61-6461-4076-b100-00000000000g").is_err());
        assert!(LabMatcher::from_namespace(NAMESPACE).is_ok());
    }

    #[test]
    fn observe_carries_lab_identity_only_for_matching_frames() {
        let h = DeviceHasher::with_salt([0u8; 32], 8);
        let m = LabMatcher::from_namespace(NAMESPACE).unwrap();
        let lab = RawAdv {
            event_type: 0x03,
            addr_type: 0x01,
            addr: [1, 2, 3, 4, 5, 0x40],
            rssi: -55,
            data: lab_adv_data(7, 9, 0x07),
        };
        let obs = h.observe(&lab, 42, Some(&m));
        assert_eq!(obs.lab_participant_key, Some(7));
        assert_eq!(obs.lab_session_key, Some(9));
        assert!(obs.lab_uuid.as_deref().unwrap().starts_with("6d6f6e61-"));

        let ambient = RawAdv {
            data: vec![0x02, 0x01, 0x06],
            ..lab
        };
        let obs = h.observe(&ambient, 42, Some(&m));
        assert_eq!(obs.lab_uuid, None);
        assert_eq!(obs.lab_participant_key, None);
    }

    #[test]
    fn v1_log_lines_still_parse() {
        // A line written by ble-rssi/1, verbatim shape: no lab fields at all.
        let line = "{\"unix_ts_ns\":1786959444439374442,\"device_hash\":\"a1b2c3d4e5f60708\",\
                    \"addr_kind\":\"rpa_resolvable\",\"pdu_type\":\"adv_ind\",\"rssi_dbm\":-63}";
        let obs: Observation = serde_json::from_str(line).unwrap();
        assert_eq!(obs.rssi_dbm, Some(-63));
        assert_eq!(obs.lab_uuid, None);
        assert_eq!(obs.lab_participant_key, None);
        assert_eq!(obs.lab_session_key, None);
    }

    #[test]
    fn parquet_roundtrips_through_the_log() {
        let dir = std::env::temp_dir().join(format!("csid-ble-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hasher = DeviceHasher::with_salt([3u8; 32], 8);
        let matcher = LabMatcher::from_namespace(NAMESPACE).unwrap();

        let mut log = ObservationLog::create(&dir, 1).unwrap();
        let t0 = 1_800_000_000_000_000_000u64;
        for i in 0..5u64 {
            let adv = RawAdv {
                event_type: 0x00,
                addr_type: 0x01,
                addr: [1, 2, 3, 4, 5, 0x40 | (i as u8 & 1)],
                rssi: if i == 3 { 127 } else { -60 - i as i8 },
                // Rows 0 and 2 are the lab handset; the rest are ambient.
                data: if i % 2 == 0 && i < 4 {
                    lab_adv_data(0x0007, 0x0009, 0x07)
                } else {
                    vec![0x02, 0x01, 0x06]
                },
            };
            log.append(&hasher.observe(&adv, t0 + i * 100_000_000, Some(&matcher)))
                .unwrap();
        }
        let ndjson = log.finish().unwrap();

        let out = dir.join(PARQUET_NAME);
        let stats = export_parquet(&ndjson, &out, &test_ctx()).unwrap();
        assert_eq!(stats.rows, 5);
        assert_eq!(stats.distinct_device_hashes, 2);
        assert_eq!(stats.rssi_null, 1);
        assert_eq!(stats.malformed_lines, 0);
        assert_eq!(stats.lab_frames, 2);
        assert_eq!(stats.distinct_lab_participants, 1);
        assert!(out.metadata().unwrap().len() > 0);

        // Self-description: the footer alone must identify the file.
        use parquet::file::reader::FileReader;
        let reader =
            parquet::file::reader::SerializedFileReader::new(File::open(&out).unwrap()).unwrap();
        let file_meta = reader.metadata().file_metadata().clone();
        assert_eq!(
            file_meta.schema_descr().num_columns(),
            11,
            "ble-rssi/2 is eleven columns"
        );
        let kvs = file_meta.key_value_metadata().expect("footer metadata");
        let get = |key: &str| {
            kvs.iter()
                .find(|kv| kv.key == format!("csid.{key}"))
                .and_then(|kv| kv.value.clone())
                .unwrap_or_else(|| panic!("footer key csid.{key} missing"))
        };
        assert_eq!(get("schema"), PARQUET_SCHEMA);
        assert_eq!(get("host"), "monad02");
        assert_eq!(get("session_id"), "monad02_lab_20260808-100000");
        assert_eq!(get("lab_namespace_uuid"), NAMESPACE);
        assert_eq!(get("salt_persisted"), "false");
        assert_eq!(get("lab_uuid_layout"), LAB_UUID_LAYOUT);

        // A truncated trailing line (power cut) costs that line, nothing more.
        let mut text = std::fs::read_to_string(&ndjson).unwrap();
        text.push_str("{\"unix_ts_ns\":1,\"devi");
        std::fs::write(&ndjson, text).unwrap();
        let stats = export_parquet(&ndjson, &out, &test_ctx()).unwrap();
        assert_eq!(stats.rows, 5);
        assert_eq!(stats.malformed_lines, 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
