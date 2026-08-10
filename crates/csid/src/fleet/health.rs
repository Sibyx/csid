//! The per-node health record and the fleet roll-up.
//!
//! ## The invariant this module exists to enforce
//!
//! **A node that could not be reached is `UNKNOWN`, never `OK`.**
//!
//! This is not defensive pedantry. The failure the cockpit exists to prevent is
//! an operator glancing at a green table and starting a 90-minute staged block
//! while one node's capture is dead. Any table that renders "no bad news" and
//! "no news" the same way produces exactly that. So absence of a measurement is
//! its own state, it sorts *worse* than `OK`, and a fleet containing one
//! unknown node cannot report a healthy fleet.
//!
//! The states, in the order they escalate:
//!
//! | State | Meaning | Roll-up |
//! |---|---|---|
//! | `OK` | measured, inside every budget | the only state that means "go" |
//! | `WARN` | measured, outside a soft budget (thermal headroom, disk, BLE quiet) | proceed, but fix it |
//! | `UNKNOWN` | not measured — unreachable, no session, too few records | **never** "go" |
//! | `FAIL` | measured, outside a hard budget (capture dead, gate floor missed) | stop |
//!
//! `UNKNOWN` sorting below `FAIL` is deliberate: a node that is definitively
//! broken is a smaller problem than a node nobody can see, but a fleet-level
//! verdict must surface the definite failure first because it is actionable.
//! Both block a "healthy" verdict.

use serde::{Deserialize, Serialize};

use super::stats::Ci;

/// A node's overall state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Measured and inside budget.
    Ok,
    /// Measured, outside a soft budget.
    Warn,
    /// Not measured. Never a pass.
    Unknown,
    /// Measured, outside a hard budget.
    Fail,
}

impl State {
    /// The two-character bench mark. Colour is an *addition* to this, never a
    /// replacement: the table has to survive a projector, a phone photo, a
    /// colour-blind operator and a piped `| tee`.
    pub fn mark(self) -> &'static str {
        match self {
            State::Ok => "OK",
            State::Warn => "!!",
            State::Unknown => "??",
            State::Fail => "XX",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Ok => "OK",
            State::Warn => "WARN",
            State::Unknown => "UNKNOWN",
            State::Fail => "FAIL",
        }
    }

    /// Fold two states to the more serious one.
    pub fn worse(self, other: State) -> State {
        if other > self {
            other
        } else {
            self
        }
    }

    /// Whether this state permits starting a staged block.
    pub fn is_go(self) -> bool {
        matches!(self, State::Ok)
    }
}

/// Why a node is not reporting. Kept as a first-class value so the table can
/// print the reason next to `??` rather than an empty cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unreachable {
    /// ssh could not establish a session.
    SshFailed(String),
    /// ssh worked; the remote command failed.
    RemoteFailed { code: Option<i32>, stderr: String },
    /// ssh worked, the command ran, the output was not parseable.
    BadOutput(String),
    /// The command did not return inside the deadline.
    TimedOut,
}

impl Unreachable {
    pub fn summary(&self) -> String {
        match self {
            Unreachable::SshFailed(e) => format!("ssh: {}", first_line(e)),
            Unreachable::RemoteFailed { code, stderr } => match code {
                Some(c) => format!("exit {c}: {}", first_line(stderr)),
                None => format!("signalled: {}", first_line(stderr)),
            },
            Unreachable::BadOutput(e) => format!("unparseable: {}", first_line(e)),
            Unreachable::TimedOut => "timed out".to_string(),
        }
    }
}

fn first_line(s: &str) -> String {
    let t = s.trim();
    let line = t.lines().next().unwrap_or("");
    if line.len() > 72 {
        format!("{}…", &line[..71])
    } else {
        line.to_string()
    }
}

/// The record class + source MAC a rate/CV measurement was scoped to.
///
/// The pre-registration scopes G1 and G2 to "records scoped to a single source
/// MAC and a single CSI record class" (§3, G1/G2). An unscoped rate on an
/// ambient channel is a sum over several interleaved transmitters and PHY
/// types, which is not a measurement of any link. Carrying the scope on the
/// health record means the table can print it, and the operator can see when a
/// node has silently scoped itself to the wrong transmitter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// e.g. `52:legacyofdm`.
    pub class: String,
    /// e.g. `ef:be:ad:de:ad:de` — the fleet's illuminator sentinel.
    pub src_mac: String,
    /// Records in the window that matched the scope.
    pub scoped_records: u64,
    /// Records in the window in total, so the operator sees how much of the
    /// channel the scope threw away.
    pub window_records: u64,
}

