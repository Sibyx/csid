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
    /// What the radio answered when asked, after the tune (`iw dev … info`).
    ///
    /// The three fields above them are the **request** — `channel`,
    /// `control_freq_mhz` and `center_freq_mhz` are computed by
    /// `caps::center_freq` from the profile, and they say what csid asked for.
    /// These say what it got. They differ when a tune is accepted by `iw` and
    /// not honoured by the radio, which leaves a session recording at the
    /// previous width while closing clean.
    ///
    /// `None` means the radio did not say — a monitor interface that is down
    /// prints no channel line. It is not a mismatch, and it is not a zero.
    ///
    /// Absent on every sidecar written before csid 0.2.0.
    #[serde(default)]
    pub achieved_control_freq_mhz: Option<u32>,
    #[serde(default)]
    pub achieved_width_mhz: Option<u32>,
    #[serde(default)]
    pub achieved_center_freq_mhz: Option<u32>,
}

/// What the radio was allowed to report (IP-139 Phase 3, C3).
///
/// A filter is a claim about the data, not a tuning detail. A capture taken
/// with `frame_types = ["data"]` and one taken without it are not the same
/// measurement, and a reader must not have to guess which they hold. So the
/// selection in force is recorded whether or not any of it is set.
///
/// **Every field here is currently `None`, and that is the finding, not an
/// omission.** `csiscope` scopes each analytical panel to one record class in
/// software because the radio was told to report everything from everyone. The
/// driver has supported `csi_frame_types` and `csi_rate_n_flags_val`/`_mask`
/// the whole time (`debugfs::knob` names all nine parameters and csid drives
/// four). Driving them is Phase 4. Recording the state is this phase, so the
/// archive gains a clean boundary: every file from 0.2.0 onward *declares* that
/// no PHY selection was in force, instead of leaving it unrecoverable.
///
/// The other two selection knobs csid does drive are not repeated here —
/// `csi_interval` is `radio.interval_us` and `csi_addresses` is
/// `radio.mac_filter`. One fact belongs in one field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterMeta {
    /// `csi_frame_types` — 802.11 frame-type bitmap. `None` = not driven.
    pub frame_types: Option<u64>,
    /// `csi_rate_n_flags_val` — collect only for this PHY. `None` = not driven.
    pub rate_n_flags_val: Option<u32>,
    /// `csi_rate_n_flags_mask` — which bits of the above are compared.
    pub rate_n_flags_mask: Option<u32>,
    /// `csi_count` — stop after N reports. `None` = not driven.
    pub count: Option<u64>,
    /// `csi_timeout` — stop after N microseconds. `None` = not driven.
    pub timeout_us: Option<u64>,
    /// Stable digest over the resolved selection triple, or [`NO_FILTER`].
    ///
    /// It exists so two differently-filtered captures cannot land in one
    /// poolable group by accident. This project has already paid for the
    /// software version of that mistake by mixing record classes in one tensor.
    pub fingerprint: String,
}

/// The reserved `fingerprint` value meaning "the radio filtered nothing".
///
/// Reserved rather than empty: an absent fingerprint and an unfiltered capture
/// are different facts, and a grouping key must not conflate them.
pub const NO_FILTER: &str = "no-filter";

impl FilterMeta {
    /// The filter csid actually put in force for this session.
    ///
    /// Takes the config so the signature does not change when Phase 4 begins
    /// populating it — only the body does, and no reader has to be revisited.
    pub fn resolve(_cfg: &ExperimentConfig) -> Self {
        let f = FilterMeta::default();
        FilterMeta {
            fingerprint: f.compute_fingerprint(),
            ..f
        }
    }

