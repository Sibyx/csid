//! Monitor-interface setup and channel tuning.
//!
//! Tuning goes through `iw`/`ip` rather than raw nl80211: it is not on the
//! latency-critical path (it happens once per session), `iw` is the reference
//! implementation for the 80/160 MHz centre-frequency argument, and shelling
//! out keeps a whole nl80211 command surface out of the daemon. The
//! *precision-critical* path — CSI event delivery — is raw netlink (`source`).

use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::caps::{self, Band, WidthCfg};
use crate::config::RadioConfig;
use crate::util::run;

/// Does a network interface exist?
pub fn interface_exists(name: &str) -> bool {
    Path::new("/sys/class/net").join(name).exists()
}

/// The wiphy index backing a netdev, read from
/// `/sys/class/net/<iface>/phy80211/index`.
///
/// Needed to address the vendor registration command at the right radio.
pub fn phy_index(iface: &str) -> Result<u32> {
    let path = Path::new("/sys/class/net")
        .join(iface)
        .join("phy80211/index");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} — is {iface} a wireless interface?",
            path.display()
        )
    })?;
    text.trim()
        .parse()
        .with_context(|| format!("parsing wiphy index from {}", path.display()))
}

/// Ensure the monitor interface exists and is up.
///
/// Note the upstream `set-monitor.sh` bitrot this replaces: it used `ifconfig`,
/// absent on Ubuntu 24.04, silently leaving the monitor interface DOWN so the
/// subsequent channel set failed.
pub fn ensure_monitor(iface: &str, monitor: &str) -> Result<()> {
    if !interface_exists(iface) {
        anyhow::bail!("interface {iface} does not exist");
    }
    if !interface_exists(monitor) {
        tracing::info!(iface, monitor, "creating monitor interface");
        run(
            "iw",
            &["dev", iface, "interface", "add", monitor, "type", "monitor"],
        )
        .with_context(|| format!("creating monitor interface {monitor} on {iface}"))?;
    }
    run("ip", &["link", "set", monitor, "up"]).with_context(|| format!("bringing {monitor} up"))?;
    Ok(())
}

/// Remove a monitor interface (best effort). Not used on the capture path —
/// sessions leave the monitor interface in place so back-to-back runs do not
/// pay the re-creation cost — but kept for operator cleanup tooling.
#[allow(dead_code)]
pub fn remove_monitor(monitor: &str) {
    if interface_exists(monitor) {
        if let Err(e) = run("iw", &["dev", monitor, "del"]) {
            tracing::warn!(monitor, error = %e, "removing monitor interface failed");
        }
    }
}

/// The resolved tuning parameters for a session.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    pub band: Band,
    pub freq: u32,
    pub center: Option<u32>,
    pub width: WidthCfg,
}

/// Resolve a radio config into concrete frequencies (pure; no hardware).
pub fn resolve(radio: &RadioConfig) -> Result<Tuning> {
    let band = caps::resolve_band(radio)?;
    let freq = caps::channel_to_freq(band, radio.channel)
        .ok_or_else(|| anyhow::anyhow!("channel {} invalid for band", radio.channel))?;
    let center = caps::center_freq(band, radio.channel, radio.width)?;
    Ok(Tuning {
        band,
        freq,
        center,
        width: radio.width,
    })
}

/// Build the `iw dev … set freq` argument vector for a tuning.
///
/// iw has two mutually exclusive syntaxes: keyword widths (`HT20`, `80MHz`, …)
/// belong to the centre-less form, and centre frequencies require the numeric
/// width (`80`). Mixing them makes iw print usage and exit 1 — which is how
/// every ≥80 MHz tune silently failed until 2026-07-27.
fn tune_args(monitor: &str, t: &Tuning) -> Vec<String> {
    let mut args: Vec<String> = ["dev", monitor, "set", "freq"]
        .into_iter()
        .map(String::from)
        .collect();
    args.push(t.freq.to_string());
    match t.center {
        Some(c) => {
            args.push(t.width.iw_numeric().to_string());
            args.push(c.to_string());
        }
        None => args.push(t.width.iw_token().to_string()),
    }
    args
}

