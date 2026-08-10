//! Fleet clock coherence — is the fleet a single timebase, or ten clocks?
//!
//! ## The gap this closes
//!
//! Inside one node the question is already settled: the BLE scanner calls the
//! same [`crate::util::now_unix_ns`] the CSI receive path calls, in the same
//! process, so those two streams are aligned by construction with no offset to
//! estimate. The daemon's documentation says so and it is true.
//!
//! **It says nothing about node A against node B.** Ten nodes each internally
//! perfect but mutually skewed by seconds would corrupt every cross-node
//! comparison in the session — the receiver-subset contrast, the per-zone
//! folds, any statement that pools links — and nothing in the capture path
//! would notice, because nothing in the capture path ever compares two nodes.
//! The captures would look immaculate and the analysis would be wrong.
//!
//! The budget is not soft. The pre-registration's cycling ramps are 30 s and
//! the T3-CHANGE contrast is defined on a ±0.5 s band, so gate G4b sets the
//! precision requirement at **250 ms**. That is the budget this module checks
//! the fleet against, and it is why the check runs *before* the delivered-rate
//! gate in the Day-0 runbook: a fleet that is not time-coherent makes every
//! later gate a measurement of nothing.
//!
//! ## How the offset is measured
//!
//! The four-timestamp exchange, the same one `collectord` uses for the phone:
//!
//! ```text
//! t1  cockpit sends            t4  cockpit receives
//!         \                       /
//!          t2 node receives  t3 node sends
//!
//! offset = ((t2 − t1) + (t3 − t4)) / 2      delay = (t4 − t1) − (t3 − t2)
//! ```
//!
//! Here the "message" is an ssh round trip and `t2`/`t3` bracket the remote
//! `date` calls, so `t3 − t2` is the node-side execution time and is subtracted
//! out. The estimate's error is bounded by **half the one-way delay asymmetry**,
//! and since we cannot observe the asymmetry we report `delay / 2` as the
//! uncertainty. That is the honest bound, and it is why the output always
//! carries `±`: an offset of 3 ms measured over a 400 ms round trip is not a
//! 3 ms offset.
//!
//! Several samples are taken and the one with the **smallest delay** is kept —
//! the classic NTP filter. A short round trip has less room to hide an
//! asymmetry, so the minimum-delay sample is the least-wrong one. Averaging
//! would be worse: it folds the long, queue-inflated samples back in.
//!
//! ## Why the node's own daemon is read too
//!
//! An ssh round trip over a tailnet is a millisecond-scale instrument on a good
//! day. `chronyd` on the node knows its own offset to a stratum-2 source to
//! microseconds and publishes its own error bound. So both are reported, and
//! the uncertainty used for the budget is the **worse** of the two — our
//! round-trip bound cannot be better than the node's own dispersion.

use serde::{Deserialize, Serialize};

use super::health::{ClockHealth, NtpState};

/// One four-timestamp exchange, in nanoseconds on each side's own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// Cockpit clock when the probe was sent.
    pub t1_ns: u64,
    /// Node clock on entry.
    pub t2_ns: u64,
    /// Node clock on exit.
    pub t3_ns: u64,
    /// Cockpit clock when the reply landed.
    pub t4_ns: u64,
}

impl Exchange {
    /// Offset of the node clock from the cockpit clock, +ve = node ahead.
    pub fn offset_ns(&self) -> i64 {
        let a = self.t2_ns as i128 - self.t1_ns as i128;
        let b = self.t3_ns as i128 - self.t4_ns as i128;
        ((a + b) / 2) as i64
    }

    /// Round-trip delay with the node's own processing time removed.
    ///
    /// Clamped at zero: a negative delay is impossible and means one of the two
    /// clocks stepped mid-exchange, which the caller should discard rather than
    /// propagate as a negative uncertainty.
    pub fn delay_ns(&self) -> i64 {
        let rt = self.t4_ns as i128 - self.t1_ns as i128;
        let proc = self.t3_ns as i128 - self.t2_ns as i128;
        ((rt - proc).max(0)) as i64
    }

    /// Structurally sane: time moves forward on both sides.
    pub fn is_plausible(&self) -> bool {
        self.t4_ns >= self.t1_ns && self.t3_ns >= self.t2_ns && self.t1_ns > 0 && self.t2_ns > 0
    }
}

