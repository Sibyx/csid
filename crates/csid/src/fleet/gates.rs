//! The pre-registered corpus gates, as commands.
//!
//! Source of record: `experiments/prereg/lab-session-2026-08-prereg-v2.md` §3,
//! frozen 2026-08-08 before any measurement from the August session existed,
//! SHA-256 `542e0d889a574a7a5085bd230a077740fe879b73accd9a63f431ade1aae021c4`.
//!
//! ## Why the gates are commands and not a checklist
//!
//! Every one of these is stated in the pre-registration as **a confidence
//! interval excluding a threshold**, and each names *which bound* decides:
//!
//! | Gate | Estimand | Unit | Criterion |
//! |---|---|---|---|
//! | G1 | delivered scoped-record rate, Hz | the 1 s bin | 95% CI **lower** bound ≥ 100 Hz |
//! | G2 | CV of inter-arrival gaps | the block (bootstrap over gaps) | 95% CI **upper** bound < 0.5 |
//! | G3 | 5 GHz `iax` AP stability | the probe | binary, four conjunctive conditions |
//! | G4b | max abs clock residual | the fold | 95% CI upper bound < 0.25 s |
//!
//! G1 says it outright: *"A point estimate above 100 Hz with a lower bound
//! below it does not pass."* An operator with a calculator and a rate readout
//! will make exactly that mistake at 11pm on Day 0, which is why the gate is
//! code and why [`GateOutcome::render`] refuses to print a point estimate
//! without its interval and without naming the deciding bound.
//!
//! ## What these commands are and are not
//!
//! They are the **bench** evaluation: same estimand, same unit, same interval
//! type, same thresholds, run in seconds on the node's own spool so the
//! operator can act. They are not the analysis of record — that stays with the
//! frozen harness (`csi_ble_calibration_eval.py`) and re-runs offline against
//! the shipped captures. Where they can differ is Monte-Carlo error on the
//! interval endpoints at B = 2000, which is O(1/√B) and does not move a gate
//! that is not already on its threshold. A gate that lands *on* its threshold
//! is reported as such, not rounded into a verdict.

use serde::{Deserialize, Serialize};

use super::stats::{self, Ci};

/// G1's registered floor: delivered scoped-record rate per link per block.
pub const G1_FLOOR_HZ: f64 = 100.0;
/// G2's registered ceiling on the inter-arrival CV.
pub const G2_CV_CEILING: f64 = 0.5;
/// G3's registered probe duration.
pub const G3_MIN_PROBE_S: u64 = 30 * 60;
/// G4a: one CSI analysis window.
pub const G4A_BUDGET_NS: u64 = 6_000_000_000;
/// G4b: half the ±0.5 s peak band of the T3-CHANGE contrast.
pub const G4B_BUDGET_NS: u64 = 250_000_000;

/// The documented consequence of a G3 failure, quoted from the pre-registration
/// §3 so the operator reads the registered text rather than a paraphrase.
pub const G3_FALLBACK: &str = "\
FAIL consequence — the documented fallback (prereg v2 §3, G3):
  The session proceeds on 2.4 GHz ONLY. Band is dropped as an analysis factor.
  `band-5ghz-occupancy-discriminability` is not addressed and NO BAND CLAIM IS MADE.
  This is NOT an abort (§11, A3): the staged capture proceeds on 2.4 GHz.
  This fallback is decided by the probe ALONE and may not be invoked, revoked or
  re-litigated after counting results are visible.";

/// The Day-0 abort text for G1, quoted from the lab plan and prereg §11 A1/A2.
pub const G1_ABORT: &str = "\
Day-0 abort criteria (prereg v2 §11, A1/A2; Lab Session Plan Day 0 step 4):
  A1 — G1 fails on the empty-room block after remediation -> STOP AND FIX; no staged
       capture proceeds. \"If this fails, stop and fix - every downstream feature
       depends on it.\"
  A2 — Illuminator silent, or delivered/nominal ratio below 0.5 on any link -> STOP AND FIX.";

/// Pass / fail / not-measured for a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateVerdict {
    Pass,
    Fail,
    /// The gate could not be evaluated. Never a pass, and never silently
    /// folded into one.
    Unknown,
}

