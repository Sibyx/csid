//! The wire format between the analysis side and the browser.
//!
//! A 996-tone 2×2 capture at 20 frames a second is a lot of numbers, and JSON
//! would spend most of the link budget on decimal digits. Each frame is
//! therefore one binary WebSocket message:
//!
//! ```text
//! [u32 LE header_len][header JSON, space-padded][f32 section LE][u8 section]
//! ```
//!
//! The header is padded so the f32 section starts 4-byte aligned — a browser
//! cannot build a `Float32Array` view over a misaligned offset. Every array is
//! declared in the header as `name: [element_offset, element_count]`, so the
//! client does zero parsing: it takes typed-array views straight onto the
//! received `ArrayBuffer`.
//!
//! Adding a field is backwards compatible in both directions: an old client
//! ignores an unknown array, and a new client checks for absence.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// What the client wants to see. Sent as JSON on the same socket; every field
/// is optional so a client can PATCH one knob.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewSettings {
    /// Chain index for every single-chain view.
    pub chain: usize,
    /// Second chain for the conjugate-multiplication step of the Doppler
    /// series. `None` disables it and leaves the spectrum CFO-contaminated.
    pub chain_b: Option<usize>,
    /// Records behind the windowed views (bundle, Doppler, timing, mixes).
    pub window: usize,
    /// Maximum waterfall rows carried per frame. The stream can exceed the
    /// frame rate thirtyfold, so this is what decides how much of the capture
    /// the waterfall actually shows rather than skips.
    pub wf_rows: usize,
    /// Zero-padded FFT length for the channel impulse response.
    ///
    /// Padding does not create resolution — that is fixed at `1/BW`, about
    /// 53 ns on a 19 MHz HE20 grid — it interpolates between taps so the
    /// profile is readable instead of four blocky points.
    pub cir_nfft: usize,
    /// Taps returned from the impulse response.
    pub cir_taps: usize,
    /// FFT length for the Doppler column.
    pub doppler_nfft: usize,
    /// dB range the waterfall quantises into.
    pub db_min: f32,
    pub db_max: f32,
    /// Subcarriers tracked as amplitude time series.
    pub series_tones: Vec<usize>,
    /// Freeze the analysis (the capture is untouched either way).
    pub paused: bool,
    /// Target frames per second.
    pub fps: f32,
    /// Record class to scope every view to, as `"<ntone>:<modulation>"`
    /// (e.g. `"56:ht"`).
    ///
    /// An ambient capture on a busy channel interleaves several incompatible
    /// geometries — legacy 52-tone, HT 56-tone, HE 242-tone — and a view that
    /// mixes them is not a measurement of anything. Unset means "follow
    /// whichever class dominates the window", which is right until the
    /// operator wants a specific one held still.
    pub class: Option<String>,
    /// Waterfall scope: `"class"` draws only the selected record class at its
    /// native tone grid; `"all"` draws **every** record on a shared frequency
    /// axis.
    ///
    /// The per-class views have to be scoped — a spectrum, a phase fit and a
    /// Doppler series are only meaningful over one geometry. The waterfall is
    /// the exception: every class occupies the same RF channel, so rows of
    /// different tone counts *can* share an axis if it is frequency rather
    /// than subcarrier index. That makes it possible to watch the whole
    /// channel without discarding anything.
    pub wf_scope: String,
    /// Column count for the shared-frequency waterfall.
    pub wf_bins: usize,
    /// Control-channel frequency in MHz, pinned by the operator.
    ///
    /// It sets the Doppler view's speed axis (`v = λ·f_D/2`). Left unset, the
    /// frequency is inferred from the record's channel number — which cannot
    /// resolve 6 GHz, whose channel numbering overlaps 2.4 GHz. The console
    /// fills this in from the running experiment and marks the axis "assumed"
    /// until it does.
    pub freq_mhz: Option<f64>,
}

impl Default for ViewSettings {
    fn default() -> Self {
        ViewSettings {
            chain: 0,
            chain_b: Some(1),
            window: 256,
            wf_rows: 48,
            cir_nfft: 2048,
            cir_taps: 128,
            doppler_nfft: 256,
            db_min: 0.0,
            db_max: 60.0,
            series_tones: vec![],
            paused: false,
            fps: 20.0,
            class: None,
            wf_scope: "class".to_string(),
            wf_bins: 256,
            freq_mhz: None,
        }
    }
}

impl ViewSettings {
    /// Clamp everything a browser could set to something a Pi can survive.
    pub fn sanitise(&mut self) {
        self.window = self.window.clamp(16, 4096);
        self.wf_rows = self.wf_rows.clamp(1, 256);
        self.cir_nfft = self.cir_nfft.clamp(64, 8192).next_power_of_two();
        self.cir_taps = self.cir_taps.clamp(8, 1024);
        self.doppler_nfft = self.doppler_nfft.clamp(32, 4096).next_power_of_two();
        self.fps = self.fps.clamp(1.0, 60.0);
        self.series_tones.truncate(8);
        if self.wf_scope != "all" {
            self.wf_scope = "class".to_string();
        }
        self.wf_bins = self.wf_bins.clamp(32, 2048);
        if self.db_max <= self.db_min {
            self.db_max = self.db_min + 1.0;
        }
        if self.chain_b == Some(self.chain) {
            self.chain_b = None;
        }
    }

    /// Frame interval implied by `fps`.
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(1.0 / self.fps.max(1.0))
    }
}

