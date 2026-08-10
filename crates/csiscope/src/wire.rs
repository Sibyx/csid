//! The header the browser reads, as types rather than as a tree.
//!
//! The frame layout is documented in [`crate::frame`]. This module is only
//! about the JSON header inside it, and about one structural decision: the
//! header is **split in two**.
//!
//! Everything derived from the window — the spectra, the bundle, the impulse
//! response, the Doppler column, the mixes, the talker table — depends only on
//! the view settings and the ring, so two browsers watching the same view see
//! byte-identical values. Only the waterfall differs, because each client holds
//! its own cursor and draws the records *it* has not drawn yet.
//!
//! So the shared part is serialised once per tick per distinct view, as a JSON
//! object body with its braces stripped, and each client splices its own few
//! fields in front of it. The alternative — building a `serde_json::Value`
//! tree per client and serialising that — was about 6% of the process on its
//! own, in `BTreeMap<String, Value>` inserts and the drops that followed.

use std::fmt;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::class::ClassKey;
use crate::dsp;

// -- small serialisation helpers ----------------------------------------------

/// `name -> [element_offset, element_count]` for the typed arrays in a frame.
///
/// A `Vec` rather than a `HashMap`: there are a dozen entries, they are
/// appended in a fixed order, and the browser looks them up by name once per
/// frame from a JS object either way.
#[derive(Default, Debug, Clone)]
pub struct ArrayMap(Vec<(&'static str, [usize; 2])>);

impl ArrayMap {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn push(&mut self, name: &'static str, offset: usize, len: usize) {
        self.0.push((name, [offset, len]));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for ArrayMap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

/// A histogram over a `Copy` key, rendered as a JSON object of
/// `"<key>": count`.
///
/// The PHY mix is five of these, over the whole window. Counting into a
/// `HashMap<String, u64>` meant a `to_string` and a hash per record per field —
/// 1,280 allocations a frame to end up with a handful of distinct values. A
/// linear scan over `Copy` keys allocates only when a genuinely new value
/// appears, and the strings are built once, at serialisation time.
#[derive(Debug, Clone)]
pub struct CountMap<K>(Vec<(K, u64)>);

impl<K> Default for CountMap<K> {
    fn default() -> Self {
        CountMap(Vec::new())
    }
}

impl<K: Copy + PartialEq> CountMap<K> {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    #[inline]
    pub fn add(&mut self, key: K) {
        for e in self.0.iter_mut() {
            if e.0 == key {
                e.1 += 1;
                return;
            }
        }
        self.0.push((key, 1));
    }
}

impl<K: fmt::Display> Serialize for CountMap<K> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            // `collect_str` renders straight into the output buffer; no
            // intermediate `String` per key.
            m.serialize_entry(&Displayed(k), v)?;
        }
        m.end()
    }
}

/// Serialises anything `Display` as a JSON string, without materialising one.
pub struct Displayed<T>(pub T);

impl<T: fmt::Display> Serialize for Displayed<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

/// A MAC address in the canonical `aa:bb:cc:dd:ee:ff` form.
///
/// The old rendering was `m.iter().map(|b| format!("{b:02x}")).join(":")` —
/// six `String`s, a `Vec` and a join, per address, per frame, for the record
/// panel and every row of the talker table. This writes 17 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mac(pub [u8; 6]);

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; 17];
        for (i, &b) in self.0.iter().enumerate() {
            let o = i * 3;
            buf[o] = HEX[(b >> 4) as usize];
            buf[o + 1] = HEX[(b & 0xf) as usize];
            if i < 5 {
                buf[o + 2] = b':';
            }
        }
        // Every byte written is ASCII by construction.
        f.write_str(std::str::from_utf8(&buf).unwrap_or("??"))
    }
}