/// Pick the least-wrong sample: minimum delay, ignoring implausible ones.
pub fn best_sample(samples: &[Exchange]) -> Option<Exchange> {
    samples
        .iter()
        .filter(|e| e.is_plausible())
        .min_by_key(|e| e.delay_ns())
        .copied()
}

/// Fold a set of exchanges plus the node's own daemon view into one estimate.
///
/// The uncertainty is the **worse** of the round-trip bound and the daemon's
/// root dispersion. Taking the better of the two would be claiming a precision
/// neither instrument has.
pub fn estimate(samples: &[Exchange], ntp: Option<NtpState>) -> Option<ClockHealth> {
    let best = best_sample(samples)?;
    let plausible = samples.iter().filter(|e| e.is_plausible()).count();
    let rt_bound = (best.delay_ns() / 2).unsigned_abs();
    let ntp_bound = ntp
        .as_ref()
        .and_then(|n| n.root_dispersion_s)
        .filter(|d| d.is_finite() && *d >= 0.0)
        .map(|d| (d * 1e9) as u64)
        .unwrap_or(0);
    Some(ClockHealth {
        offset_ns: best.offset_ns(),
        uncertainty_ns: rt_bound.max(ntp_bound),
        samples: plausible,
        ntp,
    })
}

/// The fleet-level question: how far apart are the two most-separated nodes?
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkewReport {
    /// Nodes that produced an estimate.
    pub measured: usize,
    /// Nodes that did not. **A fleet with unmeasured nodes has an unknown
    /// skew**, whatever the measured ones say.
    pub unmeasured: Vec<String>,
    /// Point estimate of the worst mutual skew, nanoseconds.
    pub skew_ns: i64,
    /// Worst-case skew including both nodes' uncertainties. This is the number
    /// the budget is checked against.
    pub skew_upper_ns: u64,
    /// The two nodes at the extremes.
    pub extremes: Option<(String, String)>,
    pub budget_ns: u64,
}

impl SkewReport {
    /// `true` only when every node was measured *and* the worst-case skew fits
    /// the budget.
    pub fn within_budget(&self) -> bool {
        self.unmeasured.is_empty() && self.measured >= 1 && self.skew_upper_ns <= self.budget_ns
    }

    pub fn render(&self) -> String {
        let mut s = if self.measured == 0 {
            "fleet skew: NOT MEASURED on any node".to_string()
        } else {
            // With one measured node there is no pair, and "monad04 ↔ monad04"
            // reads as a bug.
            let pair = self
                .extremes
                .as_ref()
                .filter(|(a, b)| a != b)
                .map(|(a, b)| format!(" ({a} ↔ {b})"))
                .unwrap_or_default();
            format!(
                "fleet skew: {:+.1} ms, worst case {:.1} ms{pair} against a {:.0} ms budget",
                self.skew_ns as f64 / 1e6,
                self.skew_upper_ns as f64 / 1e6,
                self.budget_ns as f64 / 1e6,
            )
        };
        if !self.unmeasured.is_empty() {
            s.push_str(&format!(
                "\n  UNKNOWN for {} — the fleet skew is a lower bound, not a measurement",
                self.unmeasured.join(", ")
            ));
        }
        s
    }
}

/// Compute the worst-case mutual skew across the fleet.
///
/// Each node's offset is measured against the *cockpit*, so the mutual skew
/// between two nodes is the difference of their offsets — the cockpit's own
/// clock cancels out, which is why the cockpit's clock never has to be right.
/// It only has to be stable across the few seconds the probe takes.
pub fn fleet_skew(estimates: &[(String, Option<ClockHealth>)], budget_ns: u64) -> SkewReport {
    let mut unmeasured: Vec<String> = Vec::new();
    let mut lo: Option<(String, i64, u64)> = None;
    let mut hi: Option<(String, i64, u64)> = None;

    for (host, est) in estimates {
        match est {
            None => unmeasured.push(host.clone()),
            Some(c) => {
                let entry = (host.clone(), c.offset_ns, c.uncertainty_ns);
                if lo.as_ref().is_none_or(|(_, o, _)| c.offset_ns < *o) {
                    lo = Some(entry.clone());
                }
                if hi.as_ref().is_none_or(|(_, o, _)| c.offset_ns > *o) {
                    hi = Some(entry);
                }
            }
        }
    }

    let measured = estimates.len() - unmeasured.len();
    let (skew_ns, skew_upper_ns, extremes) = match (&lo, &hi) {
        (Some((ln, lo_ns, lu)), Some((hn, hi_ns, hu))) => (
            hi_ns - lo_ns,
            (hi_ns - lo_ns).unsigned_abs() + lu + hu,
            Some((ln.clone(), hn.clone())),
        ),
        _ => (0, 0, None),
    };

    SkewReport {
        measured,
        unmeasured,
        skew_ns,
        skew_upper_ns,
        extremes,
        budget_ns,
    }
}

