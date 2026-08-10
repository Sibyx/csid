//! Deterministic resampling — the arithmetic behind the pre-registered gates.
//!
//! The pre-registration (`experiments/prereg/lab-session-2026-08-prereg-v2.md`
//! §2) fixes the resampling constants for the whole August session:
//!
//! > Bootstrap `B = 2000`, percentile interval, RNG `seed = 0`.
//!
//! and §2's statistical contract fixes how a criterion may be stated:
//!
//! > Every criterion is stated as **a confidence interval excluding a
//! > threshold**, never as a point estimate above a line. Point estimates are
//! > reported but decide nothing.
//!
//! Everything in this module exists to make that contract mechanical: a caller
//! gets a [`Ci`] back, never a bare `f64`, and the gate code names which *bound*
//! decides.
//!
//! ## Why a hand-rolled RNG
//!
//! `splitmix64` is four lines, has no dependency, and is bit-reproducible on
//! every platform and every build — so `--seed 0` on the bench laptop and
//! `--seed 0` six months later give the identical interval. A `rand` dependency
//! would give neither guarantee across versions.
//!
//! The bench interval is **not** the analysis of record: the frozen analysis is
//! `monad_knowledge/notebooks/python/csi_ble_calibration_eval.py` and the
//! reduction harness, which resample with numpy's generator. The two agree on
//! the estimand, the unit and the interval type, and differ only in Monte-Carlo
//! error, which at B = 2000 is O(1/√B) on the interval endpoints. The gate
//! decision is the same one; the bench merely takes it four hours earlier.

/// The pre-registered bootstrap replicate count.
pub const BOOTSTRAP_B: usize = 2000;
/// The pre-registered RNG seed.
pub const BOOTSTRAP_SEED: u64 = 0;
/// The pre-registered interval — two-sided 95%, percentile method.
pub const ALPHA: f64 = 0.05;

/// `splitmix64` — a deterministic, dependency-free, well-distributed PRNG.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform index in `0..n`, by Lemire's multiply-shift: take the high half
    /// of a 128-bit product rather than a modulo, so there is no modulo bias to
    /// argue about at any sample count this code sees.
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        ((self.next_u64() as u128 * n as u128) >> 64) as usize
    }
}

/// A point estimate with the interval that decides.
///
/// `lo`/`hi` are the 2.5th and 97.5th percentiles of the bootstrap
/// distribution. `n` is the number of *resampling units*, which is the honest
/// n — not the pooled row count.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Ci {
    pub point: f64,
    pub lo: f64,
    pub hi: f64,
    /// Resampling units (1 s bins for G1, inter-arrival gaps for G2).
    pub n: usize,
    /// Bootstrap replicates actually run.
    pub b: usize,
}

impl Ci {
    /// `12.3 [10.1, 14.8]` — the only rendering; there is deliberately no
    /// `Display` for the point estimate alone.
    pub fn render(&self, decimals: usize) -> String {
        format!(
            "{:.*} [{:.*}, {:.*}]",
            decimals, self.point, decimals, self.lo, decimals, self.hi
        )
    }
}

/// Percentile of an already-sorted slice, by linear interpolation between
/// order statistics (numpy's default `linear` method, so the bench and the
/// frozen harness read the same interval off the same replicates).
pub fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let w = pos - lo as f64;
    sorted[lo] * (1.0 - w) + sorted[hi] * w
}

pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample standard deviation (n − 1 denominator).
pub fn sd(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return f64::NAN;
    }
    let m = mean(xs);
    let ss: f64 = xs.iter().map(|x| (x - m) * (x - m)).sum();
    (ss / (xs.len() - 1) as f64).sqrt()
}

/// Coefficient of variation, σ/μ.
///
/// `None` when the mean is not positive: a CV around a zero or negative mean is
/// not a dispersion measure, and inter-arrival gaps are positive by
/// construction, so this can only fire on degenerate input.
pub fn cv(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let m = mean(xs);
    if !(m > 0.0) {
        return None;
    }
    let s = sd(xs);
    s.is_finite().then_some(s / m)
}

/// Percentile bootstrap over an arbitrary statistic of the sample.
///
/// Returns `None` when there are fewer than two units — an interval over one
/// unit is a fiction, and the gate must report UNKNOWN rather than a number.
pub fn bootstrap<F>(samples: &[f64], b: usize, seed: u64, stat: F) -> Option<Ci>
where
    F: Fn(&[f64]) -> Option<f64>,
{
    if samples.len() < 2 || b == 0 {
        return None;
    }
    let point = stat(samples)?;
    let mut rng = Rng::new(seed);
    let mut buf = vec![0.0f64; samples.len()];
    let mut reps: Vec<f64> = Vec::with_capacity(b);
    for _ in 0..b {
        for slot in buf.iter_mut() {
            *slot = samples[rng.below(samples.len())];
        }
        if let Some(v) = stat(&buf) {
            if v.is_finite() {
                reps.push(v);
            }
        }
    }
    if reps.len() < 2 {
        return None;
    }
    reps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(Ci {
        point,
        lo: percentile_sorted(&reps, ALPHA / 2.0),
        hi: percentile_sorted(&reps, 1.0 - ALPHA / 2.0),
        n: samples.len(),
        b: reps.len(),
    })
}

