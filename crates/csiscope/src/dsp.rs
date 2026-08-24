//! The signal-processing behind every view.
//!
//! Each function here corresponds to a representation the Wi-Fi sensing
//! literature actually uses, and the comments name which one, because "plot the
//! CSI" is under-specified: raw phase is meaningless without sanitisation, raw
//! amplitude is AGC-relative, and a Doppler axis is a lie unless you say what
//! sample rate produced it.
//!
//! | Function | Representation | Grounding |
//! |---|---|---|
//! | [`amp_db`] | subcarrier amplitude, waterfall rows | Gringoli et al. 2019 (Nexmon CSI waterfall); Ma et al. 2020 |
//! | [`bundle`] | overlaid `\|H(f)\|` envelope over a window | Choi et al. 2021/2022 ("CSI bundle" — width is the crowd feature) |
//! | [`unwrap`] + [`detrend`] | sanitised phase | Ma et al. 2020 §Phase Offsets Removal (SpotFi/SignFi linear regression) |
//! | [`cir`] | channel impulse response / power–delay profile | Bocus et al. 2022 (IFFT of the CFR) |
//! | [`doppler`] | Doppler spectrogram column | Li et al. 2022 (STFT); Zheng et al. 2019 (conjugate multiplication) |
//! | [`Validation`] | sanity checks on the extraction itself | Gringoli et al. 2019 §"Crime Scene Investigation" |
//!
//! **Amplitude is AGC-RELATIVE, not AGC-normalised** (see `csid caps`). Nothing
//! in this pipeline divides the gain out: `|H|` carries the receiver's own gain
//! setting alongside the channel, and two frames of an unchanged channel can
//! differ by several dB because the radio re-ranged (Xie et al. 2015 measured
//! 7 dB between two traces of one band). So every amplitude view reports
//! *shape*, the absolute anchor is the per-chain RSSI reported beside it, and
//! the panels say "AGC-relative" — the older wording claimed a correction that
//! was never applied.
//!
//! ## Two forms of every kernel
//!
//! Each kernel exists twice: an owning form that returns a `Vec` (what the
//! tests and the literature read like) and an `_into` form that fills a buffer
//! the caller keeps between frames. The console calls the second kind, because
//! at 20 frames a second the owning forms were spending more time in `malloc`
//! than in arithmetic — allocation was 17% of the process, against 1.7% for
//! the FFTs those allocations were feeding.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use csiq::{CsiRecord, Modulation};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Floor for log magnitude: one least-significant bit of the `i16` I/Q grid.
///
/// Real captures are full of exact zeros — 802.11 nulls the DC and guard
/// subcarriers, and the driver delivers those as `(0, 0)`. With an arbitrary
/// epsilon floor they became −120 dB spikes that dominated every automatic
/// axis and colour range in the console. One LSB is both finite and *true*:
/// the format cannot represent anything smaller, so 0 dB means "at or below
/// the quantisation floor", which is exactly what a null subcarrier is.
const MAG_FLOOR: f32 = 1.0;

/// The same floor expressed as a power, since every amplitude path now works
/// in `|H|²` and takes the square root implicitly inside the logarithm.
const POWER_FLOOR: f32 = MAG_FLOOR * MAG_FLOOR;

/// At or below this many dB, a tone is a null rather than a weak measurement.
///
/// [`MAG_FLOOR`] is one LSB, which `db_from_power` maps to exactly 0 dB, so
/// anything within a decibel of that is the quantisation floor and not a
/// reading. The browser applies the same threshold when it fits an axis.
pub const NULL_TONE_DB: f32 = 1.0;

/// Taps this far below the strongest one are excluded from the RMS delay
/// spread — see [`cir`].
const RMS_THRESHOLD_DB: f32 = -20.0;

// -- logarithm ----------------------------------------------------------------

/// `log10(2)`, the bridge from a binary exponent to decibels. Taken from
/// `std` rather than written out: a hand-typed literal is a digit away from a
/// silent scale error in every dB the waterfall renders.
const LOG10_2: f32 = std::f32::consts::LOG10_2;

/// Minimax coefficients for `log2(1+f)/f` on `f ∈ [√2/2 − 1, √2 − 1]`,
/// ascending. Fitted at degree 6, which puts the approximation error at
/// 1.7e-6 dB — below the resolution of the `f32` it is stored in, and five
/// orders of magnitude below the 256-level quantisation the waterfall applies
/// afterwards.
const LOG2_COEFFS: [f32; 7] = [
    1.442_696_5,
    -0.721_364,
    0.480_629_15,
    -0.359_350_65,
    0.295_633_6,
    -0.269_506_28,
    0.172_128_74,
];

/// Base-2 logarithm by exponent extraction and a polynomial on the mantissa.
///
/// The point is not that this is faster than `libm` scalar-to-scalar — it is,
/// but only by a factor of a few. The point is that it is *branch-free and
/// inlinable*, so a loop over a slice of subcarriers vectorises, which a call
/// to `log10f` never can. Amplitude-in-dB is the most-executed kernel in the
/// crate (every waterfall row, every bundle column, every chain spectrum), and
/// it was 8% of the process before this.
///
/// **Precondition:** `x` is finite and strictly positive. Every call site
/// reaches this through [`db_from_power`], which floors at [`POWER_FLOOR`], so
/// the subnormal and non-positive cases are unreachable rather than handled.
#[inline(always)]
fn log2_fast(x: f32) -> f32 {
    let bits = x.to_bits();
    // IEEE-754 binary32: the biased exponent is the integer part of log2.
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    // ...and the mantissa, re-hosted at exponent 0, is the fractional part's
    // argument, in [1, 2).
    let mantissa = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);

    // Re-centre on [√2/2, √2) so the polynomial argument straddles zero and a
    // degree-6 fit suffices. Written as a select rather than a branch: a
    // branch here would stop the enclosing loop from vectorising, which is the
    // entire reason this function exists.
    let high = mantissa > std::f32::consts::SQRT_2;
    let mantissa = if high { mantissa * 0.5 } else { mantissa };
    let exponent = exponent + high as i32;

    let f = mantissa - 1.0;
    // Horner, with plain multiply-add rather than `f32::mul_add`: on aarch64
    // the two lower identically, while `mul_add` on a target without FMA
    // lowers to a `fmaf` *call* and would defeat vectorisation.
    let mut p = LOG2_COEFFS[6];
    p = p * f + LOG2_COEFFS[5];
    p = p * f + LOG2_COEFFS[4];
    p = p * f + LOG2_COEFFS[3];
    p = p * f + LOG2_COEFFS[2];
    p = p * f + LOG2_COEFFS[1];
    p = p * f + LOG2_COEFFS[0];

    exponent as f32 + f * p
}

/// Power (`|H|²`) to decibels: `10·log10(p)`.
///
/// Amplitude views are `20·log10(|H|)`, which is the same number — and this
/// way the square root inside `|H|` never happens. `Complex::norm` was 8.6% of
/// the process purely to compute a value the logarithm was about to undo.
#[inline(always)]
pub fn db_from_power(power: f32) -> f32 {
    10.0 * LOG10_2 * log2_fast(power.max(POWER_FLOOR))
}

// -- geometry -----------------------------------------------------------------

/// The per-record shape: tones × chains.
///
/// `nchain = nrx * ntx` is the number of complex CSI vectors a record carries.
/// On the reference node the observed maximum is 4 (2×2 MIMO).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub ntone: usize,
    pub nrx: usize,
    pub ntx: usize,
}

impl Geometry {
    /// Read the geometry a record declares.
    pub fn of(rec: &CsiRecord) -> Self {
        Geometry {
            ntone: rec.ntone as usize,
            nrx: rec.nrx as usize,
            ntx: rec.ntx as usize,
        }
    }

    /// Number of chains (`nrx * ntx`).
    pub fn nchain(&self) -> usize {
        self.nrx * self.ntx
    }

    /// Does the I/Q payload match what the header declares?
    ///
    /// A mismatch means the record is unusable, and it is the single most
    /// common symptom of a driver-ABI drift — worth surfacing, not silently
    /// dropping.
    pub fn matches(&self, rec: &CsiRecord) -> bool {
        rec.iq.len() == 2 * self.ntone * self.nchain()
    }

    /// Human label for a chain index, e.g. `rx1·tx0`.
    pub fn chain_label(&self, chain: usize) -> String {
        if self.nchain() <= 1 {
            return "rx0·tx0".into();
        }
        // Layout is row-major over [tone][chain]; the chain axis itself is the
        // driver's rx/tx interleave, taken here as rx-major.
        let rx = chain / self.ntx.max(1);
        let tx = chain % self.ntx.max(1);
        format!("rx{rx}·tx{tx}")
    }
}

/// Does this record carry a channel estimate at all?
///
/// ## The one policy, in one place
///
/// A record can arrive with an intact header, a payload of exactly the declared
/// length, a plausible RSSI and a PHY label — and every coefficient in it equal
/// to `(0, 0)`. [`Geometry::matches`] passes it, because length is not content.
/// It is not a measurement of a very weak channel; it is the absence of a
/// measurement, and the two must not be averaged together.
///
/// Measured 2026-08-23 across monad01/02/09/10 simultaneously: 15.2–16.0% of
/// records in the analysis window were in this state, interleaved as isolated
/// singletons (mean run length 1.01) among good ones, each carrying the node's
/// *strongest* RSSI. The console drew all of them.
///
/// What that cost, before this predicate existed:
///
/// - the p05 of the amplitude bundle landed on a null for **every tone, every
///   frame, every node**, so the shaded envelope was drawn from the bottom of
///   the plot and its width was pinned at ~44 dB — the distance from a null to
///   the p95, not a property of the channel;
/// - the subcarrier time series plunged to the axis floor every fourth record,
///   which reads as violent motion in a still room;
/// - the Doppler series took a `0 + 0i` sample one time in six, and after mean
///   removal each one is a broadband impulse across the whole spectrum;
/// - the waterfall drew them as rows at the colour floor.
///
/// Five views, five independent decisions, all of them wrong in the same way.
/// This function is the decision; [`crate::analyze`] applies it once, where the
/// window is built, and reports how many it removed.
///
/// **This is not the same question as [`NULL_TONE_DB`]**, which asks whether one
/// *subcarrier* is at the quantisation floor. 802.11 genuinely nulls tones, and
/// a per-tone null in an otherwise good record is a fact about the tone plan.
/// A record in which *every* tone is null is a fact about the driver.
#[inline]
pub fn is_measurement(rec: &CsiRecord) -> bool {
    !rec.iq.is_empty() && rec.iq.iter().any(|&v| v != 0)
}

/// The `i16` slice holding one chain's interleaved coefficients, or `None` when
/// the record's payload does not match its declared dimensions.
///
/// Storage is chain-major (`nrx*ntx` contiguous blocks of `ntone`), and each
/// coefficient is **imaginary-first**. Both differ from the obvious reading —
/// see `docs/CSIQ-format-v1.md`, "The CSI matrix".
#[inline]
pub fn chain_slice(rec: &CsiRecord, chain: usize) -> Option<&[i16]> {
    let g = Geometry::of(rec);
    let nc = g.nchain();
    if nc == 0 || chain >= nc || !g.matches(rec) {
        return None;
    }
    let start = 2 * chain * g.ntone;
    Some(&rec.iq[start..start + 2 * g.ntone])
}

/// Extract one chain's channel frequency response, tone-major.
///
/// Returns an empty vector if the record's payload does not match its declared
/// dimensions.
pub fn chain(rec: &CsiRecord, chain: usize) -> Vec<Complex32> {
    let mut out = Vec::new();
    chain_into(rec, chain, &mut out);
    out
}

/// [`chain`], into a buffer the caller keeps.
pub fn chain_into(rec: &CsiRecord, chain: usize, out: &mut Vec<Complex32>) {
    out.clear();
    let Some(iq) = chain_slice(rec, chain) else {
        return;
    };
    out.reserve(iq.len() / 2);
    out.extend(
        iq.chunks_exact(2)
            .map(|c| Complex32::new(c[1] as f32, c[0] as f32)),
    );
}

/// One chain's `|H(f)|²`, straight from the `i16` payload.
///
/// This is the shortcut that matters: every amplitude view wants dB, dB wants
/// power, and power does not need the record ever to become `Complex32`. The
/// waterfall, the bundle, the per-chain spectra and the validation panel all
/// come through here, which removed roughly 1,500 `Vec<Complex32>`
/// allocations per frame.
pub fn chain_power_into(rec: &CsiRecord, chain: usize, out: &mut Vec<f32>) {
    out.clear();
    let Some(iq) = chain_slice(rec, chain) else {
        return;
    };
    out.resize(iq.len() / 2, 0.0);
    for (o, c) in out.iter_mut().zip(iq.chunks_exact(2)) {
        // `i16 -> f32` is exact; the product is not, but its relative error is
        // 6e-8, which is 2.6e-7 dB.
        let im = c[0] as f32;
        let re = c[1] as f32;
        *o = re * re + im * im;
    }
}

/// One chain's amplitude in dB, straight from the `i16` payload.
pub fn chain_amp_db_into(rec: &CsiRecord, chain: usize, out: &mut Vec<f32>) {
    chain_power_into(rec, chain, out);
    for v in out.iter_mut() {
        *v = db_from_power(*v);
    }
}

/// [`chain_amp_db_into`], writing into a fixed-width slice.
///
/// A record whose chain does not fill `out` exactly leaves it `NaN` — the
/// console draws a gap rather than a stretched or truncated spectrum, which is
/// the honest rendering of "this record does not have the geometry this row
/// promised". Writing straight into the destination is also what lets the
/// per-chain spectra and the bundle's columns be filled in parallel: each
/// chunk is an independent output with no shared buffer behind it.
pub fn chain_amp_db_into_slice(rec: &CsiRecord, chain: usize, out: &mut [f32]) {
    let Some(iq) = chain_slice(rec, chain).filter(|s| s.len() == 2 * out.len()) else {
        out.fill(f32::NAN);
        return;
    };
    for (o, c) in out.iter_mut().zip(iq.chunks_exact(2)) {
        let im = c[0] as f32;
        let re = c[1] as f32;
        *o = db_from_power(re * re + im * im);
    }
}

// -- frequency axis -----------------------------------------------------------