    /// Digest over `(frame_types, rate_n_flags_val, rate_n_flags_mask)`.
    ///
    /// `count` and `timeout_us` are deliberately excluded. They bound how much
    /// the radio reports, not which frames it selects, so two captures that
    /// differ only in duration must stay poolable.
    fn compute_fingerprint(&self) -> String {
        if self.frame_types.is_none()
            && self.rate_n_flags_val.is_none()
            && self.rate_n_flags_mask.is_none()
        {
            return NO_FILTER.to_string();
        }
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.frame_types.unwrap_or(0).to_le_bytes());
        h.update(self.rate_n_flags_val.unwrap_or(0).to_le_bytes());
        h.update(self.rate_n_flags_mask.unwrap_or(0).to_le_bytes());
        format!("{:x}", h.finalize())[..16].to_string()
    }
}

/// Which csid build wrote this capture (IP-139 Phase 3).
///
/// `csid_version` stays the semantic version and keeps its meaning unchanged —
/// redefining an existing field's format mid-archive would make old and new
/// rows incomparable in the measurement lake, which already has a
/// `csid_version` column. The build identity is therefore recorded *beside* it
/// rather than inside it.
///
/// `revision_source` is the field to read first. `none` means this build could
/// not name its own revision, which is a different fact from a revision that
/// happens to be a bare hash — see [`crate::build_info`] for why a fleet node
/// cannot always read one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildMeta {
    /// `git describe --always --dirty --tags`, an operator-supplied identity,
    /// or empty when neither was available.
    pub revision: String,
    /// `git` · `supplied` · `none`.
    pub revision_source: String,
    /// When the binary was compiled, RFC 3339 UTC.
    pub built_at: String,
    pub rustc: String,
    /// `release` on anything that ships, `debug` on a developer build.
    pub profile: String,
    /// The CSIQ container version this build writes.
    pub csiq_format_version: u16,
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
    /// `serde(default)` is load-bearing: every sidecar in the archive predates
    /// this group, and a required field here would make csid unable to read its
    /// own back catalogue. An all-empty `build` therefore means "written before
    /// build provenance existed".
    #[serde(default)]
    pub build: BuildMeta,
}

/// One node-and-host-state reading, stamped with when it was taken.
///
/// A sparse SERIES, not a per-record column. It lives in the session block
/// rather than on the records because the `.csiq` is derived from `capture.raw`
/// at teardown: a per-record sample attached during export would carry the
/// teardown instant on every record, which is a fabricated timestamp on a real
/// measurement. Sampling in the capture loop and stamping each reading is the
/// only way the times mean anything.
///
/// The TLV codes `0x40`–`0x43` remain allocated for the live datagram path,
/// where a record IS produced in the moment and the stamp is implicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStateSample {
    /// Seconds since the session opened. Relative on purpose — the fleet has no
    /// RTC and `chrony` may step the wallclock mid-session, which would move an
    /// absolute stamp and leave the reading describing the wrong instant.
    pub at_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_mc: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_flags: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spool_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_m: Option<u32>,
    /// Wi-Fi NIC die temperature, whole degrees Celsius.
    ///
    /// Whole degrees because the driver reports whole degrees. Absent on every
    /// session captured before this field existed, and absent on any tick where
    /// the firmware was not running — which is a normal state, not a fault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nic_temp_c: Option<i32>,
}

/// Close-time capture statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SummaryMeta {
    pub capture_bytes: u64,
    pub records: u64,
    /// Of `records`, how many carried an all-zero I/Q matrix.
    ///
    /// The durable counterpart of `status.empty_records`. On a persisting
    /// session it is recounted from `capture.raw` by `engine::summarize`,
    /// exactly as `records` is, so the sidecar and the raw stream cannot
    /// disagree. On a non-persisting one it is the RX thread's own count.
    ///
    /// Present so a corpus size can be quoted honestly after the fact: a
    /// session's useful record count is `records - empty_records`, and before
    /// this field existed the difference was unrecoverable from the archive
    /// without re-reading every segment.
    #[serde(default)]
    pub empty_records: u64,
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
    /// Who transmitted the records in this directory.
    ///
    /// Present on **segments**, where it is the only way to read delivery
    /// mid-run: `timesync` and `ble` describe whole-session artefacts at the
    /// session root and are therefore computed once at teardown, so a 16 h run
    /// could report nothing about its own delivery until it ended. A per-MAC
    /// record count needs no pairing and no second pass — the sealer already
    /// walks every record — and dividing the injector's count by its commanded
    /// rate is exactly the delivery fraction.
    ///
    /// It is deliberately counts and not a percentage: a receiver does not know
    /// the injector's commanded rate, and inventing a denominator here would be
    /// the same mistake as reading a frame rate as a CSI rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transmitters: Option<TransmitterCensus>,
    /// Node and host state through the session (IP-139 Phase 6).
    ///
    /// Empty when sampling is disabled or the session was too short for a tick.
    /// An empty series means "not sampled", never "the node was idle and cool".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_state: Vec<NodeStateSample>,
}