impl GateVerdict {
    pub fn label(self) -> &'static str {
        match self {
            GateVerdict::Pass => "PASS",
            GateVerdict::Fail => "FAIL",
            GateVerdict::Unknown => "UNKNOWN",
        }
    }

    pub fn worse(self, other: GateVerdict) -> GateVerdict {
        use GateVerdict::*;
        match (self, other) {
            (Fail, _) | (_, Fail) => Fail,
            (Unknown, _) | (_, Unknown) => Unknown,
            _ => Pass,
        }
    }
}

/// Which end of the interval the criterion is stated on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bound {
    Lower,
    Upper,
}

impl Bound {
    pub fn label(self) -> &'static str {
        match self {
            Bound::Lower => "95% CI lower bound",
            Bound::Upper => "95% CI upper bound",
        }
    }

    pub fn value_of(self, ci: &Ci) -> f64 {
        match self {
            Bound::Lower => ci.lo,
            Bound::Upper => ci.hi,
        }
    }
}

/// One evaluated gate, for one unit (a link, a block, a node).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOutcome {
    /// `G1`, `G2`, `G3`.
    pub gate: String,
    /// What the gate was evaluated on — a host, a block, a link.
    pub unit: String,
    /// The registered criterion, spelled out.
    pub criterion: String,
    /// The estimand, spelled out.
    pub estimand: String,
    /// The resampling unit and its count, so the honest n is never hidden.
    pub resampling_unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<Ci>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<Bound>,
    pub verdict: GateVerdict,
    /// Everything the operator needs to act, most important first.
    #[serde(default)]
    pub detail: Vec<String>,
}

impl GateOutcome {
    /// The bench rendering. Three invariants, all tested:
    ///
    /// 1. The registered threshold is printed.
    /// 2. The measured value is printed **with** its interval.
    /// 3. The deciding bound is named, with its value.
    pub fn render(&self, decimals: usize) -> String {
        let mut out = format!(
            "{} [{}]  {}\n  criterion : {}\n  estimand  : {}\n  unit      : {}\n",
            self.gate,
            self.unit,
            self.verdict.label(),
            self.criterion,
            self.estimand,
            self.resampling_unit,
        );
        match (&self.ci, self.bound) {
            (Some(ci), Some(b)) => {
                out.push_str(&format!(
                    "  measured  : {} (n = {}, B = {})\n  deciding  : {} = {:.*}\n",
                    ci.render(decimals),
                    ci.n,
                    ci.b,
                    b.label(),
                    decimals,
                    b.value_of(ci),
                ));
            }
            (Some(ci), None) => {
                out.push_str(&format!("  measured  : {}\n", ci.render(decimals)));
            }
            _ => out.push_str("  measured  : (not measurable)\n"),
        }
        for d in &self.detail {
            out.push_str(&format!("  · {d}\n"));
        }
        out
    }
}

/// Evaluate **G1 — delivered rate**.
///
/// `times_s` are the arrival timestamps of records already scoped to one source
/// MAC and one CSI record class, in seconds. The estimand is the per-1 s-bin
/// count; the CI is a percentile bootstrap over bins; the criterion is on the
/// **lower** bound.
pub fn g1_delivered_rate(unit: &str, times_s: &[f64], floor_hz: f64) -> GateOutcome {
    let bins = stats::per_second_bins(times_s);
    let ci = stats::bootstrap_mean(&bins, stats::BOOTSTRAP_B, stats::BOOTSTRAP_SEED);

    let mut detail = Vec::new();
    let verdict = match &ci {
        None => {
            detail.push(format!(
                "only {} whole 1 s bin(s) from {} records — an interval needs at least 2. \
                 Capture for longer or check that the scope matches the illuminator.",
                bins.len(),
                times_s.len()
            ));
            GateVerdict::Unknown
        }
        Some(c) => {
            if c.lo >= floor_hz {
                GateVerdict::Pass
            } else {
                if c.point >= floor_hz {
                    detail.push(format!(
                        "the POINT estimate ({:.1} Hz) clears the floor and the LOWER BOUND \
                         ({:.1} Hz) does not. The pre-registration is explicit: \
                         \"A point estimate above 100 Hz with a lower bound below it does not pass.\"",
                        c.point, c.lo
                    ));
                } else {
                    detail.push(format!(
                        "delivered rate is below the floor on the point estimate too ({:.1} Hz).",
                        c.point
                    ));
                }
                detail.push(G1_ABORT.to_string());
                GateVerdict::Fail
            }
        }
    };

    GateOutcome {
        gate: "G1".into(),
        unit: unit.into(),
        criterion: format!("95% CI LOWER bound of the delivered rate >= {floor_hz:.0} Hz"),
        estimand: "delivered scoped-record rate per link per block, Hz, over records \
                   scoped to a single source MAC and a single CSI record class"
            .into(),
        resampling_unit: format!(
            "the 1 s bin; {} whole bin(s) from {} scoped records",
            bins.len(),
            times_s.len()
        ),
        ci,
        bound: Some(Bound::Lower),
        verdict,
        detail,
    }
}

