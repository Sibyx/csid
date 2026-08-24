//! # csiq — CSIQ Interchange Format v1
//!
//! A small, dependency-light, self-describing container for Wi-Fi Channel State
//! Information (CSI), plus a parser for the raw iwlwifi/iax vendor-event stream.
//!
//! The crate is deliberately split from the `csid` capture daemon so that the
//! **format is the citable artifact**: it builds on any platform, has no OS
//! dependencies, and can be vendored by any consumer (Rust, or via the Python
//! reference reader) without pulling in netlink/Linux machinery.
//!
//! ## Three uses, one record codec
//!
//! - [`container`] — the `.csiq` file: a header with an embedded session block
//!   followed by length-framed records.
//! - [`live`] — one record per datagram for the live stream.
//! - [`raw`] — parse the lossless driver-native `capture.raw` into records.
//!
//! All three share [`tlv`], so a field is defined exactly once.
//!
//! ## Timing rule
//!
//! Every record carries three clocks. **Analyse on `ftm`** (the 320 MHz
//! RF-plane baseband timestamp, 3.125 ns/tick, wrapping every ~13.42 s — unwrap
//! with [`FtmUnwrapper`]) and **anchor wallclock on `unix_ts_ns`**. `us` is a
//! third, coarser firmware clock.

pub mod container;
pub mod error;
pub mod live;
pub mod raw;
pub mod record;
pub mod tlv;

pub use container::{Reader, Writer};
pub use error::{CsiqError, Result};
pub use record::{CsiRecord, Modulation, PhyLabel, Width, RSSI_NO_MEASUREMENT};

/// File magic at the start of every `.csiq` container.
pub const MAGIC: [u8; 4] = *b"CSIQ";

/// The format version this build reads and writes.
pub const FORMAT_VERSION: u16 = 1;

/// Per-record framing tag in the file container.
pub const RECORD_TAG: u8 = 0xA1;

/// The AX210 baseband clock rate that ticks the `ftm` field (Hz).
pub const FTM_HZ: u64 = 320_000_000;

/// Convert an *unwrapped* `ftm` tick count to seconds.
pub fn ftm_to_seconds(ticks: u64) -> f64 {
    ticks as f64 / FTM_HZ as f64
}

/// Unwraps the 32-bit `ftm` clock into a monotonic 64-bit tick count.
///
/// The `ftm` field wraps every `2^32 / 320e6 ≈ 13.42 s`. Records arrive in
/// order and back-to-back frames resolve to tens of microseconds, so a forward
/// step smaller than the previous value implies exactly one wrap.
#[derive(Debug, Default, Clone)]
pub struct FtmUnwrapper {
    last: Option<u32>,
    wraps: u64,
}

impl FtmUnwrapper {
    /// New unwrapper with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next raw `ftm` value; returns the monotonic unwrapped ticks.
    pub fn push(&mut self, raw: u32) -> u64 {
        if let Some(prev) = self.last {
            if raw < prev {
                self.wraps += 1;
            }
        }
        self.last = Some(raw);
        self.wraps * (1u64 << 32) + raw as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Modulation, PhyLabel, Width};

    fn sample_record(ntone: u16, nrx: u8, ntx: u8) -> CsiRecord {
        let coeffs = ntone as usize * nrx as usize * ntx as usize;
        let iq: Vec<i16> = (0..(2 * coeffs) as i32)
            .map(|x| (x % 97 - 48) as i16)
            .collect();
        CsiRecord {
            ftm: 4_000_000_000,
            us: 123_456,
            unix_ts_ns: 1_700_000_000_000_000_000,
            rnf: 0x0442, // HE-ish: type nibble 4, nss field 0 -> 1, mcs 2
            phy: Some(PhyLabel {
                modulation: Modulation::He,
                mcs: 2,
                nss: 1,
            }),
            bw_antsel: crate::raw::decode_bw_antsel(0x0442),
            seq: 7,
            nrx,
            ntx,
            ntone,
            rssi: vec![-43, -44][..(nrx as usize).min(2)].to_vec(),
            src_mac: [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01],
            channel: 36,
            width: Width::W80,
            iq,
        }
    }

    // ── IP-139 Phase 2: per-record PHY truth ────────────────────────────────

