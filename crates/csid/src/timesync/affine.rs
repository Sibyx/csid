//! Phone → fleet clock transfer: the affine fit, and what it can honestly claim.
//!
//! The app's `ClockGate` registers the join in the pre-registration §3.5 as
//!
//! ```text
//! unix_ts_ns ≈ a · mono_ns + b
//! ```
//!
//! and gates a fold whose residual exceeds 0.25 s out of T3. Until now that fit
//! was estimated from a handful of four-timestamp bursts. Every app datagram on
//! the air already carries `(mono_ns, seq)`, and a fleet node stamps its own
//! `unix_ts_ns` on receipt — so an illuminated session yields **thousands of
//! `(mono, unix)` pairs** instead of a few dozen, for free.
//!
//! ## What one-way delay does and does not cost you
//!
//! Every observation is
//!
//! ```text
//! y_i = a · x_i + b + d_i ,     d_i ≥ d_min > 0
//! ```
//!
//! where `d_i` is the one-way transit (app userspace → radio → air → driver →
//! our stamp). Two very different consequences:
//!
//! * **The slope is recoverable cleanly.** `d_i` is bounded and does not grow
//!   with time, so over a session of length `T` its contribution to the slope
//!   is `O(spread(d)/T)`. At a 2 ms delay spread over 30 minutes that is
//!   **1.1 ppb** — three orders of magnitude below the ~10–50 ppm skew of a
//!   consumer crystal. The ppm figure this module reports is real.
//!
//! * **The offset is biased late by `d_min`, and one-way data cannot remove
//!   it.** There is no return path, so `d_min` is not observable here. What we
//!   *can* do is refuse to pretend: the fit is placed on the **lower envelope**
//!   of the points (so the reported `b` is the largest intercept consistent
//!   with every `d_i ≥ 0`), which makes the true offset lie in
//!   `[b − d_floor, b]` with `d_floor` an upper bound on the minimum one-way
//!   delay. That interval is reported, not a bare number.
//!
//! ### Is the bias big enough to matter at 250 ms?
//!
//! No, and by a wide margin. Measured on this fleet: management-path RTT with
//! `wlan0` power-save off is **10.6 ms** (`roles/common`), so a one-way floor
//! is ~5 ms; the injector's own 200-byte frame at 6 Mbps legacy OFDM occupies
//! ~290 µs of air. [`D_FLOOR_DEFAULT_NS`] is therefore 5 ms — **2% of the G4b
//! budget**. If the budget were 1 ms this term would dominate and a return-path
//! exchange (`collectord`'s four-timestamp protocol) would be mandatory rather
//! than complementary.
//!
//! ## Why not least squares
//!
//! OLS minimises squared residuals around the *mean* of `d`, so a delay
//! distribution with a long right tail (which is what a contended channel
//! produces) drags the line up and, worse, tilts it whenever the tail is not
//! stationary. The estimator here is the standard one-way pair: a robust
//! **Theil–Sen** slope over a lower-envelope subset, then the intercept placed
//! exactly on the envelope. It is what Moon–Skelly–Towsley's linear-programming
//! clock estimator does, arrived at by a route that is easy to unit-test.

use serde::{Deserialize, Serialize};

/// Upper bound on the minimum one-way delay, nanoseconds. See the module docs
/// for where 5 ms comes from.
pub const D_FLOOR_DEFAULT_NS: u64 = 5_000_000;

/// Fewest observations that will produce a fit. Below this the slope is noise
/// dressed as a number.
pub const MIN_SAMPLES: usize = 16;

/// Points beyond this many are thinned before the O(m²) Theil–Sen pass.
const THEIL_SEN_CAP: usize = 200;

/// One observation of the phone's clocks by a fleet node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// The payload's `monotonicNanos`.
    pub mono_ns: u64,
    /// The receiving node's `unix_ts_ns` at delivery.
    pub rx_unix_ns: u64,
    /// The payload's `wallMillis`, promoted to nanoseconds. The app's own
    /// contract calls it "recorded, never load-bearing" — it is not used for
    /// the transform, only for [`AffineFit::wall_offset_ns`].
    pub tx_wall_ns: Option<u64>,
}

