//! The session sidecar (`metadata.json`).
//!
//! Design rule from IP-120: **the sidecar alone must suffice to interpret the
//! capture months later.** It is written at session open (so a crashed session
//! still has provenance) and rewritten at close with the outcome. Environment
//! capture is best-effort — a missing `ethtool` never fails a session.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::caps::Band;
use crate::config::{ExperimentConfig, GlobalConfig};
use crate::radio::Tuning;
use crate::util::{self, rfc3339_utc};

/// Sidecar schema identifier. Bump on any incompatible field change.
pub const SCHEMA: &str = "csid-session/1";

/// Lifecycle status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Session is running (sidecar written at open).
    Capturing,
    /// Ran to the configured duration.
    Complete,
    /// Stopped early by signal / `systemctl stop`.
    Stopped,
    /// Setup or runtime failure; raw kept for forensics.
    Failed,
}

/// Radio configuration as it was actually applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioMeta {
    pub interface: String,
    pub monitor: String,
    pub band: String,
    pub channel: u32,
    pub control_freq_mhz: u32,
    pub center_freq_mhz: Option<u32>,
    pub width: String,
    pub interval_us: u32,
    pub mac_filter: Vec<String>,
}

/// Host/driver/firmware environment — the part that makes a capture
/// interpretable years later.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvironmentMeta {
    pub hostname: Option<String>,
    pub kernel: Option<String>,
    pub driver_module: Option<String>,
    pub firmware: Option<String>,
    pub regdomain: Option<String>,
    pub cpu_governor: Option<String>,
    pub csid_version: String,
}

/// Close-time capture statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SummaryMeta {
    pub capture_bytes: u64,
    pub records: u64,
    pub mean_rate_hz: f64,
    pub live_dropped: u64,
    /// Distinct tone counts observed (52 / 242 / 996 …).
    pub tone_counts: Vec<u16>,
    /// Injector totals — present only for `capture.mode = "inject"` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<InjectSummary>,
    /// BLE co-capture health — present only when `[ble].enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ble: Option<BleSummary>,
    /// Time-transfer health — present only when `[timesync].enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timesync: Option<TimesyncSummary>,
}

/// What the injector actually did, for delivery-ratio analysis downstream
/// (receiver records ÷ `sent` is the arm's delivery fraction).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InjectSummary {
    pub sent: u64,
    pub errors: u64,
    pub skipped: u64,
}

/// Injector configuration as applied — the provenance the receiving side's
/// analysis keys on (sentinel MAC, commanded pace).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectMeta {
    pub rate_hz: u32,
    pub frame_bytes: usize,
    pub src_mac: String,
    pub dst_mac: String,
    pub bitrate_mbps: u32,
}

/// BLE co-capture configuration as applied, plus the pseudonymisation scheme
/// the analysis side has to understand to interpret `device_hash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BleMeta {
    pub adapter: String,
    /// Always `"passive"` — see [`crate::hci`] for why there is no other option.
    pub scan_type: String,
    pub scan_interval_ms: f64,
    pub scan_window_ms: f64,
    /// `false`: a BLE failure degrades the session, it does not fail it.
    pub required: bool,
    /// Artefact names, so the sidecar alone tells a reader what to open.
    pub artefact: String,
    pub durable_log: String,
    pub parquet_schema: String,
    /// How `device_hash` was derived. The salt itself is deliberately absent —
    /// it never leaves the capturing process's memory, which is what makes the
    /// pseudonyms unlinkable across sessions.
    pub hash_algorithm: String,
    pub hash_bytes: usize,
    pub salt_bits: usize,
    pub salt_persisted: bool,
}

