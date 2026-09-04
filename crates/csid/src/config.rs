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
    /// Where a running session publishes `csid-status/1` (see [`crate::status`]).
    ///
    /// Node-global rather than per-experiment: there is one radio, so there is
    /// one capture, so there is one status document — and a reader that had to
    /// know which experiment is running in order to find out what is running
    /// would be solving the problem backwards.
    ///
    /// An empty path disables publication.
    #[serde(default = "default_status_path")]
    pub status_path: PathBuf,

    /// Refuse to OPEN a persisting session with less than this much free space
    /// on the spool filesystem, in gigabytes. `0` disables the check.
    ///
    /// The last line of defence, and the one that was missing on 2026-08-17. The
    /// fleet started a 16 h capture on nodes holding between 843 MB and 5.2 GB
    /// free. monad02 filled at hour 13: its durable writer failed, the OOM
    /// killer took csid during teardown, and the session was never closed — so
    /// the sidecar was never sealed, `time_transfer.parquet` was never written,
    /// and `csid-sync` skipped the directory forever. Three hours of one of five
    /// receivers, and the whole run's time transfer left with a single copy.
    ///
    /// Every part of that was predictable BEFORE the radio was tuned. A capture
    /// that cannot fit should fail at second zero with a number in the message,
    /// not at hour thirteen with an OOM. Refusing costs a re-run; the other way
    /// costs the night.
    ///
    /// Not checked when `capture.persist = false` — such a session writes only
    /// its sidecar and cannot fill anything.
    #[serde(default = "default_min_free_gb")]
    pub min_free_gb: f64,
}

fn default_min_free_gb() -> f64 {
    5.0
}

fn default_status_path() -> PathBuf {
    // Beside the live socket: both are volatile, both belong to the running
    // session, and `RuntimeDirectory=csid` already creates the directory.
    PathBuf::from("/run/csid/status.json")
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            spool: PathBuf::from("/var/lib/csid"),
            hostname: None,
            status_path: default_status_path(),
            min_free_gb: default_min_free_gb(),
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
    /// ACCEPTED AND IGNORED. Retention is not implemented in this binary.
    ///
    /// The live knob is `CSID_PRUNE_GRACE_DAYS` in the systemd environment,
    /// read by `scripts/csid-prune`; the floor beside it is
    /// `CSID_PRUNE_MIN_FREE_GB`. This field is parsed only so a `config.toml`
    /// written before 2026-08-18 still loads — `SyncConfig` is
    /// `deny_unknown_fields`, so dropping it would make old configs a hard
    /// startup error rather than a no-op.
    ///
    /// It was worse than dead: csiscope rendered it as "prune after N days", so
    /// the one retention number an operator could see was the one that did
    /// nothing. Do not add a reader for it — give the script the knob.
    #[serde(default)]
    pub prune_after_days: u32,
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
    /// Frame census on the monitor interface. Off unless `enabled = true`.
    #[serde(default)]
    pub census: CensusConfig,
    /// Channel survey on the management radio at open and close. Off unless
    /// `enabled = true`.
    #[serde(default)]
    pub survey: SurveyConfig,
    /// Only read when `capture.mode = "sta"`.
    #[serde(default)]
    pub sta: StaConfig,
}

/// Frame census — who is on the air, from the raw 802.11 header, for the
/// whole session. See [`crate::census`] for why the CSI record's own source
/// address cannot answer that after the first minutes of a session.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CensusConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Busiest transmitters and beaconing BSSIDs named in the sidecar.
    #[serde(default = "default_census_top_n")]
    pub top_n: usize,
    /// Distinct transmitters tracked for the session total. Beyond this the
    /// count is reported as capped rather than growing without bound on a
    /// channel full of randomised addresses.
    #[serde(default = "default_census_max_transmitters")]
    pub max_transmitters: usize,
}

fn default_census_top_n() -> usize {
    32
}
fn default_census_max_transmitters() -> usize {
    4096
}

impl Default for CensusConfig {
    fn default() -> Self {
        CensusConfig {
            enabled: false,
            top_n: default_census_top_n(),
            max_transmitters: default_census_max_transmitters(),
        }
    }
}

impl CensusConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.top_n == 0 || self.top_n > 1024 {
            anyhow::bail!("census.top_n must be 1..=1024 (got {})", self.top_n);
        }
        if self.max_transmitters < self.top_n {
            anyhow::bail!(
                "census.max_transmitters ({}) must be >= census.top_n ({})",
                self.max_transmitters,
                self.top_n
            );
        }
        Ok(())
    }
}