/// Subcarrier spacing in Hz.
///
/// HE (802.11ax) quadruples the FFT size for the same bandwidth, so its tones
/// are 78.125 kHz apart against 312.5 kHz for legacy/HT/VHT. The PHY label is
/// authoritative when present; `ntone` alone is ambiguous (242 tones is HE20
/// *or* VHT80), so the fallback is only used when `rate_n_flags` was absent.
pub fn spacing_hz(rec: &CsiRecord) -> f64 {
    match rec.phy.map(|p| p.modulation) {
        Some(Modulation::He) | Some(Modulation::Eht) => 78_125.0,
        Some(_) => 312_500.0,
        // No PHY label: assume the dense grid for tone counts only HE produces.
        None if rec.ntone >= 242 => 78_125.0,
        None => 312_500.0,
    }
}

/// Occupied bandwidth of the delivered tone grid, in MHz.
pub fn occupied_bw_mhz(rec: &CsiRecord) -> f64 {
    rec.ntone as f64 * spacing_hz(rec) / 1e6
}

// -- cached transforms --------------------------------------------------------

/// FFT plans and window functions, kept across frames.
///
/// `FftPlanner::plan_fft` is not a lookup. It factors the length, designs a
/// radix chain and precomputes twiddle factors, and the console was calling
/// `FftPlanner::new()` inside both [`cir`] and [`doppler`] — once per frame
/// each. Planning cost about 7.5% of the process against 1.7% for running the
/// transforms it produced: four times more effort spent deciding how to
/// transform than transforming.
///
/// The Hann windows are cached for the same reason at smaller scale: they are
/// a fixed function of length, recomputed with a `cos` per tone per frame.
pub struct Transforms {
    planner: FftPlanner<f32>,
    plans: HashMap<(usize, bool), Arc<dyn Fft<f32>>>,
    hann: HashMap<usize, Arc<[f32]>>,
    /// Scratch for `process_with_scratch`, so rustfft does not allocate either.
    scratch: Vec<Complex32>,
}

impl Default for Transforms {
    fn default() -> Self {
        Self::new()
    }
}

impl Transforms {
    pub fn new() -> Self {
        Transforms {
            planner: FftPlanner::new(),
            plans: HashMap::new(),
            hann: HashMap::new(),
            scratch: Vec::new(),
        }
    }

    fn plan(&mut self, len: usize, inverse: bool) -> Arc<dyn Fft<f32>> {
        let planner = &mut self.planner;
        self.plans
            .entry((len, inverse))
            .or_insert_with(|| {
                if inverse {
                    planner.plan_fft_inverse(len)
                } else {
                    planner.plan_fft_forward(len)
                }
            })
            .clone()
    }

    /// Transform `buf` in place, reusing the persistent scratch buffer.
    fn run(&mut self, buf: &mut [Complex32], inverse: bool) {
        let fft = self.plan(buf.len(), inverse);
        let need = fft.get_inplace_scratch_len();
        if self.scratch.len() < need {
            self.scratch.resize(need, Complex32::new(0.0, 0.0));
        }
        fft.process_with_scratch(buf, &mut self.scratch[..need]);
    }

    /// A Hann window of `len` points, `0.5 − 0.5·cos(2πi/(len−1))`.
    fn hann(&mut self, len: usize) -> Arc<[f32]> {
        self.hann
            .entry(len)
            .or_insert_with(|| {
                let denom = (len.max(2) - 1) as f32;
                (0..len)
                    .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / denom).cos())
                    .collect()
            })
            .clone()
    }
}

thread_local! {
    /// Plans for the owning-form entry points ([`cir`], [`doppler`]), which the
    /// tests and benches call without threading a [`Transforms`] through. The
    /// console's own path carries an explicit one per analysis.
    static LOCAL_TRANSFORMS: RefCell<Transforms> = RefCell::new(Transforms::new());
}

// -- amplitude ----------------------------------------------------------------

/// Magnitude in dB — AGC-relative, not AGC-normalised.
///
/// Nothing here divides the receiver gain out, so read shape across subcarriers
/// rather than level. See the module note.
pub fn amp_db(h: &[Complex32]) -> Vec<f32> {
    let mut out = Vec::new();
    amp_db_into(h, &mut out);
    out
}

/// [`amp_db`], into a buffer the caller keeps.
pub fn amp_db_into(h: &[Complex32], out: &mut Vec<f32>) {
    out.clear();
    out.resize(h.len(), 0.0);
    for (o, c) in out.iter_mut().zip(h) {
        *o = db_from_power(c.re * c.re + c.im * c.im);
    }
}

/// The 5th/50th/95th percentile of `|H(f)|` in dB across a window of records —
/// the "CSI bundle" of Choi et al.
///
/// The *width* of the bundle (p95 − p05, averaged over subcarriers) is the
/// discriminative feature in that line of work: a static channel gives a tight
/// bundle, occupancy and motion widen it. It is returned as
/// [`Bundle::width_db`] so the console can show it as a single number.
#[derive(Debug, Clone, Default)]
pub struct Bundle {
    pub p05: Vec<f32>,
    pub p50: Vec<f32>,
    pub p95: Vec<f32>,
    /// Mean of `p95 − p05` over subcarriers, in dB.
    pub width_db: f32,
    /// How many records went into the estimate.
    pub n: usize,
}

/// `columns` is one `amp_db` vector per record, all the same length.
///
/// Kept for callers that already hold a `Vec` per column; the console uses
/// [`bundle_flat`], which does not build 128 vectors to throw them away.
pub fn bundle(columns: &[Vec<f32>]) -> Bundle {
    let Some(ntone) = columns.first().map(|c| c.len()) else {
        return Bundle::default();
    };
    let usable: Vec<&Vec<f32>> = columns.iter().filter(|c| c.len() == ntone).collect();
    if usable.is_empty() || ntone == 0 {
        return Bundle::default();
    }
    let mut flat = Vec::with_capacity(usable.len() * ntone);
    for c in &usable {
        flat.extend_from_slice(c);
    }
    let mut out = Bundle::default();
    bundle_flat(&flat, ntone, &mut out, &mut Vec::new());
    out
}

/// [`bundle`] over a contiguous `n × ntone` row-major buffer.
///
/// `scratch` is one column's worth of values and is reused across subcarriers
/// and across frames.
pub fn bundle_flat(flat: &[f32], ntone: usize, out: &mut Bundle, scratch: &mut Vec<f32>) {
    out.p05.clear();
    out.p50.clear();
    out.p95.clear();
    out.width_db = 0.0;
    out.n = 0;
    if ntone == 0 || flat.len() < ntone {
        return;
    }
    let n = flat.len() / ntone;
    out.n = n;
    out.p05.resize(ntone, 0.0);
    out.p50.resize(ntone, 0.0);
    out.p95.resize(ntone, 0.0);
    scratch.clear();
    scratch.resize(n, 0.0);

    let (a, b, c) = (idx(n, 0.05), idx(n, 0.50), idx(n, 0.95));
    let mut width_sum = 0.0f64;

    for t in 0..ntone {
        for (i, s) in scratch.iter_mut().enumerate() {
            *s = flat[i * ntone + t];
        }

        // Selection, not a full sort: three O(n) passes beat n·log n at 996
        // tones × 20 frames a second on a Pi 5.
        //
        // The three quantiles are ordered, so they are not three independent
        // selections. Selecting the median first partitions the column about
        // it; p05 can then only be in the left part and p95 only in the right,
        // and each of those scans half the data. Identical results, and the
        // measured cost of the whole bundle drops by rather more than the
        // factor of two that implies, because the two halves stay in cache.
        let (lo, mid, hi) = scratch.select_nth_unstable_by(b, f32::total_cmp);
        let p50 = *mid;

        let p05 = if a < b && !lo.is_empty() {
            let k = a.min(lo.len() - 1);
            *lo.select_nth_unstable_by(k, f32::total_cmp).1
        } else {
            p50
        };
        let p95 = if c > b && !hi.is_empty() {
            let k = (c - b - 1).min(hi.len() - 1);
            *hi.select_nth_unstable_by(k, f32::total_cmp).1
        } else {
            p50
        };

        out.p05[t] = p05;
        out.p50[t] = p50;
        out.p95[t] = p95;
        width_sum += (p95 - p05) as f64;
    }

    out.width_db = (width_sum / ntone as f64) as f32;
}

fn idx(n: usize, q: f64) -> usize {
    (((n - 1) as f64) * q).round() as usize
}

// -- phase --------------------------------------------------------------------

/// Raw wrapped phase in radians, `(-π, π]`.
///
/// Shown only to make the point that it is unusable on its own: measured phase
/// carries CFO, SFO/STO and packet-detection-delay terms that dominate the
/// channel term (Ma et al. 2020 §Phase Offsets Removal).
pub fn phase(h: &[Complex32]) -> Vec<f32> {
    let mut out = Vec::new();
    phase_into(h, &mut out);
    out
}

/// [`phase`], into a buffer the caller keeps.
pub fn phase_into(h: &[Complex32], out: &mut Vec<f32>) {
    out.clear();
    out.resize(h.len(), 0.0);
    for (o, c) in out.iter_mut().zip(h) {
        *o = c.im.atan2(c.re);
    }
}

/// Unwrap a phase sequence along the subcarrier axis.
pub fn unwrap(phase: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    unwrap_into(phase, &mut out);
    out
}

/// [`unwrap`], into a buffer the caller keeps.
pub fn unwrap_into(phase: &[f32], out: &mut Vec<f32>) {
    out.clear();
    out.reserve(phase.len());
    let mut offset = 0.0f32;
    let mut prev = 0.0f32;
    for (i, &p) in phase.iter().enumerate() {
        if i > 0 {
            let d = p - prev;
            if d > std::f32::consts::PI {
                offset -= 2.0 * std::f32::consts::PI;
            } else if d < -std::f32::consts::PI {
                offset += 2.0 * std::f32::consts::PI;
            }
        }
        prev = p;
        out.push(p + offset);
    }
}

/// A linear fit removed from unwrapped phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct Detrend {
    /// Slope in radians per subcarrier.
    pub slope: f32,
    /// Intercept in radians.
    pub intercept: f32,
    /// Residual delay implied by the slope, in nanoseconds.
    ///
    /// This is **not** a clean time-of-flight: it is ToF *plus* the sampling
    /// time offset the fit was there to remove. It is a stable, comparable
    /// number for spotting a link changing, not a ranging measurement.
    pub tau_ns: f32,
}

/// Remove the linear-in-subcarrier component of unwrapped phase.
///
/// This is the standard sanitisation: SFO/STO enter the measured phase as a
/// term linear in subcarrier index and CFO as a constant, so a least-squares
/// line fitted across the band and subtracted removes both and leaves the
/// channel's phase shape (SpotFi's linear regression, generalised by SignFi;
/// summarised in Ma et al. 2020 and Díaz et al. 2023).
///
/// The cost is real and worth stating in the UI: any genuinely linear component
/// of the channel goes with it.
pub fn detrend(unwrapped: &[f32], spacing_hz: f64) -> (Vec<f32>, Detrend) {
    let mut out = Vec::new();
    let d = detrend_into(unwrapped, spacing_hz, &mut out);
    (out, d)
}

/// [`detrend`], into a buffer the caller keeps.
pub fn detrend_into(unwrapped: &[f32], spacing_hz: f64, out: &mut Vec<f32>) -> Detrend {
    out.clear();
    let n = unwrapped.len();
    if n < 2 {
        out.extend_from_slice(unwrapped);
        return Detrend::default();
    }
    let nf = n as f64;
    let mean_x = (nf - 1.0) / 2.0;
    let mean_y = unwrapped.iter().map(|&v| v as f64).sum::<f64>() / nf;

    let mut sxy = 0.0f64;
    let mut sxx = 0.0f64;
    for (i, &y) in unwrapped.iter().enumerate() {
        let dx = i as f64 - mean_x;
        sxy += dx * (y as f64 - mean_y);
        sxx += dx * dx;
    }
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let intercept = mean_y - slope * mean_x;

    out.resize(n, 0.0);
    for (i, (o, &y)) in out.iter_mut().zip(unwrapped).enumerate() {
        *o = y - (slope * i as f64 + intercept) as f32;
    }

    // φ(k) = −2π·τ·k·Δf  ⇒  τ = −slope / (2π·Δf).
    let tau_ns = (-slope / (2.0 * std::f64::consts::PI * spacing_hz) * 1e9) as f32;

    Detrend {
        slope: slope as f32,
        intercept: intercept as f32,
        tau_ns,
    }
}

// -- channel impulse response -------------------------------------------------

/// Power–delay profile from the channel frequency response.
///
/// ## The axis is relative to the strongest tap, and it has to be
///
/// A commodity NIC starts its FFT at a packet boundary it detected, and the
/// detection delay differs from packet to packet. That appears in the CFR as a
/// linear phase term and in the transform as a *translation* of the whole
/// profile. Measured 2026-08-23 over fifteen seconds on a still room,
/// [`Cir::peak_bin`] swept the entire 0–127 range on two nodes: the profile was
/// sliding across the panel while nothing moved.
///
/// Absolute delay is not recoverable from this instrument — the linear term the
/// phase fit removes is partly time-of-flight, and nothing calibrates it (see
/// `crate::tones` and Ma et al. 2020 §Phase Offsets Removal). So the profile is
/// returned **peak-aligned**: the strongest tap sits at [`Cir::peak_index`] and
/// the axis runs from [`Cir::axis_start_ns`], which is negative. Reading a
/// negative delay is correct and expected; reading an absolute one is not.
///
/// Alignment also fixes a second, quieter fault. The window is truncated to
/// `taps`, and a peak that wandered towards the truncation boundary took half
/// its own profile out of the returned slice — which is why `rms_delay_ns`
/// moved by 3.6× on a single node while the room did nothing.
#[derive(Debug, Clone, Default)]
pub struct Cir {
    /// Magnitude in dB, normalised so the strongest tap is 0 dB.
    ///
    /// Tap `i` sits at delay `axis_start_ns + i * bin_ns`.
    pub mag_db: Vec<f32>,
    /// Spacing between returned taps, in nanoseconds.
    ///
    /// **This is not the resolution.** Zero-padding interpolates between the
    /// taps the bandwidth can actually distinguish; it does not create new
    /// ones. See [`Cir::resolution_ns`].
    pub bin_ns: f32,
    /// True delay resolution, `1/B`, in nanoseconds.
    ///
    /// `B` is the occupied bandwidth — the delivered tone count times the
    /// subcarrier spacing. Two paths closer together than this are one tap,
    /// whatever the plot's smoothness suggests (Xie et al. 2015: at 20 MHz the
    /// path-length ambiguity is 15 m). On the fleet's 52-tone legacy grid this
    /// is 61.5 ns against a `bin_ns` of 1.6, so a 198 ns window holds barely
    /// three resolvable taps.
    pub resolution_ns: f32,
    /// Delay of the first returned tap, in nanoseconds. Negative by
    /// construction — see the type's own documentation.
    pub axis_start_ns: f32,
    /// Index *within `mag_db`* of the strongest tap. The alignment target.
    pub peak_index: usize,
    /// Index within the un-aligned transform of the strongest tap.
    ///
    /// Kept because it is a genuine diagnostic — it is the packet-detection
    /// delay, in bins — and because a value pinned at 0 or at `nfft-1` across
    /// many frames means the rotation is wrong rather than the channel odd.
    pub peak_bin: usize,
    /// RMS delay spread over the returned taps, in nanoseconds — a one-number
    /// summary of how multipath-rich the link is.
    ///
    /// Meaningless below [`Cir::resolution_ns`]: a spread smaller than the
    /// smallest interval the bandwidth can distinguish is describing the
    /// window function, not the channel. [`Cir::spread_is_resolvable`] is the
    /// test, and the panel prints the verdict rather than the bare number.
    pub rms_delay_ns: f32,
}

