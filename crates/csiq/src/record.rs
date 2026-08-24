//! The semantic CSI record — the in-memory representation shared by the file
//! container, the live stream, and the raw-stream parser.

use serde::{Deserialize, Serialize};

/// Channel width of the *monitor* interface that captured the record.
///
/// This bounds what CSI type is decodable; the actual per-record tone count
/// (`ntone`) follows the received frame (a 20 MHz HE frame yields 242 tones
/// even on a 160 MHz monitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Width {
    Noht,
    Ht20,
    Ht40Minus,
    Ht40Plus,
    W80,
    W160,
    /// 802.11be 320 MHz — reserved; the AX210 cannot reach this, kept so the
    /// format need not change when EHT hardware arrives.
    W320,
    /// Width the capturer could not classify — value carried verbatim.
    Unknown(u16),
}

impl std::fmt::Display for Width {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Width::Unknown(v) => write!(f, "unknown({v})"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Width {
    /// Encode to the on-wire `u16` used by the `Width` TLV.
    pub fn to_code(self) -> u16 {
        match self {
            Width::Noht => 0,
            Width::Ht20 => 1,
            Width::Ht40Minus => 2,
            Width::Ht40Plus => 3,
            Width::W80 => 4,
            Width::W160 => 5,
            Width::W320 => 6,
            Width::Unknown(v) => v,
        }
    }

    /// The width as `iw` spells it — the same token the TOML configuration
    /// uses, so one vocabulary covers config, capture and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Width::Noht => "NOHT",
            Width::Ht20 => "HT20",
            Width::Ht40Minus => "HT40-",
            Width::Ht40Plus => "HT40+",
            Width::W80 => "80MHz",
            Width::W160 => "160MHz",
            Width::W320 => "320MHz",
            Width::Unknown(_) => "unknown",
        }
    }

    /// Decode from the on-wire `u16`.
    pub fn from_code(v: u16) -> Self {
        match v {
            0 => Width::Noht,
            1 => Width::Ht20,
            2 => Width::Ht40Minus,
            3 => Width::Ht40Plus,
            4 => Width::W80,
            5 => Width::W160,
            6 => Width::W320,
            other => Width::Unknown(other),
        }
    }
}

/// Channel bandwidth **of the received frame**, decoded from `rate_n_flags` v2.
///
/// Not to be confused with [`Width`], which is the *configured monitor width* —
/// a session constant that bounds what is decodable and does not describe any
/// individual frame. An ambient channel interleaves PHY types frame by frame,
/// so on those captures `Width` is the wrong answer for every record and this
/// is the right one.
///
/// Codes are the driver's own (`RATE_MCS_CHAN_WIDTH_*`), so the wire value is
/// the firmware value and no table sits between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bandwidth {
    W20,
    W40,
    W80,
    W160,
    /// 802.11be 320 MHz — the AX210 cannot reach this, kept so the format need
    /// not change when EHT hardware arrives.
    W320,
    /// A code this build does not know, carried verbatim.
    Unknown(u8),
}

impl Bandwidth {
    /// The driver's `RATE_MCS_CHAN_WIDTH_*` code.
    pub fn to_code(self) -> u8 {
        match self {
            Bandwidth::W20 => 0,
            Bandwidth::W40 => 1,
            Bandwidth::W80 => 2,
            Bandwidth::W160 => 3,
            Bandwidth::W320 => 4,
            Bandwidth::Unknown(v) => v,
        }
    }

    /// Decode the driver's `RATE_MCS_CHAN_WIDTH_*` code.
    pub fn from_code(v: u8) -> Self {
        match v {
            0 => Bandwidth::W20,
            1 => Bandwidth::W40,
            2 => Bandwidth::W80,
            3 => Bandwidth::W160,
            4 => Bandwidth::W320,
            other => Bandwidth::Unknown(other),
        }
    }

    /// Nominal channel bandwidth in MHz, or `None` for an unknown code.
    ///
    /// This is the *channel*, not the occupied tone span. A 20 MHz HE frame
    /// carries 242 tones at 78.125 kHz, which occupy about 18.9 MHz.
    pub fn mhz(self) -> Option<u32> {
        match self {
            Bandwidth::W20 => Some(20),
            Bandwidth::W40 => Some(40),
            Bandwidth::W80 => Some(80),
            Bandwidth::W160 => Some(160),
            Bandwidth::W320 => Some(320),
            Bandwidth::Unknown(_) => None,
        }
    }
}

impl std::fmt::Display for Bandwidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.mhz() {
            Some(m) => write!(f, "{m}MHz"),
            None => write!(f, "unknown({})", self.to_code()),
        }
    }
}

/// Per-frame bandwidth and antenna selection, recovered from `rate_n_flags`.
///
/// Both fields were parsed into the same 32-bit word csid has always stored and
/// then discarded. They cost no extra capture and no driver change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BwAntsel {
    pub bandwidth: Bandwidth,
    /// Active-antenna bitmask: bit 0 = antenna A, bit 1 = antenna B.
    ///
    /// The driver's own encoding (`RATE_MCS_ANT_A_MSK` / `_B_MSK` shifted down
    /// to bit 0). `0` means the word named no antenna, which is what a receive
    /// record normally carries — the field is set on transmit descriptors.
    pub antenna_sel: u8,
}