/// BLE co-capture outcome. Every field exists so that a *silently* degraded
/// scanner is as visible as an absent one — the readiness audit's R3 lesson.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BleSummary {
    /// `ok` · `degraded` (ran, but restarted or went quiet) · `failed` (never
    /// produced an observation) · `disabled`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub observations: u64,
    /// Distinct pseudonyms. An **upper bound** on devices: rotating private
    /// addresses split one device across several. Use `addr_kind` in the
    /// parquet to bound the stable-identity population.
    pub distinct_device_hashes: u64,
    pub mean_rate_hz: f64,
    pub max_gap_s: f64,
    pub gaps_over_alert: u64,
    pub gap_alert_s: f64,
    pub scan_restarts: u64,
    pub adapter_errors: u64,
    pub unparsed_events: u64,
    pub rssi_unavailable: u64,
    pub parquet_rows: u64,
    pub malformed_log_lines: u64,
}

/// Time-transfer configuration as applied — the provenance a reader needs to
/// interpret `time_transfer.parquet` without opening the config that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimesyncMeta {
    pub required: bool,
    pub artefact: String,
    pub durable_log: String,
    pub parquet_schema: String,
    /// The pairing window used to attribute an `ftm` to a received frame.
    pub ftm_tolerance_us: u64,
    /// The one-way-delay floor assumed when reporting the phone-offset
    /// interval. It biases the offset, never the slope.
    pub one_way_floor_us: u64,
}

/// Time-transfer outcome.
///
/// `rx_stamp_source` is the field to read first. A `userspace` session carries
/// the scheduler's wake-up jitter in every receive stamp — the same order as
/// the inter-node skew being measured — and must not be pooled with
/// `kernel`-stamped sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimesyncSummary {
    /// `ok` · `degraded` (ran, but nothing usable) · `failed` · `disabled`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `kernel` · `userspace` · `mixed` · `none`.
    pub rx_stamp_source: String,
    pub rows: u64,
    pub rows_csid: u64,
    pub rows_app: u64,
    pub distinct_transmitters: u64,
    pub mean_rate_hz: f64,
    /// Rows credited with a paired CSI `ftm`. A low fraction is normal on a
    /// channel the radio does not sound every frame on, and is not an error.
    pub ftm_paired: u64,
    pub frames_seen: u64,
    /// Locally transmitted frames looped back by `AF_PACKET` and skipped. A
    /// node must never "receive" its own injector.
    pub own_transmissions: u64,
    /// Encrypted data frames. Large with zero `rows_app` means the experiment
    /// SSID is not open, and the phone's stamps are unreadable from the air.
    pub protected_frames: u64,
    pub unrecognised_frames: u64,
    pub malformed_log_lines: u64,
}

/// The full sidecar document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub schema: String,
    pub session_id: String,
    pub experiment: String,
    pub tag: Option<String>,
    pub radio: RadioMeta,
    /// Present only for `capture.mode = "inject"` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<InjectMeta>,
    /// Present only when BLE co-capture is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ble: Option<BleMeta>,
    /// Present only when time transfer is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timesync: Option<TimesyncMeta>,
    pub environment: EnvironmentMeta,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: Status,
    pub summary: Option<SummaryMeta>,
    #[serde(skip)]
    path: PathBuf,
}