/// What the radio reports it is actually tuned to, read back after `tune`.
///
/// # Why a readback exists at all
///
/// `iw dev … set freq` exiting 0 is not the same fact as the radio holding the
/// width. The exit status caught the 2026-07-27 defect (mixed keyword and
/// numeric width made iw print usage and exit 1) and it catches nothing that
/// fails *after* iw is satisfied — a regulatory clamp, or a driver that accepts
/// a wide tune and quietly keeps the narrow one.
///
/// That mattered little while every profile was HT20. It matters now: the
/// measurement lake holds **zero** records above 20 MHz across 2.44 billion, so
/// no wide session has ever been confirmed to have run wide, and the first wide
/// arms are queued.
///
/// A failed wide tune leaves the radio at its previous width, records on and
/// closes clean. The sidecar alone could not tell you — `RadioMeta.center_freq_mhz`
/// is what csid *asked* for, computed by `caps::center_freq`. These fields are
/// what the radio *answered*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Achieved {
    pub control_freq_mhz: Option<u32>,
    pub width_mhz: Option<u32>,
    pub center_freq_mhz: Option<u32>,
}

/// Parse the `channel` line of `iw dev <mon> info` (pure; no hardware).
///
/// The line reads, on one iw version and several kernels:
///
/// ```text
///     channel 36 (5180 MHz), width: 80 MHz, center1: 5210 MHz
/// ```
///
/// Every field is optional on purpose. A monitor interface that is down prints
/// no channel line at all, and an unparsed field must read as "not known"
/// rather than as a mismatch — a readback that invents a disagreement would
/// fail sessions that are fine.
pub fn parse_iw_info(out: &str) -> Achieved {
    let Some(line) = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("channel ") && l.contains("width:"))
    else {
        return Achieved::default();
    };

    // "(5180 MHz)" — the control frequency, in the first parenthesis.
    let control_freq_mhz = line
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(" MHz"))
        .and_then(|(v, _)| v.trim().parse().ok());

    let after = |key: &str| -> Option<u32> {
        line.split_once(key)
            .and_then(|(_, rest)| rest.split_once(" MHz"))
            .and_then(|(v, _)| v.trim().parse().ok())
    };

    Achieved {
        control_freq_mhz,
        width_mhz: after("width:"),
        center_freq_mhz: after("center1:"),
    }
}

/// Read the achieved tuning back off the monitor interface (best effort).
pub fn read_achieved(monitor: &str) -> Achieved {
    crate::util::run_opt("iw", &["dev", monitor, "info"])
        .map(|out| parse_iw_info(&out))
        .unwrap_or_default()
}

/// Compare what was asked for against what the radio answered.
///
/// Returns the human-readable disagreements. Empty means agreement, or that
/// the radio did not say — the two are deliberately not distinguished here,
/// because neither is a reason to abort a capture. The sidecar records the
/// achieved values either way, so the archive carries the fact even when the
/// log line is missed.
pub fn tuning_mismatches(t: &Tuning, a: &Achieved) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(freq) = a.control_freq_mhz {
        if freq != t.freq {
            out.push(format!(
                "control freq: asked {} MHz, radio says {freq} MHz",
                t.freq
            ));
        }
    }
    if let Some(w) = a.width_mhz {
        let asked: u32 = t.width.iw_numeric().parse().unwrap_or(0);
        if asked != 0 && w != asked {
            out.push(format!("width: asked {asked} MHz, radio says {w} MHz"));
        }
    }
    // Only meaningful when csid supplied a centre — the keyword-width form does
    // not name one, and iw then reports the control frequency as center1.
    if let (Some(asked), Some(got)) = (t.center, a.center_freq_mhz) {
        if asked != got {
            out.push(format!("center1: asked {asked} MHz, radio says {got} MHz"));
        }
    }
    out
}