/// Channel survey on the **management** radio. See [`crate::survey`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SurveyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The radio to scan with. The management radio, never the capture radio:
    /// a scan retunes the interface it runs on.
    #[serde(default = "default_survey_interface")]
    pub interface: String,
    /// Wall-clock bound on one `iw` call, seconds.
    #[serde(default = "default_survey_timeout_s")]
    pub timeout_s: u64,
    /// BSS entries kept per survey, strongest first.
    #[serde(default = "default_survey_max_bss")]
    pub max_bss: usize,
}

fn default_survey_interface() -> String {
    "wlan0".to_string()
}
fn default_survey_timeout_s() -> u64 {
    20
}
fn default_survey_max_bss() -> usize {
    64
}

impl Default for SurveyConfig {
    fn default() -> Self {
        SurveyConfig {
            enabled: false,
            interface: default_survey_interface(),
            timeout_s: default_survey_timeout_s(),
            max_bss: default_survey_max_bss(),
        }
    }
}

impl SurveyConfig {
    pub fn validate(&self, capture_interface: &str, monitor: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.interface.is_empty() {
            anyhow::bail!("survey.interface must be set");
        }
        if self.interface == capture_interface || self.interface == monitor {
            anyhow::bail!(
                "survey.interface {:?} is the capture radio — a scan retunes the interface it \
                 runs on and would destroy the capture. Point it at the management radio.",
                self.interface
            );
        }
        if self.timeout_s == 0 || self.timeout_s > 120 {
            anyhow::bail!("survey.timeout_s must be 1..=120 (got {})", self.timeout_s);
        }
        Ok(())
    }
}

/// Associated (STA-mode) capture: CSI from the frames an access point sends
/// this node while it is a client of that access point (IP-139 C6).
///
/// The link owns the channel and the width, so `[radio].channel` and
/// `[radio].width` are **not read** in this mode — both are observed off the
/// association and recorded as observed in the sidecar. Nothing here
/// associates: the supplicant is the operator's, and this block only says
/// what the session refuses to run without.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaConfig {
    /// Fail at setup when the interface is not associated. Default `true`:
    /// an empty capture that looks like a quiet channel is the failure mode
    /// this fleet already knows.
    #[serde(default = "default_sta_require_assoc")]
    pub require_assoc: bool,
    /// Refuse a link to any other SSID. Empty accepts whatever is associated.
    #[serde(default)]
    pub ssid: String,
    /// Refuse a link to any other BSSID. Empty accepts whatever is associated.
    #[serde(default)]
    pub bssid: String,
}

fn default_sta_require_assoc() -> bool {
    true
}

impl Default for StaConfig {
    fn default() -> Self {
        StaConfig {
            require_assoc: default_sta_require_assoc(),
            ssid: String::new(),
            bssid: String::new(),
        }
    }
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
    /// Roll `capture.raw` into a fresh, self-contained segment every N of wall
    /// clock. `None` (the default) keeps the historical single-file session.
    ///
    /// A segment is a SESSION-SHAPED DIRECTORY — `<session_id>-segNNNN/` beside
    /// the session root, carrying its own `capture.raw`, `metadata.json` and
    /// `capture.csiq`. That shape is the whole point: `csid-sync` ships any
    /// directory whose sidecar reads `complete` and that has no `.synced`
    /// marker, and `csid-prune` reclaims raw bytes a grace window after that
    /// marker appears. Segments therefore upload *during* a run, retry from the
    /// on-disk queue when the node is offline, and get cleaned up — with no
    /// change to either script.
    ///
    /// WHY THIS EXISTS. Without it an N-hour capture is a single file that no
    /// consumer can read and no copy can leave the node until a clean close,
    /// because `capture.csiq` is only exported at teardown. A node lost at hour
    /// 5 of 6 costs the entire session. Segmenting bounds that loss to one
    /// segment and puts every sealed one in object storage while the run
    /// continues. It also bounds disk: `csid-prune` can reclaim shipped
    /// segments mid-run, which is what makes a 1 kHz / 80 MHz profile
    /// (~14 GB/h) survivable on a 58 GB card at all.
    ///
    /// The radio is NOT touched on rotation — same monitor VIF, same tune, same
    /// netlink registration. Only the output file rolls, so there is no
    /// retune gap and no capture hole at a segment boundary.
    #[serde(default, with = "humantime_serde::option")]
    pub segment_duration: Option<Duration>,

