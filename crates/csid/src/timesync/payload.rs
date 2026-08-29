//! Recognising a **transmit stamp** inside a received 802.11 frame.
//!
//! Two transmitters on this fleet already put their transmit time inside the
//! payload, and until now nobody read it back:
//!
//! | Transmitter | Where | Layout |
//! |---|---|---|
//! | [`crate::inject`] | raw 802.11 data payload | `b"CSID" ‖ u64 LE seq ‖ u64 LE tx_unix_ns` |
//! | the phone app (`LabPacket` / MNDP v1) | UDP payload | `b"MNDP" ‖ … ‖ u32 BE seq ‖ u64 BE t_mono_ns ‖ u64 BE t_wall_ms` |
//!
//! They are different formats on different clocks, and the difference matters
//! downstream: the injector stamps `CLOCK_REALTIME`, so its stamp is directly
//! comparable with the receiver's; the phone stamps a **sleep-continuous
//! monotonic** clock whose origin is arbitrary, so its stamp is only usable
//! through an affine fit (see [`super::affine`]). Every recognised frame
//! therefore records *which* clock its stamp came from, and nothing downstream
//! ever mixes the two.
//!
//! ## Everything here is pure
//!
//! One function, `&[u8]` in, a decision out. The socket that produces those
//! bytes lives in [`super::rx`] and is the only part that cannot be tested off
//! a Pi — which is the same split [`crate::inject`] and [`crate::ble`] use, and
//! for the same reason: a byte-layout bug should be a unit-test failure on a
//! laptop, not a silent no-op in a lab.
//!
//! ## What is deliberately not recognised
//!
//! * **Protected frames.** A WPA2/WPA3 SSID encrypts the payload over the air,
//!   so a monitor-mode receiver sees ciphertext. Those frames are counted as
//!   [`Reject::Protected`] rather than as noise, because the count is the
//!   diagnosis: a lab whose experiment SSID is not open gets *zero* app rows
//!   and a large `protected` counter, and the operator needs to be told that
//!   rather than left wondering. (The `collectord` four-timestamp exchange
//!   remains the route for an encrypted SSID — it receives the datagram as a
//!   real UDP peer, above the crypto.)
//! * **A-MSDU aggregates** and IPv6. Counted as [`Reject::NoStamp`]; adding
//!   them is a parser change, not a protocol change.
//! * **`TIME_RESPONSE` (MNDP type 3).** That direction is collector→phone, so
//!   its header stamps are the *collector's* clock, not the phone's. Reading it
//!   as a phone stamp would fold the node's own clock back into the fit and
//!   make the offset look perfect.

use serde::{Deserialize, Serialize};

/// The injector's payload magic.
pub const CSID_MAGIC: &[u8; 4] = b"CSID";
/// The mobile instrument's datagram magic (MNDP v1).
pub const MNDP_MAGIC: &[u8; 4] = b"MNDP";
/// `b"CSID" ‖ seq ‖ tx_unix_ns`.
pub const CSID_STAMP_LEN: usize = 4 + 8 + 8;
/// MNDP v1 fixed header.
pub const MNDP_HEADER_LEN: usize = 48;
/// LLC/SNAP for a bridged Ethertype payload (RFC 1042).
const LLC_SNAP: [u8; 6] = [0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00];
const ETHERTYPE_IPV4: u16 = 0x0800;
const IPPROTO_UDP: u8 = 17;

/// Which transmitter produced the stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxKind {
    /// `csid`'s own paced injector.
    Csid,
    /// The mobile instrument (MonadCount app, MNDP v1).
    App,
}

impl TxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TxKind::Csid => "csid",
            TxKind::App => "app",
        }
    }
}

/// Which clock the payload stamp is on. **Never mix these.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxClock {
    /// `CLOCK_REALTIME` nanoseconds — directly comparable with the receiver's.
    Unix,
    /// A sleep-continuous monotonic clock with an arbitrary origin. Usable only
    /// through an affine fit.
    Mono,
}

