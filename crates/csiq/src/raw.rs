//! Parser for the **raw iax vendor-event stream** — the lossless, driver-native
//! bytes `csid` writes verbatim to `capture.raw`.
//!
//! Wire framing (all length prefixes big-endian, header fields little-endian):
//!
//! ```text
//! [be32 msg_len][be32 hdr_len=272][hdr (hdr_len bytes)][be32 csi_len][csi]
//! ```
//!
//! The 272-byte header layout below is transcribed from the upstream iax
//! MATLAB/Python reader and the project's `csi_parse.py` (IP-120). Offsets are
//! centralised here as named constants so a firmware/driver revision that moves
//! a field is a one-line change and never a silent misparse. **These offsets are
//! validated against the AX210 + iax on-hardware capture; treat them as the
//! authoritative-but-driver-coupled layer.**

use std::io::{self, Read};

use crate::error::{CsiqError, Result};
use crate::record::{CsiRecord, Modulation, PhyLabel, Width};

/// The header length the iax stream declares (and this parser expects).
pub const HEADER_LEN: usize = 272;

// Little-endian field offsets within the 272-byte header.
mod off {
    pub const FTM: usize = 8; // u32, 320 MHz baseband clock
    pub const NRX: usize = 46; // u8
    pub const NTX: usize = 47; // u8
    pub const NTONE: usize = 52; // u16 (low bytes of the vendor struct's u32)
    pub const RSSI_A: usize = 60; // u8 magnitude; 61..64 reserved
    pub const RSSI_B: usize = 64; // u8 magnitude; 65..68 reserved
    pub const SRC_MAC: usize = 68; // [u8; 6]
    pub const SEQ: usize = 76; // u8
    pub const US: usize = 88; // u32, microsecond fw clock
    pub const RNF: usize = 92; // u32, rate_n_flags v2
                               // flq extension (present when hdr_len covers these offsets):
    pub const UNIX_TS_NS: usize = 208; // u64
    pub const CHANNEL: usize = 216; // u8 (driver writes one byte); 217..220 reserved
}

fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Convert the header's RSSI field into dBm.
///
/// The field is a **`u8` positive magnitude** (per the vendor reader's own
/// `iaxcsi.h`: `uint8_t opp_rssi1; uint8_t v61[3];` — the three bytes that
/// follow are padding, observed zero across 703 660 records). Both the C++ and
/// MATLAB reference readers print `-opp_rssi1`, so negating here means a
/// `CsiRecord` always carries ordinary negative dBm and the sign convention is
/// stated once, in the format, rather than in every consumer.
///
/// Measured valid range on the reference node: **-18 … -89 dBm**, a smooth
/// unimodal continuum over 659 083 records with no spikes.
///
/// The magnitude `0x7F` is Intel's "not available" sentinel — the same value as
/// `IWL_NOISE_MEAS_NOT_AVAILABLE (-127)` in `dvm/dev.h`, chosen there because it
/// "is below the range of measurable". It is not a weak measurement: -127 dBm is
/// ~26 dB below the thermal noise floor of a 20 MHz channel, and the CSI block
/// for such a chain is a byte-identical stale copy of a previous frame. See
/// [`RSSI_NO_MEASUREMENT`] and the consumer rule in the format spec.
fn rssi_dbm(raw: u8) -> i16 {
    -(raw as i16)
}

/// Decode `rate_n_flags` v2 into a [`PhyLabel`].
///
/// v2 layout (iwlwifi): MCS in bits 0–3, NSS-1 in bits 4–5, modulation type in
/// bits 8–10. Returned `None` when `rnf == 0` (field unavailable).
pub fn decode_rnf(rnf: u32) -> Option<PhyLabel> {
    if rnf == 0 {
        return None;
    }
    let mcs = (rnf & 0x0F) as u8;
    let nss = (((rnf >> 4) & 0x03) as u8) + 1;
    let mod_type = ((rnf >> 8) & 0x07) as u8;
    Some(PhyLabel {
        modulation: Modulation::from_rnf_type(mod_type),
        mcs,
        nss,
    })
}