    /// The bit positions come from `rs.h` in the driver we run, so pin them
    /// against words whose meaning is known independently.
    ///
    /// `0x4100` is the value 20 fleet arms write to `monitor_tx_rate` for
    /// 6 Mbps legacy OFDM. Decoding it must reproduce that description exactly,
    /// including the antenna bit `config.rs` documents but never named.
    #[test]
    fn the_fleets_own_injection_word_decodes_to_what_it_is_configured_as() {
        let phy = crate::raw::decode_rnf(0x4100).expect("a non-zero word decodes");
        assert_eq!(phy.modulation, Modulation::LegacyOfdm);
        assert_eq!(phy.mcs, 0, "legacy rate index 0 is 6 Mbps");

        let b = crate::raw::decode_bw_antsel(0x4100).expect("a non-zero word decodes");
        assert_eq!(b.bandwidth, crate::record::Bandwidth::W20);
        assert!(b.ant_a(), "bit 14 is RATE_MCS_ANT_A_MSK");
        assert!(!b.ant_b());
    }

    /// Bandwidth is bits 13–11, and each code is the driver's own.
    #[test]
    fn bandwidth_reads_bits_thirteen_to_eleven() {
        use crate::record::Bandwidth::*;
        for (code, want, mhz) in [
            (0u32, W20, 20u32),
            (1, W40, 40),
            (2, W80, 80),
            (3, W160, 160),
            (4, W320, 320),
        ] {
            // Carry a modulation type as well, so the word is realistic.
            let rnf = (1 << 8) | (code << 11);
            let b = crate::raw::decode_bw_antsel(rnf).unwrap();
            assert_eq!(b.bandwidth, want, "code {code}");
            assert_eq!(b.bandwidth.mhz(), Some(mhz));
            assert_eq!(b.bandwidth.to_code(), code as u8, "code round-trips");
        }
    }

    /// An unknown width code is carried verbatim rather than coerced to 20 MHz.
    /// A future firmware that adds one must not silently read as the narrowest.
    #[test]
    fn an_unknown_bandwidth_code_is_carried_not_guessed() {
        let b = crate::raw::decode_bw_antsel((1 << 8) | (7 << 11)).unwrap();
        assert_eq!(b.bandwidth, crate::record::Bandwidth::Unknown(7));
        assert_eq!(b.bandwidth.mhz(), None, "unknown has no MHz, not a default");
    }

    /// Antenna selection is a two-bit mask, not an index.
    #[test]
    fn antenna_selection_is_a_mask_over_bits_fifteen_and_fourteen() {
        let both = crate::raw::decode_bw_antsel((1 << 8) | (1 << 14) | (1 << 15)).unwrap();
        assert!(both.ant_a() && both.ant_b());
        assert_eq!(both.antenna_sel, 0b11);

        let b_only = crate::raw::decode_bw_antsel((1 << 8) | (1 << 15)).unwrap();
        assert!(!b_only.ant_a() && b_only.ant_b());
    }

    /// A `0x11` written by this build must survive a container round trip.
    #[test]
    fn bw_antsel_round_trips_through_the_container() {
        let rec = sample_record(242, 2, 1);
        assert!(rec.bw_antsel.is_some(), "the fixture must exercise the field");

        let payload = crate::tlv::encode_payload(&rec);
        let back = crate::tlv::decode_payload(&payload).expect("round trip");
        assert_eq!(back.bw_antsel, rec.bw_antsel);
        assert_eq!(back.rnf, rec.rnf);
    }