/// The fitted transform, with its honest error terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AffineFit {
    /// Whose clock this is — the app's session UUID.
    pub tx_id: String,
    pub n: usize,
    /// Span of the phone clock covered, seconds. The slope's precision is
    /// proportional to this, so it is reported next to it.
    pub span_s: f64,
    /// `a` in `unix ≈ a·mono + b`.
    pub slope: f64,
    /// `(a − 1) · 1e6` — the phone's frequency error against the fleet, ppm.
    /// **This is the number one-way data recovers cleanly.**
    pub slope_ppm: f64,
    /// `b`, the lower-envelope intercept, nanoseconds. This is the second
    /// parameter `ClockGate` needs; it is not a clock "offset", because
    /// `mono_ns` has an arbitrary origin.
    pub intercept_ns: i64,
    /// **Phone wallclock minus fleet wallclock**, nanoseconds, +ve = phone
    /// ahead. This is the number an operator means by "the offset". `None` when
    /// the app sent no wallclock.
    ///
    /// Biased **early** by the minimum one-way delay: the true offset lies in
    /// `[wall_offset_ns, wall_offset_ns + offset_bias_ns]`.
    pub wall_offset_ns: Option<i64>,
    /// Samples that carried a wallclock, so a `wall_offset_ns` from three
    /// packets is not mistaken for one from three thousand.
    pub wall_n: usize,
    /// Upper bound on the one-way delay floor, and therefore on the offset bias.
    pub offset_bias_ns: u64,
    /// The origin the fit is evaluated around. Both clocks are ~1e18 ns and
    /// f64 quantises there at 256 ns, so every evaluation differences against
    /// this first — see [`AffineFit::to_unix_ns`].
    pub origin_mono_ns: u64,
    pub origin_unix_ns: i64,
    /// Residuals `y − (a·x + b)`. Non-negative by construction; this is the
    /// observed one-way delay distribution relative to its own floor.
    pub residual_p50_ns: u64,
    pub residual_p95_ns: u64,
    pub residual_max_ns: u64,
}

impl AffineFit {
    /// Map a phone monotonic stamp onto the fleet timeline.
    ///
    /// Evaluated with the multiplication done on the *offset* from the fit's
    /// own origin, because `a · 1.8e18` in f64 quantises at 256 ns and this
    /// transform is spent against a 250 ms budget with nanosecond arithmetic
    /// either side of it.
    pub fn to_unix_ns(&self, mono_ns: u64) -> i64 {
        let d = mono_ns as i128 - self.origin_mono_ns as i128;
        self.origin_unix_ns + (self.slope * d as f64).round() as i64
    }

    /// The G4b-relevant question: is the *residual* of this join inside the
    /// budget? The registered gate is on the residual, not on the offset.
    pub fn residual_within(&self, budget_ns: u64) -> bool {
        self.residual_p95_ns <= budget_ns
    }