/// Per-source-MAC record counts for one capture directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransmitterCensus {
    /// How many distinct source MACs appeared, before `top` was truncated.
    pub distinct: u64,
    /// Busiest transmitters, descending. Truncated to keep a sidecar small on
    /// an ambient channel, which is why `distinct` is reported separately.
    pub top: Vec<TransmitterCount>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransmitterCount {
    pub mac: String,
    pub records: u64,
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
    /// Lab identity namespace (`ble-rssi/2`), canonical form. `None` = matching
    /// was not configured, so `lab_*` columns are structurally all-null rather
    /// than "nobody broadcast" — a reader must distinguish those two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_namespace_uuid: Option<String>,
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
    /// Advertisements matching the lab identity namespace (`ble-rssi/2`).
    /// Zero with a configured namespace means no consented handset was heard,
    /// which for a quest session is a finding, not a formality.
    #[serde(default)]
    pub lab_frames: u64,
    /// Distinct `lab_participant_key` values — exact consented-handset count,
    /// immune to the address-rotation bracket above.
    #[serde(default)]
    pub distinct_lab_participants: u64,
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
    ///
    /// # This is not evidence that a frame reached the air
    ///
    /// It counts what the local TX-status path reported, which is upstream of
    /// the antenna. Measured 2026-08-29 across a five-cell injection ladder:
    /// this counter read ~4,002 in **every** cell, including the three whose
    /// `rate_n_flags` word the firmware silently declined to transmit and for
    /// which `tcpdump -e` on the neighbour printed zero frames.
    ///
    /// Only a second node discriminates — the neighbour's `tcpdump`, or its
    /// `rows_csid`. Treat a healthy number here as "csid tried", never as
    /// "the room was illuminated". The guard against sending an untransmittable
    /// word lives in `config::validate_monitor_tx_rate`, not in this field.
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
    /// Fleet run identifier — the thing that makes a multi-node capture one
    /// addressable object.
    ///
    /// Every node stamps its own `session_id` from its own start instant, so
    /// five nodes in one experiment produce five unrelated ids
    /// (`…184314` … `…184323`, observed 2026-08-12) and every cross-node
    /// analysis begins by resolving them by hand. Set `CSID_RUN_ID` on the
    /// units of a fleet capture and they all carry the same value.
    ///
    /// When unset, csid generates one so the field is never absent — but see
    /// `run_id_generated`: a generated id groups nothing but its own session,
    /// and analysis code must not treat it as evidence of a fleet run.
    ///
    /// `serde(default)` is load-bearing, not tidiness: every sidecar written
    /// before this field existed lacks it, including the whole 2026-08-12
    /// fleet run and every cached segment. Without a default, adding the field
    /// would make csid unable to READ its own back catalogue. An empty string
    /// therefore means "written before run ids existed" — distinct from a
    /// generated one, and not groupable either.
    #[serde(default)]
    pub run_id: String,
    /// True when csid invented the `run_id` because none was supplied.
    #[serde(default)]
    pub run_id_generated: bool,
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
    /// What the radio was allowed to report.
    ///
    /// `serde(default)` for the same reason as `environment.build`: the whole
    /// archive predates this group, and csid must keep reading its own back
    /// catalogue. A default `FilterMeta` carries an empty `fingerprint`, which
    /// is distinct from [`NO_FILTER`] — "not recorded" is not "nothing filtered".
    #[serde(default)]
    pub filter: FilterMeta,
    pub environment: EnvironmentMeta,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: Status,
    pub summary: Option<SummaryMeta>,
    #[serde(skip)]
    path: PathBuf,
}