/// BLE co-capture liveness, as seen right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BleHealth {
    /// `ok` / `degraded` / `failed` / `disabled`.
    pub status: String,
    pub observations: u64,
    pub rate_hz: f64,
    pub max_gap_s: f64,
    pub scan_restarts: u64,
    pub unparsed_events: u64,
}

/// Disk headroom on the spool filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DiskHealth {
    pub free_gb: f64,
    pub total_gb: f64,
    /// Hours of capture the free space affords at the observed byte rate.
    /// `None` when nothing is being written, so there is no rate to divide by.
    pub hours_left: Option<f64>,
}

/// SoC temperature and the firmware throttle word.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalHealth {
    pub temp_c: Option<f32>,
    /// Degrees below the 80 °C soft limit; negative means the clock is being
    /// taken away.
    pub headroom_c: Option<f32>,
    pub throttled_now: bool,
    pub throttled_since_boot: bool,
    pub detail: String,
}

/// Clock health for one node, relative to the cockpit's reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockHealth {
    /// Offset of the node clock from the reference, nanoseconds, +ve = node
    /// ahead.
    pub offset_ns: i64,
    /// One-sided uncertainty on `offset_ns`. Reported, never hidden: an offset
    /// of 3 ms ± 40 ms is not a 3 ms offset.
    pub uncertainty_ns: u64,
    /// Round trips actually completed.
    pub samples: usize,
    /// What the node's own time daemon says about itself, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp: Option<NtpState>,
}

impl ClockHealth {
    /// The bound the budget is checked against: an offset is only inside the
    /// budget if it is inside it *including* its uncertainty.
    pub fn worst_case_ns(&self) -> u64 {
        self.offset_ns
            .unsigned_abs()
            .saturating_add(self.uncertainty_ns)
    }
}

/// A node's own time-daemon view of itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NtpState {
    pub daemon: String,
    pub synchronised: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stratum: Option<u32>,
    /// The daemon's own estimate of its offset from true time, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_offset_s: Option<f64>,
    /// The daemon's own error bound, seconds. Used as a floor on the
    /// uncertainty: our round-trip bound cannot be better than the node's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dispersion_s: Option<f64>,
}

/// The budgets a node is graded against. Every one is configurable, and every
/// one is printed with the verdict so a threshold is never invisible.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budgets {
    /// G1's registered floor, Hz. A node below it on the point estimate is
    /// FAIL; the gate command decides on the CI bound.
    pub delivered_floor_hz: f64,
    /// G2's registered ceiling.
    pub cv_ceiling: f64,
    /// Below this, WARN. The lab session's BLE arm needs a live scanner.
    pub ble_min_rate_hz: f64,
    /// Below this, WARN; below half of it, FAIL.
    pub disk_min_free_gb: f64,
    /// Below this many degrees of headroom to the 80 °C soft limit, WARN.
    pub thermal_min_headroom_c: f32,
    /// G4b's budget: 250 ms. Above it, FAIL.
    pub clock_budget_ns: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            delivered_floor_hz: super::gates::G1_FLOOR_HZ,
            cv_ceiling: super::gates::G2_CV_CEILING,
            ble_min_rate_hz: 1.0,
            disk_min_free_gb: 5.0,
            thermal_min_headroom_c: 5.0,
            clock_budget_ns: super::gates::G4B_BUDGET_NS,
        }
    }
}

