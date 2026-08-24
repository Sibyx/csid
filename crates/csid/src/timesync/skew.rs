//! Inter-node clock skew from the illumination stream — the free instrument.
//!
//! ## The idea
//!
//! One injected frame is received by every node in the room. Propagation across
//! a 10 m room is 33 ns; the nodes are stamping in nanoseconds, so for this
//! purpose the frame arrives at all of them **simultaneously**. Therefore, for
//! one sequence number `s`,
//!
//! ```text
//! t_A(s) − t_B(s)  =  (skew_A − skew_B)  +  (rx_A − rx_B)  +  O(30 ns)
//!                     └── what we want ──┘  └─ per-node RX pipeline delay ─┘
//! ```
//!
//! The transmit instant cancels **exactly** — it is the same physical event —
//! which is why this is a far better instrument than the four-timestamp ssh
//! exchange in [`crate::fleet::clock`]. There, the estimate's error is bounded
//! by half the round-trip asymmetry, i.e. by ssh over a tailnet: milliseconds
//! on a good day. Here there is no round trip to be asymmetric.
//!
//! ## Why the median and not the minimum
//!
//! The obvious NTP-style filter is "keep the minimum-delay sample". It is the
//! right filter for *one* one-way delay, and the wrong one for a **difference
//! of two**: `min_i(t_A − t_B)` picks the sample where A happened to be fast
//! *and* B happened to be slow, so it estimates
//! `skew_AB + min(rx_A) − max(rx_B)` — biased low by the whole width of B's
//! jitter distribution. The maximum is biased high by the same argument.
//!
//! The median is unbiased whenever the two nodes' delay distributions have the
//! same shape, which on ten identical Pi 5s running one binary is the sane
//! assumption. So the median is the point estimate, and `min`/`max` are
//! reported beside it as the observed envelope rather than used as estimators.
//!
//! ## What remains, and is not observable
//!
//! A fixed difference between the two nodes' RX pipeline *floors* — driver
//! path, interrupt coalescing, whether one node got a kernel timestamp and the
//! other did not — survives the median and cannot be separated from real skew
//! by this method. On identical hardware and one image it is a common-mode term
//! that cancels; on a mixed fleet it does not. That is stated in the render, not
//! hidden in a docstring.
//!
//! ## One window, or the skews do not compose
//!
//! Every pair in a fleet report is measured over the SAME sequence window. Left
//! to itself each pair spans whatever interval its own two nodes happened to
//! share, and a pair that ran twice as long carries twice as much accumulated
//! relative drift in its median. The estimates then stop composing:
//! `skew(x,y) + skew(y,z) − skew(x,z)` is not zero, and a node's offset via the
//! reference depends on which peer it was routed through. On the real
//! `coex-03_20260823-101235` arm — two nodes hearing to seq 74,587 while four
//! peers stopped at 30,366 — the triangles containing that long pair missed
//! closure by 42–62 µs, against 1–11 µs for the co-windowed ones. See
//! [`common_window`] for which nodes get a vote on the window and why.
//!
//! ## The degenerate cases, all handled explicitly
//!
//! * **One node.** No pair exists. Reported as such; never as "0 ns skew".
//! * **No common sequence numbers.** Two nodes that saw disjoint traffic are
//!   *unmeasured*, which is not the same as coherent.
//! * **No common window.** The nodes overlap in a chain rather than all at
//!   once. Every pair is unmeasured and nothing is certified — reporting the
//!   measurable pairs on different intervals is the bug, not the fix.
//! * **A clock step mid-session.** The difference series is then bimodal and its
//!   median is a number that describes neither half. Detected by splitting the
//!   common sequences in two and comparing the halves; a non-stationary pair is
//!   reported as such and never certified.

use serde::{Deserialize, Serialize};

/// One node's receipt of one transmitted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arrival {
    pub seq: u64,
    /// The receiving node's wallclock at delivery.
    pub unix_ts_ns: u64,
}

/// Pairs whose per-packet difference exceeds this are dropped as implausible —
/// they mean the two nodes matched sequence numbers from different runs of the
/// injector (a restart resets `seq` to 0), not that they are 5 s apart.
pub const MAX_PLAUSIBLE_DIFF_NS: i64 = 5_000_000_000;

/// Fewest common packets that will produce an estimate at all.
pub const MIN_COMMON_DEFAULT: usize = 20;

