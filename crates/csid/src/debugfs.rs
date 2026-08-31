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
    /// Read-only. The firmware's own die temperature, whole degrees Celsius.
    pub const NIC_TEMP: &str = "nic_temp";
}

/// Where the driver creates one debugfs directory per PCI device.
///
/// `/sys/kernel/debug/ieee80211/<phy>/iwlwifi` is a SYMLINK to the matching
/// directory here (`mvm/debugfs.c`, `debugfs_create_symlink("iwlwifi", ...)`),
/// so the two paths reach the same `iwlmvm` directory. This one is used for the
/// NIC temperature because it needs no interface name, and an interface can be
/// renamed or torn down while the radio stays present.
const IWLWIFI_DEBUGFS: &str = "/sys/kernel/debug/iwlwifi";

/// The Wi-Fi NIC's own die temperature, in whole degrees Celsius.
///
/// # This is not a sysfs read
///
/// Unlike the SoC thermal zone, `nic_temp` is a firmware round trip: the driver
/// takes `mvm->mutex`, sends a DTS measurement command, and waits for the
/// notification. Budget up to a second, and do not put it in a tight loop.
///
/// # An unreadable value is normal, not an error
///
/// `iwl_mvm_get_temp()` returns `-EIO` whenever the firmware is not running, so
/// an idle node with no capture in flight legitimately answers nothing. The file
/// is also mode `0400`, so a non-root reader gets nothing. Both are absence.
/// Absence is never reported as a temperature.
pub fn read_nic_temp_c() -> Option<i32> {
    read_nic_temp_c_in(Path::new(IWLWIFI_DEBUGFS))
}

fn read_nic_temp_c_in(root: &Path) -> Option<i32> {
    // One AX210 per node, so the first device that answers IS the radio. A node
    // with two would have to be told which one, and the fleet has none.
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path().join("iwlmvm").join(knob::NIC_TEMP);
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(c) = parse_nic_temp_c(&raw) {
            return Some(c);
        }
    }
    None
}

/// One integer, whole degrees Celsius, trailing newline.
fn parse_nic_temp_c(raw: &str) -> Option<i32> {
    raw.trim().parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The driver prints whole degrees with a trailing newline.
    #[test]
    fn whole_degrees_parse() {
        assert_eq!(parse_nic_temp_c("46\n"), Some(46));
        assert_eq!(parse_nic_temp_c("46"), Some(46));
        // The DTS reading is signed. A cold-boot node in an unheated room is a
        // real reading, not a parse failure.
        assert_eq!(parse_nic_temp_c("-3\n"), Some(-3));
    }

    /// `iwl_mvm_get_temp()` returns `-EIO` with the firmware down, so `cat`
    /// yields nothing at all. That is absence, and absence must not become 0 °C.
    #[test]
    fn an_unreadable_nic_temp_is_absence_not_zero() {
        assert_eq!(parse_nic_temp_c(""), None);
        assert_eq!(parse_nic_temp_c("\n"), None);
        assert_eq!(parse_nic_temp_c("Input/output error"), None);
        assert_eq!(read_nic_temp_c_in(Path::new("/nonexistent/iwlwifi")), None);
    }

    /// A device directory with no readable `nic_temp` must not stop the scan:
    /// on a host with a second iwlwifi device the answer is the one that reads.
    #[test]
    fn the_scan_skips_a_device_that_does_not_answer() {
        let root = std::env::temp_dir().join(format!("csid-dbgfs-{}", std::process::id()));
        let quiet = root.join("0000:00:00.0").join("iwlmvm");
        let answering = root.join("0000:01:00.0").join("iwlmvm");
        fs::create_dir_all(&quiet).unwrap();
        fs::create_dir_all(&answering).unwrap();
        fs::write(answering.join(knob::NIC_TEMP), "52\n").unwrap();

        assert_eq!(read_nic_temp_c_in(&root), Some(52));
        fs::remove_dir_all(&root).ok();
    }
}