impl BwAntsel {
    /// True when antenna A is named active.
    pub fn ant_a(self) -> bool {
        self.antenna_sel & 0b01 != 0
    }

    /// True when antenna B is named active.
    pub fn ant_b(self) -> bool {
        self.antenna_sel & 0b10 != 0
    }
}

/// PHY modulation family, decoded from `rate_n_flags` v2 (iwlwifi).
///
/// Measured on the AX210: HE 2×2 records carry `Modulation::He`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modulation {
    Cck,
    LegacyOfdm,
    Ht,
    Vht,
    He,
    /// 802.11be — reserved for forward compatibility.
    Eht,
    Unknown(u8),
}

impl Modulation {
    /// The `rate_n_flags` v2 modulation-type nibble → [`Modulation`].
    pub fn from_rnf_type(t: u8) -> Self {
        match t {
            0 => Modulation::Cck,
            1 => Modulation::LegacyOfdm,
            2 => Modulation::Ht,
            3 => Modulation::Vht,
            4 => Modulation::He,
            5 => Modulation::Eht,
            other => Modulation::Unknown(other),
        }
    }
}

/// RSSI value meaning **this chain reported no measurement**.
///
/// The firmware writes the magnitude `0x7F`, which negates to `-127` dBm —
/// Intel's documented "not available" marker (`IWL_NOISE_MEAS_NOT_AVAILABLE`).
/// It is not a weak signal: -127 dBm sits ~26 dB below the thermal noise floor
/// of a 20 MHz channel.
///
/// **Consumer rule:** when a chain reports this, that chain's slice of the CSI
/// matrix is a byte-identical stale copy of an earlier frame, not a fresh
/// measurement — discard it. Verified as an exact biconditional over 44 577
/// records: a chain reads `-127` if and only if its CSI block is a duplicate.
/// Records with one valid chain remain usable single-chain.
pub const RSSI_NO_MEASUREMENT: i16 = -127;

/// Decoded PHY label for a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhyLabel {
    pub modulation: Modulation,
    pub mcs: u8,
    /// Number of spatial streams.
    pub nss: u8,
}

/// Node and host state, sampled periodically rather than per record.
///
/// A capture a stranger can interpret must carry the conditions it was taken
/// under, and this project's own experience says which ones matter: the SoC die
/// temperature orders phase drift, a throttled node is a different instrument,
/// and a spool at its floor is how a 16-hour arm loses its last three hours.
///
/// Every field is optional because the sampler attaches a tick to the FIRST
/// record after it fires. Most records carry none, and a reader must treat these
/// as a sparse series, never as a per-record column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// SoC die temperature, millidegrees Celsius.
    pub temp_mc: Option<i32>,
    /// Raspberry Pi throttle bitmask. Non-zero means the SoC was capped.
    pub throttle_flags: Option<u32>,
    /// Bytes free on the capture spool filesystem.
    pub spool_free_bytes: Option<u64>,
    /// 1-minute load average times 1000.
    pub load_m: Option<u32>,
}

impl NodeState {
    /// True when the sampler attached nothing to this record.
    pub fn is_empty(&self) -> bool {
        self.temp_mc.is_none()
            && self.throttle_flags.is_none()
            && self.spool_free_bytes.is_none()
            && self.load_m.is_none()
    }
}

