//! Channel survey — **where the access points are**, read from the management
//! radio at session open and close and written into the sidecar.
//!
//! ## Why a capture needs one
//!
//! A passive arm's illuminators are the room's own access points, and the
//! fleet has never surveyed which channel each of them sits on. The arm
//! catalogue said so in its own words on 2026-08-30, and the wide-160 hour
//! meant to settle it captured 80 MHz through a profile misfire. An arm that
//! cannot state where its illuminators were is quoting a number it did not
//! measure, and a controller-driven channel change during a session would
//! show up as a thinner capture rather than as an event.
//!
//! The management radio (`wlan0`, the Pi 5's onboard part) is associated to
//! the campus network, so it can scan without touching the AX210, and its
//! association tells which access point it chose and on which frequency. Both
//! are reads. The survey runs `iw dev <iface> scan` and `iw dev <iface> link`,
//! keeps every BSS with its frequency, signal and SSID, and records the result
//! as **observed** values in the sidecar — the same standing `achieved_*`
//! gives the tune: what the world answered, beside what csid asked for.
//!
//! ## What a scan costs
//!
//! `brcmfmac` scans while associated by hopping off-channel for a few tens of
//! milliseconds per channel, so the management link stutters for a few
//! seconds and the CSI capture on the other radio is untouched. A scan can
//! answer `-EBUSY` while another is in flight; the runner retries once. Every
//! failure is recorded in the survey rather than raised — a session must never
//! fail because the room could not be surveyed.

use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::caps;
use crate::util::rfc3339_utc;

/// One BSS as `iw scan` reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bss {
    pub bssid: String,
    pub freq_mhz: u32,
    /// Channel number for the band, derived from the frequency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_dbm: Option<f64>,
    /// Operating width from the HT/VHT operation elements, MHz. `None` when
    /// the beacon named none, which on a legacy AP is normal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_mhz: Option<u32>,
    /// Whether this is the BSS the management radio is associated to.
    #[serde(default)]
    pub associated: bool,
}

/// The management radio's own association, from `iw link`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bssid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_dbm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_bitrate: Option<String>,
}

/// One survey: the link plus every BSS heard, at one instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Survey {
    pub interface: String,
    /// RFC 3339 UTC, the receiver's wallclock when the scan started.
    pub taken_at: String,
    pub scan_ms: u64,
    pub link: Link,
    /// Strongest first. Truncated to `max_bss`; `bss_total` says how many
    /// the scan actually returned.
    pub bss: Vec<Bss>,
    pub bss_total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key).map(str::trim)
}

/// Parse `iw dev <iface> scan` output (pure; no hardware).
pub fn parse_scan(text: &str) -> Vec<Bss> {
    let mut out: Vec<Bss> = Vec::new();
    let mut cur: Option<Bss> = None;
    // The secondary-channel offset only means 40 MHz when no VHT/HE width
    // overrides it, so it is held until the block closes.
    let mut ht_secondary = false;

    let close = |cur: &mut Option<Bss>, ht_secondary: &mut bool, out: &mut Vec<Bss>| {
        if let Some(mut b) = cur.take() {
            if b.width_mhz.is_none() {
                b.width_mhz = Some(if *ht_secondary { 40 } else { 20 });
            }
            out.push(b);
        }
        *ht_secondary = false;
    };

    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("BSS ") {
            close(&mut cur, &mut ht_secondary, &mut out);
            // `BSS 54:d7:e3:2e:a6:91(on wlan0) -- associated`
            let bssid = rest
                .split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_lowercase();
            if bssid.len() != 17 {
                continue;
            }
            cur = Some(Bss {
                bssid,
                freq_mhz: 0,
                channel: None,
                band: None,
                ssid: None,
                signal_dbm: None,
                width_mhz: None,
                associated: rest.contains("associated"),
            });
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        if let Some(v) = after(line, "freq:") {
            // `freq: 5220` on older iw, `freq: 5220.0` on newer.
            if let Some(f) = v
                .split('.')
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                b.freq_mhz = f;
                if let Some((band, ch)) = caps::freq_to_channel(f) {
                    b.channel = Some(ch);
                    b.band = Some(crate::sidecar::band_label(band).to_string());
                }
            }
        } else if let Some(v) = after(line, "signal:") {
            b.signal_dbm = v.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(v) = after(line, "SSID:") {
            b.ssid = (!v.is_empty()).then(|| v.to_string());
        } else if let Some(v) = after(line, "* secondary channel offset:") {
            ht_secondary = !v.starts_with("no secondary");
        } else if let Some(v) = after(line, "* channel width:") {
            // VHT: `1 (80 MHz)`; HE operation prints the same shape.
            b.width_mhz = v
                .split('(')
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok());
        }
    }
    close(&mut cur, &mut ht_secondary, &mut out);

    out.sort_by(|a, b| {
        b.signal_dbm
            .unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.signal_dbm.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.bssid.cmp(&b.bssid))
    });
    out
}