/// Skew between two nodes, measured through the illumination stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairSkew {
    pub a: String,
    pub b: String,
    /// Common sequence numbers actually used.
    pub n: usize,
    /// Pairs discarded as implausible (see [`MAX_PLAUSIBLE_DIFF_NS`]).
    pub discarded: usize,
    /// Sequence numbers both nodes saw that fell OUTSIDE the fleet window.
    /// What the common window cost this pair, counted rather than swallowed.
    pub outside_window: usize,
    /// The fleet window this estimate was taken over, inclusive.
    pub seq_lo: u64,
    pub seq_hi: u64,
    /// **The point estimate**: median of `t_a − t_b`, +ve = `a` ahead.
    pub median_ns: i64,
    /// Median absolute deviation of the per-packet differences — the robust
    /// spread of the two nodes' combined RX jitter.
    pub mad_ns: u64,
    /// Observed envelope. Reported, never used as an estimator.
    pub min_ns: i64,
    pub max_ns: i64,
    /// Standard error of the median from the random component alone:
    /// `1.253 · (1.4826 · MAD) / √n`. Says nothing about the unobservable
    /// difference in the two nodes' pipeline floors.
    pub stderr_ns: u64,
    /// Median of the first half minus median of the second half. A clock step
    /// mid-window shows up here and nowhere else.
    pub half_split_delta_ns: i64,
    /// `false` when the difference series is not one distribution.
    pub stationary: bool,
}

impl PairSkew {
    /// The number a budget is checked against: the point estimate plus two
    /// standard errors, i.e. ~95% on the random component.
    pub fn worst_case_ns(&self) -> u64 {
        self.median_ns.unsigned_abs() + 2 * self.stderr_ns
    }

    /// Within budget **and** measured from a stationary series. A pair whose
    /// clock stepped mid-window is never certified, however small its median.
    pub fn within(&self, budget_ns: u64) -> bool {
        self.stationary && self.worst_case_ns() <= budget_ns
    }
}

fn median_i64(v: &mut [i64]) -> i64 {
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return 0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        // Averaged as i128 so two large epoch-scale values cannot overflow.
        ((v[n / 2 - 1] as i128 + v[n / 2] as i128) / 2) as i64
    }
}

/// The one sequence interval every pair in a fleet report is measured over.
///
/// Inclusive at both ends. See [`fleet_transfer_skew`] for why one window is
/// not a detail but the thing that makes the skews compose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqWindow {
    pub lo: u64,
    pub hi: u64,
}

impl SeqWindow {
    fn contains(&self, seq: u64) -> bool {
        seq >= self.lo && seq <= self.hi
    }
}

/// Earliest arrival per sequence number, in sequence order.
///
/// Duplicates keep the **earliest** sighting: a repeat is either a
/// retransmission or a driver re-delivery, and the first has the least delay in
/// it. BTreeMap so iteration is sequence-ordered, which is what the half-split
/// stationarity test needs.
fn seq_index(v: &[Arrival]) -> std::collections::BTreeMap<u64, u64> {
    let mut m = std::collections::BTreeMap::new();
    for x in v {
        m.entry(x.seq)
            .and_modify(|t: &mut u64| *t = (*t).min(x.unix_ts_ns))
            .or_insert(x.unix_ts_ns);
    }
    m
}

/// Per-packet differences for the sequence numbers both nodes saw **inside
/// `window`**.
///
/// Returns the differences, how many were dropped as implausible, and how many
/// shared sequence numbers fell outside the window. The last is reported rather
/// than swallowed: it is what the fleet window cost this pair.
fn common_differences(a: &[Arrival], b: &[Arrival], window: SeqWindow) -> (Vec<i64>, usize, usize) {
    let (ia, ib) = (seq_index(a), seq_index(b));
    let mut diffs = Vec::new();
    let mut discarded = 0usize;
    let mut outside_window = 0usize;
    for (seq, ta) in &ia {
        let Some(tb) = ib.get(seq) else { continue };
        if !window.contains(*seq) {
            outside_window += 1;
            continue;
        }
        let d = *ta as i128 - *tb as i128;
        if d.abs() > MAX_PLAUSIBLE_DIFF_NS as i128 {
            discarded += 1;
            continue;
        }
        diffs.push(d as i64);
    }
    (diffs, discarded, outside_window)
}