    /// Write `capture.raw` at all. Default `true`.
    ///
    /// ## What `false` is for
    ///
    /// csid's product is the RECORD STREAM. A session publishes every record to
    /// the live socket, and what happens next is the consumer's business:
    /// `csiscope` renders it, and a profile that is running an experiment stores
    /// it. Persisting is therefore a CHOICE a profile makes, not a property of
    /// capturing — and until 2026-08-18 there was no way to decline it.
    ///
    /// That gap is what filled the fleet's SD cards, and the culprit was not a
    /// measurement. The `console` profile exists purely so csiscope always has a
    /// feed to attach to: it declares no `duration`, so it runs for as long as
    /// the node is up, and it wrote a `capture.raw` the whole time. On monad02 a
    /// single console session started 2026-07-29 had reached **13.34 GB** by
    /// 2026-08-18, on a 58 GB card, and the fleet was carrying four more. The
    /// overnight measurement run then needed the space that a live-view utility
    /// had quietly eaten, and both died.
    ///
    /// With `persist = false` there is no durable thread, no `capture.raw`, no
    /// segments and no CSIQ export. The SIDECAR IS STILL WRITTEN: what ran, on
    /// which channel, from when to when, and how many records it produced remain
    /// a recorded fact, because "this node was watching ch44 all week" is worth
    /// knowing even when the samples were not kept. The directory holds only
    /// `metadata.json`, so `csid-sync` ships ~1 KB and `csid-prune` finds nothing
    /// to reclaim.
    ///
    /// ## The cost, stated plainly
    ///
    /// A record dropped by the live path is GONE — there is no durable copy
    /// behind it. That is correct for a liveness feed and wrong for a
    /// measurement, which is why this defaults to `true` and why a session that
    /// neither persists nor streams is rejected by [`ExperimentConfig::validate`]
    /// rather than run as an expensive no-op.
    #[serde(default = "default_persist")]
    pub persist: bool,
}

fn default_persist() -> bool {
    true
}

fn default_mode() -> String {
    "passive".to_string()
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            mode: default_mode(),
            duration: None,
            segment_duration: None,
            persist: default_persist(),
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
    ///
    /// **Ignored when `monitor_tx_rate` is non-zero.** The radiotap RATE field
    /// is legacy-only, so `build_frame` omits it entirely once the driver rate
    /// is forced, rather than stating a second and possibly contradictory rate.
    /// The forced word carries the rate index in that case.
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
    /// Permit a `monitor_tx_rate` that [`validate_monitor_tx_rate`] refuses.
    ///
    /// **This exists so the refusal itself can be measured, and for nothing
    /// else.** The guard stops an arm from running green while illuminating
    /// nothing; it must not stop a bench from testing whether the refusal is
    /// real, which is how a wrong belief becomes permanent.
    ///
    /// The reason it is needed: `zhang2025_72c4` (IEEE Sensors Journal) reports
    /// injecting every CSI type including HE 160 MHz on this NIC, using this
    /// driver — `github.com/fflq/iax`, which is the tree we build. Our 2026-08-29
    /// bench found all three HE words refused on 5 GHz. One difference is known:
    /// that paper disables the LAR flags to unlock 5 GHz AP mode and we do not.
    /// Another is untested: their monitor-mode work is on channel 4, i.e.
    /// 2.4 GHz, and we have never attempted an HE word below 5 GHz.
    ///
    /// Any profile setting this MUST be named `explore-*`, because a session is
    /// named after its profile and a deliberate probe of a known-refused
    /// configuration is not measurement data.
    #[serde(default)]
    pub allow_untransmittable_rate: bool,
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
            allow_untransmittable_rate: false,
        }
    }
}

/// Legal 802.11a/g OFDM bitrates (Mbps). DSSS/CCK rates are excluded on
/// purpose: they carry no OFDM preamble, so the receiver would get no CSI.
pub const OFDM_BITRATES_MBPS: [u32; 8] = [6, 9, 12, 18, 24, 36, 48, 54];