/// Parse `iw dev <iface> link` output (pure; no hardware).
pub fn parse_link(text: &str) -> Link {
    let mut link = Link::default();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("Connected to ") {
            link.connected = true;
            link.bssid = rest.split_whitespace().next().map(str::to_lowercase);
        } else if line.starts_with("Not connected") {
            link.connected = false;
        } else if let Some(v) = after(line, "SSID:") {
            link.ssid = (!v.is_empty()).then(|| v.to_string());
        } else if let Some(v) = after(line, "freq:") {
            link.freq_mhz = v.split('.').next().and_then(|s| s.trim().parse().ok());
            link.channel = link
                .freq_mhz
                .and_then(caps::freq_to_channel)
                .map(|(_, ch)| ch);
        } else if let Some(v) = after(line, "signal:") {
            link.signal_dbm = v.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(v) = after(line, "tx bitrate:") {
            link.tx_bitrate = Some(v.to_string());
        }
    }
    link
}

/// Run a command under `timeout(1)`, returning stdout or the failure text.
fn run_bounded(timeout_s: u64, program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("timeout");
    cmd.arg(timeout_s.to_string()).arg(program).args(args);
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(out) => Err(format!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("spawning `timeout {program}`: {e}")),
    }
}

/// Take one survey on `cfg.interface`. Never fails: every problem lands in
/// `Survey.error` and the rest of the document says what was still readable.
pub fn take(cfg: &crate::config::SurveyConfig) -> Survey {
    let started = Instant::now();
    let mut survey = Survey {
        interface: cfg.interface.clone(),
        taken_at: rfc3339_utc(crate::util::now_unix()),
        ..Default::default()
    };

    let iface = cfg.interface.as_str();
    match run_bounded(cfg.timeout_s, "iw", &["dev", iface, "link"]) {
        Ok(text) => survey.link = parse_link(&text),
        Err(e) => survey.error = Some(e),
    }

    // A scan can answer EBUSY while the supplicant's own scan is in flight.
    // One retry after a short pause covers it; a second failure is recorded.
    let mut scan = run_bounded(cfg.timeout_s, "iw", &["dev", iface, "scan"]);
    if scan.is_err() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        scan = run_bounded(cfg.timeout_s, "iw", &["dev", iface, "scan"]);
    }
    match scan {
        Ok(text) => {
            let all = parse_scan(&text);
            survey.bss_total = all.len();
            survey.bss = all.into_iter().take(cfg.max_bss.max(1)).collect();
        }
        Err(e) => {
            survey.error = Some(match survey.error.take() {
                Some(prev) => format!("{prev}; {e}"),
                None => e,
            });
        }
    }
    survey.scan_ms = started.elapsed().as_millis() as u64;
    survey
}

