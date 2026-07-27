//! TOML configuration: a node-global file plus one file per experiment.
//!
//! Layout on a deployed node:
//!
//! ```text
//! /etc/csid/config.toml                 # node-global: spool, sync, otel, driver ABI
//! /etc/csid/experiments/<exp>.toml      # one per experiment: radio, capture, stream
//! ```
//!
//! Everything is validated before a radio is touched (`csid validate <exp>`),
//! so an invalid channel/width combination fails loudly at config time rather
//! than halfway into an unattended 30-day run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::caps;

/// Default node-global config path.
pub const DEFAULT_CONFIG: &str = "/etc/csid/config.toml";
/// Default directory holding per-experiment configs.
pub const DEFAULT_EXPERIMENT_DIR: &str = "/etc/csid/experiments";

// -- node-global --------------------------------------------------------------

/// Node-global configuration (`/etc/csid/config.toml`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub otel: OtelConfig,
    #[serde(default)]
    pub driver: DriverConfig,
}

/// Node identity and local storage.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Session spool root. One subdirectory per session.
    pub spool: PathBuf,
    /// Override the reported hostname (defaults to the system hostname).
    #[serde(default)]
    pub hostname: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            spool: PathBuf::from("/var/lib/csid"),
            hostname: None,
        }
    }
}

/// S3 shipping (executed by the sync unit, described here for the sidecar).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    #[serde(default)]
    pub enabled: bool,
    /// rclone remote name, e.g. `hetzner`.
    #[serde(default)]
    pub remote: String,
    #[serde(default)]
    pub bucket: String,
    /// Key prefix; the session lands at `<prefix>/<host>/<session_id>/`.
    #[serde(default)]
    pub prefix: String,
    /// Delete `capture.raw` this long after a verified sync.
    #[serde(default = "default_prune_days")]
    pub prune_after_days: u32,
}

fn default_prune_days() -> u32 {
    7
}

/// OpenTelemetry export (off by default — journald always works).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OtelConfig {
    #[serde(default)]
    pub enabled: bool,
    /// OTLP endpoint of the node-local Grafana Alloy.
    #[serde(default)]
    pub endpoint: String,
}

/// Driver ABI coupling, externalised so a driver revision is a config change
/// rather than a recompile.
///
/// Defaults are taken from the `iax` (fflq) iwlwifi backport sources ported by
/// IP-112 — `iwl-vendor-cmd.h`:
///
/// ```text
/// #define INTEL_OUI                       0x001735
/// IWL_MVM_VENDOR_CMD_CSI_EVENT  = 0x24
/// IWL_MVM_VENDOR_ATTR_CSI_HDR   = 0x4d
/// IWL_MVM_VENDOR_ATTR_CSI_DATA  = 0x4e
/// ```
///
/// The same subcommand serves two roles: sent *as a command* it registers this
/// socket's netlink portid with the driver (`iwl_mvm_vendor_csi_register`), and
/// the driver then delivers CSI **events** unicast to that portid. `csid doctor`
/// prints the values in use.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DriverConfig {
    /// Vendor OUI used by the CSI vendor command/event (Intel = 0x001735).
    #[serde(default = "default_vendor_oui")]
    pub vendor_oui: u32,
    /// Vendor subcommand: registers the portid, and tags the delivered events.
    #[serde(default = "default_csi_subcmd")]
    pub csi_event_subcmd: u32,
    /// Event attribute holding the CSI header blob.
    #[serde(default = "default_attr_hdr")]
    pub attr_csi_hdr: u16,
    /// Event attribute holding the CSI matrix blob.
    #[serde(default = "default_attr_data")]
    pub attr_csi_data: u16,
}

fn default_vendor_oui() -> u32 {
    0x001735 // INTEL_OUI
}
fn default_csi_subcmd() -> u32 {
    0x24 // IWL_MVM_VENDOR_CMD_CSI_EVENT
}
fn default_attr_hdr() -> u16 {
    0x4d // IWL_MVM_VENDOR_ATTR_CSI_HDR
}
fn default_attr_data() -> u16 {
    0x4e // IWL_MVM_VENDOR_ATTR_CSI_DATA
}