impl Cir {
    /// Is the reported spread larger than the bandwidth can resolve?
    ///
    /// `false` means the profile is one resolution cell wide and
    /// [`Cir::rms_delay_ns`] is a property of the Hann window.
    pub fn spread_is_resolvable(&self) -> bool {
        self.resolution_ns > 0.0 && self.rms_delay_ns >= self.resolution_ns
    }
}

/// Inverse-FFT the CFR into a power–delay profile (Bocus et al. 2022).
///
/// `nfft` zero-pads for a smoother profile; `taps` bounds what is returned
/// (the full delay span is `1/Δf`, which for HE's 78.125 kHz grid is 12.8 µs —
/// several kilometres of path, far past anything indoors).
///
/// The CFR is **Hann-windowed** before the transform. A rectangular band edge
/// produces sinc sidelobes tens of nanoseconds either side of every real tap,
/// and those artifacts are indistinguishable from multipath by eye — the
/// classic way to read reflections that are not there. Windowing costs about a
/// factor of two in main-lobe width and buys roughly 30 dB of sidelobe
/// suppression, which is the standard trade in channel sounding.
///
/// **Assumption worth knowing:** the delivered tone vector is treated as
/// contiguous and in ascending frequency order, with its centre at
/// `ntone/2`. That matches the iax matrix layout; a driver revision that
/// reordered or interleaved tones would smear this plot while leaving every
/// other view intact — which is itself a useful diagnostic.
pub fn cir(h: &[Complex32], spacing_hz: f64, nfft: usize, taps: usize) -> Cir {
    let mut out = Cir::default();
    LOCAL_TRANSFORMS.with(|t| {
        cir_into(
            &mut t.borrow_mut(),
            h,
            spacing_hz,
            nfft,
            taps,
            &mut Vec::new(),
            &mut out,
        )
    });
    out
}

/// [`cir`], with cached plans and caller-owned buffers.
#[allow(clippy::too_many_arguments)]
pub fn cir_into(
    tf: &mut Transforms,
    h: &[Complex32],
    spacing_hz: f64,
    nfft: usize,
    taps: usize,
    buf: &mut Vec<Complex32>,
    out: &mut Cir,
) {
    out.mag_db.clear();
    out.bin_ns = 0.0;
    out.resolution_ns = 0.0;
    out.axis_start_ns = 0.0;
    out.peak_index = 0;
    out.peak_bin = 0;
    out.rms_delay_ns = 0.0;

    let n = h.len();
    if n < 4 || nfft < n || spacing_hz <= 0.0 {
        return;
    }

    let window = tf.hann(n);
    buf.clear();
    buf.resize(nfft, Complex32::new(0.0, 0.0));
    // Rotate so the band centre lands on FFT bin 0: the tone grid spans
    // [−BW/2, +BW/2] but the FFT expects DC first.
    let half = n / 2;
    for (i, (&c, &w)) in h.iter().zip(window.iter()).enumerate() {
        let k = (i + nfft - half) % nfft;
        buf[k] = c * w;
    }

    tf.run(buf, true);

    // Peak over the WHOLE transform, not over the first `taps` of it.
    //
    // The old form searched only the slice it was about to return, so a
    // profile whose true peak sat outside that slice reported the strongest
    // thing inside it — a different tap, normalised to the wrong power. Since
    // the peak is what the axis is now anchored to, it has to be the real one.
    let mut peak_bin = 0usize;
    let mut peak_power = 0.0f32;
    for (i, c) in buf.iter().enumerate() {
        let p = c.re * c.re + c.im * c.im;
        if p > peak_power {
            peak_power = p;
            peak_bin = i;
        }
    }
    let peak_power = peak_power.max(POWER_FLOOR);

    // Peak-align: return a window centred on the strongest tap, read circularly
    // out of the transform. `taps` is forced even so the peak lands exactly at
    // `taps/2` and the axis is symmetric.
    let taps = taps.min(nfft).max(4) & !1;
    let lead = taps / 2;
    out.mag_db.resize(taps, 0.0);
    for (i, slot) in out.mag_db.iter_mut().enumerate() {
        let k = (peak_bin + nfft + i - lead) % nfft;
        let c = buf[k];
        *slot = c.re * c.re + c.im * c.im;
    }

    let bin_s = 1.0 / (nfft as f64 * spacing_hz);
    out.bin_ns = (bin_s * 1e9) as f32;
    // Resolution is set by the OCCUPIED BANDWIDTH, which is what the delivered
    // tones span — never by `nfft`, which only decides how finely the same
    // information is interpolated.
    out.resolution_ns = (1e9 / (n as f64 * spacing_hz)) as f32;
    out.axis_start_ns = -(lead as f64 * bin_s * 1e9) as f32;
    out.peak_index = lead;
    out.peak_bin = peak_bin;

    // RMS delay spread: the power-weighted standard deviation of tap delay,
    // measured from the power-weighted mean (not from the peak).
    //
    // Taps below `RMS_THRESHOLD_DB` are excluded. Without a threshold the
    // noise floor — which is spread uniformly across every returned tap —
    // dominates the second moment, and the reported spread becomes a function
    // of how many taps were requested rather than of the channel. Thresholding
    // at −20 dB is the standard convention for this statistic.
    //
    // The cutoff is a *power* ratio here, so the dB threshold halves.
    let cutoff = peak_power * 10f32.powf(RMS_THRESHOLD_DB / 10.0);
    let (mut total, mut m1) = (0.0f64, 0.0f64);
    for (i, &p) in out.mag_db.iter().enumerate() {
        if p >= cutoff {
            total += p as f64;
            m1 += i as f64 * p as f64;
        }
    }
    out.rms_delay_ns = if total > 0.0 {
        // Two passes about the measured mean rather than one pass on raw
        // moments: `E[i²] − E[i]²` cancels catastrophically when the profile is
        // a narrow peak at a large tap index, which is precisely the common
        // case indoors.
        let mean = m1 / total;
        let mut var = 0.0f64;
        for (i, &p) in out.mag_db.iter().enumerate() {
            if p >= cutoff {
                var += (i as f64 - mean).powi(2) * p as f64;
            }
        }
        ((var / total).sqrt() * bin_s * 1e9) as f32
    } else {
        0.0
    };

    // Normalise to the strongest tap and convert in place.
    let peak_db = db_from_power(peak_power);
    for v in out.mag_db.iter_mut() {
        *v = db_from_power(*v) - peak_db;
    }
}

// -- Doppler ------------------------------------------------------------------

/// One column of a Doppler spectrogram, plus the axis it is honest about.
///
/// ## Why the axis is pinned rather than measured
///
/// A spectrogram is a sequence of columns drawn on **one** frequency axis. The
/// console used to derive each column's `fs` from the mean packet rate of that
/// column's own window, so consecutive columns were FFTs over different sample
/// rates, stacked into one image and labelled with the newest column's range.
///
/// Measured 2026-08-23 over fifteen seconds: `max_hz` moved by 2.3–2.4× on
/// every node of the fleet, and across a longer sample from ±18 Hz to ±132 Hz.
/// Columns whose bins were 0.07 Hz wide sat beside columns whose bins were
/// 1.0 Hz wide. Nothing in the image could be compared with anything else in it.
///
/// So the caller pins a rate and every column is resampled onto **that** grid.
/// The rate is chosen from a coarse 1-2-5 ladder at or above the delivered
/// rate ([`snap_rate_hz`]), which keeps it stable across frames and never
/// narrower than the achieved Nyquist. What the window actually delivered is
/// reported separately as [`Doppler::fs_window_hz`], and the share of grid
/// slots that had no sample near them as [`Doppler::gap_frac`] — the two
/// numbers that say how much of the column is information and how much is fill
/// (MUSE-Fi, Hu et al. 2023: resample to a declared rate, tag the empty
/// instants, then transform — never transform the raw arrivals).
#[derive(Debug, Clone, Default)]
pub struct Doppler {
    /// Power in dB relative to the column peak, FFT-shifted so the centre bin
    /// is 0 Hz and the axis runs `[−fs/2, +fs/2)`.
    pub power_db: Vec<f32>,
    /// The **pinned** sample rate the frequency axis is built from, in Hz.
    /// Stable across columns; see the type's own documentation.
    pub fs_hz: f32,
    /// What this window actually delivered, in Hz. Diagnostic only — it never
    /// touches the axis.
    pub fs_window_hz: f32,
    /// Where [`Doppler::fs_hz`] came from: `"tracked"` (snapped from the
    /// delivered rate) or `"none"` (nothing to compute).
    pub fs_source: &'static str,
    /// Maximum unambiguous Doppler shift (`fs/2`), in Hz.
    pub max_hz: f32,
    /// Corresponding maximum unambiguous radial speed, in m/s
    /// (`v = λ·f_D/2`, so `v_max = λ·fs/4`).
    pub max_speed_ms: f32,
    /// Share of resample slots with no sample within half a grid step.
    ///
    /// These are filled with the series mean, so they contribute nothing to the
    /// spectrum — but they contribute nothing to the *evidence* either, and a
    /// column that is mostly fill is a picture of the fill. Reported so the
    /// panel can say so.
    pub gap_frac: f32,
    /// Seconds of history the column covers.
    pub span_s: f32,
    /// Coefficient of variation of the packet inter-arrival time. The Doppler
    /// axis assumes uniform sampling; ambient traffic is not uniform, so this
    /// is how much to distrust the axis. Below ~0.3 the resampling is benign.
    pub arrival_cv: f32,
    /// Whether a second chain was available for the conjugate-multiplication
    /// step. Without it the series still carries CFO and the spectrum smears.
    pub conjugate_pair: bool,
}

/// Round a rate up onto a 1-2-5 ladder, so a wobbling measurement maps to a
/// stable axis.
///
/// At or above, never below: the axis must always cover the delivered Nyquist,
/// or the resampling would decimate and alias. A rate of 21 Hz and a rate of
/// 49 Hz both land on 50, so the spectrogram's axis holds still while the
/// channel breathes, and only a genuine change of regime moves it.
pub fn snap_rate_hz(fs: f64) -> f64 {
    if !(fs > 0.0) {
        return 0.0;
    }
    let decade = 10f64.powf(fs.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        let step = m * decade;
        // A hair of tolerance so a rate that is already exactly on the ladder
        // is not pushed to the next rung by floating-point noise.
        if fs <= step * (1.0 + 1e-9) {
            return step;
        }
    }
    decade * 10.0
}

/// A time series ready for the STFT, with the timing metadata behind it.
#[derive(Default)]
pub struct Series {
    /// One complex scalar per record.
    pub values: Vec<Complex32>,
    /// Monotonic 320 MHz tick count per record.
    pub ticks: Vec<u64>,
    pub conjugate_pair: bool,
}

/// Reduce each record to one complex scalar for Doppler analysis.
///
/// When two chains are available this takes the **conjugate product**
/// `H_a · conj(H_b)` averaged over subcarriers. The two chains share a radio
/// and therefore share the CFO/SFO terms, so the product cancels them while
/// preserving the relative dynamics — the trick behind Widar3's DFS profile
/// (Zheng et al. 2019) and FarSense's CSI ratio. With a single chain there is
/// nothing to cancel against, and `conjugate_pair` records that the resulting
/// spectrum is CFO-contaminated.
pub fn doppler_series(
    samples: &[Arc<CsiRecord>],
    ticks: &[u64],
    chain_a: usize,
    chain_b: Option<usize>,
) -> Series {
    let mut out = Series::default();
    doppler_series_into(samples, ticks, chain_a, chain_b, &mut out);
    out
}

/// [`doppler_series`], into a buffer the caller keeps.
///
/// The reduction reads the two chains' `i16` coefficients directly and
/// accumulates the conjugate product as it goes, so neither chain is ever
/// materialised as a `Vec<Complex32>`. On a 256-record window that removed 512
/// allocations per frame.
pub fn doppler_series_into(
    samples: &[Arc<CsiRecord>],
    ticks: &[u64],
    chain_a: usize,
    chain_b: Option<usize>,
    out: &mut Series,
) {
    out.values.clear();
    out.ticks.clear();
    out.conjugate_pair = false;

    for (rec, &tick) in samples.iter().zip(ticks.iter()) {
        let Some(a) = chain_slice(rec, chain_a) else {
            continue;
        };
        let n = a.len() / 2;
        if n == 0 {
            continue;
        }
        let b = chain_b
            .and_then(|b| chain_slice(rec, b))
            .filter(|b| b.len() == a.len());

        let (mut sre, mut sim) = (0.0f32, 0.0f32);
        match b {
            Some(b) => {
                out.conjugate_pair = true;
                // (ar + i·ai)·(br − i·bi)
                for (x, y) in a.chunks_exact(2).zip(b.chunks_exact(2)) {
                    let (ai, ar) = (x[0] as f32, x[1] as f32);
                    let (bi, br) = (y[0] as f32, y[1] as f32);
                    sre += ar * br + ai * bi;
                    sim += ai * br - ar * bi;
                }
            }
            None => {
                for x in a.chunks_exact(2) {
                    sim += x[0] as f32;
                    sre += x[1] as f32;
                }
            }
        }
        let inv = 1.0 / n as f32;
        out.values.push(Complex32::new(sre * inv, sim * inv));
        out.ticks.push(tick);
    }
}