// -- node-side daemon parsing -------------------------------------------------

/// Parse `chronyc -c tracking` CSV.
///
/// Field order (chrony 4.x): `refid, ref name, stratum, ref time, system time,
/// last offset, rms offset, frequency, residual freq, skew, root delay, root
/// dispersion, update interval, leap status`.
///
/// The "system time" field is signed seconds; chrony's sign convention is
/// positive = the system clock is *fast* of NTP time, which matches the "+ve =
/// ahead" convention used everywhere else in the cockpit.
pub fn parse_chrony_tracking(csv: &str) -> Option<NtpState> {
    let line = csv.lines().find(|l| !l.trim().is_empty())?;
    let f: Vec<&str> = line.split(',').collect();
    if f.len() < 14 {
        return None;
    }
    let stratum = f[2].trim().parse::<u32>().ok();
    let system_offset_s = f[4].trim().parse::<f64>().ok();
    let root_dispersion_s = f[11].trim().parse::<f64>().ok();
    let leap = f[13].trim();
    Some(NtpState {
        daemon: "chronyd".to_string(),
        // Stratum 0 means "not synchronised" in chrony's reporting, and any
        // leap status other than Normal means the daemon is unhappy.
        synchronised: stratum.is_some_and(|s| s > 0) && leap.eq_ignore_ascii_case("Normal"),
        stratum,
        system_offset_s,
        root_dispersion_s,
    })
}