/// Reject the `inject.monitor_tx_rate` words this firmware will not transmit.
///
/// **Nothing between csid and the antenna checks this word.** In the iax
/// backport `iwl_dbgfs_monitor_tx_rate_write` is a `kstrtou32` and an
/// assignment, and `flq_iwl_mvm_set_monitor_tx_rate` copies the value straight
/// into `tx_cmd->rate_n_flags`. Neither tests modulation, width, MCS or
/// antenna. The refusal happens in firmware, which reports nothing back. This
/// function is therefore the only gate that exists anywhere on the path.
///
/// # What a refused word looks like
///
/// Benched 2026-08-29, monad01 -> monad02, five cells of 40 s. The knob
/// accepted every HE word, csid sent ~4,004 frames per cell with `errors = 0`,
/// and `tcpdump -e` on the neighbour printed **zero** frames. An HE arm reports
/// success, passes verify, runs green and illuminates nothing.
///
/// At 80 and 160 MHz it is worse than a dead injector. Holding the width fixed
/// and changing only the word, `frames_seen` fell from 10,890 to 155 and
/// timesync reported `rx_stamp_source: none`. The forced word disturbs the
/// **receive** path as well, and a deaf observer looks exactly like a quiet
/// channel.
///
/// `own_transmissions` read ~4,002 in every cell, including the silent ones, so
/// it cannot stand in for this check.
///
/// # Why HE and EHT are refused, and HT and VHT only warned
///
/// Two parties measured HE independently. This project refused `0x4400`,
/// `0x5400` and `0x5C00`. The backport author's own notes in `mvm/tx.c` mark
/// `0xc400`, `0xd400` and `0xdc00` — the same three PHYs, both antennas — with
/// `err`, and leave every HT and VHT word unmarked.
///
/// Nobody has shown HT or VHT to fail, so they warn rather than bail. `0xd300`
/// (VHT80) is the word worth benching: 75.6 MHz of occupied span against HE80's
/// 77.8 MHz, which is the delay resolution HE80 was wanted for.
pub fn validate_monitor_tx_rate(word: u32) -> Result<()> {
    // 0 leaves the knob untouched, and the radiotap RATE field carries the rate.
    if word == 0 {
        return Ok(());
    }

    // `decode_rnf` returns None only for 0, which is handled above.
    let Some(phy) = csiq::raw::decode_rnf(word) else {
        return Ok(());
    };

    match phy.modulation {
        csiq::Modulation::He | csiq::Modulation::Eht => {
            anyhow::bail!(
                "inject.monitor_tx_rate = 0x{:x} selects {:?}, which this firmware accepts and \
                 does not transmit. Measured 2026-08-29: errors = 0, zero frames on air at the \
                 neighbour, and at >= 80 MHz the node went deaf as well (frames_seen 10,890 -> \
                 155, timesync rx_stamp_source: none). The backport author marked the same three \
                 PHYs `err` in mvm/tx.c. Use 0x4100 (legacy OFDM, 20 MHz, antenna A), or bench \
                 0xd300 (VHT80) first — it carries the same occupied span.",
                word,
                phy.modulation
            );
        }
        csiq::Modulation::Cck => {
            anyhow::bail!(
                "inject.monitor_tx_rate = 0x{:x} selects CCK, which carries no OFDM preamble and \
                 so produces no CSI at the receiver. This is the same reason inject.bitrate_mbps \
                 is restricted to {:?}.",
                word,
                OFDM_BITRATES_MBPS
            );
        }
        csiq::Modulation::Unknown(t) => {
            anyhow::bail!(
                "inject.monitor_tx_rate = 0x{:x} carries modulation type {}, which this build \
                 cannot name. Refusing rather than forcing a word nothing here can reason about.",
                word,
                t
            );
        }
        csiq::Modulation::Ht | csiq::Modulation::Vht => {
            tracing::warn!(
                monitor_tx_rate = format!("0x{word:x}"),
                modulation = ?phy.modulation,
                "inject.monitor_tx_rate selects a PHY no bench has confirmed this firmware will \
                 transmit. Confirm with tcpdump on a neighbour before trusting the session — \
                 errors = 0 is not evidence a frame left the antenna."
            );
        }
        csiq::Modulation::LegacyOfdm => {}
    }

    if let Some(bw) = csiq::raw::decode_bw_antsel(word) {
        if bw.bandwidth != csiq::record::Bandwidth::W20 {
            tracing::warn!(
                monitor_tx_rate = format!("0x{word:x}"),
                bandwidth = ?bw.bandwidth,
                "inject.monitor_tx_rate names a width above 20 MHz. No injected frame wider than \
                 20 MHz has ever been confirmed on air from this fleet, and the 80/160 MHz cells \
                 of the 2026-08-29 bench were the ones that also broke the receive path."
            );
        }
        if bw.antenna_sel == 0 {
            tracing::warn!(
                monitor_tx_rate = format!("0x{word:x}"),
                "inject.monitor_tx_rate names no antenna (bits 15-14 are zero). Every word this \
                 fleet has transmitted, and every word the backport author recorded as working, \
                 names at least one."
            );
        }
    }

    Ok(())
}

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
    /// Namespace of the lab identity frame (`ble-rssi/2`): the canonical
    /// 128-bit UUID whose **first twelve bytes** mark an advertised service
    /// UUID as ours; the last four bytes of a matched frame carry the
    /// participant and session keys. Empty (the default) disables matching and
    /// the scanner behaves exactly as `ble-rssi/1` — no payload is inspected.
    /// A malformed value fails the scanner setup loudly, because matching that
    /// is silently off is indistinguishable from a room where nobody broadcast.
    #[serde(default)]
    pub lab_namespace_uuid: String,
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
            lab_namespace_uuid: String::new(),
        }
    }
}