/// Everything the cockpit knows about one node at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeHealth {
    pub host: String,
    /// `None` when the node answered. `Some` is the whole story: every
    /// measurement below is absent and the state is UNKNOWN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreachable: Option<Unreachable>,

    /// The session the node is currently writing, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    /// Records are still arriving. A sidecar saying `capturing` is not enough:
    /// a starving capture pings the watchdog exactly like a healthy one.
    pub capture_alive: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    /// Delivered rate over the probe window, Hz, with its interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_hz: Option<Ci>,
    /// Inter-arrival CV over the probe window, with its interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interarrival_cv: Option<Ci>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ble: Option<BleHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockHealth>,

    /// Human-readable reasons behind the state, most serious first.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl NodeHealth {
    /// A node we could not talk to. This is the only constructor that skips
    /// every measurement, and it is the only one that can produce UNKNOWN from
    /// a transport failure.
    pub fn unreachable(host: impl Into<String>, why: Unreachable) -> Self {
        NodeHealth {
            host: host.into(),
            unreachable: Some(why),
            session_id: None,
            experiment: None,
            capture_alive: false,
            scope: None,
            delivered_hz: None,
            interarrival_cv: None,
            ble: None,
            disk: None,
            thermal: None,
            clock: None,
            notes: Vec::new(),
        }
    }

    /// Grade the node against the budgets.
    ///
    /// Returns the state and, as a side effect, the ordered reasons. Pure: it
    /// touches nothing but `self` and `budgets`, which is what makes it
    /// testable without a fleet.
    pub fn grade(&mut self, budgets: &Budgets) -> State {
        self.notes.clear();

        if let Some(why) = &self.unreachable {
            self.notes.push(format!("not measured — {}", why.summary()));
            return State::Unknown;
        }

        let mut state = State::Ok;

        // -- capture liveness: the hard one -------------------------------
        if self.session_id.is_none() {
            self.notes.push("no session on this node".into());
            return State::Unknown;
        }
        if !self.capture_alive {
            self.notes.push("capture is NOT producing records".into());
            state = state.worse(State::Fail);
        }

        // -- delivered rate ------------------------------------------------
        match &self.delivered_hz {
            None => {
                self.notes
                    .push("delivered rate not measurable (too few scoped records)".into());
                state = state.worse(State::Unknown);
            }
            Some(ci) => {
                if ci.lo < budgets.delivered_floor_hz {
                    let verdict = if ci.point < budgets.delivered_floor_hz {
                        State::Fail
                    } else {
                        // Point clears, bound does not — the exact case the CI
                        // contract exists for. Not yet a hard failure at glance
                        // level, but the gate will refuse it.
                        State::Warn
                    };
                    self.notes.push(format!(
                        "delivered {} Hz — 95% CI lower bound below the {} Hz floor",
                        ci.render(1),
                        budgets.delivered_floor_hz
                    ));
                    state = state.worse(verdict);
                }
            }
        }

        // -- inter-arrival regularity --------------------------------------
        match &self.interarrival_cv {
            None => {
                self.notes
                    .push("inter-arrival CV not measurable (too few gaps)".into());
                state = state.worse(State::Unknown);
            }
            Some(ci) => {
                if ci.hi >= budgets.cv_ceiling {
                    let verdict = if ci.point >= budgets.cv_ceiling {
                        State::Fail
                    } else {
                        State::Warn
                    };
                    self.notes.push(format!(
                        "inter-arrival CV {} — 95% CI upper bound at or above the {} ceiling",
                        ci.render(2),
                        budgets.cv_ceiling
                    ));
                    state = state.worse(verdict);
                }
            }
        }

        // -- clock: hard, because it invalidates everything downstream ------
        match &self.clock {
            None => {
                // Not a node defect: the offset is measured *by the cockpit*,
                // against the cockpit, so a node-local probe cannot produce it.
                // It still costs the node its OK, because a node whose clock
                // nobody has checked is a node whose records cannot be pooled
                // with anyone else's.
                self.notes.push(
                    "clock offset not measured — this is a cockpit-side measurement; \
                     run `csid fleet clock` from the bench"
                        .into(),
                );
                state = state.worse(State::Unknown);
            }
            Some(c) => {
                if c.worst_case_ns() > budgets.clock_budget_ns {
                    self.notes.push(format!(
                        "clock offset {:+.1} ms ± {:.1} ms — outside the {:.0} ms budget",
                        c.offset_ns as f64 / 1e6,
                        c.uncertainty_ns as f64 / 1e6,
                        budgets.clock_budget_ns as f64 / 1e6
                    ));
                    state = state.worse(State::Fail);
                }
                if let Some(ntp) = &c.ntp {
                    if !ntp.synchronised {
                        self.notes
                            .push(format!("{} reports NOT synchronised", ntp.daemon));
                        state = state.worse(State::Fail);
                    }
                }
            }
        }

        // -- BLE: soft. A dead scanner costs the anchor arm, not the capture.
        if let Some(b) = &self.ble {
            match b.status.as_str() {
                "disabled" => {}
                "failed" => {
                    self.notes.push("BLE co-capture FAILED".into());
                    state = state.worse(State::Fail);
                }
                _ if b.rate_hz < budgets.ble_min_rate_hz => {
                    self.notes.push(format!(
                        "BLE quiet: {:.1} obs/s (max gap {:.1} s, {} restarts)",
                        b.rate_hz, b.max_gap_s, b.scan_restarts
                    ));
                    state = state.worse(State::Warn);
                }
                "degraded" => {
                    self.notes.push(format!(
                        "BLE degraded: {} restarts, max gap {:.1} s",
                        b.scan_restarts, b.max_gap_s
                    ));
                    state = state.worse(State::Warn);
                }
                _ => {}
            }
        }

        // -- disk ----------------------------------------------------------
        if let Some(d) = &self.disk {
            if d.free_gb < budgets.disk_min_free_gb / 2.0 {
                self.notes
                    .push(format!("disk critically low: {:.1} GB free", d.free_gb));
                state = state.worse(State::Fail);
            } else if d.free_gb < budgets.disk_min_free_gb {
                self.notes
                    .push(format!("disk low: {:.1} GB free", d.free_gb));
                state = state.worse(State::Warn);
            }
        }

        // -- thermal -------------------------------------------------------
        if let Some(t) = &self.thermal {
            if t.throttled_now {
                self.notes
                    .push(format!("firmware is throttling NOW: {}", t.detail));
                state = state.worse(State::Fail);
            } else if let Some(h) = t.headroom_c {
                if h < budgets.thermal_min_headroom_c {
                    self.notes.push(format!(
                        "only {h:+.1} °C of headroom to the 80 °C soft limit"
                    ));
                    state = state.worse(State::Warn);
                }
            }
        }

        // Most serious first, so the table's one-line reason is the right one.
        state
    }
}

