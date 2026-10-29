//! Airtime — **how long the channel was busy**, estimated per frame from its radiotap rate fields
//! (2026-10-29, monad-knowledge IP-179).
//!
//! ## Why an estimate
//!
//! Channel busy time is the classic occupancy proxy, and this hardware cannot report it: iwlwifi
//! implements neither `get_survey` nor `get_stats`, and the onboard brcmfmac neither
//! (monad-knowledge `docs/OBSERVABILITY.md`, verified on monad04 2026-08-13). The monitor socket
//! does see every decoded frame with its PHY parameters, so the time each one occupied the air can
//! be computed from the standard PPDU formulas. The sum is a **lower bound** on busy time: it
//! counts only frames the receiver decoded, never energy it could not decode, and it ignores
//! inter-frame spaces, packet extension and signal extension.
//!
//! ## What is computed
//!
//! | PHY | Rate source | Duration |
//! |---|---|---|
//! | DSSS/CCK (1, 2, 5.5, 11 Mbps) | radiotap Rate | 192 µs long or 96 µs short preamble + 8·L/rate |
//! | Legacy OFDM | radiotap Rate | 20 µs + 4 µs · ⌈(16 + 8·L + 6) / N_DBPS⌉ |
//! | HT | radiotap MCS | 20 + 8 + 4 + 4·N_LTF µs + symbols · (4 or 3.6 µs) |
//! | VHT | radiotap VHT | 20 + 8 + 4 + 4·N_LTF + 4 µs + symbols · (4 or 3.6 µs) |
//! | HE SU / extended-range SU | radiotap HE | 20 + 4 + 8 (16) + 4 + N_LTF · LTF µs + symbols · (12.8 + GI) µs |
//!
//! N_DBPS = N_SD · bits per subcarrier · code rate · N_SS. Inside an A-MPDU the preamble, SERVICE
//! and tail are charged once per aggregate (radiotap A-MPDU reference) and each subframe adds its
//! own bits plus the 4-byte delimiter. HE MU and trigger-based PPDUs (uplink OFDMA) carry a
//! resource-unit allocation this module does not model; they return `None` and are counted as
//! unknown, never as zero.