/// Estimate the skew between two nodes over one sequence `window`.
///
/// Returns `None` when fewer than `min_common` sequence numbers are shared
/// inside the window — two nodes that saw disjoint traffic are *unmeasured*,
/// and saying so is the whole point. A pair that had enough packets overall but
/// not inside the window is `None` for the same reason: an estimate taken over
/// a different interval than its peers is not comparable with them.
///
/// The window is a fleet-level fact, so [`fleet_transfer_skew`] computes it.
/// Measuring one isolated pair means passing that pair's own span.
pub fn pair_skew(
    a_name: &str,
    a: &[Arrival],
    b_name: &str,
    b: &[Arrival],
    min_common: usize,
    window: SeqWindow,
) -> Option<PairSkew> {
    let (diffs, discarded, outside_window) = common_differences(a, b, window);
    if diffs.len() < min_common.max(2) {
        return None;
    }
    let n = diffs.len();

    // Stationarity first, on the sequence-ordered series, before any sorting.
    let mut first: Vec<i64> = diffs[..n / 2].to_vec();
    let mut second: Vec<i64> = diffs[n / 2..].to_vec();
    let (m1, m2) = (median_i64(&mut first), median_i64(&mut second));
    let half_split_delta_ns = m1 - m2;
    // Compare the shift against the spread WITHIN each half, never against the
    // spread of the whole series: a clean step inflates the overall MAD by half
    // the step, so a whole-series tolerance would hide exactly the failure this
    // test exists to catch.
    let within_half = {
        let mut d1: Vec<i64> = first.iter().map(|d| (d - m1).abs()).collect();
        let mut d2: Vec<i64> = second.iter().map(|d| (d - m2).abs()).collect();
        median_i64(&mut d1).max(median_i64(&mut d2)).unsigned_abs()
    };

    let mut sorted = diffs;
    let median_ns = median_i64(&mut sorted);
    let min_ns = sorted[0];
    let max_ns = sorted[n - 1];

    let mut abs_dev: Vec<i64> = sorted.iter().map(|d| (d - median_ns).abs()).collect();
    let mad_ns = median_i64(&mut abs_dev).unsigned_abs();

    // σ ≈ 1.4826·MAD for a normal; SE(median) ≈ 1.253·σ/√n.
    let sigma = 1.4826 * mad_ns as f64;
    let stderr_ns = (1.253 * sigma / (n as f64).sqrt()).ceil() as u64;

    // A step is a shift between the halves that the within-half spread cannot
    // explain. 6× the within-half MAD is deliberately generous, with a 1 ms
    // floor so a noiseless synthetic stream does not flag on rounding: this
    // must catch steps, not jitter.
    let step_tolerance = (6 * within_half).max(1_000_000) as i64;
    let stationary = half_split_delta_ns.abs() <= step_tolerance;

    Some(PairSkew {
        a: a_name.to_string(),
        b: b_name.to_string(),
        n,
        discarded,
        outside_window,
        seq_lo: window.lo,
        seq_hi: window.hi,
        median_ns,
        mad_ns,
        min_ns,
        max_ns,
        stderr_ns,
        half_split_delta_ns,
        stationary,
    })
}

/// The fleet-level answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferReport {
    /// The transmitter whose stream this was measured on.
    pub tx_id: String,
    /// The one sequence window every pair below was measured over. `None` when
    /// the nodes share no common window, in which case `pairs` is empty.
    pub window: Option<SeqWindow>,
    /// Every pair that could be estimated.
    pub pairs: Vec<PairSkew>,
    /// Nodes that contributed no arrivals at all.
    pub silent: Vec<String>,
    /// Node pairs that shared too few sequence numbers.
    pub unpaired: Vec<(String, String)>,
    /// The worst pair by [`PairSkew::worst_case_ns`].
    pub worst: Option<PairSkew>,
    pub budget_ns: u64,
}

impl TransferReport {
    /// Certified only when every pair was measured, every pair is stationary,
    /// and the worst one fits the budget.
    pub fn within_budget(&self) -> bool {
        self.silent.is_empty()
            && self.unpaired.is_empty()
            && !self.pairs.is_empty()
            && self.pairs.iter().all(|p| p.within(self.budget_ns))
    }