    pub fn render(&self) -> String {
        let offset = match self.wall_offset_ns {
            Some(o) => format!(
                "{lo:+.3} .. {hi:+.3} ms (phone wallclock vs fleet, n = {n}; one-way floor \
                 <= {bias:.1} ms biases it early)",
                lo = o as f64 / 1e6,
                hi = (o + self.offset_bias_ns as i64) as f64 / 1e6,
                n = self.wall_n,
                bias = self.offset_bias_ns as f64 / 1e6,
            ),
            None => "not reported (the app sent no wallclock)".to_string(),
        };
        format!(
            "phone {tx}: {n} packets over {span:.1} s\n  \
             slope   : {ppm:+.2} ppm (a = {a:.9}) — recoverable cleanly from one-way data\n  \
             offset  : {offset}\n  \
             residual: p50 {p50:.3} ms, p95 {p95:.3} ms, max {max:.3} ms",
            tx = self.tx_id,
            n = self.n,
            span = self.span_s,
            ppm = self.slope_ppm,
            a = self.slope,
            p50 = self.residual_p50_ns as f64 / 1e6,
            p95 = self.residual_p95_ns as f64 / 1e6,
            max = self.residual_max_ns as f64 / 1e6,
        )
    }
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn median_f64(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Fit `unix ≈ a·mono + b` from one-way observations.
///
/// `d_floor_ns` is the caller's upper bound on the minimum one-way delay; it
/// does not change the fit, only the reported offset interval. Pass
/// [`D_FLOOR_DEFAULT_NS`] unless a measured value is available.
///
/// Returns `None` when there are too few samples, or when they span no time —
/// a slope from a single instant is not a slope.
pub fn fit(tx_id: &str, samples: &[Sample], d_floor_ns: u64) -> Option<AffineFit> {
    if samples.len() < MIN_SAMPLES {
        return None;
    }
    // Difference in INTEGER space before touching f64. Raw epoch nanoseconds
    // are ~1.8e18 and f64 has a 53-bit mantissa, so `x as f64 - y as f64`
    // quantises at 256 ns — one thousandth of the G4b budget thrown away
    // before the fit even starts, and enough to make a noiseless stream fail
    // to fit exactly.
    let x0 = samples.iter().map(|s| s.mono_ns).min()?;
    let y0 = samples.iter().map(|s| s.rx_unix_ns).min()?;
    let pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (
            (s.mono_ns - x0) as f64,
            (s.rx_unix_ns - y0) as f64,
        ))
        .collect();

    let x_min = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let x_max = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let span = x_max - x_min;
    if !(span > 0.0) {
        return None;
    }

    // 1. Preliminary least squares, used only to define "low residual".
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let a0 = sxy / sxx;
    let b0 = my - a0 * mx;

    // 2. Lower envelope: bin by x, keep the least-delayed point per bin. This
    //    is the "minimum-delay filter" that makes a one-way stream usable — the
    //    fastest transit in each bin is the one with the least queueing in it.
    let bins = THEIL_SEN_CAP.min(pts.len());
    let width = span / bins as f64;
    let mut best: Vec<Option<(f64, f64, f64)>> = vec![None; bins]; // (x, y, residual)
    for &(x, y) in &pts {
        let k = (((x - x_min) / width) as usize).min(bins - 1);
        let r = y - (a0 * x + b0);
        if best[k].is_none_or(|(_, _, br)| r < br) {
            best[k] = Some((x, y, r));
        }
    }
    let envelope: Vec<(f64, f64)> = best.into_iter().flatten().map(|(x, y, _)| (x, y)).collect();
    if envelope.len() < 2 {
        return None;
    }

    // 3. Theil–Sen slope over the envelope: the median of all pairwise slopes.
    //    Immune to the tail that OLS would chase.
    let mut slopes: Vec<f64> = Vec::with_capacity(envelope.len() * envelope.len() / 2);
    for i in 0..envelope.len() {
        for j in (i + 1)..envelope.len() {
            let dx = envelope[j].0 - envelope[i].0;
            if dx.abs() > 0.0 {
                slopes.push((envelope[j].1 - envelope[i].1) / dx);
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    let a = median_f64(&mut slopes);

    // 4. Place the intercept exactly on the lower envelope of ALL points: the
    //    largest b consistent with every d_i >= 0.
    let b = pts
        .iter()
        .map(|&(x, y)| y - a * x)
        .fold(f64::INFINITY, f64::min);

    // 5. Residuals are then non-negative by construction — the observed one-way
    //    delay distribution measured from its own floor.
    let mut resid: Vec<u64> = pts
        .iter()
        .map(|&(x, y)| (y - (a * x + b)).max(0.0) as u64)
        .collect();
    resid.sort_unstable();

    // 6. The offset an operator actually means: phone WALLCLOCK minus fleet
    //    wallclock. `mono_ns` has an arbitrary origin, so `b` is a transfer
    //    constant and not an offset; the phone's own `wallMillis` is the only
    //    thing that makes a cross-clock offset well-defined.
    //
    //    With `tx_wall = t + Δ` and `rx_unix = t + d` (d ≥ d_min > 0),
    //    `tx_wall − rx_unix = Δ − d`, so the MAXIMUM over packets is the
    //    least-delayed one and estimates `Δ − d_min`: biased EARLY, never late,
    //    by at most the one-way floor.
    let wall: Vec<i64> = samples
        .iter()
        .filter_map(|s| s.tx_wall_ns.map(|w| w as i64 - s.rx_unix_ns as i64))
        .collect();
    let wall_offset_ns = wall.iter().copied().max();

    // Back to absolute nanoseconds. `intercept_ns` is reported for callers that
    // want the raw `ClockGate` parameter; every evaluation inside this type goes
    // through the origin instead, for the mantissa reason above.
    let intercept_ns = (y0 as f64 + b - a * x0 as f64).round() as i64;

    Some(AffineFit {
        tx_id: tx_id.to_string(),
        n: samples.len(),
        span_s: span / 1e9,
        slope: a,
        slope_ppm: (a - 1.0) * 1e6,
        intercept_ns,
        wall_offset_ns,
        wall_n: wall.len(),
        offset_bias_ns: d_floor_ns,
        origin_mono_ns: x0,
        origin_unix_ns: y0 as i64 + b.round() as i64,
        residual_p50_ns: percentile(&resid, 0.50),
        residual_p95_ns: percentile(&resid, 0.95),
        residual_max_ns: *resid.last().unwrap_or(&0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::gates::G4B_BUDGET_NS;

    const MONO0: u64 = 900_000_000_000; // ~15 min of phone uptime
    const UNIX0: u64 = 1_786_000_000_000_000_000;

    /// Synthesise a phone stream.
    ///
    /// The phone's monotonic clock runs `ppm` fast against the fleet, its
    /// wallclock is `wall_offset_ns` ahead of the fleet's, and every packet
    /// takes a one-way delay with a floor and a heavy right tail — which is
    /// what a contended channel actually produces.
    fn stream(
        n: usize,
        rate_hz: f64,
        ppm: f64,
        wall_offset_ns: i64,
        floor_ns: u64,
        tail_ns: u64,
    ) -> Vec<Sample> {
        let a = 1.0 + ppm / 1e6;
        let period = (1e9 / rate_hz) as u64;
        (0..n as u64)
            .map(|i| {
                let mono = MONO0 + i * period;
                // Deterministic, heavy-right-tailed delay.
                let u = ((i.wrapping_mul(2862933555777941757).wrapping_add(3037000493)) >> 40)
                    as f64
                    / (1u64 << 24) as f64;
                let d = floor_ns + (tail_ns as f64 * u * u * u) as u64;
                // Fleet time at which the packet was transmitted.
                let tx_unix = UNIX0 as i64 + (a * (mono - MONO0) as f64).round() as i64;
                Sample {
                    mono_ns: mono,
                    rx_unix_ns: (tx_unix + d as i64) as u64,
                    tx_wall_ns: Some((tx_unix + wall_offset_ns) as u64),
                }
            })
            .collect()
    }

    /// The headline claim: slope (skew) is recoverable cleanly from one-way
    /// data, even with a delay tail an order of magnitude above the floor.
    #[test]
    fn the_slope_is_recovered_to_well_under_a_ppm() {
        for ppm in [-40.0, -3.5, 0.0, 12.0, 47.0] {
            let s = stream(4000, 20.0, ppm, 0, 3_000_000, 30_000_000);
            let f = fit("phone-a", &s, D_FLOOR_DEFAULT_NS).unwrap();
            assert!(
                (f.slope_ppm - ppm).abs() < 1.0,
                "commanded {ppm} ppm, fitted {:.3} ppm over {:.0} s",
                f.slope_ppm,
                f.span_s
            );
        }
    }

    /// The honest half: the offset is biased EARLY by the minimum one-way
    /// delay, the fit does not pretend otherwise, and the reported interval
    /// brackets the truth.
    #[test]
    fn the_offset_is_biased_by_the_one_way_floor_and_the_interval_covers_the_truth() {
        let true_offset = -37_000_000i64; // phone wallclock 37 ms behind the fleet
        let floor = 4_000_000u64;
        let s = stream(3000, 20.0, 5.0, true_offset, floor, 20_000_000);
        let f = fit("phone-a", &s, D_FLOOR_DEFAULT_NS).unwrap();

        let est = f.wall_offset_ns.expect("the app sent a wallclock");
        assert_eq!(f.wall_n, 3000);

        // Biased early by ~the floor, and never late.
        let err = est - true_offset;
        assert!(err < 0, "the bias is one-signed and early: {err}");
        assert!(
            (err + floor as i64).abs() < 500_000,
            "expected ~-{floor} ns of bias, got {err}"
        );

        // The reported interval contains the truth.
        let hi = est + f.offset_bias_ns as i64;
        assert!(
            est <= true_offset && true_offset <= hi,
            "[{est}, {hi}] must contain {true_offset}"
        );

        // And 5 ms of bias against a 250 ms budget is 2%.
        assert!(f.offset_bias_ns * 50 <= G4B_BUDGET_NS);
        let text = f.render();
        assert!(text.contains("one-way floor"), "{text}");
        assert!(text.contains("biases it early"), "{text}");
        assert!(text.contains("recoverable cleanly"), "{text}");
    }

    /// A phone that sent no wallclock gets no offset — not a zero.
    #[test]
    fn a_stream_without_a_wallclock_reports_no_offset_rather_than_zero() {
        let s: Vec<Sample> = stream(500, 20.0, 3.0, 0, 1_000_000, 5_000_000)
            .into_iter()
            .map(|x| Sample { tx_wall_ns: None, ..x })
            .collect();
        let f = fit("p", &s, D_FLOOR_DEFAULT_NS).unwrap();
        assert_eq!(f.wall_offset_ns, None);
        assert_eq!(f.wall_n, 0);
        // The slope is unaffected — it never depended on the wallclock.
        assert!((f.slope_ppm - 3.0).abs() < 1.0, "{}", f.slope_ppm);
        assert!(f.render().contains("no wallclock"), "{}", f.render());
    }

    /// Residuals are non-negative by construction — the intercept sits ON the
    /// envelope, not through the middle of the cloud.
    #[test]
    fn residuals_are_the_one_way_delay_measured_from_its_own_floor() {
        let s = stream(2000, 20.0, 0.0, 0, 2_000_000, 40_000_000);
        let f = fit("p", &s, D_FLOOR_DEFAULT_NS).unwrap();
        assert!(f.residual_p50_ns <= f.residual_p95_ns);
        assert!(f.residual_p95_ns <= f.residual_max_ns);
        // p50 must be well below the tail width, which is the point of not
        // using least squares.
        assert!(f.residual_p50_ns < 20_000_000, "{}", f.residual_p50_ns);
        assert!(f.residual_within(G4B_BUDGET_NS));
    }

    /// A tail wide enough to break the registered join must fail the gate
    /// rather than be smoothed away.
    #[test]
    fn a_join_whose_residual_breaks_the_budget_says_so() {
        let s = stream(2000, 20.0, 0.0, 0, 2_000_000, 2_000_000_000);
        let f = fit("p", &s, D_FLOOR_DEFAULT_NS).unwrap();
        assert!(!f.residual_within(G4B_BUDGET_NS), "p95 {}", f.residual_p95_ns);
    }

    /// The whole point of the fit: a phone stamp lands on the fleet timeline
    /// well inside the budget it is spent against.
    #[test]
    fn the_transform_maps_a_stamp_back_onto_the_fleet_timeline() {
        let s = stream(2000, 20.0, 10.0, 25_000_000, 1_000_000, 5_000_000);
        let f = fit("p", &s, D_FLOOR_DEFAULT_NS).unwrap();
        // A point inside the fitted span, and one 10 minutes past its end.
        for probe in [MONO0 + 30_000_000_000, MONO0 + 700_000_000_000] {
            let mapped = f.to_unix_ns(probe);
            let truth = UNIX0 as i64 + (1.000_01 * (probe - MONO0) as f64).round() as i64;
            assert!(
                (mapped - truth).abs() < G4B_BUDGET_NS as i64,
                "probe {probe}: mapped {mapped}, truth {truth}, delta {}",
                mapped - truth
            );
        }
    }

    #[test]
    fn degenerate_inputs_return_no_fit_rather_than_a_fabricated_one() {
        assert!(fit("p", &[], D_FLOOR_DEFAULT_NS).is_none());
        assert!(fit("p", &stream(4, 20.0, 0.0, 0, 0, 0), D_FLOOR_DEFAULT_NS).is_none());
        // Every sample at the same instant: no span, therefore no slope.
        let frozen: Vec<Sample> = (0..100)
            .map(|_| Sample { mono_ns: MONO0, rx_unix_ns: UNIX0, tx_wall_ns: None })
            .collect();
        assert!(fit("p", &frozen, D_FLOOR_DEFAULT_NS).is_none());
    }

    /// Nanosecond resolution must survive the epoch-scale magnitudes. Both
    /// clocks are ~1.8e18 ns, where f64 quantises at 256 ns; converting before
    /// differencing throws away a thousandth of the budget before the fit even
    /// starts, and this test is what catches that.
    #[test]
    fn epoch_scale_magnitudes_do_not_destroy_nanosecond_resolution() {
        // 200 ppm, noiseless, at 1.786e18 ns absolute.
        let s = stream(2000, 50.0, 200.0, 0, 0, 0);
        let f = fit("p", &s, D_FLOOR_DEFAULT_NS).unwrap();
        assert!((f.slope_ppm - 200.0).abs() < 0.05, "{}", f.slope_ppm);
        assert!(
            f.residual_max_ns <= 2,
            "a noiseless stream must fit to the nanosecond, got {} ns",
            f.residual_max_ns
        );
        // And the evaluation path keeps it too.
        assert!((f.to_unix_ns(s[1000].mono_ns) - s[1000].rx_unix_ns as i64).abs() <= 2);
    }
}
