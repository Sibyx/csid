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
    /// The bench cockpit's fleet inventory (`csid fleet …`).
    ///
    /// Meaningless on a capture node and normally absent there — but declared
    /// here because this struct is `deny_unknown_fields` and one schema for one
    /// file is worth more than a second config format. An operator who keeps a
    /// single `config.toml` synced to both the laptop and the nodes gets a file
    /// that parses in both places.
    #[serde(default)]
    pub fleet: crate::fleet::FleetConfig,
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
    /// Fleet-side BLE co-capture (IP-106 R5). Off unless `enabled = true`.
    #[serde(default)]
    pub ble: BleConfig,
    /// Time transfer over the illumination stream. Off unless `enabled = true`.
    #[serde(default)]
    pub timesync: TimesyncConfig,
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
    /// Driver-forced `rate_n_flags` for injected monitor-mode frames, written to
    /// the iax `monitor_tx_rate` debugfs knob before the session (0 = off).
    ///
    /// **Why this exists:** the radiotap RATE field is silently ignored for
    /// group-addressed (broadcast) injection on 2.4 GHz — mac80211 sends at the
    /// band's lowest *basic* rate, which is 1 Mbps **DSSS**. DSSS carries no OFDM
    /// preamble, so the receiver gets zero 52-tone CSI (measured 2026-08-10:
    /// monad01→monad02 band-24 arm, injector sent 45 005 frames, receiver logged
    /// ~33 records; tcpdump showed `1.0 Mb/s 11b`). On 5 GHz the lowest basic
    /// rate is already 6 Mbps OFDM, so injection works there by accident. Forcing
    /// the FW rate via `monitor_tx_rate` bypasses the multicast-rate fallback on
    /// both bands.
    ///
    /// Value is the AX210 new-format (v2) `rate_n_flags`. For 6 Mbps legacy OFDM:
    /// `RATE_MCS_LEGACY_OFDM_MSK (0x100) | rate_index 0`, plus an antenna bit —
    /// **verify the exact hex on hardware** by writing candidates to the debugfs
    /// knob and confirming `6.0 Mb/s` in `tcpdump -i <mon>` before trusting a run.
    /// Default 0 keeps behaviour unchanged until the value is pinned in fleet
    /// config.
    #[serde(default)]
    pub monitor_tx_rate: u32,
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
            monitor_tx_rate: 0,
        }
    }
}

/// Legal 802.11a/g OFDM bitrates (Mbps). DSSS/CCK rates are excluded on
/// purpose: they carry no OFDM preamble, so the receiver would get no CSI.
pub const OFDM_BITRATES_MBPS: [u32; 8] = [6, 9, 12, 18, 24, 36, 48, 54];

/// Fleet-side BLE co-capture, run on the same node as the CSI capture so both
/// streams carry one clock (IP-106 R5 — the BLE-anchored recalibration arm).
///
/// The scan is **always passive**: a passive scanner never transmits, so it
/// neither identifies this node to the room nor injects energy into the channel
/// the CSI capture is measuring. There is deliberately no knob to make it
/// active.
///
/// Addresses are never stored. Each observation carries a per-session salted
/// digest (see [`crate::ble::DeviceHasher`]); the salt is generated at session
/// open, lives only in memory, and is discarded at close.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BleConfig {
    /// Run the scanner alongside the CSI capture.
    #[serde(default)]
    pub enabled: bool,
    /// HCI adapter, e.g. `hci0`.
    #[serde(default = "default_ble_adapter")]
    pub adapter: String,
    /// Fail the whole session if the scanner cannot start. Default `false`:
    /// a dead BLE channel degrades honestly (recorded in the sidecar) rather
    /// than costing the CSI capture. Set `true` for calibration sessions where
    /// a capture without BLE is worthless.
    #[serde(default)]
    pub required: bool,
    /// LE scan interval in milliseconds (HCI range 2.5 – 10240 ms).
    #[serde(default = "default_ble_interval_ms")]
    pub scan_interval_ms: f64,
    /// LE scan window in milliseconds; `<= scan_interval_ms`. Equal values mean
    /// a continuously-listening scanner (one advertising channel at a time).
    #[serde(default = "default_ble_window_ms")]
    pub scan_window_ms: f64,
    /// Bytes of the SHA-256 digest kept as the device pseudonym (4–32).
    /// 8 bytes = 16 hex chars: collision-free at any plausible room population.
    #[serde(default = "default_ble_hash_bytes")]
    pub hash_bytes: usize,
    /// Restart the scanner after this many seconds without an observation.
    /// A BlueZ adapter can go quiet without erroring; this is the cure.
    #[serde(default = "default_ble_restart_after_s")]
    pub restart_after_s: f64,
    /// Delay between scanner restart attempts.
    #[serde(default = "default_ble_backoff_s")]
    pub backoff_s: f64,
    /// Observation gaps longer than this are counted in the sidecar, so a
    /// half-dead scanner is as visible as a fully dead one.
    #[serde(default = "default_ble_gap_alert_s")]
    pub gap_alert_s: f64,
    /// Flush the durable log every N observations (also flushed on a timer).
    #[serde(default = "default_ble_flush_every")]
    pub flush_every: usize,
}