/// Environment variable carrying the fleet run identifier.
pub const RUN_ID_ENV: &str = "CSID_RUN_ID";

/// Resolve the run identifier: supplied by the launcher, or invented here.
///
/// Deliberately NOT derived from the experiment name plus a time window. That
/// inference is what this field exists to replace, and it breaks exactly when
/// it matters — staggered starts, a node restarted mid-run, two runs of the
/// same experiment in one evening.
fn resolve_run_id(session_id: &str) -> (String, bool) {
    match std::env::var(RUN_ID_ENV) {
        Ok(v) if !v.trim().is_empty() => (v.trim().to_string(), false),
        _ => (format!("solo-{session_id}"), true),
    }
}

impl Sidecar {
    /// Build the open-time sidecar for a session.
    pub fn open(
        dir: &Path,
        session_id: String,
        cfg: &ExperimentConfig,
        global: &GlobalConfig,
        tuning: &Tuning,
        achieved: Option<&crate::radio::Achieved>,
    ) -> Result<Self> {
        let (run_id, run_id_generated) = resolve_run_id(&session_id);

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
            achieved_control_freq_mhz: achieved.and_then(|a| a.control_freq_mhz),
            achieved_width_mhz: achieved.and_then(|a| a.width_mhz),
            achieved_center_freq_mhz: achieved.and_then(|a| a.center_freq_mhz),
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
            // Canonicalised through the matcher so the sidecar and the parquet
            // footer carry the identical string; a malformed namespace already
            // failed the scanner setup before a sidecar existed.
            lab_namespace_uuid: cfg
                .ble
                .lab_matcher()
                .ok()
                .flatten()
                .map(|m| m.namespace().to_string()),
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
            run_id,
            run_id_generated,
            experiment: cfg.slug().to_string(),
            tag: cfg.tag.clone(),
            radio,
            inject,
            ble,
            timesync,
            filter: FilterMeta::resolve(cfg),
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

    /// Finalise the sidecar **in memory only** — no I/O.
    ///
    /// Split out of [`close`](Self::close) so a caller that must publish the
    /// finalised document somewhere *before* it may appear on disk can do so
    /// from one value. Segment sealing is that caller: the CSIQ export embeds
    /// this block, and `csid-sync` reads the on-disk `status` as its
    /// ready-to-ship signal, so the two must be written in opposite orders.
    ///
    /// Stamping `ended_at` here rather than at write time is the point. A
    /// segment's export takes seconds to minutes, so finalising and writing at
    /// two different instants would give the embedded block and the sidecar two
    /// different close times for one segment — a difference no reader could
    /// explain and every clock-seam check would flag.
    pub fn finalise(&mut self, status: Status, summary: Option<SummaryMeta>) {
        self.status = status;
        self.ended_at = Some(rfc3339_utc(util::now_unix()));
        self.summary = summary;
    }

    /// Finalise the sidecar with an outcome and (optional) summary, then persist.
    pub fn close(&mut self, status: Status, summary: Option<SummaryMeta>) {
        self.finalise(status, summary);
        self.persist();
    }

    /// Write the sidecar, logging rather than propagating a failure.
    ///
    /// A sidecar that cannot be written must never invalidate captured data —
    /// the raw stream is the source of truth and is already on disk.
    pub fn persist(&self) {
        if let Err(e) = self.write() {
            tracing::error!(error = %e, "writing close-time sidecar failed; raw capture is intact");
        }
    }

    /// Serialise to disk (pretty-printed — it is meant to be read by humans).
    pub fn write(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, json + "\n")
            .with_context(|| format!("writing sidecar {}", self.path.display()))
    }

