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
//!
//! ## The f32 section is shared, the u8 section is not
//!
//! Every `f32` array in a frame — spectra, phase, bundle, impulse response,
//! Doppler, time series — is a function of the window and the view settings
//! alone. The one `u8` array is the waterfall, which is a function of *this
//! client's cursor*. So the f32 section is built once per tick per distinct
//! view and memcpy'd into every client's frame, and only the waterfall is
//! drawn per client.

use serde::{Deserialize, Serialize};

use crate::class::ClassKey;
use crate::wire::{ArrayMap, ClientHeader};

// The f32 section is written by casting the buffer, which assumes the wire's
// declared little-endian layout is the host's. Every target this runs on is
// little-endian; saying so here means a port fails to build rather than
// silently serving byte-swapped spectra.
#[cfg(target_endian = "big")]
compile_error!("the csiscope frame format is little-endian; this target is not");

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
    /// Transmitter to scope every view to, as a lower-case colon-separated MAC.
    ///
    /// The companion to `class`, and on an illuminated capture the more
    /// important of the two: the class selector is degenerate when one geometry
    /// is 100% of the channel, while the transmitter axis separates a 100 Hz
    /// injector from the ambient talkers whose frames are interleaved with its
    /// own. Unset means "follow the busiest", which selects the injector by
    /// itself on a lit channel.
    ///
    /// A transmitter that leaves the air falls back to the busiest one, for the
    /// same reason a pinned class does: the console's job is to keep showing the
    /// channel.
    pub smac: Option<String>,
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
            smac: None,
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
        // One spelling reaches the analysis, so two clients that typed the same
        // address in different cases share one computed view rather than two.
        // An unparseable address is treated as unset for the same reason an
        // unparseable class is: the console keeps showing the channel.
        self.smac = self
            .smac
            .take()
            .and_then(|s| parse_mac(&s))
            .map(|m| format_mac(&m));
    }

    /// Frame interval implied by `fps`.
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(1.0 / self.fps.max(1.0))
    }

    /// The pinned class, if the client sent one this build understands.
    ///
    /// An unparseable class is treated as unpinned rather than as an error:
    /// the console's job is to keep showing the channel, and falling back to
    /// the dominant class is what it does when a pinned one leaves the air.
    pub fn class_key(&self) -> Option<ClassKey> {
        self.class.as_deref().and_then(|s| s.parse().ok())
    }

    /// The pinned transmitter, if the client sent one this build can parse.
    pub fn smac_bytes(&self) -> Option<[u8; 6]> {
        self.smac.as_deref().and_then(parse_mac)
    }

    /// The subset of settings the shared analysis depends on.
    ///
    /// Two clients whose views agree here get the same numbers, so they can
    /// share one analysis. `fps` and `paused` are deliberately absent: they
    /// govern *when* a client is served, not *what* it is served.
    pub fn view_key(&self) -> ViewKey {
        ViewKey {
            chain: self.chain,
            chain_b: self.chain_b,
            window: self.window,
            cir_nfft: self.cir_nfft,
            cir_taps: self.cir_taps,
            doppler_nfft: self.doppler_nfft,
            db_min: self.db_min.to_bits(),
            db_max: self.db_max.to_bits(),
            series_tones: self.series_tones.clone(),
            class: self.class.clone(),
            smac: self.smac.clone(),
            wf_scope: self.wf_scope.clone(),
            wf_bins: self.wf_bins,
            freq_mhz: self.freq_mhz.map(f64::to_bits),
        }
    }
}

/// Identity of a shared analysis. See [`ViewSettings::view_key`].
///
/// The float fields are compared by bit pattern: these are knob positions
/// echoed back from the browser, not measurements, so "the same slider value"
/// is exactly bitwise equality and `NaN` never needs to compare equal to
/// itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ViewKey {
    pub chain: usize,
    pub chain_b: Option<usize>,
    pub window: usize,
    pub cir_nfft: usize,
    pub cir_taps: usize,
    pub doppler_nfft: usize,
    pub db_min: u32,
    pub db_max: u32,
    pub series_tones: Vec<usize>,
    pub class: Option<String>,
    pub smac: Option<String>,
    pub wf_scope: String,
    pub wf_bins: usize,
    pub freq_mhz: Option<u64>,
}