/// Render a survey as the operator table `csid survey` prints.
pub fn render(s: &Survey) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "survey on {} at {} ({} ms)\n",
        s.interface, s.taken_at, s.scan_ms
    ));
    match (&s.link.connected, &s.link.bssid) {
        (true, Some(b)) => out.push_str(&format!(
            "link      : {b} {} freq {} MHz ch {} signal {} dBm\n",
            s.link.ssid.as_deref().unwrap_or("-"),
            s.link
                .freq_mhz
                .map(|f| f.to_string())
                .unwrap_or_else(|| "-".into()),
            s.link
                .channel
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            s.link
                .signal_dbm
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        )),
        _ => out.push_str("link      : not connected\n"),
    }
    out.push_str(&format!(
        "bss       : {} heard, {} listed\n",
        s.bss_total,
        s.bss.len()
    ));
    out.push_str(&format!(
        "{:<18} {:>5} {:>4} {:>5} {:>7}  {}\n",
        "bssid", "freq", "ch", "width", "signal", "ssid"
    ));
    for b in &s.bss {
        out.push_str(&format!(
            "{:<18} {:>5} {:>4} {:>5} {:>7}  {}{}\n",
            b.bssid,
            b.freq_mhz,
            b.channel
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            b.width_mhz
                .map(|w| w.to_string())
                .unwrap_or_else(|| "-".into()),
            b.signal_dbm
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "-".into()),
            b.ssid.as_deref().unwrap_or("<hidden>"),
            if b.associated { "  *" } else { "" },
        ));
    }
    if let Some(e) = &s.error {
        out.push_str(&format!("error     : {e}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN: &str = "\
BSS 54:d7:e3:2e:a6:91(on wlan0) -- associated
\tlast seen: 12.345s [boottime]
\tfreq: 5220
\tbeacon interval: 100 TUs
\tsignal: -58.00 dBm
\tSSID: eduroam
\tHT operation:
\t\t * primary channel: 44
\t\t * secondary channel offset: above
\tVHT operation:
\t\t * channel width: 1 (80 MHz)
\t\t * center freq segment 1: 42
BSS 54:d7:e3:2c:2c:b1(on wlan0)
\tfreq: 2437
\tsignal: -71.00 dBm
\tSSID: eduroam
\tHT operation:
\t\t * primary channel: 6
\t\t * secondary channel offset: no secondary
BSS aa:bb:cc:dd:ee:ff(on wlan0)
\tfreq: 5975.0
\tsignal: -80.50 dBm
\tSSID:
";

    #[test]
    fn a_scan_yields_one_bss_per_block_strongest_first() {
        let bss = parse_scan(SCAN);
        assert_eq!(bss.len(), 3);
        assert_eq!(bss[0].bssid, "54:d7:e3:2e:a6:91");
        assert_eq!(bss[0].freq_mhz, 5220);
        assert_eq!(bss[0].channel, Some(44));
        assert_eq!(bss[0].band.as_deref(), Some("5"));
        assert_eq!(bss[0].width_mhz, Some(80));
        assert_eq!(bss[0].ssid.as_deref(), Some("eduroam"));
        assert!(bss[0].associated);
        // The 2.4 GHz radio: HT20, no VHT element.
        assert_eq!(bss[1].channel, Some(6));
        assert_eq!(bss[1].width_mhz, Some(20));
        assert!(!bss[1].associated);
        // A hidden SSID stays absent, and a 6 GHz frequency resolves its band.
        assert_eq!(bss[2].ssid, None);
        assert_eq!(bss[2].band.as_deref(), Some("6"));
        assert_eq!(bss[2].channel, Some(5));
    }

    #[test]
    fn a_secondary_offset_alone_means_forty() {
        let text = "BSS 00:11:22:33:44:55(on wlan0)\n\tfreq: 5220\n\tHT operation:\n\t\t * secondary channel offset: above\n";
        assert_eq!(parse_scan(text)[0].width_mhz, Some(40));
    }

    #[test]
    fn a_link_is_read_with_its_frequency() {
        let text = "Connected to 54:d7:e3:2c:2c:b1 (on wlan0)\n\tSSID: eduroam\n\tfreq: 5220\n\tRX: 1 bytes (1 packets)\n\tsignal: -55 dBm\n\ttx bitrate: 866.7 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 2\n";
        let l = parse_link(text);
        assert!(l.connected);
        assert_eq!(l.bssid.as_deref(), Some("54:d7:e3:2c:2c:b1"));
        assert_eq!(l.freq_mhz, Some(5220));
        assert_eq!(l.channel, Some(44));
        assert_eq!(l.signal_dbm, Some(-55));
        assert!(l.tx_bitrate.unwrap().starts_with("866.7"));
        let l = parse_link("Not connected.\n");
        assert!(!l.connected);
        assert_eq!(l.bssid, None);
    }

    #[test]
    fn an_empty_scan_is_an_empty_list_not_an_error() {
        assert!(parse_scan("").is_empty());
        assert!(parse_scan("garbage\n\tfreq: 5220\n").is_empty());
    }

    #[test]
    fn a_survey_round_trips_through_json() {
        let s = Survey {
            interface: "wlan0".into(),
            taken_at: "2026-09-04T20:00:00Z".into(),
            scan_ms: 3210,
            link: parse_link("Connected to 54:d7:e3:2c:2c:b1 (on wlan0)\n\tfreq: 5220\n"),
            bss: parse_scan(SCAN),
            bss_total: 3,
            error: None,
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Survey = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
        assert!(render(&s).contains("54:d7:e3:2e:a6:91"));
    }
}