/// The whole fleet at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetReport {
    pub taken_at_unix_ns: u64,
    pub window_s: f64,
    pub budgets: Budgets,
    pub nodes: Vec<(NodeHealth, State)>,
    /// Worst-case mutual skew across the reachable nodes, if measurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_skew: Option<super::clock::SkewReport>,
}

impl FleetReport {
    /// The fleet's state: the worst node's state.
    ///
    /// There is no averaging and no quorum. Nine healthy nodes and one dead one
    /// is a dead fleet for the purposes of a staged block, because the fold the
    /// dead node was going to contribute does not exist.
    pub fn state(&self) -> State {
        if self.nodes.is_empty() {
            // A cockpit that was pointed at nothing has measured nothing.
            return State::Unknown;
        }
        self.nodes
            .iter()
            .map(|(_, s)| *s)
            .fold(State::Ok, State::worse)
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for (_, s) in &self.nodes {
            match s {
                State::Ok => c.0 += 1,
                State::Warn => c.1 += 1,
                State::Unknown => c.2 += 1,
                State::Fail => c.3 += 1,
            }
        }
        c
    }

    /// The one-line verdict: is it safe to start a staged block?
    pub fn go_no_go(&self) -> String {
        let (ok, warn, unknown, fail) = self.counts();
        let n = self.nodes.len();
        match self.state() {
            State::Ok => format!("GO — {ok}/{n} nodes OK"),
            State::Warn => format!(
                "GO WITH DEFECTS — {ok} OK, {warn} WARN of {n}; fix before the staged block"
            ),
            State::Unknown => format!(
                "NO-GO — {unknown} of {n} nodes NOT MEASURED. An unmeasured node is not a healthy node."
            ),
            State::Fail => format!(
                "NO-GO — {fail} FAIL, {unknown} UNKNOWN, {warn} WARN, {ok} OK of {n}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> NodeHealth {
        NodeHealth {
            host: "monad04".into(),
            unreachable: None,
            session_id: Some("monad04_lab-anchor_20260810-101500".into()),
            experiment: Some("lab-anchor".into()),
            capture_alive: true,
            scope: Some(Scope {
                class: "52:legacyofdm".into(),
                src_mac: "ef:be:ad:de:ad:de".into(),
                scoped_records: 3000,
                window_records: 4100,
            }),
            delivered_hz: Some(Ci {
                point: 122.5,
                lo: 118.0,
                hi: 127.0,
                n: 30,
                b: 2000,
            }),
            interarrival_cv: Some(Ci {
                point: 0.12,
                lo: 0.10,
                hi: 0.15,
                n: 3000,
                b: 2000,
            }),
            ble: Some(BleHealth {
                status: "ok".into(),
                observations: 900,
                rate_hz: 30.0,
                max_gap_s: 0.4,
                scan_restarts: 0,
                unparsed_events: 3,
            }),
            disk: Some(DiskHealth {
                free_gb: 60.0,
                total_gb: 118.0,
                hours_left: Some(40.0),
            }),
            thermal: Some(ThermalHealth {
                temp_c: Some(62.0),
                headroom_c: Some(18.0),
                throttled_now: false,
                throttled_since_boot: false,
                detail: "no throttling".into(),
            }),
            clock: Some(ClockHealth {
                offset_ns: 1_200_000,
                uncertainty_ns: 3_000_000,
                samples: 5,
                ntp: Some(NtpState {
                    daemon: "chronyd".into(),
                    synchronised: true,
                    stratum: Some(3),
                    system_offset_s: Some(0.000_12),
                    root_dispersion_s: Some(0.002),
                }),
            }),
            notes: Vec::new(),
        }
    }

    #[test]
    fn a_healthy_node_grades_ok_with_no_notes() {
        let mut n = healthy();
        assert_eq!(n.grade(&Budgets::default()), State::Ok);
        assert!(n.notes.is_empty(), "{:?}", n.notes);
    }

    /// The invariant. Repeated here rather than trusted, because every other
    /// behaviour in this file is negotiable and this one is not.
    #[test]
    fn an_unreachable_node_is_unknown_never_ok() {
        for why in [
            Unreachable::SshFailed("ssh: connect to host monad07 port 22: No route".into()),
            Unreachable::TimedOut,
            Unreachable::RemoteFailed {
                code: Some(127),
                stderr: "csid: command not found".into(),
            },
            Unreachable::BadOutput("expected value at line 1".into()),
        ] {
            let mut n = NodeHealth::unreachable("monad07", why);
            let s = n.grade(&Budgets::default());
            assert_eq!(s, State::Unknown, "{:?}", n.unreachable);
            assert!(!s.is_go());
            assert!(
                n.notes[0].starts_with("not measured"),
                "the reason must be printable: {:?}",
                n.notes
            );
        }
    }

    /// A node that answers but has no session is equally not-measured. An empty
    /// spool is not a healthy capture.
    #[test]
    fn a_reachable_node_with_no_session_is_unknown() {
        let mut n = healthy();
        n.session_id = None;
        assert_eq!(n.grade(&Budgets::default()), State::Unknown);
    }

    #[test]
    fn a_dead_capture_fails_even_when_everything_else_is_fine() {
        let mut n = healthy();
        n.capture_alive = false;
        assert_eq!(n.grade(&Budgets::default()), State::Fail);
        assert!(n.notes.iter().any(|s| s.contains("NOT producing")));
    }

    /// The CI contract at glance level: a point estimate above the floor whose
    /// lower bound is below it is not a clean node.
    #[test]
    fn a_rate_whose_bound_misses_the_floor_is_not_ok_even_though_the_point_clears() {
        let mut n = healthy();
        n.delivered_hz = Some(Ci {
            point: 104.0,
            lo: 96.0,
            hi: 112.0,
            n: 30,
            b: 2000,
        });
        let s = n.grade(&Budgets::default());
        assert_eq!(s, State::Warn);
        assert!(
            n.notes.iter().any(|m| m.contains("lower bound")),
            "{:?}",
            n.notes
        );

        // And when the point itself misses, it is a hard failure.
        n.delivered_hz = Some(Ci {
            point: 70.4,
            lo: 66.0,
            hi: 75.0,
            n: 30,
            b: 2000,
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);
    }

    #[test]
    fn a_jittery_stream_is_caught_on_the_upper_bound() {
        let mut n = healthy();
        // The readiness audit's real captures: gap CVs 1.00 … 50.96.
        n.interarrival_cv = Some(Ci {
            point: 1.47,
            lo: 1.30,
            hi: 1.70,
            n: 3000,
            b: 2000,
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);
        assert!(n.notes.iter().any(|m| m.contains("upper bound")));
    }

    #[test]
    fn a_skewed_clock_fails_because_it_invalidates_everything_downstream() {
        let mut n = healthy();
        n.clock = Some(ClockHealth {
            offset_ns: 400_000_000, // 400 ms — outside the 250 ms G4b budget
            uncertainty_ns: 2_000_000,
            samples: 5,
            ntp: None,
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);

        // An offset inside the budget but with uncertainty that pushes it out
        // must also fail: "3 ms ± 400 ms" is not a 3 ms offset.
        n.clock = Some(ClockHealth {
            offset_ns: 3_000_000,
            uncertainty_ns: 400_000_000,
            samples: 1,
            ntp: None,
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);
        assert!(n.notes.iter().any(|m| m.contains('±')), "{:?}", n.notes);
    }

    #[test]
    fn an_unsynchronised_time_daemon_fails_on_its_own() {
        let mut n = healthy();
        if let Some(c) = &mut n.clock {
            c.ntp = Some(NtpState {
                daemon: "chronyd".into(),
                synchronised: false,
                stratum: None,
                system_offset_s: None,
                root_dispersion_s: None,
            });
        }
        assert_eq!(n.grade(&Budgets::default()), State::Fail);
    }

    #[test]
    fn ble_and_disk_and_thermal_are_soft_unless_they_are_not() {
        let mut n = healthy();
        n.ble = Some(BleHealth {
            status: "degraded".into(),
            observations: 10,
            rate_hz: 0.2,
            max_gap_s: 40.0,
            scan_restarts: 3,
            unparsed_events: 900,
        });
        assert_eq!(n.grade(&Budgets::default()), State::Warn);

        let mut n = healthy();
        n.ble = Some(BleHealth {
            status: "failed".into(),
            observations: 0,
            rate_hz: 0.0,
            max_gap_s: 0.0,
            scan_restarts: 0,
            unparsed_events: 0,
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);

        let mut n = healthy();
        n.disk = Some(DiskHealth {
            free_gb: 3.0,
            total_gb: 118.0,
            hours_left: Some(1.5),
        });
        assert_eq!(n.grade(&Budgets::default()), State::Warn);
        n.disk = Some(DiskHealth {
            free_gb: 1.0,
            total_gb: 118.0,
            hours_left: Some(0.4),
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);

        // monad02's real state on 2026-07-28: 81.5 °C, soft limit seen.
        let mut n = healthy();
        n.thermal = Some(ThermalHealth {
            temp_c: Some(81.5),
            headroom_c: Some(-1.5),
            throttled_now: false,
            throttled_since_boot: true,
            detail: "soft_limit since boot".into(),
        });
        assert_eq!(n.grade(&Budgets::default()), State::Warn);
        n.thermal = Some(ThermalHealth {
            temp_c: Some(84.0),
            headroom_c: Some(-4.0),
            throttled_now: true,
            throttled_since_boot: true,
            detail: "throttled, soft_limit".into(),
        });
        assert_eq!(n.grade(&Budgets::default()), State::Fail);
    }

    #[test]
    fn states_escalate_in_the_documented_order() {
        assert_eq!(State::Ok.worse(State::Warn), State::Warn);
        assert_eq!(State::Warn.worse(State::Unknown), State::Unknown);
        assert_eq!(State::Unknown.worse(State::Fail), State::Fail);
        assert_eq!(State::Fail.worse(State::Ok), State::Fail);
        assert!(State::Ok.is_go());
        for s in [State::Warn, State::Unknown, State::Fail] {
            assert!(!s.is_go(), "{s:?}");
        }
    }

    fn report(nodes: Vec<(NodeHealth, State)>) -> FleetReport {
        FleetReport {
            taken_at_unix_ns: 1_786_000_000_000_000_000,
            window_s: 30.0,
            budgets: Budgets::default(),
            nodes,
            fleet_skew: None,
        }
    }

    /// The roll-up is the worst node, not the average. Nine green nodes and one
    /// unreachable one is a NO-GO, because the fold the tenth was going to
    /// contribute does not exist.
    #[test]
    fn one_unmeasured_node_takes_the_whole_fleet_off_go() {
        let mut nodes: Vec<(NodeHealth, State)> = (0..9).map(|_| (healthy(), State::Ok)).collect();
        let r = report(nodes.clone());
        assert_eq!(r.state(), State::Ok);
        assert!(r.go_no_go().starts_with("GO —"), "{}", r.go_no_go());

        nodes.push((
            NodeHealth::unreachable("monad10", Unreachable::TimedOut),
            State::Unknown,
        ));
        let r = report(nodes.clone());
        assert_eq!(r.state(), State::Unknown);
        assert!(r.go_no_go().starts_with("NO-GO"), "{}", r.go_no_go());
        assert!(r.go_no_go().contains("not a healthy node"));
        assert_eq!(r.counts(), (9, 0, 1, 0));

        // And a definite failure outranks the unknown in the headline.
        nodes.push((healthy(), State::Fail));
        let r = report(nodes);
        assert_eq!(r.state(), State::Fail);
        assert_eq!(r.counts(), (9, 0, 1, 1));
    }

    /// An empty fleet is not a healthy fleet either.
    #[test]
    fn an_empty_fleet_is_unknown_not_ok() {
        let r = report(Vec::new());
        assert_eq!(r.state(), State::Unknown);
        assert!(r.go_no_go().starts_with("NO-GO"));
    }
}
