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
}

/// The full sidecar document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub schema: String,
    pub session_id: String,
    pub experiment: String,
    pub tag: Option<String>,
    pub radio: RadioMeta,
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

        let sc = Sidecar {
            schema: SCHEMA.to_string(),
            session_id,
            experiment: cfg.slug().to_string(),
            tag: cfg.tag.clone(),
            radio,
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
