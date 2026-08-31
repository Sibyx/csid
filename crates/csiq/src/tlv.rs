//! The record TLV codec — the heart of CSIQ's self-description.
//!
//! A record payload is a flat sequence of `[u8 type][u32 len][value]` fields.
//! Unknown types are skipped on read, so a v1 reader tolerates records written
//! by a later minor version, and a later reader tolerates v1 records. The CSI
//! matrix is just another (large) TLV.

use crate::error::{CsiqError, Result};
use crate::record::{Bandwidth, BwAntsel, CsiRecord, Modulation, NodeState, PhyLabel, Width};

// -- TLV type codes -----------------------------------------------------------
// 0x00        reserved / padding
pub const T_FTM: u8 = 0x01; // u32
pub const T_US: u8 = 0x02; // u32
pub const T_UNIX_TS_NS: u8 = 0x03; // u64
pub const T_RNF: u8 = 0x04; // u32
pub const T_PHY: u8 = 0x05; // u8 mod, u8 mcs, u8 nss
pub const T_NRX: u8 = 0x06; // u8
pub const T_NTX: u8 = 0x07; // u8
pub const T_NTONE: u8 = 0x08; // u16
pub const T_SRC_MAC: u8 = 0x09; // [u8; 6]
pub const T_CHANNEL: u8 = 0x0A; // u32
pub const T_WIDTH: u8 = 0x0B; // u16
pub const T_RSSI: u8 = 0x0C; // i16[nrx]
pub const T_SEQ: u8 = 0x0D; // u8
pub const T_CSI_MATRIX: u8 = 0x10; // i16[2 * ntone * nrx * ntx] (I/Q interleaved)

/// Per-frame bandwidth and antenna selection: `u8 bandwidth_code, u8 antenna_sel`.
///
/// First of the `0x11..0x1F` range, which is reserved for fields recovered from
/// the 272-byte driver header. It takes the first slot because its bytes were
/// already parsed and discarded — see [`crate::raw::decode_bw_antsel`].
///
/// This is the frame's own width. [`T_WIDTH`] (`0x0B`) is the *configured
/// monitor width*, a session constant, and is retained unchanged.
pub const T_BW_ANTSEL: u8 = 0x11;

/// `u64` — `CLOCK_MONOTONIC` microseconds, from driver-header offset 200.
///
/// The clock an NTP step cannot distort, on a fleet with no RTC. `UNIX_TS_NS`
/// is exactly the field a step corrupts, `FTM` wraps every 13.42 s and `US`
/// every 71.6 min, so this is the only monotonic wall-time the record carries.
///
/// **Absent means the record is the node's own transmission**, not that the
/// clock was unavailable — see [`crate::raw::decode_mono_us`].
pub const T_MONO_US: u8 = 0x12;

// 0x13 is deliberately NOT allocated. IP-130 proposed a `REC_COUNTER` here; the
// header has none distinct from `SEQ` (0x0D), which is already a driver record
// counter. See the SEQ corrigendum in the format spec.

/// The 272-byte driver header, verbatim.
///
/// Stored whole rather than only the unclaimed bytes, so a field promoted out of
/// it later keeps the offset Appendix A gives it. That is the property that lets
/// a future reader recover a field this build did not know how to name.
pub const T_VENDOR_HDR: u8 = 0x14;

// 0x15..0x1F  reserved: further 272-byte-header recoveries
// 0x20..0x2F  reserved: EHT-specific (RU allocation, per-RU tone maps, …)
// 0x30..0x3F  reserved: 802.11bf sensing metadata

// -- 0x40..0x4F: node and host state, sampled periodically --------------------
//
// Not per-record measurements. The sampler attaches them to the first record
// after each tick, so most records carry none and a reader treats them as a
// sparse series rather than a column. They are in the FILE, not only in the
// metrics store, because a published capture must be interpretable by someone
// who has only the file and the spec — and that reader has no Mimir.

/// `i32` — SoC die temperature in millidegrees Celsius.
pub const T_NODE_TEMP_MC: u8 = 0x40;
/// `u32` — Raspberry Pi throttle flag bitmask (`vcgencmd get_throttled`).
pub const T_NODE_THROTTLE: u8 = 0x41;
/// `u64` — bytes free on the capture spool filesystem.
pub const T_NODE_SPOOL_FREE: u8 = 0x42;
/// `u32` — 1-minute load average times 1000.
pub const T_NODE_LOAD_M: u8 = 0x43;
/// `i32` — Wi-Fi NIC die temperature in whole degrees Celsius.
///
/// Degrees rather than millidegrees, unlike [`T_NODE_TEMP_MC`]: the driver's
/// `nic_temp` reports the firmware's DTS reading as an integer °C, and scaling
/// it would assert precision the sensor does not report.
pub const T_NODE_NIC_TEMP_C: u8 = 0x44;