    pub fn render(&self) -> String {
        let mut s = format!(
            "fleet skew via the illumination stream (transmitter {})\n\n",
            self.tx_id
        );
        match self.window {
            Some(w) => s.push_str(&format!(
                "  measured over the common sequence window [{}, {}] — one window for every \
                 pair,\n  so the skews compose\n\n",
                w.lo, w.hi
            )),
            None => s.push_str(
                "  NO COMMON SEQUENCE WINDOW — the nodes' streams overlap in a chain, not \
                 all at\n  once. Every pair is UNMEASURED, which is not the same as \
                 coherent.\n\n",
            ),
        }
        if self.pairs.is_empty() {
            s.push_str("  no node pair shared enough packets to estimate anything.\n");
        } else {
            s.push_str(&format!(
                "{:<3} {:<9} {:<9} {:>10} {:>9} {:>9} {:>8}  {}\n",
                "", "node A", "node B", "skew ms", "± ms", "MAD ms", "packets", "note"
            ));
            s.push_str(&"-".repeat(84));
            s.push('\n');
            for p in &self.pairs {
                let note = if !p.stationary {
                    format!(
                        "NOT STATIONARY — halves differ by {:.1} ms; a clock stepped",
                        p.half_split_delta_ns as f64 / 1e6
                    )
                } else if p.discarded > 0 {
                    format!("{} pair(s) discarded as implausible", p.discarded)
                } else if p.outside_window > 0 {
                    format!(
                        "{} shared packet(s) outside the fleet window",
                        p.outside_window
                    )
                } else {
                    String::new()
                };
                s.push_str(&format!(
                    "{:<3} {:<9} {:<9} {:>10.3} {:>9.3} {:>9.3} {:>8}  {}\n",
                    if p.within(self.budget_ns) { "OK" } else { "XX" },
                    p.a,
                    p.b,
                    p.median_ns as f64 / 1e6,
                    2.0 * p.stderr_ns as f64 / 1e6,
                    p.mad_ns as f64 / 1e6,
                    p.n,
                    note,
                ));
            }
        }

        s.push('\n');
        match &self.worst {
            Some(w) => s.push_str(&format!(
                "worst pair: {} ↔ {} at {:+.3} ms, worst case {:.3} ms against a {:.0} ms budget\n",
                w.a,
                w.b,
                w.median_ns as f64 / 1e6,
                w.worst_case_ns() as f64 / 1e6,
                self.budget_ns as f64 / 1e6,
            )),
            None => s.push_str("worst pair: NOT MEASURED\n"),
        }
        if !self.silent.is_empty() {
            s.push_str(&format!(
                "  SILENT (no stamped frames received): {} — these nodes are UNMEASURED, \
                 not coherent\n",
                self.silent.join(", ")
            ));
        }
        if !self.unpaired.is_empty() {
            let list: Vec<String> = self
                .unpaired
                .iter()
                .map(|(a, b)| format!("{a}↔{b}"))
                .collect();
            s.push_str(&format!(
                "  TOO FEW COMMON PACKETS: {} — widen --window or check that both nodes are \
                 on the illuminator's channel\n",
                list.join(", ")
            ));
        }
        s.push_str(
            "\nThe transmit instant cancels exactly (one frame, many receivers), so this is \
             NOT\nbounded by round-trip time the way `csid fleet clock` is. What it cannot \
             separate\nfrom real skew is a fixed difference between the nodes' RX pipeline \
             floors — a\ncommon-mode term on identical hardware running one image, and a real \
             bias on a\nmixed fleet.\n",
        );
        s
    }
}

/// The one sequence window every pair must be measured over.
///
/// **Why one window.** Left to itself each pair spans whatever interval its own
/// two nodes happened to share, and the skews then STOP COMPOSING: a pair that
/// ran twice as long carries twice as much accumulated relative drift in its
/// median, so `skew(x,y) + skew(y,z) − skew(x,z)` is not zero. Measured on the
/// `coex-03_20260823-101235` arm, where two nodes heard to seq 74,587 and four
/// peers stopped at 30,366, the triangles containing that long pair missed
/// closure by 42–62 µs against 1–11 µs for the co-windowed ones.
///
/// **Which nodes get a vote.** The window is the intersection of the spans of
/// the nodes that can be composed onto the **anchor** — the best-heard node,
/// ties broken on name so the answer never depends on iteration order. A node
/// sharing fewer than `min_common` sequence numbers with the anchor is left out
/// of the vote deliberately: it cannot be placed on the anchor's timeline under
/// any window, so letting it clamp one would destroy every measurable pair in
/// order to describe none. A node that merely joined **late** does get a vote,
/// and does clamp — it is composable, and composability is the point.
///
/// `None` when the voting nodes' spans have no sequence in common at all: a
/// chain rather than a window. Every pair is then unmeasured, which is not the
/// same as the fleet being coherent.
fn common_window(heard: &[(&str, &[Arrival])], min_common: usize) -> Option<SeqWindow> {
    let floor = min_common.max(2);
    let indexed: Vec<(&str, std::collections::BTreeMap<u64, u64>)> = heard
        .iter()
        .map(|(name, a)| (*name, seq_index(a)))
        .filter(|(_, ix)| ix.len() >= floor)
        .collect();
    // Best-heard first, then by name: a deterministic anchor.
    let anchor = indexed
        .iter()
        .min_by(|x, y| y.1.len().cmp(&x.1.len()).then(x.0.cmp(y.0)))?;

    let mut lo = u64::MIN;
    let mut hi = u64::MAX;
    for (name, ix) in &indexed {
        let composable =
            *name == anchor.0 || ix.keys().filter(|s| anchor.1.contains_key(*s)).count() >= floor;
        if !composable {
            continue;
        }
        lo = lo.max(*ix.keys().next()?);
        hi = hi.min(*ix.keys().next_back()?);
    }
    (lo <= hi).then_some(SeqWindow { lo, hi })
}

