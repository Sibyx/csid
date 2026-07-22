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

use std::sync::Arc;

use csiq::{CsiRecord, Modulation};
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

/// Floor for log magnitude: one least-significant bit of the `i16` I/Q grid.
///
/// Real captures are full of exact zeros — 802.11 nulls the DC and guard
/// subcarriers, and the driver delivers those as `(0, 0)`. With an arbitrary
/// epsilon floor they became −120 dB spikes that dominated every automatic
/// axis and colour range in the console. One LSB is both finite and *true*:
/// the format cannot represent anything smaller, so 0 dB means "at or below
/// the quantisation floor", which is exactly what a null subcarrier is.
const MAG_FLOOR: f32 = 1.0;

/// Taps this far below the strongest one are excluded from the RMS delay
/// spread — see [`cir`].
const RMS_THRESHOLD_DB: f32 = -20.0;

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

/// Extract one chain's channel frequency response, tone-major.
///
/// The I/Q payload is interleaved `i16`, row-major over `[tone][chain]`
/// (CSIQ v1 §CSI matrix). Returns an empty vector if the record's payload does
/// not match its declared dimensions.
pub fn chain(rec: &CsiRecord, chain: usize) -> Vec<Complex32> {
    let g = Geometry::of(rec);
    let nc = g.nchain();
    if nc == 0 || chain >= nc || !g.matches(rec) {
        return Vec::new();
    }
    // Storage is chain-major (nrx*ntx contiguous blocks of ntone), and each
    // coefficient is imaginary-first. Both differ from the obvious reading —
    // see docs/CSIQ-format-v1.md, "The CSI matrix".
    (0..g.ntone)
        .map(|t| {
            let i = 2 * (chain * g.ntone + t);
            Complex32::new(rec.iq[i + 1] as f32, rec.iq[i] as f32)
        })
        .collect()
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

// -- amplitude ----------------------------------------------------------------

/// Magnitude in dB (relative — the AGC has already normalised absolute scale).
pub fn amp_db(h: &[Complex32]) -> Vec<f32> {
    h.iter()
        .map(|c| 20.0 * c.norm().max(MAG_FLOOR).log10())
        .collect()
}

/// The 5th/50th/95th percentile of `|H(f)|` in dB across a window of records —
/// the "CSI bundle" of Choi et al.
///
/// The *width* of the bundle (p95 − p05, averaged over subcarriers) is the
/// discriminative feature in that line of work: a static channel gives a tight
/// bundle, occupancy and motion widen it. It is returned as
/// [`Bundle::width_db`] so the console can show it as a single number.
///
/// `columns` is one `amp_db` vector per record, all the same length.
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

pub fn bundle(columns: &[Vec<f32>]) -> Bundle {
    let Some(ntone) = columns.first().map(|c| c.len()) else {
        return Bundle::default();
    };
    let usable: Vec<&Vec<f32>> = columns.iter().filter(|c| c.len() == ntone).collect();
    if usable.is_empty() {
        return Bundle::default();
    }

    let n = usable.len();
    let mut p05 = vec![0.0f32; ntone];
    let mut p50 = vec![0.0f32; ntone];
    let mut p95 = vec![0.0f32; ntone];
    let mut scratch = vec![0.0f32; n];
    let mut width_sum = 0.0f64;

    for t in 0..ntone {
        for (i, col) in usable.iter().enumerate() {
            scratch[i] = col[t];
        }
        // Selection, not a full sort: three O(n) passes beat n·log n at 996
        // tones × 20 frames a second on a Pi 5.
        let (a, b, c) = (idx(n, 0.05), idx(n, 0.50), idx(n, 0.95));
        scratch.select_nth_unstable_by(a, f32::total_cmp);
        p05[t] = scratch[a];
        scratch.select_nth_unstable_by(b, f32::total_cmp);
        p50[t] = scratch[b];
        scratch.select_nth_unstable_by(c, f32::total_cmp);
        p95[t] = scratch[c];
        width_sum += (p95[t] - p05[t]) as f64;
    }

    Bundle {
        width_db: (width_sum / ntone as f64) as f32,
        p05,
        p50,
        p95,
        n,
    }
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
    h.iter().map(|c| c.im.atan2(c.re)).collect()
}

/// Unwrap a phase sequence along the subcarrier axis.
pub fn unwrap(phase: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(phase.len());
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
    out
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
    let n = unwrapped.len();
    if n < 2 {
        return (unwrapped.to_vec(), Detrend::default());
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

    let out = unwrapped
        .iter()
        .enumerate()
        .map(|(i, &y)| y - (slope * i as f64 + intercept) as f32)
        .collect();

    // φ(k) = −2π·τ·k·Δf  ⇒  τ = −slope / (2π·Δf).
    let tau_ns = (-slope / (2.0 * std::f64::consts::PI * spacing_hz) * 1e9) as f32;

    (
        out,
        Detrend {
            slope: slope as f32,
            intercept: intercept as f32,
            tau_ns,
        },
    )
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
/// reorders or interleaves tones would smear this plot while leaving every
/// other view intact — which is itself a useful diagnostic.
pub fn cir(h: &[Complex32], spacing_hz: f64, nfft: usize, taps: usize) -> Cir {
    let n = h.len();
    if n < 4 || nfft < n {
        return Cir::default();
    }
    let mut buf = vec![Complex32::new(0.0, 0.0); nfft];
    // Rotate so the band centre lands on FFT bin 0: the tone grid spans
    // [−BW/2, +BW/2] but the FFT expects DC first.
    let half = n / 2;
    for (i, &c) in h.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
        let k = (i + nfft - half) % nfft;
        buf[k] = c * w;
    }

    FftPlanner::new().plan_fft_inverse(nfft).process(&mut buf);

    let taps = taps.min(nfft);
    let mags: Vec<f32> = buf[..taps].iter().map(|c| c.norm()).collect();
    let peak = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let peak_mag = mags[peak].max(MAG_FLOOR);

    let bin_s = 1.0 / (nfft as f64 * spacing_hz);
    let bin_ns = (bin_s * 1e9) as f32;

    // RMS delay spread: the power-weighted standard deviation of tap delay,
    // measured from the power-weighted mean (not from the peak).
    //
    // Taps below `RMS_THRESHOLD_DB` are excluded. Without a threshold the
    // noise floor — which is spread uniformly across every returned tap —
    // dominates the second moment, and the reported spread becomes a function
    // of how many taps were requested rather than of the channel. Thresholding
    // at −20 dB is the standard convention for this statistic.
    let cutoff = peak_mag * 10f32.powf(RMS_THRESHOLD_DB / 20.0);
    let power: Vec<f64> = mags
        .iter()
        .map(|&m| if m >= cutoff { (m as f64).powi(2) } else { 0.0 })
        .collect();
    let total: f64 = power.iter().sum();
    let rms_delay_ns = if total > 0.0 {
        let mean_tau: f64 = power
            .iter()
            .enumerate()
            .map(|(i, p)| i as f64 * p)
            .sum::<f64>()
            / total;
        let var: f64 = power
            .iter()
            .enumerate()
            .map(|(i, p)| (i as f64 - mean_tau).powi(2) * p)
            .sum::<f64>()
            / total;
        (var.sqrt() * bin_s * 1e9) as f32
    } else {
        0.0
    };

    Cir {
        mag_db: mags
            .iter()
            .map(|&m| 20.0 * (m.max(MAG_FLOOR) / peak_mag).log10())
            .collect(),
        bin_ns,
        peak_bin: peak,
        rms_delay_ns,
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
    let mut values = Vec::with_capacity(samples.len());
    let mut out_ticks = Vec::with_capacity(samples.len());
    let mut used_pair = false;

    for (rec, &tick) in samples.iter().zip(ticks.iter()) {
        let a = chain(rec, chain_a);
        if a.is_empty() {
            continue;
        }
        let v = match chain_b.map(|b| chain(rec, b)) {
            Some(b) if b.len() == a.len() => {
                used_pair = true;
                let sum: Complex32 = a.iter().zip(b.iter()).map(|(x, y)| x * y.conj()).sum();
                sum / a.len() as f32
            }
            _ => {
                let sum: Complex32 = a.iter().sum();
                sum / a.len() as f32
            }
        };
        values.push(v);
        out_ticks.push(tick);
    }

    Series {
        values,
        ticks: out_ticks,
        conjugate_pair: used_pair,
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
    let n = series.values.len();
    if n < 8 || nfft < 8 {
        return Doppler::default();
    }

    let t0 = series.ticks[0];
    let t1 = *series.ticks.last().unwrap();
    let span_s = csiq::ftm_to_seconds(t1.saturating_sub(t0));
    if span_s <= 0.0 {
        return Doppler::default();
    }
    let fs = (n - 1) as f64 / span_s;

    // Inter-arrival dispersion: how far from uniform the sampling really was.
    let mut deltas: Vec<f64> = Vec::with_capacity(n - 1);
    for w in series.ticks.windows(2) {
        deltas.push(csiq::ftm_to_seconds(w[1].saturating_sub(w[0])));
    }
    let mean_d = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let var_d = deltas.iter().map(|d| (d - mean_d).powi(2)).sum::<f64>() / deltas.len() as f64;
    let cv = if mean_d > 0.0 {
        (var_d.sqrt() / mean_d) as f32
    } else {
        0.0
    };

    // Nearest-neighbour resample onto a uniform grid of `nfft` points.
    let mut buf = vec![Complex32::new(0.0, 0.0); nfft];
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
    for (i, v) in buf.iter_mut().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (nfft - 1) as f32).cos();
        *v = (*v - mean) * w;
    }

    FftPlanner::new().plan_fft_forward(nfft).process(&mut buf);

    // FFT-shift: negative frequencies first, so the axis reads −fs/2 … +fs/2.
    let half = nfft / 2;
    let mut power: Vec<f32> = Vec::with_capacity(nfft);
    power.extend(buf[half..].iter().map(|c| c.norm()));
    power.extend(buf[..half].iter().map(|c| c.norm()));
    let peak = power.iter().cloned().fold(MAG_FLOOR, f32::max);

    let max_hz = (fs / 2.0) as f32;
    Doppler {
        power_db: power
            .iter()
            .map(|&m| 20.0 * (m.max(MAG_FLOOR) / peak).log10())
            .collect(),
        fs_hz: fs as f32,
        max_hz,
        // v = λ·f_D/2 for a reflected (two-way) path.
        max_speed_ms: wavelength_m * max_hz / 2.0,
        arrival_cv: cv,
        conjugate_pair: series.conjugate_pair,
    }
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
    let g = Geometry::of(rec);
    let mut v = Validation {
        dimensions_ok: g.matches(rec),
        ..Default::default()
    };
    if !v.dimensions_ok || g.ntone < 20 {
        return v;
    }

    let zeros = rec
        .iq
        .chunks_exact(2)
        .filter(|c| c[0] == 0 && c[1] == 0)
        .count();
    v.zero_fraction = zeros as f32 / (rec.iq.len() / 2).max(1) as f32;

    let per_chain: Vec<Vec<f32>> = (0..g.nchain())
        .map(|c| amp_db(&chain(rec, c)))
        .filter(|a| !a.is_empty())
        .collect();
    if per_chain.is_empty() {
        return v;
    }

    let means: Vec<f32> = per_chain
        .iter()
        .map(|a| a.iter().sum::<f32>() / a.len() as f32)
        .collect();
    let hi = means.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let lo = means.iter().cloned().fold(f32::INFINITY, f32::min);
    v.chain_spread_db = hi - lo;

    if g.nchain() >= 2 {
        let a = chain(rec, 0);
        let b = chain(rec, 1);
        if a.len() == b.len() && !a.is_empty() {
            let same = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
            v.chain_identical = Some(same as f32 / a.len() as f32);
        }
    }

    // Shape checks run on chain 0; the others differ only by AGC and geometry.
    let a = &per_chain[0];
    let n = a.len();
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
    let reference: Vec<f32> = shoulders
        .iter()
        .flat_map(|&(lo, hi)| a[lo.min(n)..hi.min(n)].iter().copied())
        .collect();
    v.edge_rolloff_db = edges - mean_of(&reference);

    v
}

fn mean_of(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    s.iter().sum::<f32>() / s.len() as f32
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
    if times_ns.len() < 3 {
        return Timing::default();
    }
    let mut d: Vec<f32> = times_ns
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]) as f32 / 1000.0)
        .collect();
    let n = d.len();
    let mean = d.iter().sum::<f32>() / n as f32;
    let var = d.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32;
    d.sort_by(f32::total_cmp);

    let q = |p: f64| d[idx(n, p)];
    Timing {
        n: times_ns.len(),
        rate_hz: if mean > 0.0 { 1e6 / mean } else { 0.0 },
        mean_us: mean,
        p50_us: q(0.50),
        p95_us: q(0.95),
        p99_us: q(0.99),
        p999_us: q(0.999),
        max_us: d[n - 1],
        cv: if mean > 0.0 { var.sqrt() / mean } else { 0.0 },
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
}