/// What a client needs in order to draw waterfall rows that line up with the
/// shared analysis around them.
#[derive(Debug, Clone, Default)]
pub struct WaterfallPlan {
    /// `true` when rows of every class share one frequency axis.
    pub all_scope: bool,
    pub bins: usize,
    pub span_hz: f64,
    /// The class every other view is scoped to.
    pub class: ClassKey,
    /// Tone count of that class, i.e. the row width in `class` scope.
    pub ntone: usize,
    /// The transmitter every other view is scoped to. Rows from anyone else are
    /// not drawn, so the waterfall shows the same records the panels beside it
    /// were computed from.
    pub smac: Option<[u8; 6]>,
    pub chain: usize,
    pub db_min: f32,
    pub db_max: f32,
    /// Row budget per frame.
    pub rows: usize,
}

/// One tick's shared analysis, ready to be spliced into any number of frames.
#[derive(Debug, Clone, Default)]
pub struct SharedFrame {
    /// The shared header fields, serialised as a JSON object body without its
    /// enclosing braces.
    pub header_body: String,
    /// The f32 section, already in wire order.
    pub f32_bytes: Vec<u8>,
    pub n_f32: usize,
    pub plan: WaterfallPlan,
}

/// Accumulates the named `f32` arrays of one frame.
///
/// Values land in a single contiguous buffer as they are produced, and the
/// buffer is cast to bytes once. The previous encoder appended each value with
/// `to_le_bytes`, four bytes at a time, over arrays that reach a quarter of a
/// million elements at 996 tones.
#[derive(Default, Debug)]
pub struct F32Section {
    data: Vec<f32>,
    map: ArrayMap,
}

impl F32Section {
    pub fn clear(&mut self) {
        self.data.clear();
        self.map.clear();
    }

    /// Append an `f32` array under `name`.
    pub fn push(&mut self, name: &'static str, data: &[f32]) {
        self.map.push(name, self.data.len(), data.len());
        self.data.extend_from_slice(data);
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn map(&self) -> &ArrayMap {
        &self.map
    }

    /// The section as wire bytes.
    pub fn bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.data)
    }
}

/// Assemble one client's frame.
///
/// `u8s` is that client's waterfall; everything else comes from the shared
/// analysis and is copied, not recomputed.
pub fn encode(client: &ClientHeader, shared: &SharedFrame, u8s: &[u8], out: &mut Vec<u8>) {
    let mut header = Vec::new();
    crate::wire::header_bytes(client, &shared.header_body, &mut header);
    // Pad so the f32 section is 4-byte aligned for the browser's typed-array
    // view. JSON tolerates trailing whitespace, so spaces are free padding.
    while (4 + header.len()) % 4 != 0 {
        header.push(b' ');
    }

    out.clear();
    out.reserve(4 + header.len() + shared.f32_bytes.len() + u8s.len());
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&shared.f32_bytes);
    out.extend_from_slice(u8s);
}

/// Quantise dB values into the `[db_min, db_max]` byte range used by the
/// waterfall. Out-of-range values clamp rather than wrap, so a saturating
/// channel stays legible instead of aliasing into the dark end of the colormap.
pub fn quantise_db(values: &[f32], db_min: f32, db_max: f32, out: &mut Vec<u8>) {
    let span = (db_max - db_min).max(1e-6);
    let scale = 255.0 / span;
    out.extend(values.iter().map(|&v| {
        let n = ((v - db_min) * scale).round();
        n.clamp(0.0, 255.0) as u8
    }));
}