/// Compute every pair's skew from per-node arrival streams.
///
/// `nodes` is `(name, arrivals)` for one transmitter. Nodes are compared in
/// name order so the output is stable between runs.
///
/// Every pair is measured over the single window [`common_window`] picks, so
/// the estimates compose: a node's offset via the reference is the same number
/// whichever peer you route it through, to within the RX-floor term this method
/// cannot see.
pub fn fleet_transfer_skew(
    tx_id: &str,
    nodes: &[(String, Vec<Arrival>)],
    budget_ns: u64,
    min_common: usize,
) -> TransferReport {
    let mut sorted: Vec<&(String, Vec<Arrival>)> = nodes.iter().collect();
    sorted.sort_by(|x, y| x.0.cmp(&y.0));

    let silent: Vec<String> = sorted
        .iter()
        .filter(|(_, a)| a.is_empty())
        .map(|(n, _)| n.clone())
        .collect();
    let heard: Vec<(&str, &[Arrival])> = sorted
        .iter()
        .filter(|(_, a)| !a.is_empty())
        .map(|(n, a)| (n.as_str(), a.as_slice()))
        .collect();

    let window = common_window(&heard, min_common);

    let mut pairs = Vec::new();
    let mut unpaired = Vec::new();
    for i in 0..heard.len() {
        for j in (i + 1)..heard.len() {
            let (an, aa) = heard[i];
            let (bn, bb) = heard[j];
            // No common window means no comparable estimate for anyone. Fail
            // closed: every pair is unpaired, so `within_budget` is false.
            match window.and_then(|w| pair_skew(an, aa, bn, bb, min_common, w)) {
                Some(p) => pairs.push(p),
                None => unpaired.push((an.to_string(), bn.to_string())),
            }
        }
    }

    let worst = pairs.iter().max_by_key(|p| p.worst_case_ns()).cloned();

    TransferReport {
        tx_id: tx_id.to_string(),
        window,
        pairs,
        silent,
        unpaired,
        worst,
        budget_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::gates::G4B_BUDGET_NS;

    /// A window that clips nothing. The tests below pin the ESTIMATOR, so they
    /// must see exactly the series they build; the window has its own tests.
    const ALL: SeqWindow = SeqWindow {
        lo: u64::MIN,
        hi: u64::MAX,
    };

    const T0: u64 = 1_786_000_000_000_000_000;
    /// 25 Hz, the injector's default pace.
    const PERIOD_NS: u64 = 40_000_000;

    /// splitmix64 — the seed has to mix into the HIGH bits, or two "different"
    /// streams get correlated jitter and every difference collapses to zero.
    fn mix(i: u64, seed: u64) -> u64 {
        let mut z = i
            .wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A node's view of the injector: true arrival plus its own clock offset
    /// plus a deterministic, bounded, non-negative RX delay.
    fn stream(n: usize, offset_ns: i64, jitter_ns: u64, seed: u64) -> Vec<Arrival> {
        (0..n as u64)
            .map(|i| {
                let jitter = mix(i, seed) % (jitter_ns + 1);
                Arrival {
                    seq: i,
                    unix_ts_ns: (T0 as i64 + (i * PERIOD_NS) as i64 + offset_ns + jitter as i64)
                        as u64,
                }
            })
            .collect()
    }

    /// A node whose clock walks away at a constant rate, with no jitter, and an
    /// optional first sequence number. Relative drift is the whole point of the
    /// common-window rule: it is what makes a median depend on the interval it
    /// was taken over.
    fn drifting(first_seq: u64, n: u64, offset_ns: i64, drift_ns_per_seq: i64) -> Vec<Arrival> {
        (first_seq..first_seq + n)
            .map(|i| Arrival {
                seq: i,
                unix_ts_ns: (T0 as i64
                    + (i * PERIOD_NS) as i64
                    + offset_ns
                    + drift_ns_per_seq * i as i64) as u64,
            })
            .collect()
    }

    fn named(v: &[(&str, Vec<Arrival>)]) -> Vec<(String, Vec<Arrival>)> {
        v.iter().map(|(n, a)| (n.to_string(), a.clone())).collect()
    }

    fn skew_of(r: &TransferReport, a: &str, b: &str) -> i64 {
        r.pairs
            .iter()
            .find(|p| p.a == a && p.b == b)
            .unwrap_or_else(|| panic!("no pair {a}↔{b}"))
            .median_ns
    }

    #[test]
    fn pairs_measured_over_different_windows_would_not_compose() {
        // THE DEFECT, stated as arithmetic. Two nodes run to seq 999, a third
        // stops at 299. The second drifts 1 µs per sequence against the first,
        // so its median is -149,500 ns across 0..299 and -499,500 ns across
        // 0..999 — it depends entirely on the interval it was taken over.
        //
        // Measured per pair, `skew(a,b) + skew(b,c) - skew(a,c)` misses zero by
        // 350 µs. That is the 42-62 µs non-closure seen on the real
        // coex-03_20260823-101235 arm, reproduced large enough to be
        // unmistakable.
        let a = drifting(0, 1000, 0, 0);
        let b = drifting(0, 1000, 0, 1_000);
        let c = drifting(0, 300, 7_000_000, 0);

        // What the old per-pair behaviour produced, still reachable by asking
        // for one isolated pair over its own span.
        let long_pair = pair_skew("a", &a, "b", &b, 20, ALL).unwrap().median_ns;
        assert_eq!(long_pair, -499_500, "a pair left to its own span drifts");

        let r = fleet_transfer_skew(
            "tx",
            &named(&[("a", a), ("b", b), ("c", c)]),
            G4B_BUDGET_NS,
            20,
        );
        assert_eq!(r.window, Some(SeqWindow { lo: 0, hi: 299 }));
        assert_eq!(
            skew_of(&r, "a", "b"),
            -149_500,
            "the long pair must be measured over the SHARED window, not its own"
        );

        let closure = skew_of(&r, "a", "b") + skew_of(&r, "b", "c") - skew_of(&r, "a", "c");
        assert_eq!(
            closure, 0,
            "one window means the skews compose; off by {closure} ns"
        );
    }

    #[test]
    fn a_node_that_heard_a_disjoint_stretch_does_not_clamp_the_window() {
        // The rule must not cure the disease by killing the patient. A node
        // sharing no sequence numbers with anyone cannot be composed onto the
        // anchor under ANY window, so it gets no vote. Letting it vote would
        // empty the window and take every measurable pair down with it.
        let a = stream(300, 0, 1_000, 1);
        let b = stream(300, 4_000_000, 1_000, 2);
        let orphan: Vec<Arrival> = stream(300, 0, 1_000, 3)
            .into_iter()
            .map(|x| Arrival {
                seq: x.seq + 50_000,
                ..x
            })
            .collect();

        let r = fleet_transfer_skew(
            "tx",
            &named(&[("monad01", a), ("monad02", b), ("monad07", orphan)]),
            G4B_BUDGET_NS,
            20,
        );
        assert_eq!(r.window, Some(SeqWindow { lo: 0, hi: 299 }));
        assert_eq!(r.pairs.len(), 1, "the measurable pair must survive");
        assert!((skew_of(&r, "monad01", "monad02") + 4_000_000).abs() < 100_000);
        // The orphan is still reported as unmeasured, never as coherent.
        assert_eq!(r.unpaired.len(), 2);
        assert!(!r.within_budget());
    }

    #[test]
    fn a_late_joiner_clamps_the_window_and_the_cost_is_reported() {
        // A node that joined halfway DOES clamp, because it can be composed
        // onto the anchor and composability is the point. What that cost is not
        // silent: `outside_window` counts the shared packets the window dropped.
        let a = stream(1000, 0, 1_000, 4);
        let b = stream(1000, 2_000_000, 1_000, 5);
        let late: Vec<Arrival> = stream(500, 0, 1_000, 6)
            .into_iter()
            .map(|x| Arrival {
                seq: x.seq + 500,
                ..x
            })
            .collect();

        let r = fleet_transfer_skew(
            "tx",
            &named(&[("monad01", a), ("monad02", b), ("monad03", late)]),
            G4B_BUDGET_NS,
            20,
        );
        assert_eq!(r.window, Some(SeqWindow { lo: 500, hi: 999 }));
        let long = r
            .pairs
            .iter()
            .find(|p| p.a == "monad01" && p.b == "monad02")
            .unwrap();
        assert_eq!(long.n, 500);
        assert_eq!(
            long.outside_window, 500,
            "the discarded half must be counted, not dropped quietly"
        );
        assert_eq!((long.seq_lo, long.seq_hi), (500, 999));
    }

    #[test]
    fn a_chain_of_overlaps_is_not_a_window_and_certifies_nothing() {
        // Both peers share well over `min_common` with the anchor, so both get a
        // vote — but they sit at opposite ends of it and share nothing with each
        // other. There is no interval every pair can be measured over, so the
        // report FAILS CLOSED: no pairs, no certification. Reporting the two
        // measurable pairs on different intervals is the exact bug this rule
        // exists to remove.
        let wide = drifting(0, 1000, 0, 0); //     0..999, far the best-heard
        let early = drifting(0, 50, 1_000_000, 0); //  0..49
        let late = drifting(950, 50, 2_000_000, 0); // 950..999

        let r = fleet_transfer_skew(
            "tx",
            &named(&[("a_early", early), ("b_wide", wide), ("c_late", late)]),
            G4B_BUDGET_NS,
            20,
        );
        assert_eq!(r.window, None);
        assert!(r.pairs.is_empty());
        assert_eq!(r.unpaired.len(), 3);
        assert!(!r.within_budget(), "an unmeasured fleet is never certified");
        assert!(r.render().contains("NO COMMON SEQUENCE WINDOW"));
    }

    #[test]
    fn a_known_offset_is_recovered_through_the_jitter() {
        // B is 12 ms behind A; both have up to 400 µs of RX jitter.
        let a = stream(600, 0, 400_000, 1);
        let b = stream(600, -12_000_000, 400_000, 2);
        let p = pair_skew("monad01", &a, "monad02", &b, MIN_COMMON_DEFAULT, ALL).unwrap();
        assert_eq!(p.n, 600);
        assert!(
            (p.median_ns - 12_000_000).abs() < 200_000,
            "median {} should be within 200 µs of 12 ms",
            p.median_ns
        );
        assert!(p.stationary);
        // The whole point: the estimate is microsecond-scale, not RTT-scale.
        assert!(p.stderr_ns < 50_000, "stderr {} ns", p.stderr_ns);
        assert!(p.within(G4B_BUDGET_NS));
    }

    /// The minimum is what an NTP-style filter would pick, and it is biased by
    /// the whole width of the other node's jitter. This test pins the reason
    /// the median is the estimator.
    #[test]
    fn the_minimum_is_biased_low_and_the_median_is_not() {
        let a = stream(800, 0, 2_000_000, 11);
        let b = stream(800, 0, 2_000_000, 97);
        let p = pair_skew("a", &a, "b", &b, MIN_COMMON_DEFAULT, ALL).unwrap();
        assert!(
            p.median_ns.abs() < 150_000,
            "two coherent nodes must estimate ~0, got {}",
            p.median_ns
        );
        assert!(
            p.min_ns < -1_000_000,
            "the minimum should be dragged down by ~a full jitter width, got {}",
            p.min_ns
        );
        assert!(p.max_ns > 1_000_000, "and the maximum up, got {}", p.max_ns);
    }

    /// A clock step mid-window makes the median describe neither half. It must
    /// never be certified, however small the median happens to land.
    #[test]
    fn a_clock_step_mid_session_is_detected_and_never_certified() {
        let a = stream(400, 0, 100_000, 3);
        let mut b = stream(400, 0, 100_000, 4);
        // Halfway through, B's clock steps back 300 ms.
        for x in b.iter_mut().skip(200) {
            x.unix_ts_ns -= 300_000_000;
        }
        let p = pair_skew("a", &a, "b", &b, MIN_COMMON_DEFAULT, ALL).unwrap();
        assert!(!p.stationary, "a 300 ms step must show in the half split");
        assert!(
            p.half_split_delta_ns.abs() > 250_000_000,
            "{}",
            p.half_split_delta_ns
        );
        assert!(
            !p.within(G4B_BUDGET_NS),
            "median {} happens to be ~150 ms, but the series is two distributions",
            p.median_ns
        );
        let text = fleet_transfer_skew(
            "ef:be:ad:de:ad:de",
            &[("a".into(), a), ("b".into(), b)],
            G4B_BUDGET_NS,
            MIN_COMMON_DEFAULT,
        )
        .render();
        assert!(text.contains("NOT STATIONARY"), "{text}");
        assert!(text.contains("a clock stepped"), "{text}");
    }

    #[test]
    fn nodes_with_no_common_sequence_numbers_are_unmeasured_not_coherent() {
        let a = stream(100, 0, 1000, 5);
        let b: Vec<Arrival> = stream(100, 0, 1000, 6)
            .into_iter()
            .map(|x| Arrival {
                seq: x.seq + 10_000,
                ..x
            })
            .collect();
        assert!(pair_skew("a", &a, "b", &b, MIN_COMMON_DEFAULT, ALL).is_none());

        let r = fleet_transfer_skew(
            "tx",
            &[("a".into(), a), ("b".into(), b)],
            G4B_BUDGET_NS,
            MIN_COMMON_DEFAULT,
        );
        assert!(r.pairs.is_empty());
        assert_eq!(r.unpaired, vec![("a".to_string(), "b".to_string())]);
        assert!(!r.within_budget());
        assert!(
            r.render().contains("TOO FEW COMMON PACKETS"),
            "{}",
            r.render()
        );
    }

    /// A one-node "fleet" has no pair. It must not read as a zero skew.
    #[test]
    fn one_node_produces_no_pair_and_is_not_certified() {
        let r = fleet_transfer_skew(
            "tx",
            &[("monad04".into(), stream(500, 0, 1000, 7))],
            G4B_BUDGET_NS,
            MIN_COMMON_DEFAULT,
        );
        assert!(r.pairs.is_empty());
        assert!(r.worst.is_none());
        assert!(!r.within_budget(), "one node is not a coherent fleet");
        assert!(r.render().contains("NOT MEASURED"), "{}", r.render());
    }

    /// A node that heard nothing must not be quietly dropped from the roll-up.
    #[test]
    fn a_silent_node_makes_the_fleet_unmeasured() {
        let r = fleet_transfer_skew(
            "tx",
            &[
                ("monad01".into(), stream(400, 0, 100_000, 8)),
                ("monad02".into(), stream(400, 1_000_000, 100_000, 9)),
                ("monad07".into(), Vec::new()),
            ],
            G4B_BUDGET_NS,
            MIN_COMMON_DEFAULT,
        );
        assert_eq!(r.pairs.len(), 1);
        assert_eq!(r.silent, vec!["monad07"]);
        assert!(!r.within_budget());
        let text = r.render();
        assert!(text.contains("SILENT"), "{text}");
        assert!(text.contains("UNMEASURED, not coherent"), "{text}");
    }

    /// Sequence numbers restart at 0 when the injector restarts. Matching a
    /// seq from run 1 against the same seq from run 2 would fabricate a skew of
    /// however long the gap was.
    #[test]
    fn sequence_reuse_across_an_injector_restart_is_discarded_not_averaged() {
        let a = stream(300, 0, 100_000, 10);
        let mut b = stream(300, 0, 100_000, 12);
        // B's capture only started an hour later, so its seq 0.. is a different run.
        for x in b.iter_mut() {
            x.unix_ts_ns += 3_600_000_000_000;
        }
        assert!(pair_skew("a", &a, "b", &b, MIN_COMMON_DEFAULT, ALL).is_none());

        // Half the packets are from the same run; those alone must decide.
        let mut mixed = stream(300, 5_000_000, 100_000, 13);
        for x in mixed.iter_mut().skip(150) {
            x.unix_ts_ns += 3_600_000_000_000;
        }
        let p = pair_skew("a", &a, "b", &mixed, 20, ALL).unwrap();
        assert_eq!(p.n, 150);
        assert_eq!(p.discarded, 150);
        assert!((p.median_ns + 5_000_000).abs() < 200_000, "{}", p.median_ns);
    }

    #[test]
    fn duplicate_deliveries_keep_the_earliest_sighting() {
        let a = vec![
            Arrival {
                seq: 1,
                unix_ts_ns: T0 + 5_000,
            },
            Arrival {
                seq: 1,
                unix_ts_ns: T0 + 900_000,
            },
            Arrival {
                seq: 2,
                unix_ts_ns: T0 + PERIOD_NS,
            },
        ];
        let b = vec![
            Arrival {
                seq: 1,
                unix_ts_ns: T0,
            },
            Arrival {
                seq: 2,
                unix_ts_ns: T0 + PERIOD_NS,
            },
        ];
        let (d, _, _) = common_differences(&a, &b, ALL);
        assert_eq!(d, vec![5_000, 0], "the 900 µs re-delivery must not win");
    }

    /// Skew is antisymmetric: nothing in the estimator may privilege one side.
    #[test]
    fn the_estimate_is_antisymmetric() {
        let a = stream(400, 3_000_000, 200_000, 21);
        let b = stream(400, -3_000_000, 200_000, 21);
        let ab = pair_skew("a", &a, "b", &b, 20, ALL).unwrap();
        let ba = pair_skew("b", &b, "a", &a, 20, ALL).unwrap();
        assert_eq!(ab.median_ns, -ba.median_ns);
        assert_eq!(ab.mad_ns, ba.mad_ns);
    }

    #[test]
    fn a_fleet_within_budget_certifies_and_one_outside_does_not() {
        let ok = fleet_transfer_skew(
            "tx",
            &[
                ("monad01".into(), stream(600, 0, 200_000, 31)),
                ("monad02".into(), stream(600, 2_000_000, 200_000, 32)),
                ("monad03".into(), stream(600, -1_500_000, 200_000, 33)),
            ],
            G4B_BUDGET_NS,
            MIN_COMMON_DEFAULT,
        );
        assert_eq!(ok.pairs.len(), 3);
        assert!(ok.within_budget(), "{}", ok.render());
        assert!(
            ok.render().contains("worst pair: monad02 ↔ monad03"),
            "{}",
            ok.render()
        );

        let bad = fleet_transfer_skew(
            "tx",
            &[
                ("monad01".into(), stream(600, 0, 200_000, 41)),
                ("monad02".into(), stream(600, 400_000_000, 200_000, 42)),
            ],
            G4B_BUDGET_NS,
            MIN_COMMON_DEFAULT,
        );
        assert!(!bad.within_budget());
        assert!(bad.render().contains("XX "), "{}", bad.render());
    }
}