impl Default for DriverConfig {
    fn default() -> Self {
        DriverConfig {
            vendor_oui: default_vendor_oui(),
            csi_event_subcmd: default_csi_subcmd(),
            attr_csi_hdr: default_attr_hdr(),
            attr_csi_data: default_attr_data(),
        }
    }
}

// -- per-experiment -----------------------------------------------------------

/// One experiment's capture configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentConfig {
    /// Slug used in the session id; defaults to the file stem.
    #[serde(default)]
    pub experiment: Option<String>,
    /// Free-form operator tag recorded in the sidecar.
    #[serde(default)]
    pub tag: Option<String>,
    pub radio: RadioConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    /// Only read when `capture.mode = "inject"`.
    #[serde(default)]
    pub inject: InjectConfig,
    #[serde(default)]
    pub stream: StreamConfig,
    #[serde(default)]
    pub export: ExportConfig,
}

/// Radio / monitor-interface configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RadioConfig {
    /// The AX210 netdev (e.g. `wlp1s0`).
    pub interface: String,
    /// Monitor interface to create/use (e.g. `wlp1s0mon0`).
    #[serde(default = "default_monitor")]
    pub monitor: String,
    /// 802.11 control channel number.
    pub channel: u32,
    /// Band; required for 6 GHz because channel numbering overlaps 2.4 GHz.
    #[serde(default)]
    pub band: Option<caps::Band>,
    /// Monitor width.
    pub width: caps::WidthCfg,
    /// CSI rate cap in microseconds; `0` = unthrottled (measured ceiling
    /// ~608 Hz on a busy 20 MHz channel).
    #[serde(default)]
    pub interval_us: u32,
    /// Optional source-MAC allowlist (`csi_addresses` debugfs knob).
    #[serde(default)]
    pub mac_filter: Vec<String>,
}

fn default_monitor() -> String {
    "mon0".to_string()
}

/// Capture-session behaviour.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    /// `passive` (ambient traffic) or `inject` (passive capture **plus** the
    /// paced monitor-mode injector configured under `[inject]`).
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Session duration; `None` runs until stopped (systemd `RuntimeMaxSec`
    /// remains the outer bound).
    #[serde(default, with = "humantime_serde::option")]
    pub duration: Option<Duration>,
}

fn default_mode() -> String {
    "passive".to_string()
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            mode: default_mode(),
            duration: None,
        }
    }
}

/// Injector configuration (`capture.mode = "inject"`).
///
/// The injector transmits paced 802.11 data frames on the monitor interface —
/// the illumination source the receiving arm's analysis keys on. Defaults
/// mirror the proven illuminator arm: 25 Hz (85% delivered on contended office
/// 2.4 GHz, measured 2026-07-27), 200-byte frames, the `ef:be:ad:de:ad:de`
/// sentinel, 6 Mbps legacy OFDM (⇒ the 52-tone `legacy_ofdm` record class on
/// both bands — the band-contrast invariant).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InjectConfig {
    /// Frames per second. Absolute-deadline paced; missed slots are skipped,
    /// never bunched.
    #[serde(default = "default_inject_rate")]
    pub rate_hz: u32,
    /// 802.11 MPDU size in bytes (header + payload; radiotap not counted).
    #[serde(default = "default_inject_frame_bytes")]
    pub frame_bytes: usize,
    /// Source MAC — the analysis sentinel receivers filter on.
    #[serde(default = "default_inject_src_mac")]
    pub src_mac: String,
    /// Destination MAC. Broadcast (the default) is unACKed: loss is visible
    /// to analysis instead of being papered over by retries.
    #[serde(default = "default_inject_dst_mac")]
    pub dst_mac: String,
    /// Legacy OFDM bitrate in Mbps, requested via the radiotap RATE field.
    #[serde(default = "default_inject_bitrate")]
    pub bitrate_mbps: u32,
}