fn default_ble_adapter() -> String {
    "hci0".to_string()
}
fn default_ble_interval_ms() -> f64 {
    100.0
}
fn default_ble_window_ms() -> f64 {
    100.0
}
fn default_ble_hash_bytes() -> usize {
    8
}
fn default_ble_restart_after_s() -> f64 {
    30.0
}
fn default_ble_backoff_s() -> f64 {
    5.0
}
fn default_ble_gap_alert_s() -> f64 {
    5.0
}
fn default_ble_flush_every() -> usize {
    256
}

impl Default for BleConfig {
    fn default() -> Self {
        BleConfig {
            enabled: false,
            adapter: default_ble_adapter(),
            required: false,
            scan_interval_ms: default_ble_interval_ms(),
            scan_window_ms: default_ble_window_ms(),
            hash_bytes: default_ble_hash_bytes(),
            restart_after_s: default_ble_restart_after_s(),
            backoff_s: default_ble_backoff_s(),
            gap_alert_s: default_ble_gap_alert_s(),
            flush_every: default_ble_flush_every(),
        }
    }
}

/// HCI LE scan interval/window bounds in milliseconds (0.625 ms units,
/// 0x0004–0x4000 per the Bluetooth Core spec).
pub const BLE_SCAN_MS_MIN: f64 = 2.5;
pub const BLE_SCAN_MS_MAX: f64 = 10_240.0;

impl BleConfig {
    /// Validate everything checkable without touching the adapter.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !is_hci_adapter(&self.adapter) {
            anyhow::bail!(
                "ble.adapter must be an HCI device name like \"hci0\" (got {:?})",
                self.adapter
            );
        }
        for (label, v) in [
            ("ble.scan_interval_ms", self.scan_interval_ms),
            ("ble.scan_window_ms", self.scan_window_ms),
        ] {
            if !(BLE_SCAN_MS_MIN..=BLE_SCAN_MS_MAX).contains(&v) {
                anyhow::bail!("{label} must be {BLE_SCAN_MS_MIN}..={BLE_SCAN_MS_MAX} ms (got {v})");
            }
        }
        if self.scan_window_ms > self.scan_interval_ms {
            anyhow::bail!(
                "ble.scan_window_ms ({}) must not exceed ble.scan_interval_ms ({})",
                self.scan_window_ms,
                self.scan_interval_ms
            );
        }
        if !(4..=32).contains(&self.hash_bytes) {
            anyhow::bail!("ble.hash_bytes must be 4..=32 (got {})", self.hash_bytes);
        }
        if self.restart_after_s <= 0.0 {
            anyhow::bail!("ble.restart_after_s must be > 0");
        }
        if self.backoff_s < 0.0 {
            anyhow::bail!("ble.backoff_s must be >= 0");
        }
        if self.gap_alert_s <= 0.0 {
            anyhow::bail!("ble.gap_alert_s must be > 0");
        }
        if self.flush_every == 0 {
            anyhow::bail!("ble.flush_every must be > 0");
        }
        Ok(())
    }

    /// The adapter's numeric index (`hci0` → 0).
    pub fn adapter_index(&self) -> Result<u16> {
        self.adapter
            .strip_prefix("hci")
            .and_then(|s| s.parse::<u16>().ok())
            .with_context(|| format!("ble.adapter {:?} is not hci<N>", self.adapter))
    }

    /// Scan interval/window in HCI 0.625 ms units.
    pub fn hci_units(&self) -> (u16, u16) {
        let to_units = |ms: f64| (ms / 0.625).round().clamp(0x0004 as f64, 0x4000 as f64) as u16;
        (
            to_units(self.scan_interval_ms),
            to_units(self.scan_window_ms),
        )
    }
}