impl Sidecar {
    /// Build the open-time sidecar for a session.
    pub fn open(
        dir: &Path,
        session_id: String,
        cfg: &ExperimentConfig,
        global: &GlobalConfig,
        tuning: &Tuning,
    ) -> Result<Self> {
        let radio = RadioMeta {
            interface: cfg.radio.interface.clone(),
            monitor: cfg.radio.monitor.clone(),
            band: band_label(tuning.band).to_string(),
            channel: cfg.radio.channel,
            control_freq_mhz: tuning.freq,
            center_freq_mhz: tuning.center,
            width: cfg.radio.width.iw_token().to_string(),
            interval_us: cfg.radio.interval_us,
            mac_filter: cfg.radio.mac_filter.clone(),
        };

        let inject = (cfg.capture.mode == "inject").then(|| InjectMeta {
            rate_hz: cfg.inject.rate_hz,
            frame_bytes: cfg.inject.frame_bytes,
            src_mac: cfg.inject.src_mac.clone(),
            dst_mac: cfg.inject.dst_mac.clone(),
            bitrate_mbps: cfg.inject.bitrate_mbps,
        });

        let ble = cfg.ble.enabled.then(|| BleMeta {
            adapter: cfg.ble.adapter.clone(),
            scan_type: "passive".to_string(),
            scan_interval_ms: cfg.ble.scan_interval_ms,
            scan_window_ms: cfg.ble.scan_window_ms,
            required: cfg.ble.required,
            artefact: crate::ble::PARQUET_NAME.to_string(),
            durable_log: crate::ble::NDJSON_NAME.to_string(),
            parquet_schema: crate::ble::PARQUET_SCHEMA.to_string(),
            hash_algorithm: "sha256(salt || addr_type || addr)[:hash_bytes], hex".to_string(),
            hash_bytes: cfg.ble.hash_bytes,
            salt_bits: 256,
            salt_persisted: false,
        });

        let timesync = cfg.timesync.enabled.then(|| TimesyncMeta {
            required: cfg.timesync.required,
            artefact: crate::timesync::PARQUET_NAME.to_string(),
            durable_log: crate::timesync::NDJSON_NAME.to_string(),
            parquet_schema: crate::timesync::PARQUET_SCHEMA.to_string(),
            ftm_tolerance_us: cfg.timesync.ftm_tolerance_us,
            one_way_floor_us: cfg.timesync.one_way_floor_us,
        });

        let sc = Sidecar {
            schema: SCHEMA.to_string(),
            session_id,
            experiment: cfg.slug().to_string(),
            tag: cfg.tag.clone(),
            radio,
            inject,
            ble,
            timesync,
            environment: capture_environment(global, &cfg.radio.interface),
            started_at: rfc3339_utc(util::now_unix()),
            ended_at: None,
            status: Status::Capturing,
            summary: None,
            path: dir.join("metadata.json"),
        };
        sc.write()?;
        Ok(sc)
    }

    /// Finalise the sidecar with an outcome and (optional) summary.
    pub fn close(&mut self, status: Status, summary: Option<SummaryMeta>) {
        self.status = status;
        self.ended_at = Some(rfc3339_utc(util::now_unix()));
        self.summary = summary;
        if let Err(e) = self.write() {
            // Never let a sidecar write failure invalidate captured data.
            tracing::error!(error = %e, "writing close-time sidecar failed; raw capture is intact");
        }
    }

    /// Serialise to disk (pretty-printed — it is meant to be read by humans).
    pub fn write(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, json + "\n")
            .with_context(|| format!("writing sidecar {}", self.path.display()))
    }

    /// Path of the sidecar on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn band_label(b: Band) -> &'static str {
    match b {
        Band::Ghz24 => "2.4",
        Band::Ghz5 => "5",
        Band::Ghz6 => "6",
    }
}

/// Best-effort environment probe. Every field is optional by construction.
pub fn capture_environment(global: &GlobalConfig, iface: &str) -> EnvironmentMeta {
    let hostname = global
        .node
        .hostname
        .clone()
        .or_else(|| util::run_opt("hostname", &[]))
        .or_else(|| std::env::var("HOSTNAME").ok());

    let firmware = util::run_opt("ethtool", &["-i", iface]).and_then(|out| {
        out.lines().find_map(|l| {
            l.strip_prefix("firmware-version:")
                .map(|v| v.trim().to_string())
        })
    });

    let driver_module = util::run_opt("modinfo", &["-F", "filename", "iwlwifi"]);

    let cpu_governor = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .ok()
        .map(|s| s.trim().to_string());

    EnvironmentMeta {
        hostname,
        kernel: util::run_opt("uname", &["-r"]),
        driver_module,
        firmware,
        regdomain: crate::radio::regdomain(),
        cpu_governor,
        csid_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}