/// Tune the monitor interface, retrying once.
///
/// The retry exists for a measured hardware quirk: the first 6 GHz tune after a
/// 5 GHz retune can return a transient `-EIO`.
pub fn tune(monitor: &str, t: &Tuning) -> Result<Achieved> {
    let args = tune_args(monitor, t);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let width = args[5];

    match run("iw", &args) {
        Ok(_) => {
            tracing::info!(monitor, freq = t.freq, width, center = ?t.center, "tuned");
        }
        Err(first) => {
            tracing::warn!(error = %first, "tune failed; retrying once after 500 ms");
            thread::sleep(Duration::from_millis(500));
            run("iw", &args)
                .with_context(|| format!("tuning {monitor} to {} MHz {width}", t.freq))?;
            tracing::info!(monitor, freq = t.freq, width, "tuned on retry");
        }
    }

    // iw exiting 0 says it accepted the command, not that the radio holds the
    // tuning. Ask the radio. A disagreement warns rather than aborts: the
    // session is still a measurement, it is just not the one the profile names,
    // and the achieved values reach the sidecar either way.
    let achieved = read_achieved(monitor);
    let mismatches = tuning_mismatches(t, &achieved);
    if !mismatches.is_empty() {
        tracing::warn!(
            monitor,
            mismatches = mismatches.join("; "),
            "the radio is not tuned to what this profile asked for; \
             read radio.achieved_* in the sidecar before trusting this session's width"
        );
    }
    Ok(achieved)
}

/// What an associated (STA-mode) interface is tuned to, read off the link.
///
/// STA-mode capture commands nothing: the access point owns the channel and
/// the width, so both are **observed** here and recorded as such. `None` when
/// the interface is not associated — which `[sta].require_assoc` turns into a
/// setup failure, because an empty capture that looks like a quiet channel is
/// the failure mode this fleet already knows.
#[derive(Debug, Clone)]
pub struct ObservedLink {
    pub bssid: String,
    pub ssid: Option<String>,
    pub tuning: Tuning,
    pub width_mhz: u32,
}

/// Read the association of `iface`. Best effort on the link text, strict on
/// the outcome: an associated interface whose channel this build cannot name
/// is an error, not a guess.
pub fn read_link(iface: &str) -> Result<Option<ObservedLink>> {
    let link_text = crate::util::run_opt("iw", &["dev", iface, "link"]).unwrap_or_default();
    let link = crate::survey::parse_link(&link_text);
    if !link.connected {
        return Ok(None);
    }
    let bssid = link
        .bssid
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{iface} reports a connection with no BSSID"))?;
    let info = read_achieved(iface);
    let freq = link.freq_mhz.or(info.control_freq_mhz).ok_or_else(|| {
        anyhow::anyhow!(
            "{iface} is associated but neither `iw link` nor `iw info` names its frequency"
        )
    })?;
    let (band, _channel) = caps::freq_to_channel(freq).ok_or_else(|| {
        anyhow::anyhow!("{iface} is on {freq} MHz, which is not a channel this build knows")
    })?;
    // `iw info` on a managed interface prints the same channel line as on a
    // monitor, so the width readback is shared. A link that names no width is
    // read as 20 MHz, which is what a legacy association is.
    let width_mhz = info.width_mhz.unwrap_or(20);
    let width =
        WidthCfg::from_observed(width_mhz, freq, info.center_freq_mhz).ok_or_else(|| {
            anyhow::anyhow!(
                "{iface} reports a {width_mhz} MHz link this build has no width token for"
            )
        })?;
    Ok(Some(ObservedLink {
        bssid,
        ssid: link.ssid,
        tuning: Tuning {
            band,
            freq,
            center: width
                .needs_center()
                .then_some(info.center_freq_mhz)
                .flatten(),
            width,
        },
        width_mhz,
    }))
}