/// STFT one column of a Doppler spectrogram from an irregularly sampled series.
///
/// Ambient capture does not give uniform sampling — a record exists only when
/// somebody transmitted — so the series is first nearest-neighbour resampled
/// onto a uniform grid spanning the window, at the rate the window actually
/// achieved. `Doppler::arrival_cv` reports how irregular the input was, because
/// a frequency axis over badly non-uniform samples is decorative, not
/// quantitative.
///
/// `wavelength_m` comes from the tuned centre frequency and converts the
/// Doppler axis into a speed axis.
pub fn doppler(series: &Series, nfft: usize, wavelength_m: f32, fs_pinned: f64) -> Doppler {
    let mut out = Doppler::default();
    LOCAL_TRANSFORMS.with(|t| {
        doppler_into(
            &mut t.borrow_mut(),
            series,
            nfft,
            wavelength_m,
            fs_pinned,
            &mut Vec::new(),
            &mut out,
        )
    });
    out
}

/// [`doppler`], with cached plans and caller-owned buffers.
///
/// `fs_pinned` is the sample rate the frequency axis is built from. Pass 0 to
/// let this column snap its own from the delivered rate — correct for a
/// one-shot call, wrong for a spectrogram, where the caller must hold one rate
/// across columns. See [`Doppler`].
pub fn doppler_into(
    tf: &mut Transforms,
    series: &Series,
    nfft: usize,
    wavelength_m: f32,
    fs_pinned: f64,
    buf: &mut Vec<Complex32>,
    out: &mut Doppler,
) {
    out.power_db.clear();
    out.fs_hz = 0.0;
    out.fs_window_hz = 0.0;
    out.fs_source = "none";
    out.max_hz = 0.0;
    out.max_speed_ms = 0.0;
    out.gap_frac = 0.0;
    out.span_s = 0.0;
    out.arrival_cv = 0.0;
    out.conjugate_pair = series.conjugate_pair;

    let n = series.values.len();
    if n < 8 || nfft < 8 {
        return;
    }

    let t0 = series.ticks[0];
    let t1 = *series.ticks.last().unwrap();
    let window_span_s = csiq::ftm_to_seconds(t1.saturating_sub(t0));
    if window_span_s <= 0.0 {
        return;
    }
    let fs_window = (n - 1) as f64 / window_span_s;

    // Inter-arrival dispersion: how far from uniform the sampling really was.
    // Accumulated in one pass rather than through an intermediate vector.
    let (mut sum, mut sum_sq) = (0.0f64, 0.0f64);
    for w in series.ticks.windows(2) {
        let d = csiq::ftm_to_seconds(w[1].saturating_sub(w[0]));
        sum += d;
        sum_sq += d * d;
    }
    let count = (n - 1) as f64;
    let mean_d = sum / count;
    let var_d = (sum_sq / count - mean_d * mean_d).max(0.0);
    out.arrival_cv = if mean_d > 0.0 {
        (var_d.sqrt() / mean_d) as f32
    } else {
        0.0
    };

    // The axis. A caller drawing a spectrogram supplies it and holds it still;
    // a one-shot caller gets the same ladder applied to this window alone.
    let fs = if fs_pinned > 0.0 {
        fs_pinned
    } else {
        snap_rate_hz(fs_window)
    };
    if !(fs > 0.0) {
        return;
    }

    // Resample onto the pinned grid, ending at the newest sample and reaching
    // back `nfft / fs` seconds. A grid slot takes the nearest sample within
    // half a step; anything further away is a GAP, not a held value.
    //
    // Holding the previous sample across a long silence — which is what this
    // did before — manufactures a flat stretch of signal out of an absence of
    // one, and a flat stretch is energy at 0 Hz. The gaps are filled with the
    // series mean instead, so they land exactly on the DC term that is removed
    // two steps below and contribute nothing at all.
    let step_ns = 1e9 / fs;
    let grid_span_ns = step_ns * (nfft - 1) as f64;
    let last_ns = ticks_to_ns(t1);
    let first_ns = last_ns - grid_span_ns;
    out.span_s = (grid_span_ns / 1e9) as f32;

    buf.clear();
    buf.resize(nfft, Complex32::new(0.0, 0.0));
    let tolerance_ns = step_ns / 2.0;
    let mut cursor = 0usize;
    let mut gaps = 0usize;
    let mut filled: Complex32 = Complex32::new(0.0, 0.0);
    let mut filled_n = 0usize;
    // `hit[i]` is false where the grid found nothing; a second pass fills those
    // with the mean of the ones that did.
    let mut hit = vec![false; nfft];
    for i in 0..nfft {
        let want = first_ns + step_ns * i as f64;
        // Advance while the NEXT sample is at least as close as the current one.
        while cursor + 1 < n {
            let here = (ticks_to_ns(series.ticks[cursor]) - want).abs();
            let next = (ticks_to_ns(series.ticks[cursor + 1]) - want).abs();
            if next <= here {
                cursor += 1;
            } else {
                break;
            }
        }
        if (ticks_to_ns(series.ticks[cursor]) - want).abs() <= tolerance_ns {
            buf[i] = series.values[cursor];
            hit[i] = true;
            filled += series.values[cursor];
            filled_n += 1;
        } else {
            gaps += 1;
        }
    }
    out.gap_frac = gaps as f32 / nfft as f32;
    if filled_n == 0 {
        return;
    }
    let mean = filled / filled_n as f32;
    for (i, v) in buf.iter_mut().enumerate() {
        if !hit[i] {
            *v = mean;
        }
    }

    // Remove the static component: the LoS and every unmoving reflector sit at
    // 0 Hz and would otherwise dominate by tens of dB. The mean is taken over
    // the slots that carry a sample, so the fill cancels exactly.
    //
    // Hann window suppresses the leakage skirts that a rectangular window would
    // spread from that (never perfectly cancelled) DC term.
    let window = tf.hann(nfft);
    for (v, &w) in buf.iter_mut().zip(window.iter()) {
        *v = (*v - mean) * w;
    }

    tf.run(buf, false);

    // FFT-shift: negative frequencies first, so the axis reads −fs/2 … +fs/2.
    let half = nfft / 2;
    out.power_db.resize(nfft, 0.0);
    let mut peak = POWER_FLOOR;
    for (o, c) in out
        .power_db
        .iter_mut()
        .zip(buf[half..].iter().chain(buf[..half].iter()))
    {
        let p = c.re * c.re + c.im * c.im;
        *o = p;
        if p > peak {
            peak = p;
        }
    }
    let peak_db = db_from_power(peak);
    for v in out.power_db.iter_mut() {
        *v = db_from_power(*v) - peak_db;
    }

    let max_hz = (fs / 2.0) as f32;
    out.fs_hz = fs as f32;
    out.fs_window_hz = fs_window as f32;
    out.fs_source = if fs_pinned > 0.0 { "tracked" } else { "column" };
    out.max_hz = max_hz;
    // v = λ·f_D/2 for a reflected (two-way) path.
    out.max_speed_ms = wavelength_m * max_hz / 2.0;
}

/// 320 MHz baseband ticks as nanoseconds, in `f64`.
///
/// The window spans seconds, so `f64` nanoseconds keep every difference exact
/// well past any window the console can hold.
#[inline]
fn ticks_to_ns(ticks: u64) -> f64 {
    ticks as f64 * 1e9 / csiq::FTM_HZ as f64
}

/// Free-space wavelength for a centre frequency in MHz.
pub fn wavelength_m(freq_mhz: f64) -> f32 {
    if freq_mhz <= 0.0 {
        return 0.0;
    }
    (299_792_458.0 / (freq_mhz * 1e6)) as f32
}

// -- extraction validation ----------------------------------------------------

/// Sanity checks on the *extraction*, not on the environment.
///
/// Gringoli et al. 2019 verified their CSI was really CSI by three qualitative
/// properties visible in any correct capture: the DC subcarrier is suppressed,
/// amplitude rolls off towards the band edges under analogue filtering, and
/// separate RX chains show genuinely different amplitudes. Those are exactly
/// the checks that catch a driver-ABI drift or a misparsed matrix, so the
/// console computes them continuously instead of leaving them to the eye.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Validation {
    /// Depth of the DC null: the *weakest* tone within the middle 5% of the
    /// band, minus the whole-band mean, in dB.
    ///
    /// The minimum rather than the mean because 802.11 nulls only a handful of
    /// centre subcarriers — averaging a 3-tone null across a 12-tone window
    /// would hide it. A correct capture is clearly negative here; ≈0 means the
    /// tone grid is not centred where the parser assumes.
    ///
    /// **`None` on a tone grid that has no DC tone**, which is every 802.11
    /// used-tone set this fleet captures. The check took the minimum of the
    /// middle ±2.5% of the delivered array; on a 52-tone legacy grid that is
    /// two tones, `k = −1` and `k = +1`, both of them data. It was structurally
    /// incapable of finding a notch that is not in the array, and it therefore
    /// failed permanently — measured 2026-08-23 across four nodes, it never once
    /// reached the −3 dB the panel wanted, reading between −1.3 and +7.9 dB.
    ///
    /// A check that cannot pass is not a check. See [`crate::tones::grid`]: a
    /// `Uniform` grid (the ray-traced simulator) does deliver DC, and there the
    /// test is both applicable and meaningful.
    pub dc_notch_db: Option<f32>,
    /// Mean of the outer 10% of tones minus the mean of the inner 20%, in dB.
    /// Analogue filtering makes this negative; ≈0 means no roll-off, which on
    /// real hardware means the frequency axis is not what we assume.
    pub edge_rolloff_db: f32,
    /// Spread between the strongest and weakest chain's mean amplitude, in dB.
    /// This is an AGC/geometry difference, and it can legitimately be near
    /// zero on two well-matched antennas — so it is *not* the identical-chain
    /// test. See [`Validation::chain_identical`].
    pub chain_spread_db: f32,
    /// Fraction of coefficients that are bit-identical between the first two
    /// chains.
    ///
    /// This is the actual "are the chains real" test. Comparing mean amplitude
    /// cannot detect a misparse that copies one chain across the matrix — two
    /// copies have identical means *and* identical shape, and the mean-spread
    /// check reads 0 dB either way, which is also what two well-matched
    /// antennas read. A value near 1.0 means the parser is reading the same
    /// bytes twice; genuine chains agree on almost nothing exactly.
    /// `None` when the record carries fewer than two chains.
    pub chain_identical: Option<f32>,
    /// Fraction of coefficients that are exactly (0, 0).
    pub zero_fraction: f32,
    /// Did the I/Q payload length match the declared `ntone·nrx·ntx`?
    pub dimensions_ok: bool,
}

/// Compute the extraction-validation panel for one record.
pub fn validate(rec: &CsiRecord) -> Validation {
    let mut v = Validation::default();
    validate_into(rec, &mut Vec::new(), &mut v);
    v
}

/// [`validate`], with a caller-owned scratch buffer.
pub fn validate_into(rec: &CsiRecord, amp: &mut Vec<f32>, v: &mut Validation) {
    let g = Geometry::of(rec);
    *v = Validation {
        dimensions_ok: g.matches(rec),
        ..Default::default()
    };
    if !v.dimensions_ok || g.ntone < 20 {
        return;
    }

    let zeros = rec
        .iq
        .chunks_exact(2)
        .filter(|c| c[0] == 0 && c[1] == 0)
        .count();
    v.zero_fraction = zeros as f32 / (rec.iq.len() / 2).max(1) as f32;

    // Chain means, one chain at a time through the shared buffer — the old
    // form built an `amp_db` vector per chain and kept them all alive to take
    // two numbers from them.
    let (mut hi, mut lo) = (f32::NEG_INFINITY, f32::INFINITY);
    for c in 0..g.nchain() {
        chain_amp_db_into(rec, c, amp);
        if amp.is_empty() {
            continue;
        }
        let mean = amp.iter().sum::<f32>() / amp.len() as f32;
        hi = hi.max(mean);
        lo = lo.min(mean);
    }
    if !hi.is_finite() {
        return;
    }
    v.chain_spread_db = hi - lo;

    if g.nchain() >= 2 {
        if let (Some(a), Some(b)) = (chain_slice(rec, 0), chain_slice(rec, 1)) {
            if a.len() == b.len() && !a.is_empty() {
                let same = a
                    .chunks_exact(2)
                    .zip(b.chunks_exact(2))
                    .filter(|(x, y)| x == y)
                    .count();
                v.chain_identical = Some(same as f32 / (a.len() / 2) as f32);
            }
        }
    }

    // Shape checks run on chain 0; the others differ only by AGC and geometry.
    chain_amp_db_into(rec, 0, amp);
    let a = &amp[..];
    let n = a.len();
    if n == 0 {
        return;
    }
    let whole = a.iter().sum::<f32>() / n as f32;

    // Only ask the question where the answer can exist. On an 802.11 used-tone
    // set the driver never delivers DC, so there is no notch in this array to
    // find and the middle of it is ordinary data.
    v.dc_notch_db = match crate::tones::grid(n) {
        crate::tones::Grid::Dot11 => None,
        crate::tones::Grid::Uniform => {
            let centre_half = (n / 40).max(1); // ±2.5% of the band
            let mid = n / 2;
            let centre = &a[mid.saturating_sub(centre_half)..(mid + centre_half).min(n)];
            Some(centre.iter().cloned().fold(f32::INFINITY, f32::min) - whole)
        }
    };

    let edge = (n / 10).max(1);
    let edges =
        (a[..edge].iter().sum::<f32>() + a[n - edge..].iter().sum::<f32>()) / (2 * edge) as f32;
    // The reference is taken from two quarter-band shoulders, not from the
    // middle of the band: the middle contains the DC notch, and a deep notch
    // there would drag the reference down and hide a real roll-off.
    let shoulders = [(n / 4, (n * 2) / 5), ((n * 3) / 5, (n * 3) / 4)];
    let (mut sum, mut count) = (0.0f32, 0usize);
    for &(lo, hi) in &shoulders {
        let s = &a[lo.min(n)..hi.min(n)];
        sum += s.iter().sum::<f32>();
        count += s.len();
    }
    let reference = if count > 0 { sum / count as f32 } else { 0.0 };
    v.edge_rolloff_db = edges - reference;
}

// -- timing -------------------------------------------------------------------

/// Inter-arrival statistics over a window, measured on whichever clock the
/// caller passes in.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Timing {
    pub n: usize,
    pub rate_hz: f32,
    pub mean_us: f32,
    pub p50_us: f32,
    pub p95_us: f32,
    pub p99_us: f32,
    pub p999_us: f32,
    pub max_us: f32,
    /// Coefficient of variation of the inter-arrival time.
    pub cv: f32,
}

/// Inter-arrival distribution from a monotonically increasing time series (ns).
pub fn timing_ns(times_ns: &[u64]) -> Timing {
    timing_ns_into(times_ns, &mut Vec::new())
}