/// Parse `timedatectl show -p NTPSynchronized --value` (the fallback when there
/// is no chrony). It answers only the yes/no question, with no offset and no
/// error bound — which is exactly what gets reported, rather than a fabricated
/// zero.
pub fn parse_timedatectl(value: &str) -> Option<NtpState> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    Some(NtpState {
        daemon: "systemd-timesyncd".to_string(),
        synchronised: v.eq_ignore_ascii_case("yes") || v.eq_ignore_ascii_case("true"),
        stratum: None,
        system_offset_s: None,
        root_dispersion_s: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A textbook exchange: the node is 5 ms ahead, the link is 20 ms
    /// round-trip, the node took 1 ms to answer.
    #[test]
    fn the_four_timestamp_formula_recovers_a_known_offset() {
        // Cockpit sends at 0. One-way 10 ms. Node clock = cockpit + 5 ms.
        // So node receives at cockpit-time 10 ms => node stamps 15 ms.
        // Node answers 1 ms later => stamps 16 ms (cockpit-time 11 ms).
        // Reply lands 10 ms later => cockpit stamps 21 ms.
        let e = Exchange {
            t1_ns: 0,
            t2_ns: 15_000_000,
            t3_ns: 16_000_000,
            t4_ns: 21_000_000,
        };
        assert_eq!(e.offset_ns(), 5_000_000, "node is 5 ms ahead");
        assert_eq!(e.delay_ns(), 20_000_000, "20 ms of link, 1 ms of node");
    }

    /// The formula must be immune to the node's processing time — otherwise a
    /// slow node reads as a skewed one.
    #[test]
    fn node_side_processing_time_does_not_leak_into_the_offset() {
        let fast = Exchange {
            t1_ns: 0,
            t2_ns: 15_000_000,
            t3_ns: 15_000_000,
            t4_ns: 20_000_000,
        };
        // Same link, same true offset, but the node took 500 ms to answer.
        let slow = Exchange {
            t1_ns: 0,
            t2_ns: 15_000_000,
            t3_ns: 515_000_000,
            t4_ns: 520_000_000,
        };
        assert_eq!(fast.offset_ns(), slow.offset_ns());
        assert_eq!(fast.delay_ns(), slow.delay_ns());
    }

    #[test]
    fn the_minimum_delay_sample_wins_and_implausible_ones_are_dropped() {
        let good = Exchange {
            t1_ns: 1_000_000_000,
            t2_ns: 1_005_000_000,
            t3_ns: 1_005_500_000,
            t4_ns: 1_010_000_000,
        };
        let queued = Exchange {
            t1_ns: 2_000_000_000,
            t2_ns: 2_300_000_000,
            t3_ns: 2_300_500_000,
            t4_ns: 2_400_000_000,
        };
        // Time went backwards on the cockpit side — a step mid-exchange.
        let broken = Exchange {
            t1_ns: 3_000_000_000,
            t2_ns: 3_001_000_000,
            t3_ns: 3_002_000_000,
            t4_ns: 2_900_000_000,
        };
        assert_eq!(best_sample(&[queued, good, broken]), Some(good));
        assert_eq!(best_sample(&[broken]), None);
        assert_eq!(best_sample(&[]), None);
    }

    /// The uncertainty is half the round trip — and it is never allowed to be
    /// smaller than the node's own daemon says it is.
    #[test]
    fn the_uncertainty_is_the_worse_of_the_two_instruments() {
        // Real epochs: `is_plausible` refuses a zero t1, because a zero
        // wallclock means the clock read failed, not that time began.
        const T0: u64 = 1_786_000_000_000_000_000;
        let e = Exchange {
            t1_ns: T0,
            t2_ns: T0 + 15_000_000,
            t3_ns: T0 + 16_000_000,
            t4_ns: T0 + 21_000_000,
        };
        // No daemon: half the 20 ms round trip.
        let c = estimate(&[e], None).unwrap();
        assert_eq!(c.uncertainty_ns, 10_000_000);
        assert_eq!(c.offset_ns, 5_000_000);
        assert_eq!(c.samples, 1);

        // A daemon claiming 1 ms dispersion does not improve our 10 ms bound.
        let tight = NtpState {
            daemon: "chronyd".into(),
            synchronised: true,
            stratum: Some(3),
            system_offset_s: Some(0.000_1),
            root_dispersion_s: Some(0.001),
        };
        assert_eq!(
            estimate(&[e], Some(tight)).unwrap().uncertainty_ns,
            10_000_000
        );

        // A daemon admitting 50 ms dispersion widens it.
        let loose = NtpState {
            daemon: "chronyd".into(),
            synchronised: true,
            stratum: Some(3),
            system_offset_s: Some(0.000_1),
            root_dispersion_s: Some(0.05),
        };
        assert_eq!(
            estimate(&[e], Some(loose)).unwrap().uncertainty_ns,
            50_000_000
        );

        assert!(estimate(&[], None).is_none(), "no samples, no estimate");
    }

    fn health(offset_ms: f64, unc_ms: f64) -> ClockHealth {
        ClockHealth {
            offset_ns: (offset_ms * 1e6) as i64,
            uncertainty_ns: (unc_ms * 1e6) as u64,
            samples: 5,
            ntp: None,
        }
    }

    /// The number that matters is node-to-node, not node-to-laptop: the
    /// cockpit's own clock cancels out of the difference.
    #[test]
    fn fleet_skew_is_the_spread_between_the_extreme_nodes() {
        let est = vec![
            ("monad01".to_string(), Some(health(-4.0, 2.0))),
            ("monad02".to_string(), Some(health(1.0, 2.0))),
            ("monad03".to_string(), Some(health(6.0, 3.0))),
        ];
        let r = fleet_skew(&est, super::super::gates::G4B_BUDGET_NS);
        assert_eq!(r.measured, 3);
        assert!(r.unmeasured.is_empty());
        assert_eq!(r.skew_ns, 10_000_000, "−4 ms to +6 ms is 10 ms apart");
        // 10 ms + 2 ms + 3 ms of uncertainty.
        assert_eq!(r.skew_upper_ns, 15_000_000);
        assert_eq!(
            r.extremes,
            Some(("monad01".to_string(), "monad03".to_string()))
        );
        assert!(r.within_budget(), "15 ms fits a 250 ms budget");
        assert!(r.render().contains("250 ms budget"), "{}", r.render());
    }

    /// Ten nodes each individually fine but mutually seconds apart is exactly
    /// the failure this exists to catch.
    #[test]
    fn a_fleet_of_individually_fine_but_mutually_skewed_nodes_fails() {
        let est = vec![
            ("monad01".to_string(), Some(health(-1_500.0, 5.0))),
            ("monad02".to_string(), Some(health(1_500.0, 5.0))),
        ];
        let r = fleet_skew(&est, super::super::gates::G4B_BUDGET_NS);
        assert_eq!(r.skew_ns, 3_000_000_000, "3 seconds apart");
        assert!(!r.within_budget());
    }

    /// An unmeasured node makes the fleet skew a lower bound. It must never be
    /// reported as within budget on the strength of the nodes that answered.
    #[test]
    fn an_unmeasured_node_makes_the_fleet_skew_unknown() {
        let est = vec![
            ("monad01".to_string(), Some(health(0.0, 1.0))),
            ("monad02".to_string(), Some(health(1.0, 1.0))),
            ("monad07".to_string(), None),
        ];
        let r = fleet_skew(&est, super::super::gates::G4B_BUDGET_NS);
        assert_eq!(r.measured, 2);
        assert_eq!(r.unmeasured, vec!["monad07"]);
        assert!(
            !r.within_budget(),
            "two tight nodes must not certify a fleet with a third unseen"
        );
        assert!(r.render().contains("UNKNOWN for monad07"), "{}", r.render());
        assert!(r.render().contains("lower bound"), "{}", r.render());
    }

    /// A one-node "fleet" has a skew of zero by construction, and must not
    /// render a pair against itself.
    #[test]
    fn a_single_node_reports_no_pair() {
        let est = vec![("monad04".to_string(), Some(health(2.0, 15.0)))];
        let r = fleet_skew(&est, super::super::gates::G4B_BUDGET_NS);
        assert_eq!(r.measured, 1);
        assert_eq!(r.skew_ns, 0);
        // Still carries both endpoints' uncertainty — the same node twice.
        assert_eq!(r.skew_upper_ns, 30_000_000);
        assert!(!r.render().contains('↔'), "{}", r.render());
        assert!(r.within_budget());
    }

    #[test]
    fn a_fleet_nobody_could_measure_reports_so() {
        let est = vec![("monad01".to_string(), None), ("monad02".to_string(), None)];
        let r = fleet_skew(&est, super::super::gates::G4B_BUDGET_NS);
        assert_eq!(r.measured, 0);
        assert!(!r.within_budget());
        assert!(r.render().contains("NOT MEASURED"), "{}", r.render());
    }

    #[test]
    fn chrony_csv_is_parsed_including_the_unsynchronised_case() {
        // A real `chronyc -c tracking` line from a healthy node.
        let good = "C0248F81,192.36.143.130,2,1786000000.0,\
                    0.000000345,-0.000000123,0.000012000,-1.234,0.001,0.050,\
                    0.012345,0.002345,64.0,Normal";
        let s = parse_chrony_tracking(good).unwrap();
        assert!(s.synchronised);
        assert_eq!(s.stratum, Some(2));
        assert_eq!(s.daemon, "chronyd");
        assert!((s.system_offset_s.unwrap() - 0.000_000_345).abs() < 1e-12);
        assert!((s.root_dispersion_s.unwrap() - 0.002_345).abs() < 1e-12);

        // Stratum 0 = not synchronised, whatever else the line says.
        let unsynced = good.replacen(",2,", ",0,", 1);
        assert!(!parse_chrony_tracking(&unsynced).unwrap().synchronised);

        // A leap status other than Normal is a daemon in trouble.
        let leaping = good.replace("Normal", "Not synchronised");
        assert!(!parse_chrony_tracking(&leaping).unwrap().synchronised);

        // Nonsense is refused rather than guessed.
        assert!(parse_chrony_tracking("").is_none());
        assert!(parse_chrony_tracking("not,enough,fields").is_none());
    }

    #[test]
    fn timedatectl_answers_only_the_yes_no_question_and_admits_it() {
        let s = parse_timedatectl("yes").unwrap();
        assert!(s.synchronised);
        assert_eq!(s.stratum, None);
        assert_eq!(
            s.root_dispersion_s, None,
            "no error bound must be fabricated"
        );
        assert!(!parse_timedatectl("no").unwrap().synchronised);
        assert!(parse_timedatectl("  ").is_none());
    }
}