/// Parse `aa:bb:cc:dd:ee:ff` (any case, `-` also accepted) into six bytes.
///
/// Deliberately strict about length and separators: a half-parsed MAC would
/// scope every view to a transmitter that does not exist, and an empty screen
/// is a much worse failure than a rejected setting.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut n = 0usize;
    for part in s.split([':', '-']) {
        if n == 6 || part.len() != 2 {
            return None;
        }
        out[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(out)
}

/// The canonical spelling: lower case, colon separated.
pub fn format_mac(m: &[u8; 6]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(17);
    for (i, b) in m.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::SharedHeader;

    /// Mirrors what the browser does, so the layout contract is tested rather
    /// than asserted in a comment.
    fn decode(buf: &[u8]) -> (serde_json::Value, Vec<f32>, Vec<u8>) {
        let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let header: serde_json::Value = serde_json::from_slice(&buf[4..4 + hlen]).unwrap();
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

    fn shared_with(section: &F32Section) -> SharedFrame {
        let mut h = SharedHeader {
            n_f32: section.len(),
            f32: section.map().clone(),
            ..Default::default()
        };
        h.stream.source = "test".into();
        SharedFrame {
            header_body: crate::wire::shared_body(&h),
            f32_bytes: section.bytes().to_vec(),
            n_f32: section.len(),
            plan: WaterfallPlan::default(),
        }
    }

    #[test]
    fn frame_roundtrips_through_the_documented_layout() {
        let mut section = F32Section::default();
        section.push("amp_db", &[1.0, 2.0, 3.0]);
        section.push("phase", &[-0.5, 0.5]);
        let shared = shared_with(&section);

        let mut client = ClientHeader {
            t: "frame",
            cursor: 42,
            n_u8: 4,
            ..Default::default()
        };
        client.u8.push("wf", 0, 4);

        let mut buf = Vec::new();
        encode(&client, &shared, &[10, 20, 30, 40], &mut buf);

        let (header, f, u) = decode(&buf);
        assert_eq!(header["cursor"], 42);
        assert_eq!(header["f32"]["amp_db"], serde_json::json!([0, 3]));
        assert_eq!(header["f32"]["phase"], serde_json::json!([3, 2]));
        assert_eq!(header["u8"]["wf"], serde_json::json!([0, 4]));
        assert_eq!(f, vec![1.0, 2.0, 3.0, -0.5, 0.5]);
        assert_eq!(u, vec![10, 20, 30, 40]);
    }

    #[test]
    fn header_is_padded_for_every_length() {
        // Sweep header lengths by varying a string field, and check alignment
        // holds for all four residues.
        for pad in 0..8 {
            let mut section = F32Section::default();
            section.push("x", &[1.0]);
            let mut h = SharedHeader {
                n_f32: section.len(),
                f32: section.map().clone(),
                ..Default::default()
            };
            h.stream.source = "x".repeat(pad);
            let shared = SharedFrame {
                header_body: crate::wire::shared_body(&h),
                f32_bytes: section.bytes().to_vec(),
                n_f32: section.len(),
                plan: WaterfallPlan::default(),
            };
            let mut buf = Vec::new();
            encode(
                &ClientHeader {
                    t: "frame",
                    ..Default::default()
                },
                &shared,
                &[],
                &mut buf,
            );
            let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            assert_eq!((4 + hlen) % 4, 0, "pad {pad}");
            let (_, f, _) = decode(&buf);
            assert_eq!(f, vec![1.0]);
        }
    }

    /// The f32 section is memcpy'd rather than encoded element by element;
    /// the byte order must still be exactly what the old encoder wrote.
    #[test]
    fn the_cast_section_matches_element_wise_encoding() {
        let values: Vec<f32> = (0..64).map(|i| i as f32 * -3.25 + 0.5).collect();
        let mut section = F32Section::default();
        section.push("v", &values);
        let expect: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(section.bytes(), &expect[..]);
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

    /// Two clients differing only in frame rate must share one analysis;
    /// anything that changes the numbers must not.
    #[test]
    fn the_view_key_separates_presentation_from_measurement() {
        let base = ViewSettings::default();
        let faster = ViewSettings {
            fps: 5.0,
            paused: true,
            ..base.clone()
        };
        assert_eq!(base.view_key(), faster.view_key());

        for different in [
            ViewSettings {
                chain: 1,
                ..base.clone()
            },
            ViewSettings {
                window: 512,
                ..base.clone()
            },
            ViewSettings {
                db_max: 61.0,
                ..base.clone()
            },
            ViewSettings {
                class: Some("56:ht".into()),
                ..base.clone()
            },
            ViewSettings {
                wf_scope: "all".into(),
                ..base.clone()
            },
            ViewSettings {
                series_tones: vec![3],
                ..base.clone()
            },
        ] {
            assert_ne!(base.view_key(), different.view_key());
        }
    }
}