/// [`timing_ns`], with a caller-owned scratch buffer.
///
/// Four ordered quantiles do not need a sort. Selecting them in increasing
/// order lets each selection run on the partition the previous one left to its
/// right, so the work is a few linear passes over a shrinking slice rather
/// than `n log n` over the whole window — twice per frame, on both clocks.
pub fn timing_ns_into(times_ns: &[u64], scratch: &mut Vec<f32>) -> Timing {
    if times_ns.len() < 3 {
        return Timing::default();
    }
    scratch.clear();
    scratch.reserve(times_ns.len() - 1);
    scratch.extend(
        times_ns
            .windows(2)
            .map(|w| w[1].saturating_sub(w[0]) as f32 / 1000.0),
    );
    let d = &mut scratch[..];
    let n = d.len();

    let mean = d.iter().sum::<f32>() / n as f32;
    let var = d.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32;
    let max = d.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

    let mut out = Timing {
        n: times_ns.len(),
        rate_hz: if mean > 0.0 { 1e6 / mean } else { 0.0 },
        mean_us: mean,
        max_us: max,
        cv: if mean > 0.0 { var.sqrt() / mean } else { 0.0 },
        ..Default::default()
    };

    // Quantile indices into the fully sorted order, ascending.
    let targets = [idx(n, 0.50), idx(n, 0.95), idx(n, 0.99), idx(n, 0.999)];
    let mut values = [0.0f32; 4];
    // `base` is the index, in the original array, of `rest[0]`.
    let mut rest = &mut d[..];
    let mut base = 0usize;
    // The value most recently selected. When two quantile indices collide —
    // which they do on short windows, where p95 and p999 land on the same
    // element — the later one is that same element, already in hand.
    let mut last = 0.0f32;
    for i in 0..targets.len() {
        let k = targets[i];
        if k < base {
            values[i] = last;
            continue;
        }
        let local = k - base;
        if local >= rest.len() {
            last = rest.last().copied().unwrap_or(max);
            values[i] = last;
            continue;
        }
        let (_, mid, right) = rest.select_nth_unstable_by(local, f32::total_cmp);
        last = *mid;
        values[i] = last;
        rest = right;
        base = k + 1;
    }

    out.p50_us = values[0];
    out.p95_us = values[1];
    out.p99_us = values[2];
    out.p999_us = values[3];
    out
}

// -- the metronome ------------------------------------------------------------

/// Largest slot multiple the histogram reports. Beyond this the gaps are a
/// tail, not a pattern.
pub const MAX_MULTIPLE: usize = 16;

/// A gap counts as on-slot when it is within this fraction of an exact
/// multiple. A quarter of a slot is far wider than the jitter measured on a
/// clear channel (the 5 GHz arm's whole p95 residual is 3% of a slot) and far
/// narrower than half, where multiples would start to overlap.
const ON_SLOT_TOLERANCE: f64 = 0.25;

/// The tight tolerance: a gap this close to a multiple is *on the grid*, not
/// merely near it.
///
/// Two tolerances rather than one because the measured 2.4 GHz arm is a
/// mixture, and a single threshold cannot describe it. Its residuals are
/// bimodal — p50 is 0.001 of a slot and p75 is 0.11 — so 65.8% of gaps land
/// dead on the grid while 11.7% sit at an arbitrary phase (their fractional
/// part has p25/p50/p75 = 0.40/0.52/0.63, i.e. uniform). The 5 GHz pair, on a
/// clear channel, is 94.2% exact with fourteen off-grid gaps in twenty-nine
/// thousand.
const EXACT_SLOT_TOLERANCE: f64 = 0.05;

/// Above this on-slot fraction the source is simply on the grid.
const ON_GRID_AT: f32 = 0.90;

/// The largest deficit an *inferred* slot is allowed to imply.
///
/// A backstop, independent of the grid statistics below it. A metronome that
/// loses slots loses some of them: the measured 2.4 GHz arm lost 38.7% and the
/// 5 GHz arm 0.6%. A slot recovered from the arrivals that implies the source
/// delivered under 5% of what it "commanded" has not found a rate — it has
/// found the spacing inside a burst, and dividing the two produces a number
/// with no referent.
///
/// A declared slot is exempt: the operator stated the rate, and a source that
/// delivers 0.3% of a commanded rate is a real and important measurement.
const INFERRED_DEFICIT_CEILING: f32 = 0.95;

/// Above this *exact* fraction there is a real slot underneath, even when a
/// substantial population has been pushed off it.
const DEFERRED_AT: f32 = 0.40;

/// How a periodic transmitter is actually delivering.
///
/// ## Why the coefficient of variation is the wrong statistic here
///
/// Measured on 2026-08-17, one injector at a commanded 10 ms slot, two bands:
///
/// | | 2.4 GHz ch6 | 5 GHz ch36 |
/// |---|---|---|
/// | delivered | 61.3 Hz | 99.4 Hz |
/// | p50 / p95 / p99.9 | 10.00 / 40.00 / 80.00 ms | 10.00 / 10.03 / 20.02 ms |
/// | CV | 0.714 | 0.083 |
///
/// The percentiles are exact integer multiples of the slot. The source is not
/// jittering — it is metronomic and *losing whole slots*, 38.7% of them on
/// 2.4 GHz against 0.6% on 5 GHz. The console used to report `CV 0.71` and,
/// from that, "treat the Doppler axis as qualitative". That verdict is wrong
/// for this process: every surviving arrival is on grid, so nearest-neighbour
/// resampling is near-exact and only the missing slots need filling. Irregular
/// ambient traffic with the same CV resamples badly. One number, two physically
/// different processes.
///
/// So this measures the two quantities that do separate them: what fraction of
/// arrivals are on-slot, and what fraction of slots arrived at all. The second
/// is EXP-010's primary occupancy channel — "the delivery deficit of an
/// injected metronomic reference against its 5 GHz pair".
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Metronome {
    /// Gaps the estimate is built from.
    pub n_gaps: usize,
    /// The nominal slot, in microseconds. Zero when none could be established.
    pub slot_us: f32,
    /// `"declared"` when the capture stated `radio.interval_us`, `"inferred"`
    /// when it was recovered from the arrivals, `"none"` when neither worked.
    pub slot_source: &'static str,
    /// `1e6 / slot_us` — the rate the source is trying to achieve.
    ///
    /// **`None` when no rate can honestly be claimed**, which is whenever the
    /// slot was *inferred* and the arrivals turned out not to be metronomic.
    /// See [`Metronome::deficit`].
    pub commanded_hz: Option<f32>,
    /// Arrivals per second actually delivered over the window.
    pub delivered_hz: f32,
    /// `1 − delivered/commanded`, clamped to `[0, 1]`.
    ///
    /// ## Why this is an `Option`, and what it cost when it was not
    ///
    /// A deficit is a comparison against a commanded rate. When the capture
    /// declares `radio.interval_us` there is one, and the comparison is exact —
    /// that is EXP-010's primary occupancy channel. When it does not,
    /// [`infer_slot_us`] recovers a slot from the arrivals themselves, and it
    /// seeds on their p25.
    ///
    /// On a *bursty* source the p25 is the spacing INSIDE a burst. Measured
    /// 2026-08-23 on all four nodes: the injector's gaps had a p50 of 236 µs and
    /// a p95 of 191 ms, the inference returned a 158.6 µs slot, and the console
    /// reported a commanded rate of **6305 Hz** and a delivery deficit of
    /// **99.4%** — in red, permanently, on every node. The configured injector
    /// rate was 25 Hz and `interval_us` was 0. Nothing had commanded 6305 Hz.
    ///
    /// The console already knew. [`Metronome::verdict`] returned `irregular`,
    /// whose own documentation says "no mode at all … nothing else in this
    /// struct describes it" — and the deficit was published anyway, because it
    /// was a bare `f32` with no way to say "not applicable".
    ///
    /// So: an inferred slot yields a deficit only if the arrivals then turn out
    /// to be on a grid. A declared slot always yields one, because the operator
    /// stated the rate and losing every frame of it is exactly the measurement.
    pub deficit: Option<f32>,
    /// Share of gaps at each multiple `k = 1..=MAX_MULTIPLE`, indexed from 0.
    /// Sums to `1 − off_slot` less whatever fell beyond the last bin.
    pub multiples: Vec<f32>,
    /// Share of gaps that are not near any integer multiple. High here means
    /// the source is not metronomic and nothing else in this struct applies.
    pub off_slot: f32,
    /// Share of gaps sitting *dead* on a multiple, within 5% of a slot.
    ///
    /// The quantity that separates the two ways a metronome fails. A slot that
    /// was never transmitted leaves a gap of exactly `k` slots; a slot that was
    /// deferred by the channel leaves a gap at an arbitrary phase. Measured
    /// 2026-08-17: 65.8% exact on 2.4 GHz against 94.2% on 5 GHz.
    pub exact_slot: f32,
    /// Longest run of consecutive missed slots, i.e. `max(k) − 1`.
    pub longest_run: u32,
    /// True only when the source is squarely on the grid — the case where the
    /// Doppler axis survives resampling intact.
    pub quantised: bool,
}

impl Metronome {
    /// The verdict, in the words the panel prints.
    ///
    /// Three outcomes, because the measured arms show three mechanisms and a
    /// binary verdict misdescribes the middle one:
    ///
    /// - **`on grid`** — nearly every surviving arrival sits on a multiple, so
    ///   the missing slots are the whole story. Resampling onto a uniform grid
    ///   is near-exact and the Doppler axis is quantitative whatever the CV
    ///   says. The measured 5 GHz arm: 100% on-slot at a 0.6% deficit.
    /// - **`deferred`** — there is a real slot underneath (a clear mode at the
    ///   grid) but a substantial population has been pushed off it. On 2.4 GHz
    ///   that is CSMA/CA: the radio waits for a clear channel, so a frame is
    ///   not merely dropped, it is delayed by a random backoff — which is why
    ///   the off-grid gaps land at an arbitrary phase rather than near one.
    ///   The measured 2.4 GHz arm: 65.8% exact, 86.5% within tolerance, 38.7%
    ///   deficit.
    /// - **`irregular`** — no mode at all. Ambient traffic. Nothing else in
    ///   this struct describes it.
    pub fn verdict(&self) -> &'static str {
        if self.slot_us <= 0.0 {
            "no slot"
        } else if self.quantised {
            "on grid"
        } else if self.exact_slot >= DEFERRED_AT {
            "deferred"
        } else {
            "irregular"
        }
    }

    /// Does the missing-slot picture hold well enough to resample against?
    pub fn resamples_cleanly(&self) -> bool {
        self.quantised
    }
}

/// Recover the nominal slot from a gap distribution, in microseconds.
///
/// The p25 is the seed rather than the median, because the median is only the
/// slot while fewer than half the slots are lost, and the whole point is to
/// survive heavy loss. The refinement pass then re-centres on the gaps that
/// agree with the seed, so a seed landing between two multiples is pulled onto
/// the real one.
///
/// Returns `None` when too few gaps cluster around the seed to call it a slot
/// at all — a genuinely irregular source must not be handed a fabricated one.
fn infer_slot_us(sorted_gaps_us: &[f32]) -> Option<f64> {
    if sorted_gaps_us.len() < 8 {
        return None;
    }
    let seed = sorted_gaps_us[sorted_gaps_us.len() / 4] as f64;
    if !(seed > 0.0) {
        return None;
    }
    let (lo, hi) = (seed * 0.7, seed * 1.3);
    let near: Vec<f32> = sorted_gaps_us
        .iter()
        .copied()
        .filter(|&g| (g as f64) >= lo && (g as f64) <= hi)
        .collect();
    // A tenth of the gaps agreeing on a value is a mode; less is a coincidence.
    if near.len() * 10 < sorted_gaps_us.len() {
        return None;
    }
    Some(near[near.len() / 2] as f64)
}

/// Measure a transmitter's delivery against its slot.
///
/// `times_ns` must be ascending. `declared_us` is the capture's commanded
/// `radio.interval_us` when it has one — a declared slot is always preferred,
/// because inferring the slot from the very arrivals being judged makes a
/// source that lost every other slot look perfectly on time at half the rate.
pub fn metronome_into(
    times_ns: &[u64],
    declared_us: Option<f64>,
    scratch: &mut Vec<f32>,
    out: &mut Metronome,
) {
    out.multiples.clear();
    out.multiples.resize(MAX_MULTIPLE, 0.0);
    out.n_gaps = times_ns.len().saturating_sub(1);
    out.slot_us = 0.0;
    out.slot_source = "none";
    out.commanded_hz = None;
    out.delivered_hz = 0.0;
    out.deficit = None;
    out.off_slot = 0.0;
    out.exact_slot = 0.0;
    out.longest_run = 0;
    out.quantised = false;

    if out.n_gaps < 8 {
        return;
    }

    scratch.clear();
    scratch.reserve(out.n_gaps);
    scratch.extend(
        times_ns
            .windows(2)
            .map(|w| w[1].saturating_sub(w[0]) as f32 / 1000.0),
    );
    let span_us = times_ns[times_ns.len() - 1].saturating_sub(times_ns[0]) as f64 / 1000.0;
    if span_us <= 0.0 {
        return;
    }
    out.delivered_hz = (out.n_gaps as f64 / (span_us / 1e6)) as f32;

    let slot_us = match declared_us.filter(|&d| d > 0.0) {
        Some(d) => {
            out.slot_source = "declared";
            d
        }
        None => {
            scratch.sort_by(f32::total_cmp);
            match infer_slot_us(scratch) {
                Some(s) => {
                    out.slot_source = "inferred";
                    s
                }
                None => return,
            }
        }
    };
    out.slot_us = slot_us as f32;

    // The sort above (inference path only) reorders `scratch`, which is fine:
    // every quantity below is a property of the multiset of gaps, not of their
    // order. The longest run is the largest single gap, not a positional one.
    let mut on_slot = 0usize;
    let mut exact = 0usize;
    let mut off = 0usize;
    let mut max_k = 0u32;
    for &g in scratch.iter() {
        let ratio = g as f64 / slot_us;
        let k = ratio.round();
        let resid = (ratio - k).abs();
        // `k` is bounded by the same MAX_MULTIPLE the histogram uses, and the
        // bound is load-bearing rather than cosmetic. The residual test is a
        // FRACTION of the slot, so as `k` grows the multiples become dense
        // relative to the gaps and almost any interval lands near one of them.
        //
        // Measured on the archived segment
        // `monad01_illum-coex-03_20260823-102958-seg0003`: the inferred slot was
        // 158 µs and the largest gap was 10,246 of them. Counting those as "on
        // the grid" put 65% of gaps on-slot and 36% dead on it, which reads as a
        // metronome — for a source whose gaps run from 158 µs to 1.6 seconds.
        // With the bound applied the same capture reads 51% on-slot and 33%
        // exact, i.e. `irregular`, which is what it is.
        //
        // A gap of ten thousand slots is not a metronome that missed 10,249
        // transmissions. It is a different process, and the histogram has always
        // said so by refusing to bin it.
        if k >= 1.0 && k <= MAX_MULTIPLE as f64 && resid <= ON_SLOT_TOLERANCE {
            on_slot += 1;
            if resid <= EXACT_SLOT_TOLERANCE {
                exact += 1;
            }
            max_k = max_k.max(k as u32);
            let bin = (k as usize - 1).min(MAX_MULTIPLE - 1);
            out.multiples[bin] += 1.0;
        } else {
            off += 1;
        }
    }
    let n = out.n_gaps as f32;
    for v in out.multiples.iter_mut() {
        *v /= n;
    }
    out.off_slot = off as f32 / n;
    out.exact_slot = exact as f32 / n;
    out.longest_run = max_k.saturating_sub(1);
    out.quantised = (on_slot as f32 / n) >= ON_GRID_AT;

    // The deficit, last, because whether it may be published depends on the
    // grid statistics computed just above. A declared slot is always a fair
    // comparison; an inferred one is a fair comparison only if the arrivals it
    // was inferred from turned out to be on a grid at all.
    let commanded = 1e6 / slot_us;
    let deficit = (1.0 - out.delivered_hz as f64 / commanded).clamp(0.0, 1.0) as f32;
    let declared = out.slot_source == "declared";
    let credible = declared
        || (out.verdict() != "irregular" && deficit <= INFERRED_DEFICIT_CEILING);
    if credible {
        out.commanded_hz = Some(commanded as f32);
        out.deficit = Some(deficit);
    }
}