fn default_inject_rate() -> u32 {
    25
}
fn default_inject_frame_bytes() -> usize {
    200
}
fn default_inject_src_mac() -> String {
    "ef:be:ad:de:ad:de".to_string()
}
fn default_inject_dst_mac() -> String {
    "ff:ff:ff:ff:ff:ff".to_string()
}
fn default_inject_bitrate() -> u32 {
    6
}

impl Default for InjectConfig {
    fn default() -> Self {
        InjectConfig {
            rate_hz: default_inject_rate(),
            frame_bytes: default_inject_frame_bytes(),
            src_mac: default_inject_src_mac(),
            dst_mac: default_inject_dst_mac(),
            bitrate_mbps: default_inject_bitrate(),
        }
    }
}

/// Legal 802.11a/g OFDM bitrates (Mbps). DSSS/CCK rates are excluded on
/// purpose: they carry no OFDM preamble, so the receiver would get no CSI.
pub const OFDM_BITRATES_MBPS: [u32; 8] = [6, 9, 12, 18, 24, 36, 48, 54];

/// Best-effort live streaming. Never blocks or slows the durable path.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `unix` (v1 default) — `udp` is the opt-in network transport.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Unix-domain datagram socket path for on-node consumers.
    #[serde(default = "default_socket")]
    pub unix_socket: PathBuf,
    /// `udp` transport only: destination `host:port` targets.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Bounded queue depth; overflow drops the newest record and increments
    /// the dropped counter rather than applying backpressure to capture.
    #[serde(default = "default_max_queue")]
    pub max_queue: usize,
}

fn default_transport() -> String {
    "unix".to_string()
}
fn default_socket() -> PathBuf {
    PathBuf::from("/run/csid/live.sock")
}
fn default_max_queue() -> usize {
    4096
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            enabled: false,
            transport: default_transport(),
            unix_socket: default_socket(),
            targets: Vec::new(),
            max_queue: default_max_queue(),
        }
    }
}

/// Post-capture export behaviour.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    /// Produce a `.csiq` alongside `capture.raw` when the session closes.
    #[serde(default)]
    pub on_close: bool,
}

// -- loading + validation -----------------------------------------------------

impl GlobalConfig {
    /// Load the node-global config, falling back to defaults when absent.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "global config not found; using defaults");
            return Ok(GlobalConfig::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

impl ExperimentConfig {
    /// Load an experiment config from an explicit path.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: ExperimentConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if cfg.experiment.is_none() {
            cfg.experiment = path.file_stem().map(|s| s.to_string_lossy().to_string());
        }
        Ok(cfg)
    }

    /// Resolve `<name>` to `<experiment_dir>/<name>.toml`, or accept a path.
    pub fn resolve(name_or_path: &str, dir: &Path) -> Result<Self> {
        let direct = Path::new(name_or_path);
        if direct.is_file() {
            return Self::load(direct);
        }
        let candidate = dir.join(format!("{name_or_path}.toml"));
        if candidate.is_file() {
            return Self::load(&candidate);
        }
        anyhow::bail!(
            "experiment '{name_or_path}' not found (looked at {} and {})",
            direct.display(),
            candidate.display()
        )
    }

    /// The experiment slug used in session ids.
    pub fn slug(&self) -> &str {
        self.experiment.as_deref().unwrap_or("session")
    }