impl BleConfig {
    /// The lab matcher this config asks for: `None` when matching is not
    /// configured, `Err` when the namespace is malformed.
    pub fn lab_matcher(&self) -> anyhow::Result<Option<crate::ble::LabMatcher>> {
        if self.lab_namespace_uuid.trim().is_empty() {
            return Ok(None);
        }
        crate::ble::LabMatcher::from_namespace(self.lab_namespace_uuid.trim()).map(Some)
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
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    /// Produce a `.csiq` alongside `capture.raw` when the session closes.
    #[serde(default)]
    pub on_close: bool,
    /// Keep the 272-byte driver header verbatim in every record (TLV `0x14`).
    ///
    /// Lossless provenance: a field this build cannot name is still in the blob
    /// at the offset the spec's Appendix A gives it, so a later reader recovers
    /// it with no re-capture. That is how per-frame bandwidth was recovered for
    /// the whole archive from `rate_n_flags` — except that one happened to be
    /// stored already, and the next one will not be.
    ///
    /// Costs 272 B per record before compression. The header is 203 constant
    /// bytes out of 272 on a real capture, which is the most compressible thing
    /// in the record, so the exported `.csiq.zst` absorbs most of it — see
    /// [`crate::export::CSIQ_NAME`].
    ///
    /// Default **on**: the whole point of Phase 6 is that the archive stops
    /// discarding 238 of 272 bytes it was already handed.
    #[serde(default = "default_true")]
    pub keep_vendor_hdr: bool,
    /// Record node and host state in the stream, once per this many seconds.
    ///
    /// `0` disables it. See [`crate::nodestate`] for why the file carries state
    /// the metrics store already has.
    #[serde(default = "default_node_state_seconds")]
    pub node_state_seconds: u64,
}

fn default_true() -> bool {
    true
}

fn default_node_state_seconds() -> u64 {
    60
}

/// Hand-written, NOT derived.
///
/// `derive(Default)` would give `keep_vendor_hdr = false` when the whole
/// `[export]` table is absent while serde's field default gives `true` when the
/// table exists without the key — one setting with two answers, decided by
/// whether an unrelated key happens to be present. Every default lives here and
/// the field attributes point at the same functions.
impl Default for ExportConfig {
    fn default() -> Self {
        ExportConfig {
            on_close: false,
            keep_vendor_hdr: default_true(),
            node_state_seconds: default_node_state_seconds(),
        }
    }
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
        // In STA mode the link owns the channel and the width, so the radio
        // block's channel is not read and must not be validated as a tune.
        if self.capture.mode == "sta" {
            if self.radio.interface.is_empty() {
                anyhow::bail!("radio.interface must be set");
            }
            if !self.sta.bssid.is_empty() && !is_mac(&self.sta.bssid) {
                anyhow::bail!("sta.bssid {:?} is not a MAC address", self.sta.bssid);
            }
            // Every sibling that reads raw 802.11 frames needs the monitor
            // interface, which an associated capture does not create.
            if self.timesync.enabled {
                anyhow::bail!("timesync.enabled is not available with capture.mode = \"sta\" (no monitor interface)");
            }
            if self.census.enabled {
                anyhow::bail!("census.enabled is not available with capture.mode = \"sta\" (no monitor interface)");
            }
        } else {
            caps::validate_radio(&self.radio)?;
        }
        self.census.validate()?;
        self.survey
            .validate(&self.radio.interface, &self.radio.monitor)?;