/// Current regulatory domain (best effort, for the sidecar).
pub fn regdomain() -> Option<String> {
    let out = crate::util::run_opt("iw", &["reg", "get"])?;
    out.lines()
        .find(|l| l.trim_start().starts_with("country"))
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tune_args_use_numeric_width_with_center() {
        let t = Tuning {
            band: Band::Ghz5,
            freq: 5180,
            center: Some(5210),
            width: WidthCfg::W80,
        };
        assert_eq!(
            tune_args("wlp1s0mon0", &t),
            ["dev", "wlp1s0mon0", "set", "freq", "5180", "80", "5210"]
        );
    }

    // ── achieved-tuning readback ──────────────────────────────────────────
    //
    // The fixtures are real `iw dev … info` output shapes. The parser is pure
    // so the 80 MHz case can be pinned without an 80 MHz radio — which matters,
    // because no session has ever been confirmed to run at 80 MHz.

    const IW_INFO_80: &str = "Interface wlp1s0mon0
\tifindex 5
\twdev 0x2
\taddr 02:6d:6f:6e:00:01
\ttype monitor
\twiphy 0
\tchannel 44 (5220 MHz), width: 80 MHz, center1: 5210 MHz
\ttxpower 22.00 dBm
";

    const IW_INFO_HT20: &str = "Interface wlp1s0mon0
\ttype monitor
\tchannel 6 (2437 MHz), width: 20 MHz, center1: 2437 MHz
";

    #[test]
    fn parses_a_wide_tuning() {
        assert_eq!(
            parse_iw_info(IW_INFO_80),
            Achieved {
                control_freq_mhz: Some(5220),
                width_mhz: Some(80),
                center_freq_mhz: Some(5210),
            }
        );
    }

    #[test]
    fn parses_a_narrow_tuning() {
        assert_eq!(
            parse_iw_info(IW_INFO_HT20),
            Achieved {
                control_freq_mhz: Some(2437),
                width_mhz: Some(20),
                center_freq_mhz: Some(2437),
            }
        );
    }

    /// A monitor interface that is down prints no channel line. Every field
    /// must read as "not known", never as 0 — a zero here would be indexed and
    /// plotted as a real frequency.
    #[test]
    fn an_interface_with_no_channel_line_yields_no_values() {
        let out = "Interface wlp1s0mon0\n\ttype monitor\n";
        assert_eq!(parse_iw_info(out), Achieved::default());
    }

    /// The case the readback exists for: iw accepted an 80 MHz tune and the
    /// radio held 20 MHz. Nothing in the exit status shows this.
    #[test]
    fn a_width_the_radio_did_not_take_is_a_mismatch() {
        let t = Tuning {
            band: Band::Ghz5,
            freq: 5220,
            center: Some(5210),
            width: WidthCfg::W80,
        };
        let achieved = Achieved {
            control_freq_mhz: Some(5220),
            width_mhz: Some(20),
            center_freq_mhz: Some(5220),
        };
        let m = tuning_mismatches(&t, &achieved);
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m.iter().any(|s| s.contains("width")), "{m:?}");
        assert!(m.iter().any(|s| s.contains("center1")), "{m:?}");
    }

    #[test]
    fn a_tuning_the_radio_honoured_reports_nothing() {
        let t = Tuning {
            band: Band::Ghz5,
            freq: 5220,
            center: Some(5210),
            width: WidthCfg::W80,
        };
        assert!(tuning_mismatches(&t, &parse_iw_info(IW_INFO_80)).is_empty());
    }

    /// A silent radio must not read as a disagreement, or every session on a
    /// host where the readback fails would log a false alarm.
    #[test]
    fn a_radio_that_did_not_answer_is_not_a_mismatch() {
        let t = Tuning {
            band: Band::Ghz24,
            freq: 2462,
            center: None,
            width: WidthCfg::Ht20,
        };
        assert!(tuning_mismatches(&t, &Achieved::default()).is_empty());
    }

    /// The keyword-width form names no centre, and iw then echoes the control
    /// frequency as center1. That is agreement, not drift.
    #[test]
    fn the_keyword_width_form_does_not_compare_a_centre() {
        let t = Tuning {
            band: Band::Ghz24,
            freq: 2437,
            center: None,
            width: WidthCfg::Ht20,
        };
        assert!(tuning_mismatches(&t, &parse_iw_info(IW_INFO_HT20)).is_empty());
    }

    #[test]
    fn tune_args_use_keyword_width_without_center() {
        let t = Tuning {
            band: Band::Ghz24,
            freq: 2462,
            center: None,
            width: WidthCfg::Ht20,
        };
        assert_eq!(
            tune_args("wlp1s0mon0", &t),
            ["dev", "wlp1s0mon0", "set", "freq", "2462", "HT20"]
        );
    }
}