// -- little cursor over a byte slice ------------------------------------------

struct Buf<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Buf<'a> {
    fn new(b: &'a [u8]) -> Self {
        Buf { b, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(CsiqError::Malformed("tlv value past end of payload"));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

// -- write helpers ------------------------------------------------------------

fn put_tlv(out: &mut Vec<u8>, ty: u8, value: &[u8]) {
    out.push(ty);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

fn i16s_to_bytes(v: &[i16]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 2);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn bytes_to_i16s(b: &[u8]) -> Result<Vec<i16>> {
    if b.len() % 2 != 0 {
        return Err(CsiqError::Malformed("odd byte count for i16 array"));
    }
    Ok(b.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect())
}

/// Serialize a [`CsiRecord`] to its TLV payload (no framing).
pub fn encode_payload(r: &CsiRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + r.iq.len() * 2);
    put_tlv(&mut out, T_FTM, &r.ftm.to_le_bytes());
    put_tlv(&mut out, T_US, &r.us.to_le_bytes());
    put_tlv(&mut out, T_UNIX_TS_NS, &r.unix_ts_ns.to_le_bytes());
    put_tlv(&mut out, T_RNF, &r.rnf.to_le_bytes());
    if let Some(p) = r.phy {
        let mod_code = match p.modulation {
            Modulation::Cck => 0,
            Modulation::LegacyOfdm => 1,
            Modulation::Ht => 2,
            Modulation::Vht => 3,
            Modulation::He => 4,
            Modulation::Eht => 5,
            Modulation::Unknown(v) => v,
        };
        put_tlv(&mut out, T_PHY, &[mod_code, p.mcs, p.nss]);
    }
    if let Some(b) = r.bw_antsel {
        put_tlv(&mut out, T_BW_ANTSEL, &[b.bandwidth.to_code(), b.antenna_sel]);
    }
    if let Some(m) = r.mono_us {
        put_tlv(&mut out, T_MONO_US, &m.to_le_bytes());
    }
    if let Some(h) = &r.vendor_hdr {
        put_tlv(&mut out, T_VENDOR_HDR, h);
    }
    if let Some(v) = r.node.temp_mc {
        put_tlv(&mut out, T_NODE_TEMP_MC, &v.to_le_bytes());
    }
    if let Some(v) = r.node.throttle_flags {
        put_tlv(&mut out, T_NODE_THROTTLE, &v.to_le_bytes());
    }
    if let Some(v) = r.node.spool_free_bytes {
        put_tlv(&mut out, T_NODE_SPOOL_FREE, &v.to_le_bytes());
    }
    if let Some(v) = r.node.load_m {
        put_tlv(&mut out, T_NODE_LOAD_M, &v.to_le_bytes());
    }
    if let Some(v) = r.node.nic_temp_c {
        put_tlv(&mut out, T_NODE_NIC_TEMP_C, &v.to_le_bytes());
    }
    put_tlv(&mut out, T_SEQ, &[r.seq]);
    put_tlv(&mut out, T_NRX, &[r.nrx]);
    put_tlv(&mut out, T_NTX, &[r.ntx]);
    put_tlv(&mut out, T_NTONE, &r.ntone.to_le_bytes());
    put_tlv(&mut out, T_SRC_MAC, &r.src_mac);
    put_tlv(&mut out, T_CHANNEL, &r.channel.to_le_bytes());
    put_tlv(&mut out, T_WIDTH, &r.width.to_code().to_le_bytes());
    put_tlv(&mut out, T_RSSI, &i16s_to_bytes(&r.rssi));
    put_tlv(&mut out, T_CSI_MATRIX, &i16s_to_bytes(&r.iq));
    out
}

/// Parse a TLV payload back into a [`CsiRecord`]. Unknown TLV types are skipped.
pub fn decode_payload(payload: &[u8]) -> Result<CsiRecord> {
    let mut buf = Buf::new(payload);
    // Defaults; required fields are validated at the end.
    let mut ftm = None;
    let mut us = 0u32;
    let mut unix_ts_ns = 0u64;
    let mut rnf = 0u32;
    let mut phy = None;
    let mut seq = 0u8;
    let mut nrx = None;
    let mut ntx = None;
    let mut ntone = None;
    let mut rssi = Vec::new();
    let mut src_mac = [0u8; 6];
    let mut channel = 0u32;
    let mut width = Width::Unknown(0);
    let mut bw_antsel = None;
    let mut mono_us = None;
    let mut vendor_hdr = None;
    let mut node = NodeState::default();
    let mut iq = Vec::new();

    while buf.remaining() > 0 {
        let ty = buf.u8()?;
        let len = buf.u32()? as usize;
        let val = buf.take(len)?;
        match ty {
            T_FTM => {
                ftm = Some(u32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("ftm len"))?,
                ))
            }
            T_US => {
                us = u32::from_le_bytes(val.try_into().map_err(|_| CsiqError::Malformed("us len"))?)
            }
            T_UNIX_TS_NS => {
                unix_ts_ns = u64::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("unix_ts_ns len"))?,
                )
            }
            T_RNF => {
                rnf = u32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("rnf len"))?,
                )
            }
            T_PHY => {
                if val.len() != 3 {
                    return Err(CsiqError::Malformed("phy len"));
                }
                phy = Some(PhyLabel {
                    modulation: Modulation::from_rnf_type(val[0]),
                    mcs: val[1],
                    nss: val[2],
                });
            }
            T_SEQ => seq = *val.first().ok_or(CsiqError::Malformed("seq len"))?,
            T_NRX => nrx = Some(*val.first().ok_or(CsiqError::Malformed("nrx len"))?),
            T_NTX => ntx = Some(*val.first().ok_or(CsiqError::Malformed("ntx len"))?),
            T_NTONE => {
                ntone = Some(u16::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("ntone len"))?,
                ))
            }
            T_SRC_MAC => {
                if val.len() != 6 {
                    return Err(CsiqError::Malformed("src_mac len"));
                }
                src_mac.copy_from_slice(val);
            }
            T_CHANNEL => {
                channel = u32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("channel len"))?,
                )
            }
            T_WIDTH => {
                let code = u16::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("width len"))?,
                );
                width = Width::from_code(code);
            }
            T_RSSI => rssi = bytes_to_i16s(val)?,
            T_BW_ANTSEL => {
                if val.len() < 2 {
                    return Err(CsiqError::Malformed("bw_antsel len"));
                }
                bw_antsel = Some(BwAntsel {
                    bandwidth: Bandwidth::from_code(val[0]),
                    antenna_sel: val[1],
                });
            }
            T_MONO_US => {
                mono_us = Some(u64::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("mono_us len"))?,
                ))
            }
            T_VENDOR_HDR => vendor_hdr = Some(val.to_vec()),
            T_NODE_TEMP_MC => {
                node.temp_mc = Some(i32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("node temp len"))?,
                ))
            }
            T_NODE_THROTTLE => {
                node.throttle_flags = Some(u32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("node throttle len"))?,
                ))
            }
            T_NODE_SPOOL_FREE => {
                node.spool_free_bytes = Some(u64::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("node spool len"))?,
                ))
            }
            T_NODE_LOAD_M => {
                node.load_m = Some(u32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("node load len"))?,
                ))
            }
            T_NODE_NIC_TEMP_C => {
                node.nic_temp_c = Some(i32::from_le_bytes(
                    val.try_into()
                        .map_err(|_| CsiqError::Malformed("node nic temp len"))?,
                ))
            }
            T_CSI_MATRIX => iq = bytes_to_i16s(val)?,
            _ => { /* unknown type: skip (forward compatibility) */ }
        }
    }

    Ok(CsiRecord {
        ftm: ftm.ok_or(CsiqError::Malformed("missing ftm"))?,
        us,
        unix_ts_ns,
        rnf,
        phy,
        // No `0x11` in this record — every file written before csid 0.2.0. The
        // same bits are in `rnf`, which the record already carries, so recover
        // them rather than reporting a field the capture genuinely holds as
        // absent. `BW_ANTSEL` IS this function applied at write time, so the
        // fallback returns the same value, not an approximation of it.
        bw_antsel: bw_antsel.or_else(|| crate::raw::decode_bw_antsel(rnf)),
        mono_us,
        vendor_hdr,
        node,
        seq,
        nrx: nrx.ok_or(CsiqError::Malformed("missing nrx"))?,
        ntx: ntx.ok_or(CsiqError::Malformed("missing ntx"))?,
        ntone: ntone.ok_or(CsiqError::Malformed("missing ntone"))?,
        rssi,
        src_mac,
        channel,
        width,
        iq,
    })
}