// -- the CSI ratio ------------------------------------------------------------

/// `H_a / H_b` across two chains, as amplitude in dB and phase in radians.
///
/// The one representation whose **phase** is stable from packet to packet
/// without fitting anything. CFO, SFO and packet-detection delay are common to
/// both chains of one radio, so the division cancels them exactly (FarSense,
/// Zeng et al. 2019) — where the console's sanitised phase subtracts a
/// least-squares line and takes any genuinely linear part of the channel with
/// it.
///
/// The denominator is floored at one LSB of the `i16` grid: a nulled subcarrier
/// is an exact zero, and dividing by it would put an infinity in the middle of
/// the trace.
pub fn ratio_into(
    rec: &csiq::CsiRecord,
    chain_a: usize,
    chain_b: usize,
    amp_db: &mut Vec<f32>,
    phase: &mut Vec<f32>,
) -> bool {
    amp_db.clear();
    phase.clear();
    let (Some(a), Some(b)) = (chain_slice(rec, chain_a), chain_slice(rec, chain_b)) else {
        return false;
    };
    let n = (a.len() / 2).min(b.len() / 2);
    if n == 0 {
        return false;
    }
    amp_db.reserve(n);
    phase.reserve(n);
    for t in 0..n {
        // Storage is (im, re) per tone; see `chain_into`.
        let (ai, ar) = (a[2 * t] as f32, a[2 * t + 1] as f32);
        let (bi, br) = (b[2 * t] as f32, b[2 * t + 1] as f32);
        let den = (br * br + bi * bi).max(1.0);
        // H_a · conj(H_b) / |H_b|²
        let re = (ar * br + ai * bi) / den;
        let im = (ai * br - ar * bi) / den;
        amp_db.push(db_from_power(re * re + im * im));
        phase.push(im.atan2(re));
    }
    true
}

// -- per-tone statistics ------------------------------------------------------

/// Per-subcarrier behaviour over the window.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToneStats {
    /// Columns the statistics were computed over.
    pub n: usize,
    /// Tones whose median sits at the quantisation floor — nulled, not measured.
    pub null_tones: usize,
    /// The widest per-tone spread in the band, in dB.
    pub max_spread_db: f32,
    /// Array index of that tone, so the readout can name it.
    pub max_spread_tone: usize,
}