/// Percentile bootstrap on the mean — G1's estimator over 1 s bins.
pub fn bootstrap_mean(samples: &[f64], b: usize, seed: u64) -> Option<Ci> {
    bootstrap(samples, b, seed, |xs| {
        let m = mean(xs);
        m.is_finite().then_some(m)
    })
}

/// Percentile bootstrap on the CV — G2's estimator over inter-arrival gaps.
pub fn bootstrap_cv(samples: &[f64], b: usize, seed: u64) -> Option<Ci> {
    bootstrap(samples, b, seed, cv)
}

/// Whole 1 s bins of a monotone timestamp series, in seconds.
///
/// Returns the per-bin **count**, which at a 1 s bin width *is* the rate in Hz.
///
/// The trailing partial bin is dropped, and so is a leading partial bin: a
/// half-second of records at the end of a block is a count of ~50 that would
/// enter the bootstrap as a legitimate 50 Hz observation and drag the lower
/// bound below the floor. Dropping it costs at most one unit out of 175 (the
/// v2 full-staircase sub-block) and removes a systematic downward bias.
///
/// Timestamps need not be sorted; they are bucketed by floor division from the
/// minimum, so a reordered delivery lands in the bin it belongs to.
pub fn per_second_bins(times_s: &[f64]) -> Vec<f64> {
    if times_s.len() < 2 {
        return Vec::new();
    }
    let mut t0 = f64::INFINITY;
    let mut t1 = f64::NEG_INFINITY;
    for &t in times_s {
        if !t.is_finite() {
            continue;
        }
        t0 = t0.min(t);
        t1 = t1.max(t);
    }
    if !t0.is_finite() || !t1.is_finite() || t1 <= t0 {
        return Vec::new();
    }
    let whole = (t1 - t0).floor() as usize;
    if whole == 0 {
        return Vec::new();
    }
    let mut bins = vec![0.0f64; whole];
    for &t in times_s {
        if !t.is_finite() {
            continue;
        }
        let idx = (t - t0).floor() as usize;
        if idx < whole {
            bins[idx] += 1.0;
        }
    }
    bins
}

