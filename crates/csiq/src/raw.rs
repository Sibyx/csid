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
use crate::record::{Bandwidth, BwAntsel, CsiRecord, Modulation, NodeState, PhyLabel, Width};

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
    pub const MONO_US: usize = 200; // u64, CLOCK_MONOTONIC microseconds
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

/// `rate_n_flags` **version 2** field positions.
///
/// Transcribed from the driver this project actually runs — `rs.h` in the
/// pinned `iax` tree (`drivers/net/wireless/intel/iwlwifi/fw/api/rs.h`,
/// upstream ref `20d21a7f`), section "rate_n_flags bit field version 2". Read
/// from `/usr/src/iax-csi-iwlwifi-6x-port` on a deployed node, 2026-08-24.
///
/// v1 and v2 disagree about almost every field above bit 7, so a v1 constant
/// used here would misparse silently. The names below are the driver's.
mod rnf {
    /// Bits 10–8: `RATE_MCS_MOD_TYPE_MSK`.
    pub const MOD_TYPE_POS: u32 = 8;
    pub const MOD_TYPE_MSK: u32 = 0x7;

    /// Bits 13–11: `RATE_MCS_CHAN_WIDTH_MSK`.
    /// (0) 20 (1) 40 (2) 80 (3) 160 (4) 320 MHz.
    pub const CHAN_WIDTH_POS: u32 = 11;
    pub const CHAN_WIDTH_MSK: u32 = 0x7;

    /// Bits 15–14: antenna selection. Bit 14 = antenna A, bit 15 = antenna B.
    /// `RATE_MCS_ANT_A_MSK` is `1 << 14`, `RATE_MCS_ANT_B_MSK` is `2 << 14`.
    pub const ANT_POS: u32 = 14;
    pub const ANT_MSK: u32 = 0x3;
}

/// Decode the per-frame bandwidth and antenna selection from `rate_n_flags`.
///
/// These bits have been in every record csid has ever written — `rnf` is stored
/// verbatim as TLV `0x04` — and were parsed away. Recovering them needs no new
/// capture and no driver change, only a decoder.
///
/// Returns `None` for `rnf == 0`, which is how the header reports "no rate
/// information", exactly as [`decode_rnf`] does.
///
/// # Validated against real captures
///
/// The header gives the layout; these are the records. Decoded across 82 cached
/// `.csiq` files, 159,716 records, 2026-08-24:
///
/// | modulation | bandwidth | antenna | tones | chains | records |
/// |---|---|---|---:|---:|---:|
/// | legacy OFDM | 20 MHz | A+B | 52 | 2 | 158,541 |
/// | HT | 20 MHz | A+B | 56 | 2 | 1,105 |
/// | HE | 20 MHz | A+B | 242 | 2 | 68 |
/// | legacy OFDM | 20 MHz | B | 52 | 2 | 2 |
///
/// Every row is internally consistent: 52, 56 and 242 tones are exactly what
/// legacy, HT and HE produce **in 20 MHz**. A misread width field would have
/// paired 242 tones with 80 MHz, which is VHT80's geometry and not HE20's.
/// Bits 16 and above are set on precisely the 1,173 non-legacy records
/// (LDPC/STBC/HE fields), which is a second independent consistency check.
///
/// Two limits, stated because they matter:
///
/// * **No non-zero bandwidth code has been observed in the wild.** The corpus is
///   99.76% one PHY geometry, so 20 MHz is all there is to see. The 40/80/160
///   codes rest on the driver header alone until a wide capture exists.
/// * Bits 7–5 were zero in all 159,716 records, which is why the NSS mask
///   divergence noted in [`decode_rnf`] is latent rather than active.
pub fn decode_bw_antsel(rnf: u32) -> Option<BwAntsel> {
    if rnf == 0 {
        return None;
    }
    Some(BwAntsel {
        bandwidth: Bandwidth::from_code(
            ((rnf >> rnf::CHAN_WIDTH_POS) & rnf::CHAN_WIDTH_MSK) as u8,
        ),
        antenna_sel: ((rnf >> rnf::ANT_POS) & rnf::ANT_MSK) as u8,
    })
}