fn is_hci_adapter(s: &str) -> bool {
    s.strip_prefix("hci")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Time transfer over the illumination stream (`time_transfer.parquet`).
///
/// Reads the transmit stamps both of this lab's transmitters already put in
/// their payloads — the injector's `b"CSID" ‖ seq ‖ tx_unix_ns` and the phone's
/// MNDP header — and records them beside this node's own receive stamp. See
/// [`crate::timesync`] for what that buys and what it cannot claim.
///
/// Off by default, exactly like `[ble]`: it opens a second `AF_PACKET` socket
/// and a second thread, and a capture profile should have to ask for that.
/// Enabled on the lab-session profiles.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimesyncConfig {
    /// Run the receiver alongside the CSI capture.
    #[serde(default)]
    pub enabled: bool,
    /// Fail the whole session if the receiver cannot start. Default `false`:
    /// the CSI capture is worth more than the time-transfer artefact. Set
    /// `true` on a session whose analysis pools nodes, where a capture without
    /// a measurable inter-node skew cannot be certified against G4b.
    #[serde(default)]
    pub required: bool,
    /// Flush the durable log every N rows (also flushed on a 2 s timer).
    #[serde(default = "default_timesync_flush_every")]
    pub flush_every: usize,
    /// How near in time a CSI record must be to a received frame, in
    /// microseconds, to be credited with that frame's `ftm`. The default is far
    /// below the 40 ms inter-frame spacing of a 25 Hz injector, so a pairing is
    /// unambiguous; an unpaired row keeps `ftm = null` rather than borrowing a
    /// neighbour's.
    #[serde(default = "default_timesync_ftm_tolerance_us")]
    pub ftm_tolerance_us: u64,
    /// Upper bound on the minimum one-way delay, microseconds — used only to
    /// widen the reported phone-offset interval, never to change the fit.
    /// Default 5000 µs, from this fleet's measured 10.6 ms management RTT with
    /// `wlan0` power-save off. See [`crate::timesync::affine`].
    #[serde(default = "default_timesync_one_way_floor_us")]
    pub one_way_floor_us: u64,
}

fn default_timesync_flush_every() -> usize {
    256
}
fn default_timesync_ftm_tolerance_us() -> u64 {
    2_000
}
fn default_timesync_one_way_floor_us() -> u64 {
    5_000
}

impl Default for TimesyncConfig {
    fn default() -> Self {
        TimesyncConfig {
            enabled: false,
            required: false,
            flush_every: default_timesync_flush_every(),
            ftm_tolerance_us: default_timesync_ftm_tolerance_us(),
            one_way_floor_us: default_timesync_one_way_floor_us(),
        }
    }
}

impl TimesyncConfig {
    pub fn ftm_tolerance_ns(&self) -> u64 {
        self.ftm_tolerance_us.saturating_mul(1_000)
    }

    pub fn one_way_floor_ns(&self) -> u64 {
        self.one_way_floor_us.saturating_mul(1_000)
    }