/// Parse a single fixed 272-byte header + CSI matrix into a [`CsiRecord`].
///
/// `width` is not present in the raw header (it is a session-level property);
/// the caller supplies the monitor width it configured. `csi` is the raw CSI
/// byte payload (interleaved little-endian `i16` I/Q).
pub fn parse_record(hdr: &[u8], csi: &[u8], width: Width) -> Result<CsiRecord> {
    if hdr.len() < off::RNF + 4 {
        return Err(CsiqError::Malformed("header shorter than base fields"));
    }
    let nrx = hdr[off::NRX];
    let ntx = hdr[off::NTX];
    let ntone = le_u16(hdr, off::NTONE);
    let rnf = le_u32(hdr, off::RNF);

    let mut src_mac = [0u8; 6];
    src_mac.copy_from_slice(&hdr[off::SRC_MAC..off::SRC_MAC + 6]);

    // Per-RX-chain RSSI (two entries in the header; keep `nrx` of them),
    // **negated into dBm** — see [`rssi_dbm`].
    let rssi_all = [rssi_dbm(hdr[off::RSSI_A]), rssi_dbm(hdr[off::RSSI_B])];
    let rssi = rssi_all[..(nrx as usize).min(2)].to_vec();

    // flq extension fields are only present on long-enough headers.
    let (unix_ts_ns, channel) = if hdr.len() > off::CHANNEL {
        (le_u64(hdr, off::UNIX_TS_NS), hdr[off::CHANNEL] as u32)
    } else {
        (0, 0)
    };

    if csi.len() % 2 != 0 {
        return Err(CsiqError::Malformed("odd CSI byte length"));
    }
    let iq: Vec<i16> = csi
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    Ok(CsiRecord {
        ftm: le_u32(hdr, off::FTM),
        us: le_u32(hdr, off::US),
        unix_ts_ns,
        rnf,
        phy: decode_rnf(rnf),
        seq: hdr[off::SEQ],
        nrx,
        ntx,
        ntone,
        rssi,
        src_mac,
        channel,
        width,
        iq,
    })
}

/// Streaming reader over a raw iax capture.
pub struct RawReader<R: Read> {
    inner: R,
    width: Width,
}

impl<R: Read> RawReader<R> {
    /// Wrap a raw byte source. `width` is the monitor width the session used
    /// (stamped onto every record, since the raw header does not carry it).
    pub fn new(inner: R, width: Width) -> Self {
        RawReader { inner, width }
    }

    /// Read the next record, or `Ok(None)` at clean end of stream.
    pub fn next_record(&mut self) -> Result<Option<CsiRecord>> {
        let msg_len = match read_be_u32(&mut self.inner) {
            Ok(n) => n as usize,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut body = vec![0u8; msg_len];
        self.inner.read_exact(&mut body)?;
        if body.len() < 4 {
            return Err(CsiqError::Malformed("message body too short for hdr_len"));
        }
        let hdr_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        let hdr_start = 4;
        let hdr_end = hdr_start + hdr_len;
        if body.len() < hdr_end + 4 {
            return Err(CsiqError::Malformed(
                "message body too short for header + csi_len",
            ));
        }
        let hdr = &body[hdr_start..hdr_end];
        let csi_len = u32::from_be_bytes([
            body[hdr_end],
            body[hdr_end + 1],
            body[hdr_end + 2],
            body[hdr_end + 3],
        ]) as usize;
        let csi_start = hdr_end + 4;
        let csi_end = csi_start + csi_len;
        if body.len() < csi_end {
            return Err(CsiqError::Malformed(
                "message body too short for csi payload",
            ));
        }
        let csi = &body[csi_start..csi_end];
        Ok(Some(parse_record(hdr, csi, self.width)?))
    }
}

impl<R: Read> Iterator for RawReader<R> {
    type Item = Result<CsiRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

fn read_be_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