        // Segment rotation. A too-short segment turns a capture into a sealing
        // treadmill (every rotation costs an fsync plus a CSIQ export of the
        // segment just closed); a segment longer than the session never fires
        // and is almost certainly a units mistake — both are cheap to catch
        // here and expensive to discover at hour six of an unattended run.
        // A session that neither keeps its records nor publishes them tunes a
        // radio, burns a core and produces nothing. Cheap to catch here; on the
        // node it looks exactly like a working capture.
        if !self.capture.persist && !self.stream.enabled {
            anyhow::bail!(
                "capture.persist = false with stream.enabled = false: this session would \
                 discard every record it captured. Enable the stream (so a consumer such as \
                 csiscope receives them) or set capture.persist = true."
            );
        }
        // Rotation is a property of the file being written, so it means nothing
        // without one. Silently ignoring it would leave a profile that reads as
        // if it were bounding disk while it wrote nothing at all.
        if !self.capture.persist && self.capture.segment_duration.is_some() {
            anyhow::bail!(
                "capture.segment_duration is set with capture.persist = false: there is no \
                 capture.raw to roll. Drop one of the two."
            );
        }
        if !self.capture.persist && self.export.on_close {
            anyhow::bail!(
                "export.on_close = true with capture.persist = false: a CSIQ export reads \
                 capture.raw, which this session does not write. Set export.on_close = false."
            );
        }

        if let Some(seg) = self.capture.segment_duration {
            if seg < Duration::from_secs(60) {
                anyhow::bail!(
                    "capture.segment_duration must be >= 60s (got {}); \
                     shorter segments spend more time sealing than capturing",
                    humantime_serde::re::humantime::format_duration(seg)
                );
            }
            if let Some(total) = self.capture.duration {
                if seg >= total {
                    anyhow::bail!(
                        "capture.segment_duration ({}) must be shorter than capture.duration ({}) \
                         — as written the session would produce exactly one segment, so drop \
                         segment_duration or shorten it",
                        humantime_serde::re::humantime::format_duration(seg),
                        humantime_serde::re::humantime::format_duration(total)
                    );
                }
            }
        }

