//! The live-stream datagram: one CSI record per datagram, self-contained so a
//! subscriber can join at any time. Reuses the exact TLV record payload as the
//! file container — the format is defined once.
//!
//! ```text
//! LiveDatagram
//!   magic        "CL"     2 bytes
//!   version      u8       = 1
//!   session_uid  u64 LE   stable per capture session (subscriber resync)
//!   seq          u32 LE   monotonic datagram counter (gaps = dropped)
//!   payload      TLV      one record (see `tlv`)
//! ```

use crate::error::{CsiqError, Result};
use crate::record::CsiRecord;
use crate::tlv;

/// Live datagram magic.
pub const LIVE_MAGIC: [u8; 2] = *b"CL";
/// Live datagram version.
pub const LIVE_VERSION: u8 = 1;

/// Encode one record as a live datagram.
pub fn encode(session_uid: u64, seq: u32, r: &CsiRecord) -> Vec<u8> {
    let payload = tlv::encode_payload(r);
    let mut out = Vec::with_capacity(15 + payload.len());
    out.extend_from_slice(&LIVE_MAGIC);
    out.push(LIVE_VERSION);
    out.extend_from_slice(&session_uid.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// A decoded live datagram.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveDatagram {
    pub session_uid: u64,
    pub seq: u32,
    pub record: CsiRecord,
}

/// Decode a live datagram from a received buffer.
pub fn decode(buf: &[u8]) -> Result<LiveDatagram> {
    if buf.len() < 15 {
        return Err(CsiqError::Malformed("live datagram too short"));
    }
    if buf[0..2] != LIVE_MAGIC {
        return Err(CsiqError::BadMagic {
            expected: [LIVE_MAGIC[0], LIVE_MAGIC[1], 0, 0],
            found: [buf[0], buf[1], 0, 0],
        });
    }
    if buf[2] != LIVE_VERSION {
        return Err(CsiqError::UnsupportedVersion(buf[2] as u16));
    }
    let session_uid = u64::from_le_bytes(buf[3..11].try_into().unwrap());
    let seq = u32::from_le_bytes(buf[11..15].try_into().unwrap());
    let record = tlv::decode_payload(&buf[15..])?;
    Ok(LiveDatagram {
        session_uid,
        seq,
        record,
    })
}