/// Median and temporal spread for every tone, from the decimated column buffer.
///
/// `columns` is `cols × ntone` in dB, exactly the buffer the percentile bundle
/// already reads, so this adds a pass rather than an extraction.
///
/// **Spread is reported in dB, not as a coefficient of variation.** A CV of a
/// logarithmic quantity is not a CV of the underlying amplitude, and the two
/// differ by whatever the mean level happens to be. For small excursions the
/// conversion is `CV ≈ spread_dB / 8.686`; the panel prints that relation
/// rather than silently applying it.
pub fn tone_stats_into(
    columns: &[f32],
    ntone: usize,
    median_db: &mut Vec<f32>,
    spread_db: &mut Vec<f32>,
    null_frac: &mut Vec<f32>,
    scratch: &mut Vec<f32>,
    out: &mut ToneStats,
) {
    median_db.clear();
    spread_db.clear();
    null_frac.clear();
    *out = ToneStats::default();
    if ntone == 0 || columns.len() < ntone {
        return;
    }
    let cols = columns.len() / ntone;
    out.n = cols;
    median_db.resize(ntone, f32::NAN);
    spread_db.resize(ntone, f32::NAN);
    null_frac.resize(ntone, 0.0);

    for t in 0..ntone {
        scratch.clear();
        let mut zeros = 0usize;
        let mut sum = 0.0f64;
        let mut sumsq = 0.0f64;
        for c in 0..cols {
            let v = columns[c * ntone + t];
            if !v.is_finite() {
                continue;
            }
            // An exact zero coefficient reaches the console as 0 dB, the
            // quantisation floor. It is an absence of measurement, not a very
            // weak one, and averaging it in would drag the tone's statistics
            // towards a value the radio never reported.
            if v <= NULL_TONE_DB {
                zeros += 1;
                continue;
            }
            scratch.push(v);
            sum += v as f64;
            sumsq += (v as f64) * (v as f64);
        }
        null_frac[t] = zeros as f32 / cols as f32;
        if scratch.is_empty() {
            out.null_tones += 1;
            continue;
        }
        let k = scratch.len() / 2;
        scratch.select_nth_unstable_by(k, f32::total_cmp);
        median_db[t] = scratch[k];
        let m = sum / scratch.len() as f64;
        let var = (sumsq / scratch.len() as f64 - m * m).max(0.0);
        let sd = var.sqrt() as f32;
        spread_db[t] = sd;
        if sd > out.max_spread_db {
            out.max_spread_db = sd;
            out.max_spread_tone = t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ntone: u16, nrx: u8, ntx: u8, f: impl Fn(usize, usize) -> (i16, i16)) -> CsiRecord {
        let nc = (nrx * ntx) as usize;
        // Build the buffer the way the driver does: chain-major blocks of
        // tones, each coefficient imaginary-first.
        let mut iq = Vec::with_capacity(2 * ntone as usize * nc);
        for c in 0..nc {
            for t in 0..ntone as usize {
                let (re, im) = f(t, c);
                iq.push(im);
                iq.push(re);
            }
        }
        CsiRecord {
            ftm: 0,
            us: 0,
            unix_ts_ns: 0,
            rnf: 0,
            phy: None,
            seq: 0,
            nrx,
            ntx,
            ntone,
            rssi: vec![-40; nrx as usize],
            src_mac: [0; 6],
            channel: 36,
            width: csiq::Width::W80,
            iq,
        }
    }

    /// The approximation is only allowed to be invisible. The console
    /// quantises the waterfall to 256 levels over a 60 dB range — a quarter of
    /// a dB per level — and the numeric panels show one decimal, so the budget
    /// is 1e-3 dB and the fit should sit orders of magnitude under it.
    #[test]
    fn the_fast_logarithm_is_indistinguishable_from_libm() {
        let mut worst = 0.0f32;
        // Sweep the whole range a power can take: one LSB squared up to the
        // largest `|H|²` an i16 pair can produce, plus every binade between.
        let mut p = POWER_FLOOR;
        while p < 4.3e9 {
            let exact = 10.0 * (p as f64).log10();
            let err = (db_from_power(p) as f64 - exact).abs() as f32;
            worst = worst.max(err);
            p *= 1.000_37; // ~1900 points per binade
        }
        assert!(
            worst < 1e-3,
            "fast log10 worst error was {worst} dB over the i16 power range"
        );

        // And the identity the amplitude path relies on: 20·log10|H| computed
        // through the power form must match the direct form.
        for &(re, im) in &[(1.0f32, 0.0f32), (700.0, -300.0), (32767.0, 32767.0)] {
            let direct = 20.0 * (re * re + im * im).sqrt().max(MAG_FLOOR).log10();
            let viapower = db_from_power(re * re + im * im);
            assert!(
                (direct - viapower).abs() < 1e-3,
                "({re},{im}): {direct} vs {viapower}"
            );
        }
    }

    /// A cached plan must produce exactly what a freshly built one did.
    #[test]
    fn cached_plans_do_not_change_the_transform() {
        let h: Vec<Complex32> = (0..56)
            .map(|i| Complex32::new((i as f32 * 0.3).cos(), (i as f32 * 0.3).sin()))
            .collect();
        let a = cir(&h, 312_500.0, 512, 64);
        // Second call reuses the thread-local plan and window.
        let b = cir(&h, 312_500.0, 512, 64);
        assert_eq!(a.mag_db, b.mag_db);
        assert_eq!(a.peak_bin, b.peak_bin);
        assert_eq!(a.rms_delay_ns, b.rms_delay_ns);
    }

    #[test]
    fn chain_extraction_reads_chain_major_storage() {
        // Encode the chain index in the real part so a transposed read — or a
        // swapped I/Q — is immediately visible.
        let r = rec(4, 2, 2, |t, c| ((c * 100 + t) as i16, 0));
        for c in 0..4 {
            let h = chain(&r, c);
            assert_eq!(h.len(), 4);
            for (t, v) in h.iter().enumerate() {
                assert_eq!(v.re, (c * 100 + t) as f32, "chain {c} tone {t}");
            }
        }
        assert!(chain(&r, 4).is_empty(), "out-of-range chain yields nothing");
    }

    /// The `i16`-direct amplitude path must agree with going through
    /// `Complex32` — it is the same measurement, taken without the detour.
    #[test]
    fn the_direct_amplitude_path_matches_the_complex_one() {
        let r = rec(64, 2, 1, |t, c| {
            (((t * 7) as i16) - 200, ((c * 31 + t * 3) as i16) - 50)
        });
        for c in 0..2 {
            let via_complex = amp_db(&chain(&r, c));
            let mut direct = Vec::new();
            chain_amp_db_into(&r, c, &mut direct);
            assert_eq!(direct.len(), via_complex.len());
            for (i, (d, v)) in direct.iter().zip(&via_complex).enumerate() {
                assert!((d - v).abs() < 1e-3, "tone {i}: {d} vs {v}");
            }
        }
    }

    #[test]
    fn dimension_mismatch_is_detected_not_panicked() {
        let mut r = rec(8, 2, 1, |_, _| (1, 1));
        r.iq.truncate(4);
        assert!(!Geometry::of(&r).matches(&r));
        assert!(chain(&r, 0).is_empty());
        assert!(!validate(&r).dimensions_ok);
    }

    #[test]
    fn spacing_follows_the_phy_label_not_just_tone_count() {
        let mut r = rec(242, 1, 1, |_, _| (1, 0));
        // 242 tones with no label: assume the dense HE grid.
        assert_eq!(spacing_hz(&r), 78_125.0);
        // 242 tones labelled VHT is VHT80 on the sparse grid.
        r.phy = Some(csiq::PhyLabel {
            modulation: Modulation::Vht,
            mcs: 4,
            nss: 2,
        });
        assert_eq!(spacing_hz(&r), 312_500.0);
        assert!((occupied_bw_mhz(&r) - 75.625).abs() < 0.01);
    }

    #[test]
    fn unwrap_removes_two_pi_jumps() {
        let wrapped = [3.0, -3.0, 3.0];
        let u = unwrap(&wrapped);
        // Each step must be the small one, not the 6-radian wrap-around.
        assert!((u[1] - u[0] - (2.0 * std::f32::consts::PI - 6.0)).abs() < 1e-4);
        assert!((u[2] - u[1] + (2.0 * std::f32::consts::PI - 6.0)).abs() < 1e-4);
    }

    #[test]
    fn detrend_recovers_a_planted_delay() {
        // Plant a pure delay: φ(k) = −2π·τ·k·Δf with τ = 40 ns.
        let spacing = 312_500.0f64;
        let tau = 40e-9f64;
        let raw: Vec<f32> = (0..56)
            .map(|k| (-2.0 * std::f64::consts::PI * tau * k as f64 * spacing) as f32)
            .collect();
        let (residual, d) = detrend(&raw, spacing);
        assert!(
            (d.tau_ns - 40.0).abs() < 0.5,
            "recovered tau {} ns",
            d.tau_ns
        );
        // A pure linear phase leaves nothing behind.
        assert!(residual.iter().all(|r| r.abs() < 1e-3));
    }

    #[test]
    fn cir_places_a_planted_tap_at_the_right_delay() {
        // A single path at 100 ns is a linear phase ramp across the band.
        let spacing = 312_500.0f64;
        let tau = 100e-9f64;
        let n = 56usize;
        let h: Vec<Complex32> = (0..n)
            .map(|i| {
                // Frequency offset from band centre.
                let k = i as f64 - n as f64 / 2.0;
                let ph = -2.0 * std::f64::consts::PI * tau * k * spacing;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let c = cir(&h, spacing, 512, 128);
        // `peak_bin` is the tap's index in the UN-aligned transform, which is
        // where the planted delay actually lands.
        let expect = (tau / (1.0 / (512.0 * spacing))).round() as usize;
        assert!(
            c.peak_bin.abs_diff(expect) <= 1,
            "peak at bin {} expected ~{expect}",
            c.peak_bin
        );
        // The returned profile is peak-aligned, so within it the peak sits at
        // `peak_index` — the centre — whatever the packet-detection delay was.
        assert_eq!(c.peak_index, c.mag_db.len() / 2);
        assert!(c.mag_db[c.peak_index] == 0.0, "peak must normalise to 0 dB");
        assert!(
            c.axis_start_ns < 0.0,
            "a peak-aligned axis starts before the peak"
        );
        // Resolution is 1/B and has nothing to do with `nfft`.
        let expected_res = 1e9 / (h.len() as f64 * spacing);
        assert!((c.resolution_ns as f64 - expected_res).abs() < 1e-6);
        assert!(
            c.resolution_ns > c.bin_ns * 8.0,
            "zero-padding interpolates; it must not be reported as resolution"
        );
    }

    #[test]
    fn the_rate_ladder_is_stable_and_never_narrows() {
        // Every rung maps to itself: a rate already on the ladder must not be
        // pushed to the next one by floating-point noise.
        for rung in [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0] {
            assert_eq!(snap_rate_hz(rung), rung, "rung {rung}");
        }
        // At or above, never below — an axis under the delivered Nyquist aliases.
        for fs in [0.4, 1.7, 9.3, 21.3, 33.0, 37.4, 264.0, 608.0, 1883.0] {
            let snapped = snap_rate_hz(fs);
            assert!(snapped >= fs, "{fs} snapped down to {snapped}");
            // And never wastefully far above: at most one rung, i.e. 2.5x.
            assert!(snapped <= fs * 2.5, "{fs} snapped all the way to {snapped}");
        }
        // The measured wobble, 9.3 to 21.3 Hz, has to land somewhere stable.
        assert_eq!(snap_rate_hz(21.3), 50.0);
        assert_eq!(snap_rate_hz(0.0), 0.0);
        assert_eq!(snap_rate_hz(-1.0), 0.0);
    }

    #[test]
    fn a_record_of_zeros_is_not_a_measurement() {
        let good = rec(52, 2, 1, |t, c| (10 + t as i16, c as i16));
        assert!(is_measurement(&good));

        let mut empty = good.clone();
        empty.iq.iter_mut().for_each(|v| *v = 0);
        assert!(!is_measurement(&empty));
        // It still passes every structural check, which is the whole problem.
        assert!(Geometry::of(&empty).matches(&empty));
        assert_eq!(validate(&empty).zero_fraction, 1.0);

        // A single least-significant bit anywhere is a reading.
        let mut one = empty.clone();
        one.iq[7] = 1;
        assert!(is_measurement(&one));
    }

    /// A hole in the arrivals is reported, not papered over.
    ///
    /// Holding the previous sample across a silence manufactures a flat stretch
    /// of signal, and a flat stretch is energy at 0 Hz. The gaps go to the mean
    /// instead, which the DC removal then cancels exactly.
    #[test]
    fn doppler_reports_the_share_of_the_grid_it_had_to_fill() {
        let fs = 400.0f64;
        let n = 512usize;
        // Half the window is delivered; then the source stops for as long again.
        let ticks: Vec<u64> = (0..n)
            .map(|i| {
                let t = if i < n / 2 {
                    i as f64 / fs
                } else {
                    (n / 2) as f64 / fs + (i - n / 2) as f64 / fs * 8.0
                };
                (t * csiq::FTM_HZ as f64) as u64
            })
            .collect();
        let values: Vec<Complex32> = (0..n)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * 50.0 * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let s = Series {
            values,
            ticks,
            conjugate_pair: true,
        };
        let d = doppler(&s, 512, wavelength_m(5180.0), fs);
        assert_eq!(d.fs_hz, fs as f32, "the pinned axis is used verbatim");
        assert_eq!(d.fs_source, "tracked");
        assert!(
            d.gap_frac > 0.05,
            "a stalled source must show as fill, not as signal: {}",
            d.gap_frac
        );
        assert!(d.span_s > 0.0);

        // A fully delivered window has nothing to fill.
        let ticks: Vec<u64> = (0..n)
            .map(|i| (i as f64 / fs * csiq::FTM_HZ as f64) as u64)
            .collect();
        let values: Vec<Complex32> = (0..n).map(|_| Complex32::new(1.0, 0.0)).collect();
        let dense = doppler(
            &Series {
                values,
                ticks,
                conjugate_pair: true,
            },
            512,
            wavelength_m(5180.0),
            fs,
        );
        assert!(dense.gap_frac < 0.02, "gap_frac {}", dense.gap_frac);
    }

    /// The same planted tone, on an axis nobody pinned: the ladder widens the
    /// range but must not move the physics.
    #[test]
    fn doppler_recovers_a_tone_on_a_snapped_axis() {
        let fs = 400.0f64;
        let n = 1024usize;
        let ticks: Vec<u64> = (0..n)
            .map(|i| (i as f64 / fs * csiq::FTM_HZ as f64) as u64)
            .collect();
        let values: Vec<Complex32> = (0..n)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * 50.0 * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let d = doppler(
            &Series {
                values,
                ticks,
                conjugate_pair: true,
            },
            512,
            wavelength_m(5180.0),
            0.0,
        );
        assert_eq!(d.fs_hz, 500.0, "400 Hz snaps up to the 500 Hz rung");
        assert_eq!(d.fs_source, "column");
        let peak = d
            .power_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let hz = (peak as f32 - 256.0) * d.fs_hz / 512.0;
        assert!((hz - 50.0).abs() < 3.0, "peak at {hz} Hz, expected 50");
    }

    #[test]
    fn doppler_finds_a_planted_tone() {
        // 400 Hz sampling, a rotating phasor at +50 Hz.
        let fs = 400.0f64;
        let n = 512usize;
        let ticks: Vec<u64> = (0..n)
            .map(|i| (i as f64 / fs * csiq::FTM_HZ as f64) as u64)
            .collect();
        let values: Vec<Complex32> = (0..n)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * 50.0 * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let s = Series {
            values,
            ticks,
            conjugate_pair: true,
        };
        let d = doppler(&s, 512, wavelength_m(5180.0), fs);
        let peak = d
            .power_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        // Bin → Hz: (peak − nfft/2) · fs/nfft.
        let hz = (peak as f32 - 256.0) * d.fs_hz / 512.0;
        assert!((hz - 50.0).abs() < 2.0, "peak at {hz} Hz, expected 50");
        assert!(d.arrival_cv < 0.05, "uniform input must read as uniform");
        assert!((d.max_hz - 200.0).abs() < 1.0);
    }

    /// The conjugate product taken from the `i16` payload must equal the one
    /// taken from materialised `Complex32` chains.
    #[test]
    fn the_direct_doppler_reduction_matches_the_complex_one() {
        let r = Arc::new(rec(64, 2, 1, |t, c| {
            ((t as i16 * 3 - 90), (c as i16 * 40 + t as i16 - 20))
        }));
        let recs = vec![r.clone()];
        let ticks = vec![0u64];

        for pair in [None, Some(1usize)] {
            let s = doppler_series(&recs, &ticks, 0, pair);
            let a = chain(&r, 0);
            let expect = match pair {
                Some(b) => {
                    let b = chain(&r, b);
                    let sum: Complex32 = a.iter().zip(&b).map(|(x, y)| x * y.conj()).sum();
                    sum / a.len() as f32
                }
                None => a.iter().sum::<Complex32>() / a.len() as f32,
            };
            let got = s.values[0];
            assert!(
                (got.re - expect.re).abs() < 1e-2 && (got.im - expect.im).abs() < 1e-2,
                "pair {pair:?}: {got} vs {expect}"
            );
        }
    }

    #[test]
    fn bundle_width_grows_with_variability() {
        let steady: Vec<Vec<f32>> = (0..64).map(|_| vec![10.0, 10.0, 10.0]).collect();
        assert_eq!(bundle(&steady).width_db, 0.0);

        let jittery: Vec<Vec<f32>> = (0..64)
            .map(|i| {
                let v = if i % 2 == 0 { 5.0 } else { 15.0 };
                vec![v, v, v]
            })
            .collect();
        assert!(bundle(&jittery).width_db > 9.0);
    }

    /// Reusing the median's partitions must not change which values come out.
    /// Checked against a plain sort, over sizes that exercise the degenerate
    /// small-`n` cases where the three quantile indices collide.
    #[test]
    fn bundle_quantiles_match_a_full_sort() {
        for n in [1usize, 2, 3, 5, 16, 127, 128] {
            let columns: Vec<Vec<f32>> = (0..n)
                .map(|i| {
                    (0..7)
                        .map(|t| ((i * 37 + t * 11) % 91) as f32 - 40.0)
                        .collect()
                })
                .collect();
            let got = bundle(&columns);
            for t in 0..7 {
                let mut col: Vec<f32> = columns.iter().map(|c| c[t]).collect();
                col.sort_by(f32::total_cmp);
                assert_eq!(got.p05[t], col[idx(n, 0.05)], "n={n} t={t} p05");
                assert_eq!(got.p50[t], col[idx(n, 0.50)], "n={n} t={t} p50");
                assert_eq!(got.p95[t], col[idx(n, 0.95)], "n={n} t={t} p95");
            }
        }
    }

    /// The check that could never pass. A used-tone set has no DC tone, so the
    /// middle of the delivered array is data and testing it for a notch reports
    /// a failure about the channel's shape rather than about the extraction.
    #[test]
    fn the_dc_check_is_not_applicable_to_an_802_11_used_tone_set() {
        for ntone in [52u16, 56, 114, 242, 484, 996] {
            let r = rec(ntone, 2, 1, |t, _| (100 + (t % 17) as i16, 3));
            assert_eq!(
                validate(&r).dc_notch_db,
                None,
                "{ntone}-tone is a used-tone set and has no DC bin"
            );
        }
        // A contiguous grid — what the ray-traced simulator delivers — does.
        let uniform = rec(64, 2, 1, |t, _| (100 + (t % 17) as i16, 3));
        assert!(validate(&uniform).dc_notch_db.is_some());
    }

    #[test]
    fn validation_sees_a_suppressed_dc_and_a_rolled_off_edge() {
        let n = 100usize;
        let r = rec(n as u16, 2, 1, |t, c| {
            // Notch at the centre, taper at the edges, chain 1 6 dB down.
            let centre_dist = (t as f32 - n as f32 / 2.0).abs();
            let mut a = if centre_dist < 2.0 { 10.0 } else { 1000.0 };
            if t < n / 10 || t >= n - n / 10 {
                a *= 0.3;
            }
            if c == 1 {
                a *= 0.5;
            }
            (a as i16, 0)
        });
        let v = validate(&r);
        assert!(v.dimensions_ok);
        // 100 tones is not an 802.11 used-tone set, so this grid does carry DC
        // and the check is applicable.
        let notch = v.dc_notch_db.expect("a uniform grid has a DC bin to test");
        assert!(notch < -5.0, "dc notch was {notch}");
        assert!(
            v.edge_rolloff_db < -5.0,
            "rolloff was {}",
            v.edge_rolloff_db
        );
        assert!(v.chain_spread_db > 4.0, "spread was {}", v.chain_spread_db);
        assert_eq!(v.zero_fraction, 0.0);
        // Chain 1 is chain 0 scaled, so it is not bit-identical.
        assert!(v.chain_identical.unwrap() < 0.5);

        // A misparse that copies one chain across the matrix reads 0 dB of
        // mean spread — indistinguishable from well-matched antennas — but
        // 100% identical coefficients.
        let copied = rec(64, 2, 1, |t, _| ((100 + t as i16), 3));
        let cv = validate(&copied);
        assert_eq!(cv.chain_spread_db, 0.0);
        assert_eq!(cv.chain_identical, Some(1.0));
    }

    #[test]
    fn timing_percentiles_track_the_tail() {
        // 1 ms spacing with one 50 ms stall.
        let mut t = vec![0u64];
        for i in 1..1000u64 {
            let step = if i == 500 { 50_000_000 } else { 1_000_000 };
            t.push(t[i as usize - 1] + step);
        }
        let s = timing_ns(&t);
        assert!((s.p50_us - 1000.0).abs() < 1.0);
        assert!(s.max_us > 49_000.0);
        assert!(s.rate_hz > 900.0 && s.rate_hz < 1000.0);
    }

    // -- the metronome --------------------------------------------------------

    /// Arrivals from a metronomic source that drops whole slots.
    ///
    /// `keep` decides, per slot, whether the arrival happened; the clock keeps
    /// running either way, which is exactly what makes the surviving gaps
    /// integer multiples.
    fn slotted(slot_us: u64, n_slots: usize, keep: impl Fn(usize) -> bool) -> Vec<u64> {
        (0..n_slots)
            .filter(|&i| keep(i))
            .map(|i| i as u64 * slot_us * 1000)
            .collect()
    }

    /// The 5 GHz control arm: essentially every slot arrives.
    #[test]
    fn a_clean_metronome_reads_as_no_deficit() {
        let t = slotted(10_000, 3000, |_| true);
        let mut m = Metronome::default();
        metronome_into(&t, None, &mut Vec::new(), &mut m);

        assert_eq!(m.slot_source, "inferred");
        assert!((m.slot_us - 10_000.0).abs() < 1.0, "slot {}", m.slot_us);
        let commanded = m.commanded_hz.expect("an on-grid source has a rate");
        assert!((commanded - 100.0).abs() < 0.1);
        let deficit = m.deficit.expect("an on-grid source has a deficit");
        assert!(deficit < 0.01, "deficit {deficit}");
        assert!(m.quantised);
        assert_eq!(m.verdict(), "on grid");
        assert!(m.multiples[0] > 0.99, "all mass at 1x: {:?}", &m.multiples[..4]);
        assert_eq!(m.longest_run, 0);
    }

    /// The 2.4 GHz arm, reproduced: a 10 ms metronome delivering ~61 Hz, whose
    /// percentiles land on 1x, 4x and 8x the slot. The console used to report
    /// this as CV 0.71 and call the Doppler axis qualitative.
    #[test]
    fn slot_loss_is_a_deficit_not_jitter() {
        // Drop ~39% of slots in runs, so the surviving gaps are 1x..8x.
        let t = slotted(10_000, 6000, |i| !matches!(i % 13, 1 | 2 | 3 | 7 | 11));
        let mut m = Metronome::default();
        metronome_into(&t, Some(10_000.0), &mut Vec::new(), &mut m);

        assert_eq!(m.slot_source, "declared");
        assert!((m.slot_us - 10_000.0).abs() < 1.0);
        let deficit = m.deficit.expect("a declared slot always yields a deficit");
        assert!(
            (deficit - 0.385).abs() < 0.02,
            "deficit {deficit} should be ~5/13"
        );
        // Every gap is still an exact multiple: the source never jittered.
        assert!(m.quantised, "off-slot {}", m.off_slot);
        assert!(m.off_slot < 0.01);
        assert!(m.multiples[0] > 0.3, "1x share {}", m.multiples[0]);
        assert!(m.multiples[3] > 0.1, "4x share {}", m.multiples[3]);
        assert_eq!(m.longest_run, 3);
        assert_eq!(m.verdict(), "on grid");
    }

    /// The arrivals that broke this panel, replayed from the capture itself.
    ///
    /// Not a fixture written to look like the fleet — the fleet's own arrival
    /// times, lifted out of
    /// `monad01_illum-coex-03_20260823-102958-seg0003/capture.raw` and kept
    /// verbatim beside this test. Every hand-written approximation of this
    /// process came out more regular than the real one, and regularity is
    /// exactly what is under test.
    ///
    /// What the console did with it: inferred a 158 µs slot, called that a
    /// commanded 6317 Hz, and reported a 99.7% delivery deficit in red — for a
    /// transmitter configured at 250 Hz with no throttle at all, delivering
    /// 29 Hz. The largest gap was 10,246 of those "slots".
    #[test]
    fn the_real_injector_gets_no_fabricated_deficit() {
        let text = include_str!("../tests/fixtures/injector-arrivals-ch3.txt");
        let ticks: Vec<u64> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.trim().parse::<u64>().expect("a tick count per line"))
            .collect();
        assert_eq!(ticks.len(), 1743, "the fixture is the whole transmitter");

        // The DSP works in nanoseconds; the fixture is in 320 MHz ticks, as the
        // hardware counts.
        let times_ns: Vec<u64> = ticks
            .iter()
            .map(|&t| (t as f64 * 1e9 / csiq::FTM_HZ as f64) as u64)
            .collect();

        let mut m = Metronome::default();
        metronome_into(&times_ns, None, &mut Vec::new(), &mut m);

        // The delivered rate is measured, so it is always reported.
        assert!(
            (m.delivered_hz - 29.2).abs() < 1.0,
            "delivered {} Hz, the segment's own rate is 29.2",
            m.delivered_hz
        );
        // The slot is still inferred and still shown — it is a real feature of
        // the arrivals, it is just not a rate.
        assert_eq!(m.slot_source, "inferred");
        assert!(
            (m.slot_us - 158.0).abs() < 5.0,
            "slot {} µs, the burst spacing is ~158",
            m.slot_us
        );

        // And nothing is divided by it.
        assert_eq!(
            m.verdict(),
            "irregular",
            "on-slot statistics: exact {} off {}",
            m.exact_slot,
            m.off_slot
        );
        assert_eq!(m.deficit, None, "no rate was commanded to be short of");
        assert_eq!(m.commanded_hz, None);
    }

    /// The bound that does the work above, stated on its own.
    ///
    /// The residual test is a fraction of the slot, so as the multiple grows the
    /// grid becomes dense relative to the gaps and almost anything lands near
    /// it. Measured on the same capture: unbounded, 65.2% of gaps read as
    /// on-slot and 35.6% as dead on it; bounded at MAX_MULTIPLE, 51.1% and
    /// 32.9%. The second pair is the honest reading of a source whose gaps span
    /// four decades.
    #[test]
    fn a_gap_of_ten_thousand_slots_is_not_on_the_grid() {
        // A clean 10 ms metronome, with one arrival an hour late.
        let mut t: Vec<u64> = (0..200u64).map(|i| i * 10_000_000).collect();
        let last = *t.last().unwrap();
        t.push(last + 10_000_000 * 10_000);
        let mut m = Metronome::default();
        metronome_into(&t, Some(10_000.0), &mut Vec::new(), &mut m);

        // 199 gaps at 1x and one at 10,000x. The long one is off-slot, not a
        // 10,000-slot loss, so `longest_run` describes the bins that exist.
        assert!(m.off_slot > 0.0 && m.off_slot < 0.02, "off {}", m.off_slot);
        assert!(
            m.longest_run < MAX_MULTIPLE as u32,
            "longest_run {} exceeds what the histogram can hold",
            m.longest_run
        );
        // The source is still on the grid: one outlier does not unmake it.
        assert_eq!(m.verdict(), "on grid");
    }

    /// The operator's own number always survives. A declared slot means the
    /// rate was commanded, and losing every frame of it is the measurement —
    /// not a reason to withhold it.
    #[test]
    fn a_declared_slot_still_reports_a_deficit_on_an_irregular_source() {
        let mut t: Vec<u64> = Vec::new();
        let mut now = 0u64;
        for _ in 0..20 {
            for _ in 0..30 {
                t.push(now);
                now += 160_000;
            }
            now += 200_000_000;
        }
        let mut m = Metronome::default();
        metronome_into(&t, Some(10_000.0), &mut Vec::new(), &mut m);
        assert_eq!(m.slot_source, "declared");
        assert!(m.deficit.is_some(), "a commanded rate is always comparable");
        assert!((m.commanded_hz.unwrap() - 100.0).abs() < 0.1);
    }

    /// Inference survives half the slots going missing — which is more loss
    /// than the measured 2.4 GHz arm suffers.
    #[test]
    fn the_slot_is_recovered_from_heavily_thinned_arrivals() {
        let t = slotted(10_000, 4000, |i| i % 2 == 0 || i % 7 == 0);
        let mut m = Metronome::default();
        metronome_into(&t, None, &mut Vec::new(), &mut m);
        assert!((m.slot_us - 10_000.0).abs() < 100.0, "slot {}", m.slot_us);
        assert!(m.quantised);
    }

    /// A declared slot beats an inferred one, and this is why: a source that
    /// lost every other slot infers 20 ms and reports no deficit at all.
    #[test]
    fn a_declared_slot_sees_a_deficit_that_inference_hides() {
        let t = slotted(10_000, 4000, |i| i % 2 == 0);

        let mut inferred = Metronome::default();
        metronome_into(&t, None, &mut Vec::new(), &mut inferred);
        assert!((inferred.slot_us - 20_000.0).abs() < 100.0);
        assert!(
            inferred.deficit.unwrap() < 0.01,
            "inference sees a healthy 50 Hz source"
        );

        let mut declared = Metronome::default();
        metronome_into(&t, Some(10_000.0), &mut Vec::new(), &mut declared);
        let deficit = declared.deficit.unwrap();
        assert!((deficit - 0.5).abs() < 0.01, "deficit {deficit}");
    }

    /// The measured 2.4 GHz arm, whose gaps are a *mixture* — and which is why
    /// the verdict has three outcomes instead of two.
    ///
    /// Reproduced from the archive (`explore-ble-coex-24`, 2026-08-17, injector
    /// `02:6d:6f:6e:00:10`): 65.8% of gaps sit within 2% of a multiple, 86.5%
    /// within 25%, and the remainder land at an arbitrary phase — their
    /// fractional part is uniform, p25/p50/p75 = 0.40/0.52/0.63. That is not
    /// jitter around the grid, it is CSMA/CA pushing a transmission by a random
    /// backoff. Calling it `on grid` would overstate the Doppler axis; calling
    /// it `irregular` would throw away a slot that is plainly there.
    #[test]
    fn a_deferred_metronome_is_neither_on_grid_nor_irregular() {
        let slot_us = 10_000u64;
        let mut t = Vec::new();
        let mut clock = 0u64;
        for i in 0..4000usize {
            // ~30% of slots are lost outright; of what is left, about one in
            // six is deferred into the gap by a pseudo-random backoff.
            if matches!(i % 10, 1 | 4 | 8) {
                clock += slot_us * 1000;
                continue;
            }
            let deferral = if i % 6 == 0 {
                // Uniform in the slot, as the archive's fractional parts are.
                ((i * 7919) % 1000) as u64 * slot_us / 2000
            } else {
                0
            };
            t.push(clock * 1000 + deferral * 1000);
            clock += slot_us;
        }
        // The clock above advances in slots; rebuild it in nanoseconds.
        let t: Vec<u64> = t.iter().map(|v| v / 1000).collect();

        let mut m = Metronome::default();
        metronome_into(&t, Some(slot_us as f64), &mut Vec::new(), &mut m);

        assert!(m.slot_us > 0.0);
        assert!(
            m.exact_slot >= 0.40,
            "a real slot must still be visible: exact {}",
            m.exact_slot
        );
        assert!(!m.quantised, "on-slot was {}", 1.0 - m.off_slot);
        assert_eq!(m.verdict(), "deferred");
        assert!(!m.resamples_cleanly());
    }

    /// Ambient traffic is not a metronome, and must not be described as one.
    #[test]
    fn irregular_arrivals_are_refused_rather_than_fitted() {
        // Gaps spread over a decade with no mode.
        let mut t = vec![0u64];
        for i in 1..400u64 {
            let step = 1_000_000 + (i * 7919 % 40_000) as u64 * 1000;
            t.push(t[i as usize - 1] + step);
        }
        let mut m = Metronome::default();
        metronome_into(&t, None, &mut Vec::new(), &mut m);
        assert!(!m.quantised, "off-slot {}", m.off_slot);
        assert_eq!(m.verdict(), "irregular");
    }

    #[test]
    fn too_few_arrivals_produce_nothing_rather_than_a_guess() {
        let t = slotted(10_000, 5, |_| true);
        let mut m = Metronome::default();
        metronome_into(&t, None, &mut Vec::new(), &mut m);
        assert_eq!(m.slot_us, 0.0);
        assert_eq!(m.verdict(), "no slot");
    }

    // -- ratio ----------------------------------------------------------------

    /// The property the ratio exists for: a per-packet phase offset common to
    /// both chains divides out exactly, where the raw phase carries it.
    #[test]
    fn the_ratio_cancels_a_common_phase_offset() {
        let ntone = 52usize;
        let build = |offset: f32| {
            // Chain-major, imaginary-first — see `chain_slice`.
            let mut iq = Vec::new();
            for c in 0..2usize {
                for t in 0..ntone {
                    // Chain b is chain a rotated by a fixed per-chain angle and
                    // scaled, plus the common per-packet offset.
                    let ang = 0.05 * t as f32 + offset + if c == 1 { 0.7 } else { 0.0 };
                    let mag = if c == 1 { 300.0 } else { 600.0 };
                    iq.push((mag * ang.sin()) as i16); // im
                    iq.push((mag * ang.cos()) as i16); // re
                }
            }
            CsiRecord {
                ftm: 0,
                us: 0,
                unix_ts_ns: 0,
                rnf: 0,
                phy: None,
                seq: 0,
                nrx: 2,
                ntx: 1,
                ntone: ntone as u16,
                rssi: vec![-50; 2],
                src_mac: [0; 6],
                channel: 6,
                width: csiq::Width::Ht20,
                iq,
            }
        };

        let (mut a1, mut p1) = (Vec::new(), Vec::new());
        let (mut a2, mut p2) = (Vec::new(), Vec::new());
        assert!(ratio_into(&build(0.0), 0, 1, &mut a1, &mut p1));
        assert!(ratio_into(&build(1.9), 0, 1, &mut a2, &mut p2));

        // Two "packets" differing only by a common offset give the same ratio.
        for t in 0..ntone {
            assert!((p1[t] - p2[t]).abs() < 0.05, "tone {t}: {} vs {}", p1[t], p2[t]);
            assert!((a1[t] - a2[t]).abs() < 0.5, "tone {t}");
        }
        // |H_a/H_b| = 600/300 = 2, i.e. +6 dB.
        assert!((a1[10] - 6.02).abs() < 0.4, "{}", a1[10]);
    }

    #[test]
    fn the_ratio_needs_two_chains_that_exist() {
        let rec = CsiRecord {
            ftm: 0,
            us: 0,
            unix_ts_ns: 0,
            rnf: 0,
            phy: None,
            seq: 0,
            nrx: 1,
            ntx: 1,
            ntone: 8,
            rssi: vec![-50],
            src_mac: [0; 6],
            channel: 6,
            width: csiq::Width::Ht20,
            iq: vec![1; 16],
        };
        let (mut a, mut p) = (Vec::new(), Vec::new());
        assert!(!ratio_into(&rec, 0, 1, &mut a, &mut p));
        assert!(a.is_empty());
    }

    // -- tone statistics ------------------------------------------------------

    #[test]
    fn tone_stats_find_the_moving_tone_and_the_nulls() {
        let (ntone, cols) = (16usize, 64usize);
        let mut columns = vec![0.0f32; cols * ntone];
        for c in 0..cols {
            for t in 0..ntone {
                columns[c * ntone + t] = match t {
                    // A null tone: exactly the quantisation floor, always.
                    7 => 0.0,
                    // One tone that swings; the rest are steady.
                    3 => 40.0 + if c % 2 == 0 { 6.0 } else { -6.0 },
                    _ => 40.0,
                };
            }
        }
        let (mut med, mut spread, mut nulls, mut scratch) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut out = ToneStats::default();
        tone_stats_into(
            &columns, ntone, &mut med, &mut spread, &mut nulls, &mut scratch, &mut out,
        );

        assert_eq!(out.n, cols);
        assert_eq!(out.max_spread_tone, 3);
        assert!((out.max_spread_db - 6.0).abs() < 0.1, "{}", out.max_spread_db);
        assert_eq!(out.null_tones, 1);
        assert_eq!(nulls[7], 1.0);
        assert!(nulls[3] == 0.0);
        assert!((med[0] - 40.0).abs() < 0.01);
        assert!(spread[0] < 0.01, "a steady tone has no spread");
        // A nulled tone has no median at all; it is not a very weak reading.
        assert!(med[7].is_nan());
    }

    /// The nested selection must give exactly what sorting would, including
    /// on the short windows where the upper quantile indices collapse onto
    /// one another.
    #[test]
    fn timing_quantiles_match_a_full_sort() {
        for n in [3usize, 4, 10, 64, 257, 1000] {
            let times: Vec<u64> = (0..n as u64)
                .scan(0u64, |acc, i| {
                    *acc += 1_000_000 + (i * 7919 % 3000) * 1000;
                    Some(*acc)
                })
                .collect();
            let got = timing_ns(&times);
            let mut d: Vec<f32> = times
                .windows(2)
                .map(|w| (w[1] - w[0]) as f32 / 1000.0)
                .collect();
            let m = d.len();
            d.sort_by(f32::total_cmp);
            assert_eq!(got.p50_us, d[idx(m, 0.50)], "n={n} p50");
            assert_eq!(got.p95_us, d[idx(m, 0.95)], "n={n} p95");
            assert_eq!(got.p99_us, d[idx(m, 0.99)], "n={n} p99");
            assert_eq!(got.p999_us, d[idx(m, 0.999)], "n={n} p999");
            assert_eq!(got.max_us, d[m - 1], "n={n} max");
        }
    }
}