    /// Re-target this sidecar at another file.
    ///
    /// Used by segment rotation: a segment's sidecar is a clone of the
    /// session's open-time one (so it inherits the radio / inject / timesync /
    /// environment blocks verbatim and is self-describing) pointed at the
    /// segment's own directory. `path` is `#[serde(skip)]`, so it is the one
    /// field a clone does not carry meaningfully.
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// Path of the sidecar on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The band's sidecar spelling. Public because the status document
/// ([`crate::status`]) must say the same word the sidecar says — a capture that
/// reports "2.4" while it runs and "2.4 GHz" once closed is one field with two
/// answers, and every join across the two would have to know that.
pub fn band_label(b: Band) -> &'static str {
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
        build: build_meta(),
    }
}

/// The build identity baked in by `build.rs`.
pub fn build_meta() -> BuildMeta {
    use crate::build_info;
    BuildMeta {
        revision: build_info::REVISION.to_string(),
        revision_source: build_info::REVISION_SOURCE.to_string(),
        built_at: build_info::built_at(),
        rustc: build_info::RUSTC.to_string(),
        profile: build_info::PROFILE.to_string(),
        csiq_format_version: csiq::FORMAT_VERSION,
    }
}

#[cfg(test)]
mod run_id_tests {
    use super::*;

    /// A supplied run id is carried verbatim and NOT marked generated — this is
    /// what makes five nodes one addressable run.
    #[test]
    fn supplied_run_id_is_used() {
        temp_env::with_var(RUN_ID_ENV, Some("exp-lib-01"), || {
            let (id, generated) = resolve_run_id("monad02_x_20260812-184314");
            assert_eq!(id, "exp-lib-01");
            assert!(!generated);
        });
    }

    /// Whitespace-only is treated as absent: a systemd `Environment=CSID_RUN_ID=`
    /// with nothing after it must not silently group every node under "".
    #[test]
    fn blank_run_id_is_treated_as_absent() {
        temp_env::with_var(RUN_ID_ENV, Some("   "), || {
            let (id, generated) = resolve_run_id("monad02_x_20260812-184314");
            assert!(generated);
            assert!(id.starts_with("solo-"));
        });
    }

    /// Unset: the field is still populated, but flagged so analysis code cannot
    /// mistake a solo session for a fleet run.
    #[test]
    fn missing_run_id_is_generated_and_flagged() {
        temp_env::with_var_unset(RUN_ID_ENV, || {
            let (id, generated) = resolve_run_id("monad02_x_20260812-184314");
            assert!(generated);
            assert_eq!(id, "solo-monad02_x_20260812-184314");
        });
    }
}

#[cfg(test)]
mod legacy_sidecar_tests {
    use super::*;