/// Evaluate **G2 — inter-arrival regularity**.
///
/// The criterion is on the **upper** bound: a stream is admitted only if its CV
/// is confidently below 0.5, not merely estimated below it.
pub fn g2_interarrival_cv(unit: &str, times_s: &[f64], ceiling: f64) -> GateOutcome {
    let (gaps, dropped) = stats::gaps_s(times_s);
    let ci = stats::bootstrap_cv(&gaps, stats::BOOTSTRAP_B, stats::BOOTSTRAP_SEED);

    let mut detail = Vec::new();
    if dropped > 0 {
        detail.push(format!(
            "{dropped} non-positive gap(s) dropped (duplicate or reordered stamps); \
             they are excluded from the CV rather than counted as perfect cadence"
        ));
    }
    let verdict = match &ci {
        None => {
            detail.push(format!(
                "only {} usable gap(s) — an interval needs at least 2.",
                gaps.len()
            ));
            GateVerdict::Unknown
        }
        Some(c) => {
            if c.hi < ceiling {
                GateVerdict::Pass
            } else {
                if c.point < ceiling {
                    detail.push(format!(
                        "the POINT estimate ({:.2}) is under the ceiling and the UPPER BOUND \
                         ({:.2}) is not. The gate is on the upper bound.",
                        c.point, c.hi
                    ));
                }
                detail.push(
                    "Doppler is blocked by JITTER, not by rate (prereg v2 §3, G2: 0 of 500 \
                     Doppler windows passed this on the four audited real captures; gap CVs \
                     1.00, 1.47, 2.60, 50.96). Remedy: throttled fixed-cadence illumination — \
                     `capture.mode = \"inject\"` with a paced rate, or the app's paced-UDP \
                     illuminator. Ambient traffic will not pass this gate."
                        .into(),
                );
                detail.push(
                    "Exclusion E2 (prereg v2 §11): a window failing G2 is excluded from the \
                     Doppler and eigencount features ONLY, and is RETAINED for amplitude-CV \
                     features, which do not require cadence regularity."
                        .into(),
                );
                GateVerdict::Fail
            }
        }
    };

    GateOutcome {
        gate: "G2".into(),
        unit: unit.into(),
        criterion: format!("95% CI UPPER bound of the inter-arrival CV < {ceiling}"),
        estimand: "CV of inter-arrival gaps within the block, scoped to one record class".into(),
        resampling_unit: format!("the inter-arrival gap; {} usable gap(s)", gaps.len()),
        ci,
        bound: Some(Bound::Upper),
        verdict,
        detail,
    }
}

/// The four conjunctive conditions of **G3**, each independently observable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct G3Conditions {
    /// (i) ≥ 30 min continuous capture with a station associated to the 5 GHz AP.
    pub probe_seconds: Option<u64>,
    /// (i) again — the association half. Minimum station count seen across the
    /// probe. Zero at any sample means the phone dropped off.
    pub min_associated_stations: Option<u32>,
    /// (ii) `iax` driver / firmware resets in the node journal over the probe.
    pub driver_resets: Option<u32>,
    /// (iii) G1 on the probe capture.
    pub g1: Option<GateVerdict>,
    /// (iv) G2 on the probe capture.
    pub g2: Option<GateVerdict>,
}