    /// Forward compatibility, both directions — the versioning promise the
    /// format makes, tested rather than asserted.
    ///
    /// A record with no `0x11` (every file in the archive) must decode with the
    /// field absent, and a record carrying an unknown *future* code must decode
    /// as if that code were not there.
    #[test]
    fn a_reader_tolerates_both_a_missing_and_an_unknown_field() {
        // A v1 record carries no 0x11, but it does carry `rnf`. The reader
        // recovers the field from those same bits rather than losing it, which
        // is what makes the whole existing archive readable without re-capture.
        let mut old = sample_record(52, 1, 1);
        old.bw_antsel = None;
        let back = crate::tlv::decode_payload(&crate::tlv::encode_payload(&old)).unwrap();
        assert_eq!(
            back.bw_antsel,
            crate::raw::decode_bw_antsel(old.rnf),
            "an absent 0x11 is recovered from rnf, which is the same bits"
        );

        // A record with neither is genuinely absent, and must stay absent.
        let mut blank = sample_record(52, 1, 1);
        blank.bw_antsel = None;
        blank.rnf = 0;
        let back = crate::tlv::decode_payload(&crate::tlv::encode_payload(&blank)).unwrap();
        assert_eq!(back.bw_antsel, None, "no rate information is not 20 MHz");

        // A future field this build has never heard of, appended to a valid record.
        let mut payload = crate::tlv::encode_payload(&sample_record(52, 1, 1));
        payload.push(0x7E); // unallocated
        payload.extend_from_slice(&4u32.to_le_bytes());
        payload.extend_from_slice(&[1, 2, 3, 4]);
        let back = crate::tlv::decode_payload(&payload).expect("unknown types are skipped");
        assert_eq!(back.ntone, 52);
        assert!(back.bw_antsel.is_some());
    }

    /// `rnf == 0` is the header's "no rate information", and both decoders must
    /// agree about it. A zero word is not a 20 MHz frame on antenna none.
    #[test]
    fn a_zero_rate_word_yields_no_bandwidth_and_no_phy() {
        assert!(crate::raw::decode_rnf(0).is_none());
        assert!(crate::raw::decode_bw_antsel(0).is_none());
    }

    /// The layout and I/Q order are invisible in amplitude — |a+bi| == |b+ai|,
    /// and a transposed read still "looks like CSI". Only an explicit test
    /// pins them, which is why this one exists.
    #[test]
    fn csi_storage_is_chain_major_and_imag_first() {
        let (ntone, nrx, ntx) = (4usize, 2u8, 1u8);
        let chains = nrx as usize * ntx as usize;
        // Build as the driver does: chain-major blocks, imaginary first.
        // Encode (chain, tone) into the real part so any mis-read shows up.
        let mut iq = Vec::new();
        for c in 0..chains {
            for t in 0..ntone {
                iq.push(-((c * 100 + t) as i16)); // imag
                iq.push((c * 100 + t) as i16); // real
            }
        }
        let mut r = sample_record(ntone as u16, nrx, ntx);
        r.iq = iq;

        // chain() reads one contiguous block.
        for c in 0..chains {
            let ch = r.chain(c).expect("chain in range");
            assert_eq!(ch.len(), ntone);
            for (t, (re, im)) in ch.iter().enumerate() {
                assert_eq!(*re, (c * 100 + t) as f32, "chain {c} tone {t} real");
                assert_eq!(*im, -((c * 100 + t) as f32), "chain {c} tone {t} imag");
            }
        }
        assert!(r.chain(chains).is_none(), "out-of-range chain");

        // complex() presents the same data as a tone-major view.
        let flat = r.complex().expect("dimensions match");
        assert_eq!(flat.len(), ntone * chains);
        for t in 0..ntone {
            for c in 0..chains {
                assert_eq!(flat[t * chains + c].0, (c * 100 + t) as f32);
            }
        }
    }