/// Accumulates named typed arrays and emits the binary frame.
#[derive(Default)]
pub struct Encoder {
    f32s: Vec<f32>,
    u8s: Vec<u8>,
    map32: HashMap<String, [usize; 2]>,
    map8: HashMap<String, [usize; 2]>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an `f32` array under `name`.
    pub fn f32s(&mut self, name: &str, data: &[f32]) {
        self.map32
            .insert(name.to_string(), [self.f32s.len(), data.len()]);
        self.f32s.extend_from_slice(data);
    }

    /// Append a `u8` array under `name`.
    pub fn u8s(&mut self, name: &str, data: &[u8]) {
        self.map8
            .insert(name.to_string(), [self.u8s.len(), data.len()]);
        self.u8s.extend_from_slice(data);
    }

    /// Serialise. `meta` is merged into the header object.
    pub fn finish(self, mut meta: Value) -> Vec<u8> {
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("f32".into(), json!(self.map32));
            obj.insert("u8".into(), json!(self.map8));
            obj.insert("n_f32".into(), json!(self.f32s.len()));
            obj.insert("n_u8".into(), json!(self.u8s.len()));
        }
        let mut header = serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec());
        // Pad so the f32 section is 4-byte aligned for the browser's typed-array
        // view. JSON tolerates trailing whitespace, so spaces are free padding.
        while (4 + header.len()) % 4 != 0 {
            header.push(b' ');
        }

        let mut out = Vec::with_capacity(4 + header.len() + self.f32s.len() * 4 + self.u8s.len());
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        for v in &self.f32s {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.u8s);
        out
    }
}

/// Quantise dB values into the `[db_min, db_max]` byte range used by the
/// waterfall. Out-of-range values clamp rather than wrap, so a saturating
/// channel stays legible instead of aliasing into the dark end of the colormap.
pub fn quantise_db(values: &[f32], db_min: f32, db_max: f32, out: &mut Vec<u8>) {
    let span = (db_max - db_min).max(1e-6);
    out.extend(values.iter().map(|&v| {
        let n = ((v - db_min) / span * 255.0).round();
        n.clamp(0.0, 255.0) as u8
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors what the browser does, so the layout contract is tested rather
    /// than asserted in a comment.
    fn decode(buf: &[u8]) -> (Value, Vec<f32>, Vec<u8>) {
        let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let header: Value = serde_json::from_slice(&buf[4..4 + hlen]).unwrap();
        let f32_start = 4 + hlen;
        assert_eq!(f32_start % 4, 0, "f32 section must be 4-byte aligned");
        let n32 = header["n_f32"].as_u64().unwrap() as usize;
        let f = (0..n32)
            .map(|i| {
                let o = f32_start + i * 4;
                f32::from_le_bytes(buf[o..o + 4].try_into().unwrap())
            })
            .collect();
        let u = buf[f32_start + n32 * 4..].to_vec();
        (header, f, u)
    }

    #[test]
    fn frame_roundtrips_through_the_documented_layout() {
        let mut e = Encoder::new();
        e.f32s("amp_db", &[1.0, 2.0, 3.0]);
        e.f32s("phase", &[-0.5, 0.5]);
        e.u8s("wf", &[10, 20, 30, 40]);
        let buf = e.finish(json!({"t": "frame", "cursor": 42}));

        let (header, f, u) = decode(&buf);
        assert_eq!(header["cursor"], 42);
        assert_eq!(header["f32"]["amp_db"], json!([0, 3]));
        assert_eq!(header["f32"]["phase"], json!([3, 2]));
        assert_eq!(header["u8"]["wf"], json!([0, 4]));
        assert_eq!(f, vec![1.0, 2.0, 3.0, -0.5, 0.5]);
        assert_eq!(u, vec![10, 20, 30, 40]);
    }

    #[test]
    fn header_is_padded_for_every_length() {
        // Sweep header lengths by varying a string field, and check alignment
        // holds for all four residues.
        for pad in 0..8 {
            let mut e = Encoder::new();
            e.f32s("x", &[1.0]);
            let buf = e.finish(json!({"note": "x".repeat(pad)}));
            let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            assert_eq!((4 + hlen) % 4, 0, "pad {pad}");
            let (_, f, _) = decode(&buf);
            assert_eq!(f, vec![1.0]);
        }
    }

    #[test]
    fn quantisation_clamps_instead_of_wrapping() {
        let mut out = Vec::new();
        quantise_db(&[-50.0, 0.0, 30.0, 60.0, 200.0], 0.0, 60.0, &mut out);
        assert_eq!(out, vec![0, 0, 128, 255, 255]);
    }

    #[test]
    fn settings_sanitise_absurd_input() {
        let mut s = ViewSettings {
            window: 1_000_000,
            wf_rows: 0,
            cir_nfft: 300,
            doppler_nfft: 5,
            fps: 1000.0,
            db_min: 10.0,
            db_max: 5.0,
            chain: 2,
            chain_b: Some(2),
            series_tones: (0..40).collect(),
            ..Default::default()
        };
        s.sanitise();
        assert_eq!(s.window, 4096);
        assert_eq!(s.wf_rows, 1);
        assert_eq!(s.cir_nfft, 512, "must round up to a power of two");
        assert_eq!(s.doppler_nfft, 32);
        assert_eq!(s.fps, 60.0);
        assert!(s.db_max > s.db_min);
        assert_eq!(s.chain_b, None, "a chain cannot pair with itself");
        assert_eq!(s.series_tones.len(), 8);
    }
}