/// Evaluate **G3 — the Day-0 5 GHz `iax` AP stability probe**.
///
/// Binary and conjunctive: PASS requires all four conditions. Any condition
/// that was not observed makes the gate UNKNOWN, **not** a pass — an
/// unobserved reset count is not a zero reset count, and this is the single
/// largest schedule risk in the plan, so it does not get the benefit of the
/// doubt.
pub fn g3_ap_stability(unit: &str, c: &G3Conditions) -> GateOutcome {
    let mut detail = Vec::new();
    let mut verdict = GateVerdict::Pass;

    // Each condition is `(observed?, prose)`. `None` is "we did not look",
    // which is deliberately distinct from "we looked and it was fine".
    let conditions: Vec<(Option<bool>, String)> = vec![
        (
            c.probe_seconds.map(|s| s >= G3_MIN_PROBE_S),
            format!(
                "(i-a) >= {} min continuous capture — observed {}",
                G3_MIN_PROBE_S / 60,
                c.probe_seconds
                    .map(|s| format!("{:.1} min", s as f64 / 60.0))
                    .unwrap_or_else(|| "nothing".into())
            ),
        ),
        (
            c.min_associated_stations.map(|n| n >= 1),
            format!(
                "(i-b) a station associated to the 5 GHz AP throughout — minimum seen {}",
                c.min_associated_stations
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unobserved".into())
            ),
        ),
        (
            c.driver_resets.map(|n| n == 0),
            format!(
                "(ii) zero iax driver/firmware resets in the node journal — {}",
                c.driver_resets
                    .map(|n| format!("{n} seen"))
                    .unwrap_or_else(|| "journal not read".into())
            ),
        ),
        (
            c.g1.map(|v| v == GateVerdict::Pass),
            format!(
                "(iii) G1 passes on the probe capture — {}",
                c.g1.map(|v| v.label()).unwrap_or("not evaluated")
            ),
        ),
        (
            c.g2.map(|v| v == GateVerdict::Pass),
            format!(
                "(iv) G2 passes on the probe capture — {}",
                c.g2.map(|v| v.label()).unwrap_or("not evaluated")
            ),
        ),
    ];

    for (ok, text) in conditions {
        match ok {
            Some(true) => detail.push(format!("PASS  {text}")),
            Some(false) => {
                detail.push(format!("FAIL  {text}"));
                verdict = verdict.worse(GateVerdict::Fail);
            }
            None => {
                detail.push(format!("UNKNOWN  {text}"));
                verdict = verdict.worse(GateVerdict::Unknown);
            }
        }
    }

    // The fallback text is printed on anything that is not a clean PASS: an
    // UNKNOWN probe leaves the operator with the same decision to make as a
    // FAIL, and the registered fallback is what they need in front of them.
    if verdict != GateVerdict::Pass {
        detail.push(G3_FALLBACK.to_string());
    } else {
        detail.push(
            "5 GHz is admitted as an analysis factor. Band enters as a WITHIN-FOLD stratum, \
             never as an additional fold (prereg v2 §2)."
                .into(),
        );
    }

    GateOutcome {
        gate: "G3".into(),
        unit: unit.into(),
        criterion: "binary; PASS requires ALL FOUR of (i) >= 30 min associated capture, \
                    (ii) zero iax resets, (iii) G1 passes, (iv) G2 passes"
            .into(),
        estimand: "did the iax firmware survive a 5 GHz AP the phone associates to?".into(),
        resampling_unit: "the probe (one binary observation, decided on Day 0 before any \
                          staged participant data exists)"
            .into(),
        ci: None,
        bound: None,
        verdict,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean 122.5 Hz stream — the measured 5 GHz PoC baseline.
    fn paced(rate_hz: f64, seconds: f64, jitter: f64) -> Vec<f64> {
        let n = (rate_hz * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate_hz;
                // Deterministic, bounded, zero-mean wobble.
                t + jitter * ((i * 7919 % 101) as f64 / 100.0 - 0.5) / rate_hz
            })
            .collect()
    }

    #[test]
    fn g1_passes_a_stream_whose_lower_bound_clears_the_floor() {
        let out = g1_delivered_rate("monad04/link-A", &paced(122.5, 60.0, 0.2), G1_FLOOR_HZ);
        assert_eq!(out.verdict, GateVerdict::Pass, "{}", out.render(1));
        let ci = out.ci.unwrap();
        assert!(ci.lo >= 100.0, "{ci:?}");
        assert_eq!(out.bound, Some(Bound::Lower));
    }

    /// The 2.4 GHz illuminated baseline: 21.2 Hz. Nowhere near the floor.
    #[test]
    fn g1_fails_a_stream_far_below_the_floor_and_prints_the_abort_text() {
        let out = g1_delivered_rate("monad04/link-A", &paced(21.2, 60.0, 0.1), G1_FLOOR_HZ);
        assert_eq!(out.verdict, GateVerdict::Fail);
        let text = out.render(1);
        assert!(text.contains("STOP AND FIX"), "{text}");
        assert!(text.contains("A1"), "{text}");
        assert!(text.contains("A2"), "{text}");
    }

    /// The whole reason the gate is on the lower bound.
    #[test]
    fn g1_refuses_a_point_estimate_that_clears_a_line_its_bound_does_not() {
        // A stream that averages ~105 Hz but swings wildly second to second.
        let mut times = Vec::new();
        let mut t = 0.0;
        for s in 0..40 {
            let this_second = if s % 2 == 0 { 60 } else { 150 };
            for i in 0..this_second {
                times.push(t + i as f64 / this_second as f64);
            }
            t += 1.0;
        }
        let out = g1_delivered_rate("monad04", &times, G1_FLOOR_HZ);
        let ci = out.ci.unwrap();
        assert!(ci.point > 100.0, "the point estimate clears: {ci:?}");
        assert_eq!(out.verdict, GateVerdict::Fail, "the bound does not: {ci:?}");
        let text = out.render(1);
        assert!(
            text.contains("does not pass"),
            "the registered sentence must be quoted: {text}"
        );
        assert!(text.contains("95% CI lower bound ="), "{text}");
    }

    #[test]
    fn g1_reports_unknown_rather_than_zero_when_there_is_nothing_to_measure() {
        for times in [vec![], vec![0.0], vec![0.0, 0.1, 0.2]] {
            let out = g1_delivered_rate("monad07", &times, G1_FLOOR_HZ);
            assert_eq!(out.verdict, GateVerdict::Unknown, "{times:?}");
            assert!(out.ci.is_none());
            assert!(out.render(1).contains("not measurable"));
        }
    }

    #[test]
    fn g2_passes_a_paced_stream_and_fails_a_bursty_one() {
        let out = g2_interarrival_cv("monad04", &paced(122.5, 30.0, 0.2), G2_CV_CEILING);
        assert_eq!(out.verdict, GateVerdict::Pass, "{}", out.render(2));
        assert_eq!(out.bound, Some(Bound::Upper));
        assert!(out.ci.unwrap().hi < 0.5);

        // Ambient traffic: tight clusters separated by long silences. This is
        // the real-capture failure mode the gate exists for.
        let mut times = Vec::new();
        let mut t = 0.0;
        for _ in 0..60 {
            for i in 0..20 {
                times.push(t + i as f64 * 0.002);
            }
            t += 1.0;
        }
        let out = g2_interarrival_cv("monad04", &times, G2_CV_CEILING);
        assert_eq!(out.verdict, GateVerdict::Fail, "{}", out.render(2));
        let text = out.render(2);
        assert!(text.contains("blocked by JITTER"), "{text}");
        assert!(text.contains("fixed-cadence"), "{text}");
        assert!(
            text.contains("RETAINED for amplitude-CV"),
            "E2 keeps amplitude features alive; that must be said: {text}"
        );
    }

    #[test]
    fn g2_counts_the_gaps_it_dropped_rather_than_silently_improving() {
        let mut times = paced(100.0, 10.0, 0.0);
        // A duplicate stamp and a reordered pair.
        times.insert(50, times[50]);
        times.swap(200, 201);
        let out = g2_interarrival_cv("monad04", &times, G2_CV_CEILING);
        assert!(
            out.detail.iter().any(|d| d.contains("non-positive gap")),
            "{:?}",
            out.detail
        );
    }

    fn all_good() -> G3Conditions {
        G3Conditions {
            probe_seconds: Some(1860),
            min_associated_stations: Some(1),
            driver_resets: Some(0),
            g1: Some(GateVerdict::Pass),
            g2: Some(GateVerdict::Pass),
        }
    }

    #[test]
    fn g3_passes_only_when_all_four_conditions_hold() {
        let out = g3_ap_stability("monad01 (AP) + monad04 (Rx)", &all_good());
        assert_eq!(out.verdict, GateVerdict::Pass, "{}", out.render(1));
        assert!(out.render(1).contains("WITHIN-FOLD stratum"));
        assert!(
            !out.render(1).contains("2.4 GHz ONLY"),
            "the fallback must not be printed on a pass"
        );
    }

    #[test]
    fn g3_fails_on_any_single_condition_and_always_prints_the_registered_fallback() {
        let cases: Vec<(&str, G3Conditions)> = vec![
            (
                "short probe",
                G3Conditions {
                    probe_seconds: Some(600),
                    ..all_good()
                },
            ),
            (
                "phone dropped off",
                G3Conditions {
                    min_associated_stations: Some(0),
                    ..all_good()
                },
            ),
            (
                "firmware reset",
                G3Conditions {
                    driver_resets: Some(1),
                    ..all_good()
                },
            ),
            (
                "G1 failed",
                G3Conditions {
                    g1: Some(GateVerdict::Fail),
                    ..all_good()
                },
            ),
            (
                "G2 failed",
                G3Conditions {
                    g2: Some(GateVerdict::Fail),
                    ..all_good()
                },
            ),
        ];
        for (why, c) in cases {
            let out = g3_ap_stability("probe", &c);
            assert_eq!(out.verdict, GateVerdict::Fail, "{why}");
            let text = out.render(1);
            assert!(text.contains("2.4 GHz ONLY"), "{why}: {text}");
            assert!(text.contains("NO BAND CLAIM IS MADE"), "{why}: {text}");
            assert!(text.contains("NOT an abort"), "{why}: {text}");
        }
    }

    /// An unread journal is not a clean journal. This is the schedule-critical
    /// gate; it does not get the benefit of the doubt.
    #[test]
    fn g3_is_unknown_not_pass_when_a_condition_was_never_observed() {
        let out = g3_ap_stability(
            "probe",
            &G3Conditions {
                driver_resets: None,
                ..all_good()
            },
        );
        assert_eq!(out.verdict, GateVerdict::Unknown);
        let text = out.render(1);
        assert!(text.contains("journal not read"), "{text}");
        assert!(
            text.contains("2.4 GHz ONLY"),
            "an unknown probe leaves the same decision as a failed one: {text}"
        );
    }

    /// A definite failure must not be masked by an unknown elsewhere.
    #[test]
    fn a_definite_g3_failure_outranks_an_unknown() {
        let out = g3_ap_stability(
            "probe",
            &G3Conditions {
                driver_resets: Some(3),
                min_associated_stations: None,
                ..all_good()
            },
        );
        assert_eq!(out.verdict, GateVerdict::Fail);
    }

    /// The rendering contract: threshold, value-with-interval, deciding bound.
    #[test]
    fn a_rendered_gate_never_shows_a_bare_point_estimate() {
        for out in [
            g1_delivered_rate("u", &paced(122.5, 40.0, 0.2), G1_FLOOR_HZ),
            g2_interarrival_cv("u", &paced(122.5, 40.0, 0.2), G2_CV_CEILING),
        ] {
            let text = out.render(2);
            let ci = out.ci.unwrap();
            // 1. the threshold
            assert!(text.contains("criterion :"), "{text}");
            // 2. the value with its interval
            assert!(text.contains(&ci.render(2)), "{text}");
            // 3. the deciding bound, named and valued
            assert!(text.contains("deciding  :"), "{text}");
            assert!(text.contains(out.bound.unwrap().label()), "{text}");
            // and the honest n
            assert!(text.contains(&format!("n = {}", ci.n)), "{text}");
        }
    }

    #[test]
    fn gate_verdicts_escalate_with_fail_dominant() {
        use GateVerdict::*;
        assert_eq!(Pass.worse(Unknown), Unknown);
        assert_eq!(Unknown.worse(Fail), Fail);
        assert_eq!(Fail.worse(Pass), Fail);
        assert_eq!(Pass.worse(Pass), Pass);
    }
}
