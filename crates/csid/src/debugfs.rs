//! The `iwlmvm` debugfs control surface (the flq/iax CSI knobs).
//!
//! Contract learned on hardware (IP-120): knob writes must be `value\n` — a
//! zero-length write returns `EINVAL`. Reading the knob directory requires
//! root; `csid` runs as a system unit with `CAP_NET_ADMIN` and root-owned
//! debugfs access.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Knob names exposed by the iax `iwlmvm` debugfs directory.
///
/// The full surface is named here even where `csid` does not drive it yet
/// (injector mode will use `monitor_tx_rate`), so the driver contract is
/// documented in one place.
#[allow(dead_code)]
pub mod knob {
    pub const CSI_ENABLED: &str = "csi_enabled";
    pub const CSI_INTERVAL: &str = "csi_interval";
    pub const CSI_ADDRESSES: &str = "csi_addresses";
    pub const CSI_FRAME_TYPES: &str = "csi_frame_types";
    pub const CSI_COUNT: &str = "csi_count";
    pub const CSI_TIMEOUT: &str = "csi_timeout";
    pub const CSI_RATE_N_FLAGS_MASK: &str = "csi_rate_n_flags_mask";
    pub const CSI_RATE_N_FLAGS_VAL: &str = "csi_rate_n_flags_val";
    pub const MONITOR_TX_RATE: &str = "monitor_tx_rate";
}

/// Handle to one radio's `iwlmvm` debugfs directory.
#[derive(Debug, Clone)]
pub struct Knobs {
    dir: PathBuf,
}

impl Knobs {
    /// Locate the `iwlmvm` debugfs directory for a netdev, via
    /// `/sys/class/net/<iface>/phy80211/name` →
    /// `/sys/kernel/debug/ieee80211/<phy>/iwlwifi/iwlmvm`.
    ///
    /// (Verified on hardware: the knobs live under `iwlwifi/iwlmvm`, *not*
    /// directly under the phy directory.)
    pub fn for_interface(iface: &str) -> Result<Self> {
        let phy_name_path = PathBuf::from("/sys/class/net")
            .join(iface)
            .join("phy80211/name");
        let phy = fs::read_to_string(&phy_name_path)
            .with_context(|| {
                format!(
                    "reading {} — is {iface} a wireless interface?",
                    phy_name_path.display()
                )
            })?
            .trim()
            .to_string();
        let dir = PathBuf::from("/sys/kernel/debug/ieee80211")
            .join(&phy)
            .join("iwlwifi")
            .join("iwlmvm");
        if !dir.is_dir() {
            anyhow::bail!(
                "{} not found — is the CSI-capable iwlwifi (iax) driver loaded? \
                 (`csid doctor` checks this)",
                dir.display()
            );
        }
        Ok(Knobs { dir })
    }

    /// Construct from an explicit directory (tests, unusual layouts).
    #[allow(dead_code)]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Knobs { dir: dir.into() }
    }

    /// The directory in use.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write a knob. Always appends the newline the driver requires.
    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        let path = self.dir.join(name);
        fs::write(&path, format!("{value}\n"))
            .with_context(|| format!("writing knob {} = {value:?}", path.display()))?;
        tracing::debug!(knob = name, value, "debugfs knob written");
        Ok(())
    }

    /// Best-effort knob write — logs and continues (used on teardown paths).
    pub fn set_best_effort(&self, name: &str, value: &str) {
        if let Err(e) = self.set(name, value) {
            tracing::warn!(knob = name, error = %e, "debugfs knob write failed (continuing)");
        }
    }

    /// Enable or disable CSI reporting.
    pub fn set_csi_enabled(&self, on: bool) -> Result<()> {
        self.set(knob::CSI_ENABLED, if on { "1" } else { "0" })
    }

    /// Rate cap in microseconds (`0` = unthrottled).
    pub fn set_interval(&self, us: u32) -> Result<()> {
        self.set(knob::CSI_INTERVAL, &us.to_string())
    }

    /// Source-MAC allowlist. An empty list clears the filter.
    pub fn set_addresses(&self, macs: &[String]) -> Result<()> {
        self.set(knob::CSI_ADDRESSES, &macs.join(","))
    }
}
