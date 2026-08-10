//! The one screen the operator actually reads.
//!
//! ## Design constraints, from the bench rather than from taste
//!
//! - **Legible across a room.** The state column is a two-character mark
//!   (`OK` `!!` `??` `XX`) *and* a colour. Colour alone fails a projector, a
//!   phone photo, a colour-blind operator, and `| tee run.log`; a mark alone is
//!   slower to scan. Both, always.
//! - **A bad node must be findable without reading.** Bad rows are prefixed
//!   with the mark in the first column, so the eye scans one column of two
//!   characters rather than ten columns of numbers.
//! - **Numbers carry their intervals.** The delivered-rate and CV columns show
//!   the point estimate for scanning, and the row's reason line carries the
//!   interval when the value is not clean. A bare point estimate never decides
//!   anything (pre-registration §2), and the gate commands print the full
//!   interval.
//! - **`??` is never green.** Enforced by the type, not by the palette: see
//!   [`super::health::State`].
//! - **The verdict is one line, at the bottom, in words.** "GO" / "NO-GO", not
//!   a count the operator has to interpret at 11pm.

use super::health::{FleetReport, NodeHealth, State};

/// ANSI colour, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub colour: bool,
}

impl Palette {
    /// Colour when stdout is a terminal and the environment has not asked
    /// otherwise. `NO_COLOR` is honoured because a cockpit that ignores it ends
    /// up pasting escape codes into a lab notebook.
    pub fn detect(force_off: bool) -> Self {
        if force_off || std::env::var_os("NO_COLOR").is_some() {
            return Palette { colour: false };
        }
        #[cfg(unix)]
        let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 };
        #[cfg(not(unix))]
        let tty = false;
        Palette { colour: tty }
    }

    pub fn plain() -> Self {
        Palette { colour: false }
    }

    fn paint(&self, state: State, text: &str) -> String {
        if !self.colour {
            return text.to_string();
        }
        // Bold on everything that is not OK: on a dim projector the weight
        // carries further than the hue.
        let code = match state {
            State::Ok => "32",        // green
            State::Warn => "1;33",    // bold yellow
            State::Unknown => "1;35", // bold magenta — deliberately not grey:
            // "no news" must be as loud as "bad news".
            State::Fail => "1;31", // bold red
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn cell(v: Option<f64>, decimals: usize, unit: &str) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.*}{unit}", decimals),
        _ => "—".to_string(),
    }
}

fn short_session(id: Option<&str>) -> String {
    // `monad04_lab-anchor_20260810-101500` -> `lab-anchor_101500`: the two
    // fields that differ between nodes are the ones worth the width.
    match id {
        None => "—".into(),
        Some(s) => {
            let parts: Vec<&str> = s.split('_').collect();
            match parts.as_slice() {
                [_host, exp, stamp] => {
                    let time = stamp.split('-').next_back().unwrap_or(stamp);
                    format!("{exp}@{time}")
                }
                _ => s.to_string(),
            }
        }
    }
}