    #[test]
    fn tlv_roundtrip_preserves_record() {
        let r = sample_record(242, 2, 2);
        let payload = tlv::encode_payload(&r);
        let back = tlv::decode_payload(&payload).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn container_roundtrip_with_session() {
        let session = serde_json::json!({"session_id": "unit_test", "schema": "csiq/1"});
        let mut buf = Vec::new();
        {
            let mut w = Writer::new(&mut buf, Some(&session)).unwrap();
            w.write_record(&sample_record(52, 1, 1)).unwrap();
            w.write_record(&sample_record(996, 2, 2)).unwrap();
            assert_eq!(w.record_count(), 2);
            w.finish().unwrap();
        }
        let mut r = Reader::new(&buf[..]).unwrap();
        assert_eq!(r.session().unwrap()["session_id"], "unit_test");
        let recs: Vec<_> = (&mut r).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].ntone, 52);
        assert_eq!(recs[1].ntone, 996);
    }

    #[test]
    fn container_rejects_bad_magic() {
        let bytes = b"NOPExxxxxxxxxxxx";
        let got = Reader::new(&bytes[..]);
        assert!(
            matches!(got, Err(CsiqError::BadMagic { .. })),
            "expected BadMagic error"
        );
    }

    #[test]
    fn live_datagram_roundtrip() {
        let r = sample_record(242, 2, 2);
        let dg = live::encode(0xABCD, 99, &r);
        let decoded = live::decode(&dg).unwrap();
        assert_eq!(decoded.session_uid, 0xABCD);
        assert_eq!(decoded.seq, 99);
        assert_eq!(decoded.record, r);
    }

    #[test]
    fn unknown_tlv_is_skipped() {
        let r = sample_record(52, 1, 1);
        let mut payload = tlv::encode_payload(&r);
        // Append an unknown TLV: type 0x7E, 3 bytes.
        payload.push(0x7E);
        payload.extend_from_slice(&3u32.to_le_bytes());
        payload.extend_from_slice(&[1, 2, 3]);
        let back = tlv::decode_payload(&payload).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn ftm_unwrap_crosses_wrap() {
        let mut u = FtmUnwrapper::new();
        assert_eq!(u.push(u32::MAX - 10), (u32::MAX - 10) as u64);
        // Wrap: next value is small -> one wrap added.
        let after = u.push(5);
        assert_eq!(after, (1u64 << 32) + 5);
    }

    #[test]
    fn raw_parser_reads_synthetic_stream() {
        // Build one synthetic raw message matching the documented framing.
        let mut hdr = vec![0u8; raw::HEADER_LEN];
        // ftm@8
        hdr[8..12].copy_from_slice(&123u32.to_le_bytes());
        hdr[46] = 2; // nrx
        hdr[47] = 2; // ntx
        hdr[52..54].copy_from_slice(&52u16.to_le_bytes()); // ntone
                                                           // The firmware writes RSSI as a positive magnitude, as measured on
                                                           // hardware; the parser must hand back dBm.
        hdr[60..64].copy_from_slice(&43i32.to_le_bytes()); // rssi a
        hdr[64..68].copy_from_slice(&44i32.to_le_bytes()); // rssi b
        hdr[68..74].copy_from_slice(&[1, 2, 3, 4, 5, 6]); // src mac
        hdr[76] = 9; // seq
        hdr[88..92].copy_from_slice(&555u32.to_le_bytes()); // us
        hdr[92..96].copy_from_slice(&0x0442u32.to_le_bytes()); // rnf
        hdr[208..216].copy_from_slice(&42u64.to_le_bytes()); // unix_ts_ns
        hdr[216..220].copy_from_slice(&36u32.to_le_bytes()); // channel

        let coeffs = 52 * 2 * 2;
        let mut csi = Vec::new();
        for i in 0..(2 * coeffs) {
            csi.extend_from_slice(&((i as i16) - 100).to_le_bytes());
        }

        // Frame it: [be32 msg_len][be32 hdr_len][hdr][be32 csi_len][csi]
        let mut body = Vec::new();
        body.extend_from_slice(&(raw::HEADER_LEN as u32).to_be_bytes());
        body.extend_from_slice(&hdr);
        body.extend_from_slice(&(csi.len() as u32).to_be_bytes());
        body.extend_from_slice(&csi);
        let mut stream = Vec::new();
        stream.extend_from_slice(&(body.len() as u32).to_be_bytes());
        stream.extend_from_slice(&body);

        let mut rr = raw::RawReader::new(&stream[..], Width::W80);
        let rec = rr.next_record().unwrap().unwrap();
        assert_eq!(rec.nrx, 2);
        assert_eq!(rec.ntone, 52);
        assert_eq!(rec.channel, 36);
        assert_eq!(
            rec.rssi,
            vec![-43, -44],
            "the driver's positive magnitude must reach consumers as dBm"
        );
        assert_eq!(rec.unix_ts_ns, 42);
        assert_eq!(rec.phy.unwrap().modulation, Modulation::He);
        assert_eq!(rec.iq.len(), 2 * coeffs);
        assert!(rr.next_record().unwrap().is_none());
    }
}