/// What the radiotap header says about one frame's PHY.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Phy {
    /// Legacy rate in 500 kbps units.
    pub rate_500k: Option<u8>,
    pub short_preamble: bool,
    pub ht: Option<Ht>,
    pub vht: Option<Vht>,
    pub he: Option<He>,
    pub ampdu_ref: Option<u32>,
    /// Bytes after the radiotap header, the FCS excluded when the header says it is present.
    pub psdu_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ht {
    pub mcs: u8,
    pub bw_mhz: u16,
    pub short_gi: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vht {
    pub mcs: u8,
    pub nss: u8,
    pub bw_mhz: u16,
    pub short_gi: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct He {
    /// 0 SU, 1 extended-range SU, 2 MU, 3 trigger-based.
    pub format: u8,
    pub mcs: u8,
    pub nss: u8,
    /// `None` for a resource-unit allocation (MU, TB).
    pub bw_mhz: Option<u16>,
    pub gi_us: f64,
    /// HE-LTF symbol, without GI: 3.2, 6.4 or 12.8 µs.
    pub ltf_us: f64,
}

/// (alignment, size) of the radiotap fields 0..=23 in the default namespace.
const FIELDS: [(usize, usize); 24] = [
    (8, 8),  // 0 TSFT
    (1, 1),  // 1 Flags
    (1, 1),  // 2 Rate
    (2, 4),  // 3 Channel
    (1, 2),  // 4 FHSS
    (1, 1),  // 5 antenna signal
    (1, 1),  // 6 antenna noise
    (2, 2),  // 7 lock quality
    (2, 2),  // 8 TX attenuation
    (2, 2),  // 9 dB TX attenuation
    (1, 1),  // 10 dBm TX power
    (1, 1),  // 11 antenna
    (1, 1),  // 12 dB antenna signal
    (1, 1),  // 13 dB antenna noise
    (2, 2),  // 14 RX flags
    (2, 2),  // 15 TX flags
    (1, 1),  // 16 RTS retries
    (1, 1),  // 17 data retries
    (4, 8),  // 18 XChannel
    (1, 3),  // 19 MCS
    (4, 8),  // 20 A-MPDU status
    (2, 12), // 21 VHT
    (8, 12), // 22 timestamp
    (2, 12), // 23 HE
];

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read the PHY fields of a radiotap-prefixed frame. `None` when the header is not radiotap.
///
/// Only the first presence word's fields are read: they are laid out first, in bit order, and
/// every field needed here (Flags, Rate, MCS, A-MPDU, VHT, HE) is among them.
pub fn parse(frame: &[u8]) -> Option<Phy> {
    if frame.len() < 8 || frame[0] != 0 {
        return None;
    }
    let rt_len = u16_at(frame, 2)? as usize;
    if !(8..=1024).contains(&rt_len) || rt_len > frame.len() {
        return None;
    }
    let present = u32_at(frame, 4)?;
    let mut words = 1;
    while u32_at(frame, 4 + 4 * (words - 1))? & (1 << 31) != 0 {
        words += 1;
        if 4 + 4 * words > rt_len {
            return None;
        }
    }
    let hdr = &frame[..rt_len];
    let mut off = 4 + 4 * words;
    let mut phy = Phy::default();
    let mut fcs = false;
    for (bit, &(align, size)) in FIELDS.iter().enumerate() {
        if present & (1 << bit) == 0 {
            continue;
        }
        off = (off + align - 1) & !(align - 1);
        let f = hdr.get(off..off + size)?;
        match bit {
            1 => {
                fcs = f[0] & 0x10 != 0;
                phy.short_preamble = f[0] & 0x02 != 0;
            }
            2 => phy.rate_500k = Some(f[0]),
            19 => {
                let (known, flags, mcs) = (f[0], f[1], f[2]);
                let bw40 = known & 0x01 != 0 && flags & 0x03 == 1;
                let sgi = known & 0x04 != 0 && flags & 0x04 != 0;
                phy.ht = Some(Ht { mcs, bw_mhz: if bw40 { 40 } else { 20 }, short_gi: sgi });
            }
            20 => phy.ampdu_ref = Some(u32::from_le_bytes([f[0], f[1], f[2], f[3]])),
            21 => {
                let known = u16::from_le_bytes([f[0], f[1]]);
                let flags = f[2];
                let bw_code = f[3];
                let mcs_nss = f[4];
                let bw = match bw_code {
                    0 => 20,
                    1..=3 => 40,
                    4..=10 => 80,
                    11..=25 => 160,
                    _ => 20,
                };
                phy.vht = Some(Vht {
                    mcs: mcs_nss >> 4,
                    nss: (mcs_nss & 0x0f).max(1),
                    bw_mhz: if known & 0x0040 != 0 { bw } else { 20 },
                    short_gi: known & 0x0004 != 0 && flags & 0x04 != 0,
                });
            }
            23 => {
                let d: Vec<u16> = (0..6).map(|i| u16::from_le_bytes([f[2 * i], f[2 * i + 1]])).collect();
                let format = (d[0] & 0x03) as u8;
                let mcs = ((d[2] >> 8) & 0x0f) as u8;
                let ru = d[4] & 0x0f;
                let bw_known = d[0] & 0x4000 != 0;
                let bw = match (bw_known, ru) {
                    (false, _) => Some(20),
                    (true, 0) => Some(20),
                    (true, 1) => Some(40),
                    (true, 2) => Some(80),
                    (true, 3) => Some(160),
                    (true, _) => None,
                };
                let gi_us = if d[1] & 0x0002 != 0 {
                    match (d[4] >> 4) & 0x03 {
                        1 => 1.6,
                        2 => 3.2,
                        _ => 0.8,
                    }
                } else {
                    0.8
                };
                let ltf_us = if d[1] & 0x0004 != 0 {
                    match (d[4] >> 6) & 0x03 {
                        1 => 3.2,
                        3 => 12.8,
                        _ => 6.4,
                    }
                } else {
                    6.4
                };
                phy.he = Some(He {
                    format,
                    mcs,
                    nss: ((d[5] & 0x0f) as u8).max(1),
                    bw_mhz: bw,
                    gi_us,
                    ltf_us,
                });
            }
            _ => {}
        }
        off += size;
    }
    let len = frame.len() - rt_len;
    phy.psdu_len = if fcs { len.saturating_sub(4) } else { len };
    Some(phy)
}

/// Bits per subcarrier × code rate for MCS 0..=11.
fn bits_per_sc(mcs: u8) -> Option<f64> {
    Some(match mcs {
        0 => 0.5,
        1 => 1.0,
        2 => 1.5,
        3 => 2.0,
        4 => 3.0,
        5 => 4.0,
        6 => 4.5,
        7 => 5.0,
        8 => 6.0,
        9 => 20.0 / 3.0,
        10 => 7.5,
        11 => 25.0 / 3.0,
        _ => return None,
    })
}

/// Data subcarriers of an HT/VHT channel.
fn nsd_vht(bw: u16) -> Option<f64> {
    Some(match bw {
        20 => 52.0,
        40 => 108.0,
        80 => 234.0,
        160 => 468.0,
        _ => return None,
    })
}

/// Data subcarriers of an HE channel.
fn nsd_he(bw: u16) -> Option<f64> {
    Some(match bw {
        20 => 234.0,
        40 => 468.0,
        80 => 980.0,
        160 => 1960.0,
        _ => return None,
    })
}

fn ht_ltf(nss: u8) -> f64 {
    if nss == 3 {
        4.0
    } else {
        nss as f64
    }
}

/// Preamble (µs), data bits per symbol, symbol duration (µs) of one PPDU; `None` when unknown.
fn shape(phy: &Phy) -> Option<(f64, f64, f64)> {
    if let Some(he) = phy.he {
        if he.format > 1 {
            return None;
        }
        let ndbps = nsd_he(he.bw_mhz?)? * bits_per_sc(he.mcs)? * he.nss as f64;
        let sig_a = if he.format == 1 { 16.0 } else { 8.0 };
        let preamble = 20.0 + 4.0 + sig_a + 4.0 + ht_ltf(he.nss) * (he.ltf_us + he.gi_us);
        return Some((preamble, ndbps, 12.8 + he.gi_us));
    }
    if let Some(v) = phy.vht {
        let ndbps = nsd_vht(v.bw_mhz)? * bits_per_sc(v.mcs)? * v.nss as f64;
        let preamble = 20.0 + 8.0 + 4.0 + 4.0 * ht_ltf(v.nss) + 4.0;
        return Some((preamble, ndbps, if v.short_gi { 3.6 } else { 4.0 }));
    }
    if let Some(h) = phy.ht {
        if h.mcs >= 32 {
            return None;
        }
        let nss = h.mcs / 8 + 1;
        let ndbps = nsd_vht(h.bw_mhz)? * bits_per_sc(h.mcs % 8)? * nss as f64;
        let preamble = 20.0 + 8.0 + 4.0 + 4.0 * ht_ltf(nss);
        return Some((preamble, ndbps, if h.short_gi { 3.6 } else { 4.0 }));
    }
    let r = phy.rate_500k?;
    match r {
        12 | 18 | 24 | 36 | 48 | 72 | 96 | 108 => Some((20.0, 4.0 * r as f64 / 2.0, 4.0)),
        _ => None,
    }
}

/// On-air duration of one frame in µs. `first_of_aggregate` charges the preamble, SERVICE and tail;
/// a later subframe of the same A-MPDU adds only its own bits and delimiter.
pub fn duration_us(phy: &Phy, first_of_aggregate: bool) -> Option<f64> {
    let len = phy.psdu_len as f64;
    if let (None, None, None, Some(r)) = (phy.he, phy.vht, phy.ht, phy.rate_500k) {
        if matches!(r, 2 | 4 | 11 | 22) {
            let preamble = if phy.short_preamble && r != 2 { 96.0 } else { 192.0 };
            return Some(preamble + 8.0 * len / (r as f64 / 2.0));
        }
    }
    let (preamble, ndbps, tsym) = shape(phy)?;
    if ndbps <= 0.0 {
        return None;
    }
    if phy.ampdu_ref.is_some() {
        let bits = 8.0 * (len + 4.0);
        return Some(if first_of_aggregate {
            preamble + (22.0 + bits) / ndbps * tsym
        } else {
            bits / ndbps * tsym
        });
    }
    let symbols = ((16.0 + 8.0 * len + 6.0) / ndbps).ceil();
    Some(preamble + symbols * tsym)
}

/// Tracks the A-MPDU reference so the preamble is charged once per aggregate.
#[derive(Debug, Default)]
pub struct Aggregates {
    last_ref: Option<u32>,
}

impl Aggregates {
    /// Duration of one frame given the aggregate seen before it.
    pub fn duration_us(&mut self, phy: &Phy) -> Option<f64> {
        let first = match phy.ampdu_ref {
            Some(r) => {
                let first = self.last_ref != Some(r);
                self.last_ref = Some(r);
                first
            }
            None => {
                self.last_ref = None;
                true
            }
        };
        duration_us(phy, first)
    }
}

/// One minute's airtime: injector and other transmitters apart, and the frames left unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MinuteAirtime {
    pub injector_us: f64,
    pub other_us: f64,
    pub known_frames: u64,
    pub unknown_frames: u64,
}

impl MinuteAirtime {
    pub fn note(&mut self, us: Option<f64>, injector: bool) {
        match us {
            Some(us) => {
                self.known_frames += 1;
                if injector {
                    self.injector_us += us;
                } else {
                    self.other_us += us;
                }
            }
            None => self.unknown_frames += 1,
        }
    }
}

/// File name inside the node_exporter textfile directory.
pub const FILE_NAME: &str = "csid_airtime.prom";
/// Per-minute record inside the session directory (shipped with the session).
pub const LOG_NAME: &str = "airtime.jsonl";

/// One `airtime.jsonl` line.
pub fn json_line(minute_start: u64, host: &str, session_id: &str, a: &MinuteAirtime) -> String {
    serde_json::json!({
        "minute_unix": minute_start,
        "host": host,
        "session_id": session_id,
        "injector_s": (a.injector_us / 1e3).round() / 1e3,
        "other_s": (a.other_us / 1e3).round() / 1e3,
        "known_frames": a.known_frames,
        "unknown_frames": a.unknown_frames,
    })
    .to_string()
}

/// Replace `<dir>/csid_airtime.prom` atomically.
pub fn write_textfile(dir: &std::path::Path, minute_start: u64, a: &MinuteAirtime) -> std::io::Result<()> {
    let path = dir.join(FILE_NAME);
    let tmp = path.with_extension("prom.tmp");
    std::fs::write(&tmp, prometheus_text(minute_start, a))?;
    std::fs::rename(&tmp, &path)
}

/// Render one minute as node_exporter textfile input.
pub fn prometheus_text(minute_start: u64, a: &MinuteAirtime) -> String {
    let mut s = String::new();
    s.push_str("# HELP csid_airtime_minute_start_seconds Unix start of the minute the airtime describes.\n");
    s.push_str("# TYPE csid_airtime_minute_start_seconds gauge\n");
    s.push_str(&format!("csid_airtime_minute_start_seconds {minute_start}\n"));
    s.push_str("# HELP csid_airtime_seconds Decoded-frame airtime in the minute (a lower bound on busy time), by source.\n");
    s.push_str("# TYPE csid_airtime_seconds gauge\n");
    s.push_str(&format!("csid_airtime_seconds{{source=\"injector\"}} {:.6}\n", a.injector_us / 1e6));
    s.push_str(&format!("csid_airtime_seconds{{source=\"other\"}} {:.6}\n", a.other_us / 1e6));
    s.push_str("# HELP csid_airtime_frames Frames in the minute whose airtime was computed (known) or not (unknown).\n");
    s.push_str("# TYPE csid_airtime_frames gauge\n");
    s.push_str(&format!("csid_airtime_frames{{rate=\"known\"}} {}\n", a.known_frames));
    s.push_str(&format!("csid_airtime_frames{{rate=\"unknown\"}} {}\n", a.unknown_frames));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A radiotap header with the given present word and field bytes, then `body`.
    fn frame(present: u32, fields: &[u8], body_len: usize) -> Vec<u8> {
        let len = 8 + fields.len();
        let mut f = vec![0u8, 0, (len & 0xff) as u8, (len >> 8) as u8];
        f.extend_from_slice(&present.to_le_bytes());
        f.extend_from_slice(fields);
        f.extend(std::iter::repeat_n(0u8, body_len));
        f
    }

    #[test]
    fn legacy_6_mbps_100_bytes() {
        // Flags (no FCS) + Rate 12 (6 Mbps). N_DBPS 24: ceil((16 + 800 + 6) / 24) = 35 symbols.
        let f = frame(0b110, &[0x00, 12], 100);
        let p = parse(&f).unwrap();
        assert_eq!((p.rate_500k, p.psdu_len), (Some(12), 100));
        assert_eq!(duration_us(&p, true), Some(20.0 + 35.0 * 4.0));
    }

    #[test]
    fn the_fcs_is_not_airtime_payload_twice() {
        let f = frame(0b110, &[0x10, 12], 104);
        assert_eq!(parse(&f).unwrap().psdu_len, 100);
    }

    #[test]
    fn dsss_1_mbps_uses_the_long_preamble() {
        let f = frame(0b110, &[0x02, 2], 10);
        let p = parse(&f).unwrap();
        assert_eq!(duration_us(&p, true), Some(192.0 + 80.0));
    }

    #[test]
    fn ht_mcs7_20_mhz_long_gi() {
        // present: MCS (bit 19). Field at offset 8: known 0x07, flags 0 (20 MHz, long GI), mcs 7.
        let f = frame(1 << 19, &[0x07, 0x00, 7], 1000);
        let p = parse(&f).unwrap();
        assert_eq!(p.ht, Some(Ht { mcs: 7, bw_mhz: 20, short_gi: false }));
        // N_DBPS 52 * 5 = 260; ceil((16 + 8000 + 6) / 260) = 31 symbols; preamble 36.
        assert_eq!(duration_us(&p, true), Some(36.0 + 31.0 * 4.0));
    }

    #[test]
    fn vht_80_mhz_two_streams_aligns_its_field() {
        // Flags at 8, then VHT aligned to 2 -> offset 10. known: bw (0x40) + GI (0x04); flags SGI;
        // bandwidth 4 (80 MHz); mcs 9, nss 2.
        let mut fields = vec![0x00u8, 0x00];
        fields.extend_from_slice(&[0x44, 0x00, 0x04, 4, 0x92, 0, 0, 0, 0, 0, 0, 0]);
        let f = frame((1 << 1) | (1 << 21), &fields, 1500);
        let p = parse(&f).unwrap();
        assert_eq!(p.vht, Some(Vht { mcs: 9, nss: 2, bw_mhz: 80, short_gi: true }));
        let ndbps: f64 = 234.0 * (20.0 / 3.0) * 2.0;
        let symbols = ((16.0 + 12000.0 + 6.0) / ndbps).ceil();
        assert_eq!(duration_us(&p, true), Some(20.0 + 8.0 + 4.0 + 8.0 + 4.0 + symbols * 3.6));
    }

    #[test]
    fn he_trigger_based_is_unknown_not_zero() {
        let mut fields = vec![0u8; 12];
        fields[0] = 0x03; // data1 format 3 (TB)
        let f = frame(1 << 23, &fields, 200);
        let p = parse(&f).unwrap();
        assert_eq!(duration_us(&p, true), None);
        let mut m = MinuteAirtime::default();
        m.note(duration_us(&p, true), false);
        assert_eq!((m.unknown_frames, m.other_us), (1, 0.0));
    }

    #[test]
    fn an_aggregate_pays_its_preamble_once() {
        // MCS + A-MPDU: MCS at 8..11, A-MPDU aligned to 4 -> 12..20.
        let mut fields = vec![0x07u8, 0x00, 7, 0];
        fields.extend_from_slice(&[9, 0, 0, 0, 0, 0, 0, 0]);
        let f = frame((1 << 19) | (1 << 20), &fields, 500);
        let p = parse(&f).unwrap();
        assert_eq!(p.ampdu_ref, Some(9));
        let mut agg = Aggregates::default();
        let first = agg.duration_us(&p).unwrap();
        let second = agg.duration_us(&p).unwrap();
        assert!(first > second + 30.0, "the first carries the preamble");
        assert!((second - 8.0 * 504.0 / 260.0 * 4.0).abs() < 1e-9);
    }

    #[test]
    fn not_radiotap_is_none() {
        assert_eq!(parse(&[1, 0, 8, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse(&[0, 0, 3]), None);
    }

    #[test]
    fn the_text_has_one_help_per_family() {
        let a = MinuteAirtime { injector_us: 1.5e6, other_us: 2e5, known_frames: 10, unknown_frames: 2 };
        let t = prometheus_text(60, &a);
        assert!(t.contains("csid_airtime_seconds{source=\"injector\"} 1.500000"));
        assert!(t.contains("csid_airtime_frames{rate=\"unknown\"} 2"));
        for name in ["csid_airtime_minute_start_seconds", "csid_airtime_seconds", "csid_airtime_frames"] {
            assert_eq!(t.matches(&format!("# HELP {name} ")).count(), 1);
        }
    }
}