        match self.capture.mode.as_str() {
            "passive" | "sta" => {}
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
                match validate_monitor_tx_rate(inj.monitor_tx_rate) {
                    Ok(()) => {}
                    Err(e) if inj.allow_untransmittable_rate => {
                        tracing::warn!(
                            refusal = %e,
                            "inject.allow_untransmittable_rate is set — proceeding with a rate \
                             word this build believes the firmware will not transmit. Verify on \
                             air at a NEIGHBOUR (tcpdump, or its rows_csid); errors = 0 and \
                             own_transmissions are not evidence."
                        );
                    }
                    Err(e) => return Err(e),
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

    /// A segmented profile is legal on its own terms, and 80 MHz has nothing to
    /// do with it — the pair `segment_duration >= duration` is what validate()
    /// rejects. This is the shape `csid bench` used to build by shortening
    /// `duration` while leaving the profile's segment length alone, which made
    /// every segmenting profile look like it had an invalid radio config.
    #[test]
    fn rejects_segment_longer_than_session() {
        let segmented = SAMPLE.replace(
            "duration = \"30m\"",
            "duration = \"30m\"\nsegment_duration = \"5m\"",
        );
        let cfg: ExperimentConfig = toml::from_str(&segmented).unwrap();
        cfg.validate()
            .expect("5m segments inside a 30m session are fine");

        // Shorten the session below the segment — what bench did.
        let bench_shaped =
            segmented.replace("duration = \"30m\"\nsegment", "duration = \"30s\"\nsegment");
        let cfg: ExperimentConfig = toml::from_str(&bench_shaped).unwrap();
        assert!(
            cfg.validate().is_err(),
            "a 5m segment inside a 30s session must be rejected"
        );

        // Dropping segmentation — what bench now does — makes it valid again,
        // proving the radio config (ch36 @ 80MHz) was never the problem.
        let mut cfg: ExperimentConfig = toml::from_str(&bench_shaped).unwrap();
        cfg.capture.segment_duration = None;
        cfg.validate()
            .expect("ch36 @ 80MHz is valid; only the segment/duration pair was wrong");
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

    /// Segmentation is opt-in; every existing experiment file predates the
    /// field and must keep validating and behaving exactly as before.
    #[test]
    fn segmentation_is_absent_by_default() {
        let cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        assert!(cfg.capture.segment_duration.is_none());
        cfg.validate().unwrap();
    }

    /// `SAMPLE` already carries a `[capture]` table, so these build on the
    /// parsed struct rather than appending TOML (which would be a duplicate
    /// key, not a second section).
    fn sample_with_segments(duration: Option<&str>, segment: Option<&str>) -> ExperimentConfig {
        let parse = |s: &str| humantime_serde::re::humantime::parse_duration(s).unwrap();
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.duration = duration.map(parse);
        cfg.capture.segment_duration = segment.map(parse);
        cfg
    }

    /// Persisting must stay the default. A profile that says nothing about
    /// storage keeps its data — the opposite default would turn every existing
    /// experiment profile into a stream-only session on upgrade.
    #[test]
    fn persist_defaults_to_true() {
        let cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        assert!(cfg.capture.persist);
        assert!(CaptureConfig::default().persist);
    }

    /// The live-view case this exists for: stream on, storage off.
    #[test]
    fn a_stream_only_session_is_valid() {
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.persist = false;
        cfg.capture.segment_duration = None;
        cfg.stream.enabled = true;
        cfg.export.on_close = false;
        cfg.validate().unwrap();
    }

    /// Neither kept nor published is a radio tuned for nothing, and on the node
    /// it looks exactly like a working capture.
    #[test]
    fn a_session_that_neither_persists_nor_streams_is_rejected() {
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.persist = false;
        cfg.capture.segment_duration = None;
        cfg.stream.enabled = false;
        cfg.export.on_close = false;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("discard every record"), "{err}");
    }

    /// Rotation and export both read a file this session does not write. Failing
    /// loudly beats ignoring them, which would leave a profile that reads as if
    /// it were bounding disk while writing nothing at all.
    #[test]
    fn stream_only_rejects_the_knobs_that_need_a_file() {
        let base = || {
            let mut c: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
            c.capture.persist = false;
            c.capture.segment_duration = None;
            c.stream.enabled = true;
            c.export.on_close = false;
            c
        };

        let mut with_segments = base();
        with_segments.capture.segment_duration = Some(Duration::from_secs(1800));
        let err = with_segments.validate().unwrap_err().to_string();
        assert!(
            err.contains("no \ncapture.raw to roll") || err.contains("capture.raw to roll"),
            "{err}"
        );

        let mut with_export = base();
        with_export.export.on_close = true;
        let err = with_export.validate().unwrap_err().to_string();
        assert!(err.contains("CSIQ export"), "{err}");
    }

    #[test]
    fn a_segment_shorter_than_a_minute_is_rejected() {
        // Below this the run spends more time sealing (fsync + CSIQ export of
        // the segment just closed) than capturing.
        let err = sample_with_segments(Some("1h"), Some("5s"))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("60s"), "{err}");
    }

    /// A segment at least as long as the session yields exactly one segment —
    /// invariably a units slip (`30m` vs `30s`), and one that would otherwise
    /// only reveal itself as "why did nothing upload?" hours into a run.
    #[test]
    fn a_segment_no_shorter_than_the_session_is_rejected() {
        let err = sample_with_segments(Some("30m"), Some("30m"))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("shorter than"), "{err}");
    }

    #[test]
    fn a_sane_segment_validates() {
        let cfg = sample_with_segments(Some("12h"), Some("30m"));
        cfg.validate().unwrap();
        assert_eq!(
            cfg.capture.segment_duration,
            Some(Duration::from_secs(1800))
        );
    }

    /// An open-ended session (the console feed, a drift run stopped by hand)
    /// is exactly where rotation matters most, so it must not need a duration.
    #[test]
    fn segmentation_works_without_a_session_duration() {
        sample_with_segments(None, Some("30m")).validate().unwrap();
    }

    /// The field must survive a TOML round trip — an operator writes it by
    /// hand in `/etc/csid/experiments/<exp>.toml` and Ansible renders it there.
    #[test]
    fn segment_duration_round_trips_through_toml() {
        let cfg = sample_with_segments(Some("12h"), Some("30m"));
        let rendered = toml::to_string(&cfg).unwrap();
        assert!(rendered.contains("segment_duration"), "{rendered}");
        let back: ExperimentConfig = toml::from_str(&rendered).unwrap();
        assert_eq!(
            back.capture.segment_duration,
            Some(Duration::from_secs(1800))
        );
    }

    // ── inject.monitor_tx_rate ────────────────────────────────────────────
    //
    // The words below are the ones actually written to hardware. `0x4100` is
    // what all 47 fleet arms carry; the HE trio is what the 2026-08-29 bench
    // refused; `0xd300` is the VHT80 candidate the backport author left
    // unmarked. Naming real words keeps the test honest about what it pins.