/// The monotonic microsecond clock at header offset 200.
///
/// The fleet has no RTC, so nodes boot in the past and `chrony` steps them —
/// and a step mid-capture shifts every `unix_ts_ns` with no symptom in the file.
/// `ftm` wraps every 13.42 s and `us` every 71.6 min, so this is the only
/// monotonic wall-time a record carries.
///
/// # Zero means "own transmission", not "unavailable"
///
/// Measured over 2,433 records of a real illuminated capture, as an exact
/// biconditional with no exceptions:
///
/// | source MAC | `mono_us` set | records |
/// |---|---|---:|
/// | `ef:be:ad:de:ad:de` (this node's injector) | **no** | 1,743 |
/// | three ambient transmitters | **yes** | 690 |
///
/// A locally generated frame never traverses the receive path that stamps this
/// clock, so the field reads zero. That makes `None` a *semantic* marker — the
/// record is the node's own transmission looped back — and it is the only
/// per-record marker for that fact the CSI stream has. The timesync receiver
/// counts the same thing separately as `own_transmissions`.
///
/// Verified on one node and one capture. Treat a future capture that breaks the
/// biconditional as a finding, not as noise.
///
/// Sanity of the clock itself, same file: strictly monotonic over its non-zero
/// samples, spanning 60.007 s against the host clock's 60.008 s — 3 ppm — with
/// an implied uptime of 110.9 h at the first sample.
pub fn decode_mono_us(hdr: &[u8]) -> Option<u64> {
    if hdr.len() < off::MONO_US + 8 {
        return None;
    }
    let v = le_u64(hdr, off::MONO_US);
    (v != 0).then_some(v)
}

/// Decode `rate_n_flags` v2 into a [`PhyLabel`].
///
/// v2 layout (iwlwifi): MCS in bits 0–3, NSS-1 in bits 4–5, modulation type in
/// bits 8–10. Returned `None` when `rnf == 0` (field unavailable).
pub fn decode_rnf(rnf: u32) -> Option<PhyLabel> {
    if rnf == 0 {
        return None;
    }
    // NOTE, 2026-08-24: two divergences from `rs.h` v2, left as they are
    // because changing them would alter `PhyLabel` on every record ever read,
    // and neither can produce a wrong value on data seen so far.
    //
    //  * NSS is `RATE_MCS_NSS_MSK = 1 << 4` — a SINGLE bit, 0 = one stream,
    //    1 = two. Reading two bits here would report NSS 3 or 4 if bit 5 were
    //    ever set. Bits 7–5 are reserved and have been zero in every record
    //    decoded, and the AX210 has two antennas, so no such value can be real.
    //  * The legacy rate index is `RATE_LEGACY_RATE_MSK = 0x7` (bits 2–0),
    //    while `RATE_MCS_CODE_MSK = 0xf` applies to HT/VHT/HE/EHT MCS. The mask
    //    is therefore modulation-dependent, and 0xF is one bit too wide for a
    //    legacy record.
    let mcs = (rnf & 0x0F) as u8;
    let nss = (((rnf >> 4) & 0x03) as u8) + 1;
    let mod_type = ((rnf >> rnf::MOD_TYPE_POS) & rnf::MOD_TYPE_MSK) as u8;
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
    parse_record_opts(hdr, csi, width, false)
}

/// [`parse_record`], with control over whether the driver header is kept.
///
/// `keep_vendor_hdr` stores the 272 bytes verbatim on the record. It is off by
/// default because the blob triples a 52-tone record's metadata, and on only
/// where the caller has somewhere to put it — the exporter, which writes a
/// compressed container.
pub fn parse_record_opts(
    hdr: &[u8],
    csi: &[u8],
    width: Width,
    keep_vendor_hdr: bool,
) -> Result<CsiRecord> {
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
        bw_antsel: decode_bw_antsel(rnf),
        mono_us: decode_mono_us(hdr),
        // The parser does not keep the blob by default: it is 272 B per record
        // and the caller decides whether lossless provenance is worth it.
        vendor_hdr: keep_vendor_hdr.then(|| hdr.to_vec()),
        node: NodeState::default(),
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
    keep_vendor_hdr: bool,
}

impl<R: Read> RawReader<R> {
    /// Wrap a raw byte source. `width` is the monitor width the session used
    /// (stamped onto every record, since the raw header does not carry it).
    pub fn new(inner: R, width: Width) -> Self {
        RawReader {
            inner,
            width,
            keep_vendor_hdr: false,
        }
    }

    /// Keep the 272-byte driver header verbatim on every record it yields.
    ///
    /// Builder-style rather than a second constructor: the exporter is the only
    /// caller that wants the blob, and a `new_with_flags` would make every other
    /// call site state a preference it does not have.
    pub fn keeping_vendor_hdr(mut self, keep: bool) -> Self {
        self.keep_vendor_hdr = keep;
        self
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
        Ok(Some(parse_record_opts(hdr, csi, self.width, self.keep_vendor_hdr)?))
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