/// Render the fleet table.
pub fn fleet_table(report: &FleetReport, p: &Palette) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "csid fleet — {} node(s), {:.0} s window, {}\n\n",
        report.nodes.len(),
        report.window_s,
        crate::util::rfc3339_utc(report.taken_at_unix_ns / 1_000_000_000),
    ));

    let header = format!(
        "{:<3} {:<9} {:<20} {:>9} {:>7} {:>8} {:>8} {:>7} {:>9}  {}",
        "", "node", "session", "rate Hz", "CV", "ble Hz", "disk GB", "°C", "clock ms", "scope"
    );
    out.push_str(&header);
    out.push('\n');
    out.push_str(&"-".repeat(header.len().min(120)));
    out.push('\n');

    // Worst first: the row that needs attention is at the top, where the eye
    // already is.
    let mut rows: Vec<&(NodeHealth, State)> = report.nodes.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.host.cmp(&b.0.host)));

    for (node, state) in rows {
        let rate = node.delivered_hz.map(|c| c.point);
        let cv = node.interarrival_cv.map(|c| c.point);
        let ble = node.ble.as_ref().map(|b| b.rate_hz);
        let disk = node.disk.map(|d| d.free_gb);
        let temp = node.thermal.as_ref().and_then(|t| t.temp_c).map(f64::from);
        let clock = node.clock.as_ref().map(|c| c.offset_ns as f64 / 1e6);
        let scope = node
            .scope
            .as_ref()
            .map(|s| format!("{} {}", s.class, s.src_mac))
            .unwrap_or_else(|| "—".into());

        let line = format!(
            "{:<3} {:<9} {:<20} {:>9} {:>7} {:>8} {:>8} {:>7} {:>9}  {}",
            state.mark(),
            truncate(&node.host, 9),
            truncate(&short_session(node.session_id.as_deref()), 20),
            cell(rate, 1, ""),
            cell(cv, 2, ""),
            cell(ble, 1, ""),
            cell(disk, 1, ""),
            cell(temp, 1, ""),
            cell(clock, 1, ""),
            scope,
        );
        out.push_str(&p.paint(*state, &line));
        out.push('\n');

        // The reason, indented, under the row it belongs to — never in a
        // separate block the operator has to correlate by eye.
        for note in &node.notes {
            for (i, l) in note.lines().enumerate() {
                let prefix = if i == 0 { "    -> " } else { "       " };
                out.push_str(&p.paint(*state, &format!("{prefix}{l}")));
                out.push('\n');
            }
        }
    }

    if let Some(skew) = &report.fleet_skew {
        out.push('\n');
        let s = if skew.within_budget() {
            State::Ok
        } else if skew.measured == 0 || !skew.unmeasured.is_empty() {
            State::Unknown
        } else {
            State::Fail
        };
        out.push_str(&p.paint(s, &format!("{}  {}", s.mark(), skew.render())));
        out.push('\n');
    }

    out.push('\n');
    let verdict = report.go_no_go();
    out.push_str(&p.paint(report.state(), &format!("=== {verdict} ===")));
    out.push('\n');
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::health::{Budgets, ClockHealth, DiskHealth, Scope, Unreachable};
    use crate::fleet::stats::Ci;

    fn node(host: &str, rate: f64, cv: f64) -> NodeHealth {
        NodeHealth {
            host: host.into(),
            unreachable: None,
            session_id: Some(format!("{host}_lab-anchor_20260810-101500")),
            experiment: Some("lab-anchor".into()),
            capture_alive: true,
            scope: Some(Scope {
                class: "52:legacyofdm".into(),
                src_mac: "ef:be:ad:de:ad:de".into(),
                scoped_records: 3600,
                window_records: 4200,
            }),
            delivered_hz: Some(Ci {
                point: rate,
                lo: rate - 4.0,
                hi: rate + 4.0,
                n: 30,
                b: 2000,
            }),
            interarrival_cv: Some(Ci {
                point: cv,
                lo: cv - 0.02,
                hi: cv + 0.02,
                n: 3600,
                b: 2000,
            }),
            ble: None,
            disk: Some(DiskHealth {
                free_gb: 60.0,
                total_gb: 118.0,
                hours_left: Some(40.0),
            }),
            thermal: None,
            clock: Some(ClockHealth {
                offset_ns: 1_000_000,
                uncertainty_ns: 2_000_000,
                samples: 5,
                ntp: None,
            }),
            notes: Vec::new(),
        }
    }

    fn graded(mut n: NodeHealth) -> (NodeHealth, State) {
        let s = n.grade(&Budgets::default());
        (n, s)
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

    #[test]
    fn a_healthy_fleet_renders_every_node_with_an_ok_mark_and_a_go_verdict() {
        let r = report(
            (1..=3)
                .map(|i| graded(node(&format!("monad0{i}"), 122.5, 0.12)))
                .collect(),
        );
        let text = fleet_table(&r, &Palette::plain());
        assert!(text.matches("OK ").count() >= 3, "{text}");
        assert!(text.contains("=== GO — 3/3 nodes OK ==="), "{text}");
        assert!(text.contains("52:legacyofdm ef:be:ad:de:ad:de"), "{text}");
    }

    /// The property the whole screen exists for: a bad node is visible without
    /// reading a number, and the reason is on the line beneath it.
    #[test]
    fn a_bad_node_is_marked_sorted_to_the_top_and_carries_its_reason() {
        let mut bad = node("monad07", 70.4, 0.12);
        bad.capture_alive = false;
        let r = report(vec![
            graded(node("monad01", 122.5, 0.12)),
            graded(node("monad02", 122.5, 0.12)),
            graded(bad),
            graded(node("monad03", 122.5, 0.12)),
        ]);
        let text = fleet_table(&r, &Palette::plain());
        let body: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("---"))
            .skip(1)
            .collect();
        assert!(
            body[0].starts_with("XX "),
            "worst row must be first: {text}"
        );
        assert!(body[0].contains("monad07"), "{text}");
        assert!(
            body[1].contains("NOT producing records"),
            "the reason must sit under the row: {text}"
        );
        assert!(text.contains("=== NO-GO"), "{text}");
    }

    /// Colour is an addition to the mark, never a replacement for it: with
    /// colour stripped, the table still distinguishes all four states.
    #[test]
    fn every_state_is_distinguishable_with_colour_stripped() {
        let mut dead = node("monad02", 122.5, 0.12);
        dead.capture_alive = false;
        let mut warm = node("monad03", 122.5, 0.12);
        warm.thermal = Some(crate::fleet::health::ThermalHealth {
            temp_c: Some(81.5),
            headroom_c: Some(-1.5),
            throttled_now: false,
            throttled_since_boot: true,
            detail: "soft_limit since boot".into(),
        });
        let r = report(vec![
            graded(node("monad01", 122.5, 0.12)),
            graded(dead),
            graded(warm),
            graded(NodeHealth::unreachable("monad07", Unreachable::TimedOut)),
        ]);
        let text = fleet_table(&r, &Palette::plain());
        for mark in ["OK ", "!! ", "?? ", "XX "] {
            assert!(text.contains(mark), "state mark {mark:?} missing:\n{text}");
        }
        assert!(!text.contains('\x1b'), "plain palette must emit no escapes");
    }

    #[test]
    fn colour_is_emitted_when_asked_and_unknown_is_never_green() {
        let p = Palette { colour: true };
        let ok = p.paint(State::Ok, "x");
        let unknown = p.paint(State::Unknown, "x");
        assert!(ok.contains("\x1b[32m"), "{ok:?}");
        assert!(
            !unknown.contains("32m"),
            "UNKNOWN must not be green: {unknown:?}"
        );
        assert!(unknown.contains("1;35"), "{unknown:?}");
        assert_eq!(p.paint(State::Fail, "x"), "\x1b[1;31mx\x1b[0m");
        // NO_COLOR / --no-color wins.
        assert!(!Palette::detect(true).colour);
    }

    /// An unreachable node shows the reason, not an empty row that reads as
    /// "nothing wrong".
    #[test]
    fn an_unreachable_node_shows_why_rather_than_a_row_of_dashes() {
        let r = report(vec![graded(NodeHealth::unreachable(
            "monad10",
            Unreachable::SshFailed("connect to host monad10 port 22: No route to host".into()),
        ))]);
        let text = fleet_table(&r, &Palette::plain());
        assert!(text.contains("?? "), "{text}");
        assert!(text.contains("monad10"), "{text}");
        assert!(text.contains("No route to host"), "{text}");
        assert!(text.contains("not measured"), "{text}");
        assert!(
            text.contains("NO-GO") && text.contains("not a healthy node"),
            "{text}"
        );
    }

    #[test]
    fn the_fleet_skew_line_is_rendered_when_measured() {
        let mut r = report(vec![graded(node("monad01", 122.5, 0.12))]);
        r.fleet_skew = Some(crate::fleet::clock::fleet_skew(
            &[
                ("monad01".into(), node("monad01", 1.0, 0.1).clock),
                ("monad02".into(), node("monad02", 1.0, 0.1).clock),
            ],
            crate::fleet::gates::G4B_BUDGET_NS,
        ));
        let text = fleet_table(&r, &Palette::plain());
        assert!(text.contains("fleet skew"), "{text}");
        assert!(text.contains("250 ms budget"), "{text}");
    }

    #[test]
    fn session_ids_shorten_to_the_fields_that_differ_between_nodes() {
        assert_eq!(
            short_session(Some("monad04_lab-anchor_20260810-101500")),
            "lab-anchor@101500"
        );
        assert_eq!(short_session(None), "—");
        // An unexpected shape is passed through rather than mangled.
        assert_eq!(short_session(Some("weird")), "weird");
    }

    #[test]
    fn long_names_truncate_without_breaking_the_columns() {
        assert_eq!(truncate("monad04", 9), "monad04");
        assert_eq!(truncate("a-very-long-experiment", 10), "a-very-lo…");
        assert_eq!(truncate("", 5), "");
    }
}
