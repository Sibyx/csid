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

/// Tune the monitor interface, retrying once.
///
/// The retry exists for a measured hardware quirk: the first 6 GHz tune after a
/// 5 GHz retune can return a transient `-EIO`.
pub fn tune(monitor: &str, t: &Tuning) -> Result<()> {
    let freq = t.freq.to_string();
    let width = t.width.iw_token();
    let center = t.center.map(|c| c.to_string());

    let mut args: Vec<&str> = vec!["dev", monitor, "set", "freq", &freq, width];
    if let Some(c) = center.as_deref() {
        args.push(c);
    }

    match run("iw", &args) {
        Ok(_) => {
            tracing::info!(monitor, freq = t.freq, width, center = ?t.center, "tuned");
            Ok(())
        }
        Err(first) => {
            tracing::warn!(error = %first, "tune failed; retrying once after 500 ms");
            thread::sleep(Duration::from_millis(500));
            run("iw", &args)
                .with_context(|| format!("tuning {monitor} to {} MHz {width}", t.freq))
                .map(|_| {
                    tracing::info!(monitor, freq = t.freq, width, "tuned on retry");
                })
        }
    }
}

/// Current regulatory domain (best effort, for the sidecar).
pub fn regdomain() -> Option<String> {
    let out = crate::util::run_opt("iw", &["reg", "get"])?;
    out.lines()
        .find(|l| l.trim_start().starts_with("country"))
        .map(|l| l.trim().to_string())
}
