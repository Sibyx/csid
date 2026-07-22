//! The record TLV codec — the heart of CSIQ's self-description.
//!
//! A record payload is a flat sequence of `[u8 type][u32 len][value]` fields.
//! Unknown types are skipped on read, so a v1 reader tolerates records written
//! by a later minor version, and a later reader tolerates v1 records. The CSI
//! matrix is just another (large) TLV.

use crate::error::{CsiqError, Result};
use crate::record::{CsiRecord, Modulation, PhyLabel, Width};

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
                                   // 0x20..0x2F  reserved: EHT-specific (RU allocation, per-RU tone maps, …)
                                   // 0x30..0x3F  reserved: 802.11bf sensing metadata

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