    /// A sidecar written before `run_id` existed must still parse.
    ///
    /// This is not hypothetical: the entire 2026-08-12 fleet run (120 segments)
    /// and every cached capture predate the field. A required field here would
    /// have made csid unable to read its own back catalogue — caught by the
    /// probe test, pinned here on purpose.
    #[test]
    fn sidecar_without_run_id_still_parses() {
        let json = serde_json::json!({
            "schema": "csid-session/1",
            "session_id": "monad02_drift-overnight-illum_20260812-184314",
            "experiment": "drift-overnight-illum",
            "tag": null,
            "radio": {
                "interface": "wlp1s0", "monitor": "wlp1s0mon0", "band": "2.4",
                "channel": 11, "control_freq_mhz": 2462, "center_freq_mhz": null,
                "width": "HT20", "interval_us": 0, "mac_filter": []
            },
            "environment": { "csid_version": "0.1.0" },
            "started_at": "2026-08-12T18:43:14Z",
            "ended_at": null,
            "status": "capturing",
            "summary": null
        });
        let sc: Sidecar = serde_json::from_value(json).expect("legacy sidecar must parse");
        assert_eq!(sc.run_id, "", "legacy sidecars carry no run id");
        assert!(!sc.run_id_generated);

        // The IP-139 groups must not make the back catalogue unreadable.
        assert_eq!(sc.environment.build.revision_source, "");
        assert_eq!(
            sc.filter.fingerprint, "",
            "an unrecorded filter is not the same fact as an unfiltered capture"
        );
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    /// The archive carries exactly ONE distinct `csid_version` — `0.1.0`, the
    /// never-bumped literal — across every capture ever taken, while the daemon
    /// gained injection, time transfer, segmentation, the BLE scanner and the
    /// empty-record counter. A file's provenance could therefore not tell a
    /// July capture from an August one.
    #[test]
    fn a_build_names_itself_or_says_it_cannot() {
        let b = build_meta();
        assert!(
            matches!(b.revision_source.as_str(), "git" | "supplied" | "none"),
            "revision_source is a closed vocabulary, got {:?}",
            b.revision_source
        );
        assert_eq!(
            b.revision.is_empty(),
            b.revision_source == "none",
            "a build with no revision must say so, and one with a revision must name its origin"
        );
        assert_eq!(b.csiq_format_version, csiq::FORMAT_VERSION);
    }

    /// The version literal is what P7 is about. Pin the bump so the next reader
    /// of this file cannot quietly ship another year on `0.1.0`.
    #[test]
    fn the_semantic_version_moved_off_the_never_bumped_literal() {
        assert_ne!(
            env!("CARGO_PKG_VERSION"),
            "0.1.0",
            "0.1.0 is the string the whole archive already carries"
        );
    }

    /// A capture that filtered nothing must SAY it filtered nothing. Leaving
    /// the group empty would make "unfiltered" and "unrecorded" the same value,
    /// and the poolable-group key cannot conflate those.
    #[test]
    fn an_unfiltered_capture_declares_itself_unfiltered() {
        let cfg: ExperimentConfig = toml::from_str(
            r#"
[radio]
interface = "wlp1s0"
channel = 11
width = "HT20"
"#,
        )
        .unwrap();

        let f = FilterMeta::resolve(&cfg);
        assert_eq!(f.fingerprint, NO_FILTER);
        assert!(f.frame_types.is_none());
        assert!(f.rate_n_flags_val.is_none());
        assert!(f.rate_n_flags_mask.is_none());
    }

    /// Two differently-filtered captures must not land in one poolable group.
    /// This project has already paid for the software version of that mistake
    /// by mixing record classes in one tensor.
    #[test]
    fn different_selections_fingerprint_differently() {
        let a = FilterMeta {
            rate_n_flags_val: Some(0x4100),
            rate_n_flags_mask: Some(0x7F00),
            ..Default::default()
        };
        let b = FilterMeta {
            rate_n_flags_val: Some(0x4200),
            rate_n_flags_mask: Some(0x7F00),
            ..Default::default()
        };

        let fa = a.compute_fingerprint();
        let fb = b.compute_fingerprint();
        assert_ne!(fa, fb);
        assert_ne!(fa, NO_FILTER);
        assert_eq!(fa, a.compute_fingerprint(), "the digest must be stable");
    }

    /// Duration is not selection. Two captures that differ only in how long the
    /// radio was allowed to report are the same measurement and must pool.
    #[test]
    fn bounds_do_not_change_the_fingerprint() {
        let a = FilterMeta {
            frame_types: Some(0b1000),
            ..Default::default()
        };
        let mut b = a.clone();
        b.count = Some(50_000);
        b.timeout_us = Some(600_000_000);

        assert_eq!(a.compute_fingerprint(), b.compute_fingerprint());
    }

    /// `empty_records` must be present on every summary. `NULL` is not zero:
    /// it means the counter did not exist when the file was written, and
    /// `useful_yield` is uncomputable rather than perfect.
    #[test]
    fn empty_records_is_always_serialised() {
        let json = serde_json::to_value(SummaryMeta::default()).unwrap();
        assert!(
            json.get("empty_records").is_some(),
            "a summary that omits the field makes a zero-yield capture look flawless"
        );
        assert_eq!(json["empty_records"], 0);
    }
}
