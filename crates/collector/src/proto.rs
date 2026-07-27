//! The MNDP v1 wire format spoken by the mobile instrument.
//!
//! This is the receiving half of `LabPacket` in the MonadCount app; the two definitions must stay
//! byte-identical, so the layout is restated here in full rather than described by reference:
//!
//! ```text
//!  0  4  magic "MNDP"
//!  4  1  version
//!  5  1  type      1 = DATA, 2 = TIME_REQUEST, 3 = TIME_RESPONSE, 4 = SESSION_HELLO
//!  6  2  flags
//!  8 16  session id (raw UUID bytes)
//! 24  4  sequence
//! 28  8  t_mono_ns  sender's sleep-continuous monotonic clock
//! 36  8  t_wall_ms  sender's wall clock (recorded, never load-bearing)
//! 44  2  payload length
//! 46  2  reserved
//! ```
//!
//! All multi-byte fields are big-endian. A TIME_RESPONSE carries the collector's two reference
//! stamps (t2 = receive, t3 = send) as two big-endian u64 in the payload — that pair is what makes
//! the four-timestamp offset estimate possible on the phone.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MNDP";
pub const VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 48;

pub const TYPE_DATA: u8 = 1;
pub const TYPE_TIME_REQUEST: u8 = 2;
pub const TYPE_TIME_RESPONSE: u8 = 3;
/// Sent once at session start and repeated with each clock burst.
///
/// Data packets carry only a session UUID, but sessions are filed in object storage under
/// `<participant>/<session>/`, so the collector has to learn the participant from somewhere. A
/// small repeated announcement costs far less than widening every packet at 200 Hz, and repetition
/// means a collector that started late — or that missed the first datagram, which is the normal
/// case on a contended channel — still learns who it is talking to.
pub const TYPE_SESSION_HELLO: u8 = 4;

/// A decoded datagram. The payload is borrowed from the receive buffer to keep the hot path free
/// of allocation.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    pub version: u8,
    pub kind: u8,
    pub session: [u8; 16],
    pub sequence: u32,
    pub t_mono_ns: u64,
    pub t_wall_ms: u64,
    pub payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < HEADER_SIZE {
            bail!("short datagram: {} bytes", bytes.len());
        }
        if bytes[0..4] != MAGIC {
            bail!("bad magic");
        }
        let payload_len = u16::from_be_bytes([bytes[44], bytes[45]]) as usize;
        if HEADER_SIZE + payload_len > bytes.len() {
            bail!("payload length {payload_len} overruns datagram");
        }

        let mut session = [0u8; 16];
        session.copy_from_slice(&bytes[8..24]);

        Ok(Packet {
            version: bytes[4],
            kind: bytes[5],
            session,
            sequence: u32::from_be_bytes(bytes[24..28].try_into().unwrap()),
            t_mono_ns: u64::from_be_bytes(bytes[28..36].try_into().unwrap()),
            t_wall_ms: u64::from_be_bytes(bytes[36..44].try_into().unwrap()),
            payload: &bytes[HEADER_SIZE..HEADER_SIZE + payload_len],
        })
    }

    /// Canonical lowercase UUID string for the session id.
    pub fn session_uuid(&self) -> String {
        let h = |r: std::ops::Range<usize>| -> String {
            self.session[r].iter().map(|b| format!("{b:02x}")).collect()
        };
        format!(
            "{}-{}-{}-{}-{}",
            h(0..4),
            h(4..6),
            h(6..8),
            h(8..10),
            h(10..16)
        )
    }
}

/// Build a TIME_RESPONSE.
///
/// The sequence number is echoed deliberately: the phone rejects a reply whose sequence does not
/// match its request, because a stale reply from an earlier exchange looks *fast* and would bias
/// the minimum-delay filter in precisely the wrong direction.
pub fn encode_time_response(
    session: &[u8; 16],
    sequence: u32,
    t2_ns: u64,
    t3_ns: u64,
) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_SIZE + 16];
    out[0..4].copy_from_slice(&MAGIC);
    out[4] = VERSION;
    out[5] = TYPE_TIME_RESPONSE;
    out[8..24].copy_from_slice(session);
    out[24..28].copy_from_slice(&sequence.to_be_bytes());
    // The reply's own header timestamps are the collector's reference clock; the phone reads t2/t3
    // from the payload, but keeping the header honest makes packet captures readable.
    out[28..36].copy_from_slice(&t3_ns.to_be_bytes());
    out[36..44].copy_from_slice(&(t3_ns / 1_000_000).to_be_bytes());
    out[44..46].copy_from_slice(&16u16.to_be_bytes());
    out[HEADER_SIZE..HEADER_SIZE + 8].copy_from_slice(&t2_ns.to_be_bytes());
    out[HEADER_SIZE + 8..HEADER_SIZE + 16].copy_from_slice(&t3_ns.to_be_bytes());
    out
}

/// Contents of a SESSION_HELLO payload (UTF-8 JSON).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct Hello {
    #[serde(default)]
    pub participant_id: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub ap_id: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub commanded_rate_hz: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u8, seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_SIZE + payload.len()];
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = kind;
        out[8..24].copy_from_slice(&[0xab; 16]);
        out[24..28].copy_from_slice(&seq.to_be_bytes());
        out[28..36].copy_from_slice(&123u64.to_be_bytes());
        out[36..44].copy_from_slice(&456u64.to_be_bytes());
        out[44..46].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        out[HEADER_SIZE..].copy_from_slice(payload);
        out
    }

    #[test]
    fn decodes_a_data_packet() {
        let bytes = header(TYPE_DATA, 7, &[1, 2, 3, 4]);
        let p = Packet::decode(&bytes).unwrap();
        assert_eq!(p.kind, TYPE_DATA);
        assert_eq!(p.sequence, 7);
        assert_eq!(p.t_mono_ns, 123);
        assert_eq!(p.payload, &[1, 2, 3, 4]);
        assert_eq!(p.session_uuid(), "abababab-abab-abab-abab-ababababababab"[..36].to_string());
    }

    #[test]
    fn rejects_foreign_traffic() {
        assert!(Packet::decode(b"not a monad packet at all, just noise on the port").is_err());
        assert!(Packet::decode(&[0u8; 8]).is_err());
    }

    #[test]
    fn rejects_a_lying_payload_length() {
        let mut bytes = header(TYPE_DATA, 1, &[9]);
        bytes[44..46].copy_from_slice(&9000u16.to_be_bytes());
        assert!(Packet::decode(&bytes).is_err());
    }

    #[test]
    fn time_response_round_trips_the_stamps() {
        let reply = encode_time_response(&[0xcd; 16], 3, 111, 222);
        let p = Packet::decode(&reply).unwrap();
        assert_eq!(p.kind, TYPE_TIME_RESPONSE);
        assert_eq!(p.sequence, 3);
        assert_eq!(u64::from_be_bytes(p.payload[0..8].try_into().unwrap()), 111);
        assert_eq!(u64::from_be_bytes(p.payload[8..16].try_into().unwrap()), 222);
    }
}