    /// Validate everything checkable without touching a socket.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.flush_every == 0 {
            anyhow::bail!("timesync.flush_every must be > 0");
        }
        // Half the 40 ms inter-frame spacing of the slowest sane injector pace.
        if self.ftm_tolerance_us == 0 || self.ftm_tolerance_us > 20_000 {
            anyhow::bail!(
                "timesync.ftm_tolerance_us must be 1..=20000 (got {}) — a window wider than \
                 half the inter-frame spacing makes the ftm pairing ambiguous",
                self.ftm_tolerance_us
            );
        }
        if self.one_way_floor_us > 1_000_000 {
            anyhow::bail!(
                "timesync.one_way_floor_us must be <= 1000000 (got {}) — a one-second one-way \
                 delay floor would make the reported phone-offset interval meaningless",
                self.one_way_floor_us
            );
        }
        Ok(())
    }
}

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
            other => {
                anyhow::bail!("capture.mode must be \"passive\" or \"inject\" (got {other:?})")
            }
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

        self.ble.validate()?;
        self.timesync.validate()?;

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

    /// `GlobalConfig` is `deny_unknown_fields`, so an operator who keeps ONE
    /// `config.toml` synced to both the bench laptop and the capture nodes
    /// would have every node refuse to start if `[fleet]` were not declared
    /// here. It is, and this test is why it stays.
    #[test]
    fn a_node_config_carrying_the_cockpits_fleet_section_still_parses() {
        let text = r#"
[node]
spool = "/var/lib/csid"

[fleet]
nodes = ["monad01", "monad02"]
user = "monad"
clock_budget_ms = 250

[fleet.addresses]
monad04 = "monad.local"
"#;
        let g: GlobalConfig = toml::from_str(text).unwrap();
        assert_eq!(g.node.spool, PathBuf::from("/var/lib/csid"));
        assert_eq!(g.fleet.nodes.len(), 2);
        assert_eq!(g.fleet.clock_budget_ms, 250);
        assert_eq!(
            g.fleet.addresses.get("monad04").map(String::as_str),
            Some("monad.local")
        );

        // And a node config with no [fleet] at all is unchanged.
        let bare: GlobalConfig = toml::from_str("[node]\nspool = \"/var/lib/csid\"\n").unwrap();
        assert!(bare.fleet.nodes.is_empty());
        assert_eq!(bare.fleet.user, "monad");
    }

    /// The shipped example must parse — it is installed verbatim to
    /// `/etc/csid/config.toml` by the deployment guide.
    #[test]
    fn the_shipped_example_config_parses() {
        let text = include_str!("../../../config/config.toml");
        let g: GlobalConfig = toml::from_str(text).expect("config/config.toml must parse");
        assert_eq!(g.driver.vendor_oui, 0x001735);
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
    fn ble_defaults_are_off_and_valid() {
        let cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        assert!(!cfg.ble.enabled);
        assert_eq!(cfg.ble.adapter, "hci0");
        cfg.validate().unwrap();
    }

    #[test]
    fn ble_enabled_validates_and_converts_hci_units() {
        let toml_src = format!("{SAMPLE}\n[ble]\nenabled = true\nadapter = \"hci1\"\n");
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.ble.adapter_index().unwrap(), 1);
        // 100 ms / 0.625 ms = 160 units.
        assert_eq!(cfg.ble.hci_units(), (160, 160));
    }

    #[test]
    fn ble_rejects_window_wider_than_interval() {
        let toml_src = format!(
            "{SAMPLE}\n[ble]\nenabled = true\nscan_interval_ms = 50\nscan_window_ms = 100\n"
        );
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("scan_window_ms"), "unexpected error: {err}");
    }

    #[test]
    fn ble_rejects_bad_adapter_name() {
        let toml_src = format!("{SAMPLE}\n[ble]\nenabled = true\nadapter = \"bluetooth0\"\n");
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn disabled_ble_section_is_never_validated() {
        // Operators flip `enabled` back and forth; nonsense in a disabled
        // section must not fail the config (same rule as [inject]).
        let toml_src = format!("{SAMPLE}\n[ble]\nenabled = false\nhash_bytes = 999\n");
        let cfg: ExperimentConfig = toml::from_str(&toml_src).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn timesync_defaults_are_off_and_valid() {
        let cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        assert!(!cfg.timesync.enabled);
        assert_eq!(cfg.timesync.ftm_tolerance_ns(), 2_000_000);
        assert_eq!(cfg.timesync.one_way_floor_ns(), 5_000_000);
        cfg.validate().unwrap();
    }

    /// A pairing window wider than half the inter-frame spacing would let one
    /// CSI record be credited to two different frames.
    #[test]
    fn timesync_rejects_an_ambiguous_ftm_window() {
        let src = format!("{SAMPLE}\n[timesync]\nenabled = true\nftm_tolerance_us = 50000\n");
        let cfg: ExperimentConfig = toml::from_str(&src).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn a_disabled_timesync_section_is_never_validated() {
        // Same rule as [inject] and [ble]: operators flip `enabled` back and
        // forth without pruning the section.
        let src = format!("{SAMPLE}\n[timesync]\nenabled = false\nftm_tolerance_us = 0\n");
        let cfg: ExperimentConfig = toml::from_str(&src).unwrap();
        cfg.validate().unwrap();
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
