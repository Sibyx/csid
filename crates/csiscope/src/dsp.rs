//! The signal-processing behind every view.
//!
//! Each function here corresponds to a representation the Wi-Fi sensing
//! literature actually uses, and the comments name which one, because "plot the
//! CSI" is under-specified: raw phase is meaningless without sanitisation, raw
//! amplitude is AGC-normalised, and a Doppler axis is a lie unless you say what
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
//! **Amplitude is AGC-normalised** (see `csid caps`): `|H|` carries channel
//! *shape*, not absolute scale. Every amplitude view is therefore relative, and
//! the absolute anchor is the per-chain RSSI reported alongside it.
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

/// Taps this far below the strongest one are excluded from the RMS delay
/// spread — see [`cir`].
const RMS_THRESHOLD_DB: f32 = -20.0;

// -- logarithm ----------------------------------------------------------------

/// `log10(2)`, the bridge from a binary exponent to decibels.
const LOG10_2: f32 = 0.301_029_995_7;

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
                    .map(|i| {
                        0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / denom).cos()
                    })
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

/// Magnitude in dB (relative — the AGC has already normalised absolute scale).
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
#[derive(Debug, Clone, Default)]
pub struct Cir {
    /// Magnitude in dB, normalised so the strongest tap is 0 dB. Tap `i` sits
    /// at delay `i * bin_ns`; the axis is not materialised because it is
    /// uniform and the consumer can build it from one scalar.
    pub mag_db: Vec<f32>,
    /// Delay resolution (bin spacing) in nanoseconds.
    pub bin_ns: f32,
    /// Index of the strongest tap.
    pub peak_bin: usize,
    /// RMS delay spread over the returned taps, in nanoseconds — a one-number
    /// summary of how multipath-rich the link is.
    pub rms_delay_ns: f32,
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
        cir_into(&mut t.borrow_mut(), h, spacing_hz, nfft, taps, &mut Vec::new(), &mut out)
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
    out.peak_bin = 0;
    out.rms_delay_ns = 0.0;

    let n = h.len();
    if n < 4 || nfft < n {
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

    let taps = taps.min(nfft);
    // Work in power throughout: the peak search, the threshold and the second
    // moment are all monotone in it, and dB comes out of the same logarithm
    // the amplitude path uses.
    let mut peak = 0usize;
    let mut peak_power = 0.0f32;
    out.mag_db.resize(taps, 0.0);
    for (i, c) in buf[..taps].iter().enumerate() {
        let p = c.re * c.re + c.im * c.im;
        out.mag_db[i] = p;
        if p > peak_power {
            peak_power = p;
            peak = i;
        }
    }
    let peak_power = peak_power.max(POWER_FLOOR);

    let bin_s = 1.0 / (nfft as f64 * spacing_hz);
    out.bin_ns = (bin_s * 1e9) as f32;
    out.peak_bin = peak;

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
    for v in out.mag_db.iter_mut() {
        *v = db_from_power(*v) - db_from_power(peak_power);
    }
}

// -- Doppler ------------------------------------------------------------------

/// One column of a Doppler spectrogram, plus the axis it is honest about.
#[derive(Debug, Clone, Default)]
pub struct Doppler {
    /// Power in dB relative to the column peak, FFT-shifted so the centre bin
    /// is 0 Hz and the axis runs `[−fs/2, +fs/2)`.
    pub power_db: Vec<f32>,
    /// Effective sample rate used for the frequency axis, in Hz.
    pub fs_hz: f32,
    /// Maximum unambiguous Doppler shift (`fs/2`), in Hz.
    pub max_hz: f32,
    /// Corresponding maximum unambiguous radial speed, in m/s
    /// (`v = λ·f_D/2`, so `v_max = λ·fs/4`).
    pub max_speed_ms: f32,
    /// Coefficient of variation of the packet inter-arrival time. The Doppler
    /// axis assumes uniform sampling; ambient traffic is not uniform, so this
    /// is how much to distrust the axis. Below ~0.3 the resampling is benign.
    pub arrival_cv: f32,
    /// Whether a second chain was available for the conjugate-multiplication
    /// step. Without it the series still carries CFO and the spectrum smears.
    pub conjugate_pair: bool,
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
        let b = chain_b.and_then(|b| chain_slice(rec, b)).filter(|b| b.len() == a.len());

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
pub fn doppler(series: &Series, nfft: usize, wavelength_m: f32) -> Doppler {
    let mut out = Doppler::default();
    LOCAL_TRANSFORMS.with(|t| {
        doppler_into(
            &mut t.borrow_mut(),
            series,
            nfft,
            wavelength_m,
            &mut Vec::new(),
            &mut out,
        )
    });
    out
}

/// [`doppler`], with cached plans and caller-owned buffers.
pub fn doppler_into(
    tf: &mut Transforms,
    series: &Series,
    nfft: usize,
    wavelength_m: f32,
    buf: &mut Vec<Complex32>,
    out: &mut Doppler,
) {
    out.power_db.clear();
    out.fs_hz = 0.0;
    out.max_hz = 0.0;
    out.max_speed_ms = 0.0;
    out.arrival_cv = 0.0;
    out.conjugate_pair = series.conjugate_pair;

    let n = series.values.len();
    if n < 8 || nfft < 8 {
        return;
    }

    let t0 = series.ticks[0];
    let t1 = *series.ticks.last().unwrap();
    let span_s = csiq::ftm_to_seconds(t1.saturating_sub(t0));
    if span_s <= 0.0 {
        return;
    }
    let fs = (n - 1) as f64 / span_s;

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

    // Nearest-neighbour resample onto a uniform grid of `nfft` points.
    buf.clear();
    buf.resize(nfft, Complex32::new(0.0, 0.0));
    let mut cursor = 0usize;
    for (i, slot) in buf.iter_mut().enumerate() {
        let want = t0 as f64 + (t1 - t0) as f64 * (i as f64 / (nfft - 1) as f64);
        while cursor + 1 < n && (series.ticks[cursor + 1] as f64) < want {
            cursor += 1;
        }
        *slot = series.values[cursor];
    }

    // Remove the static component: the LoS and every unmoving reflector sit at
    // 0 Hz and would otherwise dominate by tens of dB.
    let mean: Complex32 = buf.iter().sum::<Complex32>() / nfft as f32;
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
    out.max_hz = max_hz;
    // v = λ·f_D/2 for a reflected (two-way) path.
    out.max_speed_ms = wavelength_m * max_hz / 2.0;
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
    pub dc_notch_db: f32,
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

    let centre_half = (n / 40).max(1); // ±2.5% of the band
    let mid = n / 2;
    let centre = &a[mid.saturating_sub(centre_half)..(mid + centre_half).min(n)];
    v.dc_notch_db = centre.iter().cloned().fold(f32::INFINITY, f32::min) - whole;

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
        let expect = (tau / (1.0 / (512.0 * spacing))).round() as usize;
        assert!(
            c.peak_bin.abs_diff(expect) <= 1,
            "peak at bin {} expected ~{expect}",
            c.peak_bin
        );
        assert!(c.mag_db[c.peak_bin] == 0.0, "peak must normalise to 0 dB");
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
        let d = doppler(&s, 512, wavelength_m(5180.0));
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
        assert!(v.dc_notch_db < -5.0, "dc notch was {}", v.dc_notch_db);
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