    /// The word the whole fleet runs on must keep validating, or every arm
    /// stops at config load.
    #[test]
    fn the_fleet_word_is_accepted() {
        validate_monitor_tx_rate(0x4100).unwrap();
    }

    /// 0 is "knob off", not a rate. It must not be decoded as CCK (type 0).
    #[test]
    fn zero_means_the_knob_is_off_and_is_not_read_as_cck() {
        validate_monitor_tx_rate(0).unwrap();
    }

    /// All three words the bench sent with `errors = 0` and nothing on air.
    #[test]
    fn every_benched_he_word_is_refused() {
        for word in [0x4400_u32, 0x5400, 0x5C00] {
            let err = validate_monitor_tx_rate(word).unwrap_err().to_string();
            assert!(err.contains("does not transmit"), "0x{word:x}: {err}");
            assert!(err.contains("He"), "0x{word:x}: {err}");
        }
    }

    /// The backport author's own `err`-marked words carry both antennas, so
    /// the refusal must key on the modulation type and not on the antenna bits.
    #[test]
    fn the_backport_authors_err_words_are_refused_too() {
        for word in [0xc400_u32, 0xd400, 0xdc00] {
            validate_monitor_tx_rate(word).unwrap_err();
        }
    }

    /// EHT is refused on the same grounds, ahead of hardware that could send it.
    #[test]
    fn eht_is_refused() {
        // modulation type 5, 20 MHz, antenna A.
        validate_monitor_tx_rate(0x4500).unwrap_err();
    }

    /// CCK carries no OFDM preamble, so it yields no CSI — the same rule
    /// `inject.bitrate_mbps` already enforces.
    #[test]
    fn cck_is_refused_for_the_same_reason_as_a_cck_bitrate() {
        let err = validate_monitor_tx_rate(0x4000).unwrap_err().to_string();
        assert!(err.contains("no OFDM preamble"), "{err}");
    }

    /// A modulation type this build cannot name is refused rather than sent.
    #[test]
    fn an_unnameable_modulation_type_is_refused() {
        // type 6 — reserved, decoded as Unknown(6).
        let err = validate_monitor_tx_rate(0x4600).unwrap_err().to_string();
        assert!(err.contains("cannot name"), "{err}");
    }

    /// HT and VHT warn and pass. Nobody has measured them failing, and VHT80
    /// is the word worth benching — refusing it would need a deploy to undo.
    #[test]
    fn ht_and_vht_words_are_allowed() {
        for word in [0xc200_u32, 0xca00, 0xc300, 0xcb00, 0xd300, 0xdb00] {
            validate_monitor_tx_rate(word)
                .unwrap_or_else(|e| panic!("0x{word:x} should warn, not bail: {e}"));
        }
    }

    /// The whole point is that the check runs at config load, before the radio
    /// is touched — not somewhere the operator has to remember to look.
    #[test]
    fn an_he_word_fails_the_experiment_config_validation() {
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.mode = "inject".to_string();
        cfg.inject.monitor_tx_rate = 0x5400;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("monitor_tx_rate"), "{err}");
    }

    /// The escape hatch must let a deliberate bench through — otherwise the
    /// guard freezes a belief instead of protecting a measurement.
    #[test]
    fn an_explicit_opt_in_lets_a_refused_word_through() {
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.mode = "inject".to_string();
        cfg.inject.monitor_tx_rate = 0xc400; // HE20, both antennas
        cfg.inject.allow_untransmittable_rate = true;
        cfg.validate().unwrap();
    }

    /// Without the opt-in the same word is still refused. The hatch must be
    /// opt-in, never a default, or the guard is decorative.
    #[test]
    fn the_opt_in_defaults_off() {
        assert!(!InjectConfig::default().allow_untransmittable_rate);
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.mode = "inject".to_string();
        cfg.inject.monitor_tx_rate = 0xc400;
        cfg.validate().unwrap_err();
    }

    /// A passive session never injects, so its knob is never written and must
    /// not be able to block a capture.
    #[test]
    fn a_passive_session_ignores_the_inject_knob() {
        let mut cfg: ExperimentConfig = toml::from_str(SAMPLE).unwrap();
        cfg.capture.mode = "passive".to_string();
        cfg.inject.monitor_tx_rate = 0x5400;
        cfg.validate().unwrap();
    }
}
