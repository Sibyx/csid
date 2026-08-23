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