impl TxClock {
    pub fn as_str(self) -> &'static str {
        match self {
            TxClock::Unix => "unix",
            TxClock::Mono => "mono",
        }
    }
}

/// A recognised transmit stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub kind: TxKind,
    /// Transmitter identity: the sentinel MAC for `csid`, the session UUID for
    /// the app. Distinct from `tx_mac` because two phones share no MAC but two
    /// *sessions* of one phone must not be pooled either.
    pub tx_id: String,
    /// 802.11 `addr2` — who put the frame on the air, whatever the payload says.
    pub tx_mac: [u8; 6],
    pub seq: u64,
    pub tx_stamp_ns: u64,
    pub tx_clock: TxClock,
    /// The app's `wallMillis`, promoted to nanoseconds. `None` for `csid`
    /// frames, whose stamp is already a wallclock.
    pub tx_wall_ns: Option<u64>,
}

/// Why a frame carried no usable stamp. Every variant is counted, because the
/// *mix* of rejections is the diagnosis when a session produces no rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Not a radiotap-prefixed frame — the socket is not on a monitor interface.
    NotRadiotap,
    /// Truncated below what the declared headers require.
    Short,
    /// Not an 802.11 data frame carrying a body (management, control, null-data).
    NotDataFrame,
    /// Encrypted over the air. See the module docs.
    Protected,
    /// A data frame whose body is not a format this recognises.
    NoStamp,
}