    /// Validate everything checkable without touching hardware.
    pub fn validate(&self) -> Result<()> {
        caps::validate_radio(&self.radio)?;

        match self.capture.mode.as_str() {
            "passive" => {}
            "inject" => {
                let inj = &self.inject;
                if inj.rate_hz == 0 || inj.rate_hz > 1000 {
                    anyhow::bail!("inject.rate_hz must be 1..=1000 (got {})", inj.rate_hz);
                }
                if inj.frame_bytes < 64 || inj.frame_bytes > 1500 {
                    anyhow::bail!(
                        "inject.frame_bytes must be 64..=1500 (got {})",
                        inj.frame_bytes
                    );
                }
                if !is_mac(&inj.src_mac) {
                    anyhow::bail!("inject.src_mac {:?} is not a MAC address", inj.src_mac);
                }
                if !is_mac(&inj.dst_mac) {
                    anyhow::bail!("inject.dst_mac {:?} is not a MAC address", inj.dst_mac);
                }
                if !OFDM_BITRATES_MBPS.contains(&inj.bitrate_mbps) {
                    anyhow::bail!(
                        "inject.bitrate_mbps must be an OFDM rate {:?} (got {}) — \
                         CCK rates carry no CSI",
                        OFDM_BITRATES_MBPS,
                        inj.bitrate_mbps
                    );
                }
            }
            other => anyhow::bail!(
                "capture.mode must be \"passive\" or \"inject\" (got {other:?})"
            ),
        }

        if self.stream.enabled {
            match self.stream.transport.as_str() {
                "unix" => {}
                "udp" => {
                    if self.stream.targets.is_empty() {
                        anyhow::bail!("stream.transport = \"udp\" requires at least one target");
                    }
                }
                other => {
                    anyhow::bail!("stream.transport must be \"unix\" or \"udp\" (got {other:?})")
                }
            }
            if self.stream.max_queue == 0 {
                anyhow::bail!("stream.max_queue must be > 0");
            }
        }

        for mac in &self.radio.mac_filter {
            if !is_mac(mac) {
                anyhow::bail!("radio.mac_filter entry {mac:?} is not a MAC address");
            }
        }
        Ok(())
    }
}

fn is_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
tag = "smoke"

[radio]
interface = "wlp1s0"
monitor = "wlp1s0mon0"
channel = 36
width = "80MHz"
interval_us = 0
mac_filter = ["aa:bb:cc:dd:ee:ff"]

[capture]
mode = "passive"
duration = "30m"

[stream]
enabled = true
transport = "unix"
unix_socket = "/run/csid/live.sock"
max_queue = 4096

[export]
on_close = true
"#;

    #[test]
    fn parses_and_validates_sample() {
        let cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.radio.channel, 36);
        assert_eq!(cfg.capture.duration, Some(Duration::from_secs(1800)));
        assert!(cfg.stream.enabled);
        assert!(cfg.export.on_close);
    }

    #[test]
    fn rejects_160mhz_on_24ghz() {
        let bad = SAMPLE
            .replace("channel = 36", "channel = 6")
            .replace("\"80MHz\"", "\"160MHz\"");
        let cfg: ExperimentConfig = toml::from_str(&bad).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_mac() {
        let bad = SAMPLE.replace("aa:bb:cc:dd:ee:ff", "not-a-mac");
        let cfg: ExperimentConfig = toml::from_str(&bad).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_key() {
        let bad = format!("{SAMPLE}\n[bogus]\nx = 1\n");
        assert!(toml::from_str::<ExperimentConfig>(&bad).is_err());
    }

    #[test]
    fn inject_mode_validates_with_defaults() {
        let toml_src = SAMPLE.replace("mode = \"passive\"", "mode = \"inject\"");
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.inject.rate_hz, 25);
        assert_eq!(cfg.inject.src_mac, "ef:be:ad:de:ad:de");
    }

    #[test]
    fn inject_rejects_cck_bitrate() {
        let toml_src = format!(
            "{}\n[inject]\nbitrate_mbps = 11\n",
            SAMPLE.replace("mode = \"passive\"", "mode = \"inject\"")
        );
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("no CSI"), "unexpected error: {err}");
    }

    #[test]
    fn inject_rejects_zero_rate() {
        let toml_src = format!(
            "{}\n[inject]\nrate_hz = 0\n",
            SAMPLE.replace("mode = \"passive\"", "mode = \"inject\"")
        );
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn passive_mode_ignores_inject_section() {
        // An [inject] section with nonsense must not fail a passive config —
        // operators flip `mode` back and forth without pruning sections.
        let toml_src = format!("{SAMPLE}\n[inject]\nrate_hz = 0\n");
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        cfg.validate().unwrap();
    }
}