/// A single CSI record: the triple-clock provenance, PHY labels, per-chain
/// RSSI, geometry, source, and the raw CSI matrix.
///
/// Timing rule (see the CSIQ spec): **analyse on `ftm`** (the 320 MHz RF-plane
/// clock, 3.125 ns, wraps every ~13.42 s — unwrap with [`unwrap_ftm`]) and
/// **anchor wallclock on `unix_ts_ns`**.
///
/// [`unwrap_ftm`]: crate::unwrap_ftm
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsiRecord {
    /// 320 MHz baseband timestamp (3.125 ns tick). Wraps at 2^32 ticks.
    pub ftm: u32,
    /// Firmware microsecond clock (wraps ~71.6 min). Third timescale.
    pub us: u32,
    /// Kernel wallclock at vendor-event delivery (nanoseconds since epoch).
    pub unix_ts_ns: u64,
    /// Raw `rate_n_flags` v2 word (kept verbatim; `phy` is its decode).
    pub rnf: u32,
    /// Decoded PHY label (may be absent if `rnf` was unavailable).
    pub phy: Option<PhyLabel>,
    /// Per-frame bandwidth and antenna selection, decoded from the same `rnf`.
    ///
    /// `None` on a record written before CSIQ carried the field, and on any
    /// record whose `rnf` was unavailable. Absent is not 20 MHz.
    #[serde(default)]
    pub bw_antsel: Option<BwAntsel>,
    /// `CLOCK_MONOTONIC` microseconds, the clock an NTP step cannot distort.
    ///
    /// **`None` means this record is the node's own transmission looped back**,
    /// not that the clock was unavailable. Measured as an exact biconditional
    /// over 2,433 records on one node — see [`crate::raw::decode_mono_us`].
    #[serde(default)]
    pub mono_us: Option<u64>,
    /// The 272-byte driver header verbatim, when the writer kept it.
    ///
    /// Lossless provenance: a field this build cannot name is still in here at
    /// the offset Appendix A gives it, so a later reader can recover it with no
    /// re-capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_hdr: Option<Vec<u8>>,
    /// Node and host state, when the sampler attached a tick to this record.
    #[serde(default, skip_serializing_if = "NodeState::is_empty")]
    pub node: NodeState,
    /// 802.11 sequence byte (NOT a reliable completeness counter — see spec).
    pub seq: u8,
    /// Number of RX chains.
    pub nrx: u8,
    /// Number of TX spatial streams in the sounding.
    pub ntx: u8,
    /// Number of subcarriers (tones): 52/56/242/484/996/1992 (…4096 for EHT).
    pub ntone: u16,
    /// Per-chain RSSI in **dBm** (negative), one entry per RX chain.
    ///
    /// The absolute amplitude reference for the record: `iq` is AGC-*relative*
    /// and carries channel *shape* only, so any absolute scale must come from
    /// here. `0` means the chain reported no measurement, not 0 dBm.
    ///
    /// The driver delivers this as a positive magnitude; the raw parser negates
    /// it, so the sign convention is applied once and never again.
    pub rssi: Vec<i16>,
    /// Source MAC of the sounded frame.
    pub src_mac: [u8; 6],
    /// Control-channel index (802.11 channel number).
    pub channel: u32,
    /// Monitor-interface width at capture time.
    pub width: Width,
    /// Interleaved I/Q, `i16`, length `2 * ntone * nrx * ntx`, row-major over
    /// `[tone][chain]`. Stored verbatim, so the amplitude carries the
    /// receiver's gain setting as well as the channel — see `rssi`.
    pub iq: Vec<i16>,
}

impl CsiRecord {
    /// Which RX chains actually measured this record.
    ///
    /// A `false` entry means the chain reported [`RSSI_NO_MEASUREMENT`] and its
    /// CSI is stale — exclude it rather than treating it as a weak signal.
    pub fn chains_measured(&self) -> Vec<bool> {
        self.rssi
            .iter()
            .map(|r| *r != RSSI_NO_MEASUREMENT)
            .collect()
    }

    /// True when every reported chain carries a real measurement.
    pub fn fully_measured(&self) -> bool {
        !self.rssi.is_empty() && self.rssi.iter().all(|r| *r != RSSI_NO_MEASUREMENT)
    }

    /// Number of complex CSI coefficients (`ntone * nrx * ntx`).
    pub fn coeff_count(&self) -> usize {
        self.ntone as usize * self.nrx as usize * self.ntx as usize
    }

    /// Materialise the CSI matrix as `(re, im)` `f32` pairs in **tone-major**
    /// order, i.e. `index(tone t, chain c) = t * (nrx*ntx) + c`.
    ///
    /// Note the two conversions this performs, because the stored bytes are the
    /// driver's and neither is what a naive read assumes:
    ///
    /// * storage is **chain-major** — `nrx*ntx` contiguous blocks of `ntone`
    ///   coefficients — so this de-interleaves into the tone-major view;
    /// * each coefficient is stored **imaginary first, then real**.
    ///
    /// Returns `None` if `iq` does not match the declared dimensions.
    pub fn complex(&self) -> Option<Vec<(f32, f32)>> {
        let n = self.coeff_count();
        if self.iq.len() != 2 * n || n == 0 {
            return None;
        }
        let chains = self.nrx as usize * self.ntx as usize;
        let ntone = self.ntone as usize;
        let mut out = Vec::with_capacity(n);
        for t in 0..ntone {
            for c in 0..chains {
                let i = 2 * (c * ntone + t); // chain-major storage
                out.push((self.iq[i + 1] as f32, self.iq[i] as f32)); // im, re
            }
        }
        Some(out)
    }

    /// One chain's frequency response as `(re, im)` pairs, tone-ordered.
    ///
    /// Prefer this over slicing [`complex`](Self::complex): it reads the
    /// chain's contiguous block directly.
    pub fn chain(&self, chain: usize) -> Option<Vec<(f32, f32)>> {
        let chains = self.nrx as usize * self.ntx as usize;
        let ntone = self.ntone as usize;
        if chain >= chains || self.iq.len() != 2 * self.coeff_count() || ntone == 0 {
            return None;
        }
        Some(
            (0..ntone)
                .map(|t| {
                    let i = 2 * (chain * ntone + t);
                    (self.iq[i + 1] as f32, self.iq[i] as f32)
                })
                .collect(),
        )
    }
}