/// Positive, finite gaps between consecutive timestamps, in seconds.
///
/// Non-positive gaps (duplicate or reordered stamps) are dropped rather than
/// entering the CV: a zero gap inflates the CV without being a cadence defect,
/// and a negative one is not a gap at all. The count of dropped gaps is
/// returned so the caller can surface it rather than silently improving the
/// number.
pub fn gaps_s(times_s: &[f64]) -> (Vec<f64>, usize) {
    let mut out = Vec::with_capacity(times_s.len().saturating_sub(1));
    let mut dropped = 0usize;
    for w in times_s.windows(2) {
        let g = w[1] - w[0];
        if g.is_finite() && g > 0.0 {
            out.push(g);
        } else {
            dropped += 1;
        }
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rng_is_bit_reproducible_across_runs() {
        let a: Vec<usize> = (0..8).scan(Rng::new(0), |r, _| Some(r.below(1000))).collect();
        let b: Vec<usize> = (0..8).scan(Rng::new(0), |r, _| Some(r.below(1000))).collect();
        assert_eq!(a, b, "seed 0 must give the same stream every time");
        let c: Vec<usize> = (0..8).scan(Rng::new(1), |r, _| Some(r.below(1000))).collect();
        assert_ne!(a, c);
        assert!(a.iter().all(|&i| i < 1000));
    }

    #[test]
    fn percentiles_interpolate_the_way_numpy_does() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile_sorted(&xs, 0.0), 1.0);
        assert_eq!(percentile_sorted(&xs, 1.0), 4.0);
        // pos = 0.5 * 3 = 1.5 -> midway between 2 and 3.
        assert!((percentile_sorted(&xs, 0.5) - 2.5).abs() < 1e-12);
        assert!(percentile_sorted(&[], 0.5).is_nan());
        assert_eq!(percentile_sorted(&[7.0], 0.5), 7.0);
    }

    #[test]
    fn cv_is_sigma_over_mu_and_refuses_a_degenerate_mean() {
        // sd of [1,2,3,4,5] is 1.5811..., mean 3.
        let v = cv(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        assert!((v - 1.5811388300841898 / 3.0).abs() < 1e-12, "{v}");
        // A perfectly paced stream has CV 0.
        assert_eq!(cv(&[1.0; 50]), Some(0.0));
        assert_eq!(cv(&[1.0]), None);
        assert_eq!(cv(&[0.0, 0.0, 0.0]), None);
    }

    #[test]
    fn the_bootstrap_brackets_the_point_estimate_and_is_reproducible() {
        let xs: Vec<f64> = (0..200).map(|i| 100.0 + (i % 7) as f64).collect();
        let a = bootstrap_mean(&xs, 2000, 0).unwrap();
        let b = bootstrap_mean(&xs, 2000, 0).unwrap();
        assert_eq!(a, b, "same seed, same interval");
        assert!(a.lo < a.point && a.point < a.hi, "{a:?}");
        assert_eq!(a.n, 200);
        assert_eq!(a.b, 2000);
        // Sanity: the mean of the sample is 103.0 within a whisker.
        assert!((a.point - 103.0).abs() < 0.1, "{a:?}");
    }

    /// The whole point of the CI contract: a sample whose *point* estimate
    /// clears the line but whose lower bound does not must not pass.
    #[test]
    fn a_wide_sample_can_clear_the_line_on_the_point_and_miss_on_the_bound() {
        // Mean 105, but hugely dispersed: 60 and 150 alternating.
        let xs: Vec<f64> = (0..40).map(|i| if i % 2 == 0 { 60.0 } else { 150.0 }).collect();
        let ci = bootstrap_mean(&xs, 2000, 0).unwrap();
        assert!(ci.point > 100.0, "point estimate clears the floor: {ci:?}");
        assert!(ci.lo < 100.0, "but the lower bound does not: {ci:?}");
    }

    #[test]
    fn an_interval_over_fewer_than_two_units_is_refused() {
        assert!(bootstrap_mean(&[], 2000, 0).is_none());
        assert!(bootstrap_mean(&[100.0], 2000, 0).is_none());
        assert!(bootstrap_cv(&[0.01], 2000, 0).is_none());
        assert!(bootstrap_mean(&[1.0, 2.0], 0, 0).is_none());
    }

    #[test]
    fn binning_drops_the_partial_tail_rather_than_reporting_it_as_a_slow_second() {
        // 3.5 s of a clean 10 Hz stream: 35 stamps, 3 whole bins of 10.
        let times: Vec<f64> = (0..35).map(|i| i as f64 * 0.1).collect();
        let bins = per_second_bins(&times);
        assert_eq!(bins, vec![10.0, 10.0, 10.0], "the half-second tail is dropped");

        // Under one whole second there is nothing to bin.
        assert!(per_second_bins(&[0.0, 0.1, 0.2]).is_empty());
        assert!(per_second_bins(&[]).is_empty());
        assert!(per_second_bins(&[1.0]).is_empty());
    }

    #[test]
    fn binning_tolerates_out_of_order_delivery() {
        let mut times: Vec<f64> = (0..20).map(|i| i as f64 * 0.1).collect();
        times.swap(3, 9);
        assert_eq!(per_second_bins(&times), vec![10.0]);
    }

    #[test]
    fn gaps_drop_non_positive_steps_and_count_them() {
        let (g, dropped) = gaps_s(&[0.0, 0.1, 0.1, 0.05, 0.3]);
        // 0.1 (ok), 0.0 (dropped), -0.05 (dropped), 0.25 (ok)
        assert_eq!(dropped, 2);
        assert_eq!(g.len(), 2);
        assert!((g[0] - 0.1).abs() < 1e-12);
        assert!((g[1] - 0.25).abs() < 1e-12);
    }

    /// The G2 baseline from the readiness audit: real captures produced gap CVs
    /// of 1.00, 1.47, 2.60 and 50.96. A bursty stream must land far above the
    /// 0.5 ceiling, and a paced one far below it, on this estimator.
    #[test]
    fn the_cv_estimator_separates_a_paced_stream_from_a_bursty_one() {
        let paced: Vec<f64> = (0..500).map(|i| 0.01 + (i % 3) as f64 * 0.0001).collect();
        let paced_ci = bootstrap_cv(&paced, 2000, 0).unwrap();
        assert!(paced_ci.hi < 0.5, "paced stream must pass: {paced_ci:?}");

        // Bursts: 100 tight arrivals then a long silence, repeated.
        let bursty: Vec<f64> = (0..500)
            .map(|i| if i % 100 == 0 { 2.0 } else { 0.001 })
            .collect();
        let bursty_ci = bootstrap_cv(&bursty, 2000, 0).unwrap();
        assert!(bursty_ci.lo > 0.5, "bursty stream must fail: {bursty_ci:?}");
    }

    #[test]
    fn a_ci_renders_with_its_interval_never_bare() {
        let ci = Ci {
            point: 122.53,
            lo: 118.11,
            hi: 126.94,
            n: 175,
            b: 2000,
        };
        assert_eq!(ci.render(1), "122.5 [118.1, 126.9]");
        assert_eq!(ci.render(2), "122.53 [118.11, 126.94]");
    }
}