impl Serialize for Mac {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

// -- the shared half of the header --------------------------------------------

#[derive(Serialize, Debug, Clone, Default)]
pub struct ClassEntry {
    pub key: ClassKey,
    pub label: String,
    pub count: u64,
    pub share: f64,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct ClassInfo {
    pub key: ClassKey,
    pub label: String,
    /// Did the operator pin this class, or is it just the current majority?
    pub pinned: bool,
    pub share: f64,
    pub count: u64,
    pub available: Vec<ClassEntry>,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct GeometryInfo {
    pub ntone: usize,
    pub nrx: usize,
    pub ntx: usize,
    pub nchain: usize,
    pub chain: usize,
    pub chain_b: Option<usize>,
    pub chain_labels: Vec<String>,
    pub dimensions_ok: bool,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct RadioInfo {
    pub channel: u32,
    pub width: String,
    pub freq_mhz: f64,
    /// True while the frequency is inferred from the channel number rather
    /// than pinned by the operator — 6 GHz channel numbering overlaps 2.4 GHz,
    /// so the inference genuinely cannot resolve it.
    pub freq_assumed: bool,
    pub spacing_hz: f64,
    pub bw_mhz: f64,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct PhyInfo {
    pub modulation: String,
    pub mcs: u8,
    pub nss: u8,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct RecordInfo {
    pub session_uid: u64,
    pub seq: u32,
    pub ftm: u32,
    pub ftm_ticks: u64,
    pub us: u32,
    pub unix_ts_ns: u64,
    pub recv_ns: u64,
    pub rssi: Vec<i16>,
    pub src_mac: Mac,
    pub rnf: u32,
    pub phy: Option<PhyInfo>,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct PhaseFit {
    pub slope_rad_per_tone: f32,
    pub intercept_rad: f32,
    pub tau_ns: f32,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct BundleInfo {
    pub width_db: f32,
    pub n: usize,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct CirInfo {
    pub bin_ns: f32,
    pub peak_bin: usize,
    pub rms_delay_ns: f32,
    pub taps: usize,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct DopplerInfo {
    pub fs_hz: f32,
    pub max_hz: f32,
    pub max_speed_ms: f32,
    pub arrival_cv: f32,
    pub conjugate_pair: bool,
    pub nfft: usize,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct TimingInfo {
    pub ftm: dsp::Timing,
    pub host: dsp::Timing,
    pub hist_max_us: f32,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct ClockInfo {
    pub host_span_us: f64,
    pub fw_span_us: f64,
    pub ftm_span_us: f64,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct SeriesInfo {
    pub tones: Vec<usize>,
    pub len: usize,
    pub rssi_chains: usize,
}

/// Distribution of PHY labels, tone counts and widths over the window — the
/// live version of the "CSI mix" column `csid bench` reports per run.
#[derive(Serialize, Debug, Clone, Default)]
pub struct MixInfo {
    pub modulation: CountMap<crate::class::Phy>,
    pub ntone: CountMap<u16>,
    pub nss: CountMap<u8>,
    pub mcs: CountMap<u8>,
    pub width: CountMap<WidthKey>,
}

impl MixInfo {
    pub fn clear(&mut self) {
        self.modulation.clear();
        self.ntone.clear();
        self.nss.clear();
        self.mcs.clear();
        self.width.clear();
    }
}

/// `csiq::Width` is `Copy + Eq` but not `Hash`; wrapping it keeps the count
/// map's key requirements honest without touching the format crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidthKey(pub csiq::Width);

impl fmt::Display for WidthKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct Talker {
    pub mac: Mac,
    pub count: u64,
    pub rate_hz: f64,
    pub rssi: Option<f64>,
    pub last_ns: u64,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct StreamInfo {
    pub window: usize,
    pub window_all: usize,
    pub depth: usize,
    pub total: u64,
    pub received: u64,
    pub decode_errors: u64,
    pub sender_gaps: u64,
    pub session_changes: u64,
    pub bytes: u64,
    pub source: String,
    pub uptime_s: u64,
}

/// The waterfall's *plan* — scope, width and frequency span — is shared even
/// though its pixels are not: it depends on the settings and the window, not
/// on where any one client's cursor sits.
#[derive(Serialize, Debug, Clone, Default)]
pub struct WaterfallInfo {
    pub scope: &'static str,
    pub bins: usize,
    pub span_mhz: f64,
}

/// Everything in the header that does not depend on which client is asking.
#[derive(Serialize, Debug, Clone, Default)]
pub struct SharedHeader {
    pub waterfall: WaterfallInfo,
    pub class: ClassInfo,
    pub geometry: GeometryInfo,
    pub radio: RadioInfo,
    pub record: RecordInfo,
    pub phase_fit: PhaseFit,
    pub bundle: BundleInfo,
    pub cir: CirInfo,
    pub doppler: DopplerInfo,
    pub timing: TimingInfo,
    pub clocks: ClockInfo,
    pub series: SeriesInfo,
    pub validation: dsp::Validation,
    pub mix: MixInfo,
    pub talkers: Vec<Talker>,
    pub stream: StreamInfo,
    pub f32: ArrayMap,
    pub n_f32: usize,
}

/// The fields only this client can fill in.
#[derive(Serialize, Debug, Clone, Default)]
pub struct ClientHeader {
    pub t: &'static str,
    /// Absolute ring index this client has now drawn up to.
    pub cursor: u64,
    /// Records of the selected class the client did not receive — dropped by
    /// the ring or by the row budget. Both belong in the same honest count.
    pub skipped: u64,
    /// Records that arrived but belong to another class.
    pub other_class: u64,
    pub wf_rows: usize,
    pub u8: ArrayMap,
    pub n_u8: usize,
}

/// Serialise `shared` as a JSON object *body*: the braces removed, ready to be
/// spliced after another object's fields.
///
/// `serde_json` always renders a struct as `{...}`, so stripping the first and
/// last byte is exact rather than a guess; the assertion states that
/// dependency instead of leaving it implicit.
pub fn shared_body(shared: &SharedHeader) -> String {
    let mut s = serde_json::to_string(shared).unwrap_or_else(|_| "{}".into());
    debug_assert!(s.starts_with('{') && s.ends_with('}'));
    s.truncate(s.len().saturating_sub(1));
    s.remove(0);
    s
}

/// Splice a client's own fields in front of a shared body, producing the
/// complete header object.
pub fn header_bytes(client: &ClientHeader, shared_body: &str, out: &mut Vec<u8>) {
    out.clear();
    let mut c = serde_json::to_vec(client).unwrap_or_else(|_| b"{}".to_vec());
    // `{...}` -> `{...,<shared>}`. An empty shared body (no analysis yet)
    // would leave a trailing comma, so it is checked rather than assumed.
    if shared_body.is_empty() {
        out.append(&mut c);
        return;
    }
    c.pop(); // the closing brace
    out.append(&mut c);
    out.push(b',');
    out.extend_from_slice(shared_body.as_bytes());
    out.push(b'}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mac_renders_the_canonical_form() {
        assert_eq!(
            Mac([0xde, 0xad, 0xbe, 0xef, 0x00, 0x07]).to_string(),
            "de:ad:be:ef:00:07"
        );
        assert_eq!(Mac([0; 6]).to_string(), "00:00:00:00:00:00");
        assert_eq!(Mac([0xff; 6]).to_string(), "ff:ff:ff:ff:ff:ff");
        // And it agrees with the formatting it replaced.
        for m in [[1u8, 2, 3, 4, 5, 6], [0xab, 0, 0xff, 0x10, 0x0f, 0x99]] {
            let old = m
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":");
            assert_eq!(Mac(m).to_string(), old);
        }
    }

    #[test]
    fn a_count_map_serialises_as_an_object_of_counts() {
        let mut m: CountMap<u16> = CountMap::default();
        for _ in 0..3 {
            m.add(52);
        }
        m.add(242);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v["52"], 3);
        assert_eq!(v["242"], 1);
    }

    /// The splice must produce exactly the object the browser would have got
    /// from one serialisation of the whole header.
    #[test]
    fn the_spliced_header_is_a_single_valid_object() {
        let mut shared = SharedHeader::default();
        shared.stream.source = "test".into();
        shared.n_f32 = 12;
        shared.f32.push("amp_db", 0, 12);
        let body = shared_body(&shared);

        let mut client = ClientHeader {
            t: "frame",
            cursor: 99,
            skipped: 4,
            other_class: 2,
            wf_rows: 7,
            n_u8: 5,
            ..Default::default()
        };
        client.u8.push("waterfall", 0, 5);

        let mut out = Vec::new();
        header_bytes(&client, &body, &mut out);
        let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");

        assert_eq!(v["t"], "frame");
        assert_eq!(v["cursor"], 99);
        assert_eq!(v["skipped"], 4);
        assert_eq!(v["wf_rows"], 7);
        assert_eq!(v["n_f32"], 12);
        assert_eq!(v["n_u8"], 5);
        assert_eq!(v["f32"]["amp_db"], serde_json::json!([0, 12]));
        assert_eq!(v["u8"]["waterfall"], serde_json::json!([0, 5]));
        assert_eq!(v["stream"]["source"], "test");
    }

    /// A client that ticks before any analysis has run still has to emit a
    /// parseable frame rather than `{...,}`.
    #[test]
    fn an_empty_shared_body_does_not_produce_a_trailing_comma() {
        let mut out = Vec::new();
        header_bytes(
            &ClientHeader {
                t: "frame",
                ..Default::default()
            },
            "",
            &mut out,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["t"], "frame");
    }
}