/// Format a MAC as `aa:bb:cc:dd:ee:ff`.
pub fn mac_string(m: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// Parse `aa:bb:cc:dd:ee:ff` back into bytes.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut n = 0;
    for (i, part) in s.split(':').enumerate() {
        if i >= 6 {
            return None;
        }
        out[i] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(out)
}

/// Canonical UUID text for the app's 16 raw session bytes.
fn uuid_string(b: &[u8]) -> String {
    let h =
        |r: std::ops::Range<usize>| -> String { b[r].iter().map(|x| format!("{x:02x}")).collect() };
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

/// Length of the 802.11 MAC header for this frame control, or `None` if this is
/// not a data frame with a body.
///
/// Data frames are 24 bytes, plus 6 for the fourth address when both DS bits
/// are set (WDS), plus 2 for the QoS control field, plus 4 more for HT Control
/// when the Order bit is set on a QoS frame. Null-data subtypes (bit 2 of the
/// subtype) have no body at all and are rejected here rather than producing an
/// empty-payload rejection later.
fn data_header_len(fc0: u8, fc1: u8) -> Option<usize> {
    let ftype = (fc0 >> 2) & 0x03;
    let subtype = (fc0 >> 4) & 0x0F;
    if ftype != 2 {
        return None; // management (0) / control (1) / extension (3)
    }
    if subtype & 0x04 != 0 {
        return None; // *-null (no data body)
    }
    let qos = subtype & 0x08 != 0;
    let mut len = 24usize;
    if fc1 & 0x03 == 0x03 {
        len += 6; // addr4 (WDS)
    }
    if qos {
        len += 2;
        if fc1 & 0x80 != 0 {
            len += 4; // HT Control, present only on Order-flagged QoS frames
        }
    }
    Some(len)
}

/// Recognise a transmit stamp in one radiotap-prefixed 802.11 frame.
pub fn recognise(frame: &[u8]) -> Result<Stamp, Reject> {
    // -- radiotap ---------------------------------------------------------
    if frame.len() < 8 || frame[0] != 0 {
        return Err(Reject::NotRadiotap);
    }
    let rt_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if !(8..=1024).contains(&rt_len) || rt_len > frame.len() {
        return Err(Reject::NotRadiotap);
    }
    let mpdu = &frame[rt_len..];

    // -- 802.11 -----------------------------------------------------------
    if mpdu.len() < 24 {
        return Err(Reject::Short);
    }
    let (fc0, fc1) = (mpdu[0], mpdu[1]);
    let hdr_len = data_header_len(fc0, fc1).ok_or(Reject::NotDataFrame)?;
    if fc1 & 0x40 != 0 {
        return Err(Reject::Protected);
    }
    if mpdu.len() <= hdr_len {
        return Err(Reject::Short);
    }
    // addr2 is the transmitter address in every DS combination.
    let mut tx_mac = [0u8; 6];
    tx_mac.copy_from_slice(&mpdu[10..16]);
    let body = &mpdu[hdr_len..];

    // -- the injector's own payload ---------------------------------------
    // Written straight into the 802.11 body with no LLC/SNAP shim, so it is
    // recognised before the IP path is even considered.
    if body.len() >= CSID_STAMP_LEN && &body[..4] == CSID_MAGIC {
        return Ok(Stamp {
            kind: TxKind::Csid,
            tx_id: mac_string(&tx_mac),
            tx_mac,
            seq: u64::from_le_bytes(body[4..12].try_into().unwrap()),
            tx_stamp_ns: u64::from_le_bytes(body[12..20].try_into().unwrap()),
            tx_clock: TxClock::Unix,
            tx_wall_ns: None,
        });
    }

    // -- LLC/SNAP → IPv4 → UDP → MNDP -------------------------------------
    let udp = ipv4_udp_payload(body).ok_or(Reject::NoStamp)?;
    mndp_stamp(udp, tx_mac).ok_or(Reject::NoStamp)
}

/// Walk LLC/SNAP → IPv4 → UDP and return the UDP payload.
fn ipv4_udp_payload(body: &[u8]) -> Option<&[u8]> {
    if body.len() < 8 || body[..6] != LLC_SNAP {
        return None;
    }
    if u16::from_be_bytes([body[6], body[7]]) != ETHERTYPE_IPV4 {
        return None; // IPv6 / ARP / anything else — see the module docs
    }
    let ip = body.get(8..)?;
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let ihl = (ip[0] & 0x0F) as usize * 4;
    if ihl < 20 || ip.len() < ihl {
        return None;
    }
    if ip[9] != IPPROTO_UDP {
        return None;
    }
    // A fragmented datagram's later fragments carry no UDP header. The first
    // fragment does, and that is the one with the stamp.
    let frag_offset = u16::from_be_bytes([ip[6], ip[7]]) & 0x1FFF;
    if frag_offset != 0 {
        return None;
    }
    let udp = ip.get(ihl..)?;
    if udp.len() < 8 {
        return None;
    }
    udp.get(8..)
}

/// Decode an MNDP v1 header into a stamp on the phone's monotonic clock.
fn mndp_stamp(dgram: &[u8], tx_mac: [u8; 6]) -> Option<Stamp> {
    if dgram.len() < MNDP_HEADER_LEN || dgram[..4] != *MNDP_MAGIC {
        return None;
    }
    // Type 3 is TIME_RESPONSE — collector→phone. Its header stamps belong to
    // the collector, so reading them as phone stamps would fold this node's own
    // clock into the fit and make the offset look perfect.
    let kind = dgram[5];
    if kind == 3 {
        return None;
    }
    let t_mono_ns = u64::from_be_bytes(dgram[28..36].try_into().unwrap());
    let t_wall_ms = u64::from_be_bytes(dgram[36..44].try_into().unwrap());
    Some(Stamp {
        kind: TxKind::App,
        tx_id: uuid_string(&dgram[8..24]),
        tx_mac,
        seq: u32::from_be_bytes(dgram[24..28].try_into().unwrap()) as u64,
        tx_stamp_ns: t_mono_ns,
        tx_clock: TxClock::Mono,
        // Recorded, never load-bearing — the app's own contract for this field.
        tx_wall_ns: (t_wall_ms > 0).then(|| t_wall_ms.saturating_mul(1_000_000)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: [u8; 6] = [0xef, 0xbe, 0xad, 0xde, 0xad, 0xde];

    /// Build a radiotap + 802.11 data frame around a body, with whatever
    /// optional header fields this frame control declares — so a test never
    /// accidentally lays the body at the wrong offset.
    fn frame(fc0: u8, fc1: u8, src: [u8; 6], body: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8, 0];
        f.extend_from_slice(&9u16.to_le_bytes()); // it_len
        f.extend_from_slice(&(1u32 << 2).to_le_bytes());
        f.push(12); // rate
        f.push(fc0);
        f.push(fc1);
        f.extend_from_slice(&[0, 0]); // duration
        f.extend_from_slice(&[0xff; 6]); // addr1
        f.extend_from_slice(&src); // addr2
        f.extend_from_slice(&src); // addr3
        f.extend_from_slice(&[0, 0]); // seq ctrl
        if fc1 & 0x03 == 0x03 {
            f.extend_from_slice(&src); // addr4 (WDS)
        }
        if fc0 & 0x80 != 0 {
            f.extend_from_slice(&[0, 0]); // QoS control
            if fc1 & 0x80 != 0 {
                f.extend_from_slice(&[0, 0, 0, 0]); // HT Control
            }
        }
        f.extend_from_slice(body);
        f
    }

    fn csid_body(seq: u64, tx_ns: u64) -> Vec<u8> {
        let mut b = CSID_MAGIC.to_vec();
        b.extend_from_slice(&seq.to_le_bytes());
        b.extend_from_slice(&tx_ns.to_le_bytes());
        b.resize(176, 0); // the injector zero-pads to frame_bytes
        b
    }

    /// The app's datagram, wrapped exactly as it appears on the air.
    fn mndp_body(kind: u8, session: [u8; 16], seq: u32, mono: u64, wall_ms: u64) -> Vec<u8> {
        let mut d = vec![0u8; MNDP_HEADER_LEN + 4];
        d[0..4].copy_from_slice(MNDP_MAGIC);
        d[4] = 1;
        d[5] = kind;
        d[8..24].copy_from_slice(&session);
        d[24..28].copy_from_slice(&seq.to_be_bytes());
        d[28..36].copy_from_slice(&mono.to_be_bytes());
        d[36..44].copy_from_slice(&wall_ms.to_be_bytes());
        d[44..46].copy_from_slice(&4u16.to_be_bytes());

        let mut udp = Vec::new();
        udp.extend_from_slice(&40000u16.to_be_bytes()); // src port
        udp.extend_from_slice(&9999u16.to_be_bytes()); // dst port
        udp.extend_from_slice(&((8 + d.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]); // checksum
        udp.extend_from_slice(&d);

        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        let total = (20 + udp.len()) as u16;
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        ip[9] = IPPROTO_UDP;
        ip[12..16].copy_from_slice(&[192, 168, 1, 50]);
        ip[16..20].copy_from_slice(&[192, 168, 1, 1]);
        ip.extend_from_slice(&udp);

        let mut body = LLC_SNAP.to_vec();
        body.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        body.extend_from_slice(&ip);
        body
    }

    /// The layout is a contract with `inject::build_frame`. If this test is
    /// edited to match a change there, the analysis side must be edited too.
    #[test]
    fn the_injectors_own_frame_round_trips_through_the_recogniser() {
        let f = frame(
            0x08,
            0x00,
            SENTINEL,
            &csid_body(4242, 1_786_000_000_123_456_789),
        );
        let s = recognise(&f).unwrap();
        assert_eq!(s.kind, TxKind::Csid);
        assert_eq!(s.seq, 4242);
        assert_eq!(s.tx_stamp_ns, 1_786_000_000_123_456_789);
        assert_eq!(s.tx_clock, TxClock::Unix);
        assert_eq!(s.tx_mac, SENTINEL);
        assert_eq!(s.tx_id, "ef:be:ad:de:ad:de");
        assert_eq!(s.tx_wall_ns, None, "a unix stamp needs no wallclock echo");
    }

    /// Built by the real builder rather than a hand-rolled copy, so a change to
    /// the injector's wire format fails HERE rather than at the lab.
    #[test]
    fn it_recognises_a_frame_the_injector_actually_built() {
        let cfg = crate::config::InjectConfig::default();
        let built = crate::inject::build_frame(&cfg, 7, 1_786_000_000_000_000_001);
        let s = recognise(&built).expect("the injector's own frame must be recognisable");
        assert_eq!(s.kind, TxKind::Csid);
        assert_eq!(s.seq, 7);
        assert_eq!(s.tx_stamp_ns, 1_786_000_000_000_000_001);
        assert_eq!(s.tx_id, cfg.src_mac);
    }

    /// The recogniser must survive the injector's OTHER radiotap shape.
    ///
    /// `build_frame` emits an 8-byte header with no fields when the driver rate
    /// is forced, which is the shape every fleet arm produces. `recognise`
    /// reads `it_len` rather than assuming 9, and this pins that.
    #[test]
    fn it_recognises_a_frame_built_with_an_empty_radiotap_header() {
        let mut cfg = crate::config::InjectConfig::default();
        cfg.monitor_tx_rate = 0x4100;
        let built = crate::inject::build_frame(&cfg, 11, 1_786_000_000_000_000_002);
        assert_eq!(
            u16::from_le_bytes([built[2], built[3]]),
            8,
            "the fixture must exercise the empty-header path"
        );
        let s = recognise(&built).expect("an empty radiotap header is still radiotap");
        assert_eq!(s.kind, TxKind::Csid);
        assert_eq!(s.seq, 11);
        assert_eq!(s.tx_stamp_ns, 1_786_000_000_000_000_002);
        assert_eq!(s.tx_id, cfg.src_mac);
    }

    #[test]
    fn the_app_datagram_is_recognised_on_the_monotonic_clock() {
        let session = [0xab; 16];
        let phone = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
        // QoS data (subtype 0x8) with ToDS set — what a station sends to an AP.
        let f = frame(
            0x88,
            0x01,
            phone,
            &mndp_body(1, session, 99, 12_345_678_901, 1_786_000_000_123),
        );
        let s = recognise(&f).unwrap();
        assert_eq!(s.kind, TxKind::App);
        assert_eq!(s.seq, 99);
        assert_eq!(s.tx_stamp_ns, 12_345_678_901);
        assert_eq!(s.tx_clock, TxClock::Mono, "mono must never be read as unix");
        assert_eq!(s.tx_mac, phone);
        assert_eq!(s.tx_id, "abababab-abab-abab-abab-abababababab");
        assert_eq!(s.tx_wall_ns, Some(1_786_000_000_123_000_000));
    }

    /// SESSION_HELLO carries a stamp too, and a session that only ever sent
    /// those would otherwise look like a session that sent nothing.
    #[test]
    fn session_hello_also_carries_a_usable_stamp() {
        let f = frame(
            0x88,
            0x01,
            [1, 2, 3, 4, 5, 6],
            &mndp_body(4, [7; 16], 3, 500, 0),
        );
        let s = recognise(&f).unwrap();
        assert_eq!(s.kind, TxKind::App);
        assert_eq!(s.seq, 3);
        assert_eq!(s.tx_wall_ns, None, "a zero wallclock is absent, not 1970");
    }

    /// The collector's reply is stamped on the COLLECTOR's clock. Reading it as
    /// a phone stamp would fold this node's own clock into the affine fit and
    /// make the phone offset look perfect.
    #[test]
    fn the_collectors_time_response_is_refused() {
        let f = frame(
            0x88,
            0x01,
            [1, 2, 3, 4, 5, 6],
            &mndp_body(3, [7; 16], 3, 500, 0),
        );
        assert_eq!(recognise(&f), Err(Reject::NoStamp));
    }

    /// An encrypted experiment SSID yields zero app rows. That has to be
    /// diagnosable from the counters, not look like an empty room.
    #[test]
    fn an_encrypted_frame_is_counted_as_protected_not_as_noise() {
        let f = frame(
            0x88,
            0x41,
            [1, 2, 3, 4, 5, 6],
            &mndp_body(1, [7; 16], 1, 1, 1),
        );
        assert_eq!(recognise(&f), Err(Reject::Protected));
    }

    #[test]
    fn header_length_follows_the_frame_control_bits() {
        assert_eq!(data_header_len(0x08, 0x00), Some(24)); // data
        assert_eq!(data_header_len(0x88, 0x01), Some(26)); // QoS data, ToDS
        assert_eq!(data_header_len(0x08, 0x03), Some(30)); // WDS (both DS bits)
        assert_eq!(data_header_len(0x88, 0x83), Some(36)); // WDS + QoS + HT Control
        assert_eq!(data_header_len(0x48, 0x00), None, "null-data has no body");
        assert_eq!(data_header_len(0xC8, 0x00), None, "QoS-null has no body");
        assert_eq!(data_header_len(0x80, 0x00), None, "beacon is management");
        assert_eq!(data_header_len(0xD4, 0x00), None, "ACK is control");
    }

    /// The optional header fields shift the payload. Getting any of them wrong
    /// reads the magic a few bytes late and silently sees nothing at all —
    /// which on a QoS-only network would be a session with zero rows and no
    /// error.
    #[test]
    fn every_optional_header_field_offset_is_honoured() {
        let body = csid_body(1, 2);
        let plain = recognise(&frame(0x08, 0x00, SENTINEL, &body)).unwrap();
        for (fc0, fc1, what) in [
            (0x88u8, 0x00u8, "QoS"),
            (0x88, 0x01, "QoS + ToDS"),
            (0x08, 0x03, "WDS"),
            (0x88, 0x83, "WDS + QoS + HT Control"),
        ] {
            let got = recognise(&frame(fc0, fc1, SENTINEL, &body))
                .unwrap_or_else(|e| panic!("{what}: {e:?}"));
            assert_eq!(plain, got, "{what}");
        }
    }

    #[test]
    fn non_monitor_and_truncated_input_are_refused_rather_than_guessed() {
        assert_eq!(recognise(&[]), Err(Reject::NotRadiotap));
        assert_eq!(
            recognise(&[1, 0, 9, 0, 0, 0, 0, 0, 0]),
            Err(Reject::NotRadiotap)
        );
        // it_len longer than the buffer.
        assert_eq!(
            recognise(&[0, 0, 0xff, 0x00, 0, 0, 0, 0, 0]),
            Err(Reject::NotRadiotap)
        );
        // Radiotap only, no MPDU.
        assert_eq!(recognise(&[0, 0, 9, 0, 0, 0, 0, 0, 0]), Err(Reject::Short));
        // A data frame whose body stops inside the magic.
        assert_eq!(
            recognise(&frame(0x08, 0x00, SENTINEL, b"CSID\x01")),
            Err(Reject::NoStamp)
        );
    }

    #[test]
    fn ordinary_traffic_is_rejected_without_a_false_positive() {
        // TCP rather than UDP.
        let mut body = LLC_SNAP.to_vec();
        body.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[9] = 6; // TCP
        ip.extend_from_slice(&[0u8; 40]);
        body.extend_from_slice(&ip);
        assert_eq!(
            recognise(&frame(0x08, 0x00, SENTINEL, &body)),
            Err(Reject::NoStamp)
        );

        // UDP that is not MNDP.
        let f = frame(0x08, 0x00, SENTINEL, &{
            let mut b = mndp_body(1, [0; 16], 1, 1, 1);
            let magic_at = b.len() - (MNDP_HEADER_LEN + 4);
            b[magic_at..magic_at + 4].copy_from_slice(b"XXXX");
            b
        });
        assert_eq!(recognise(&f), Err(Reject::NoStamp));
    }

    #[test]
    fn mac_text_round_trips() {
        assert_eq!(mac_string(&SENTINEL), "ef:be:ad:de:ad:de");
        assert_eq!(parse_mac("ef:be:ad:de:ad:de"), Some(SENTINEL));
        assert_eq!(parse_mac("ef:be:ad:de:ad"), None);
        assert_eq!(parse_mac("zz:be:ad:de:ad:de"), None);
    }
}
