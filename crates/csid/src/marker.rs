//! Fleet-side block markers — the **precision boundary reference** for a lab
//! session.
//!
//! ## Why this exists, and why it outranks the app marker
//!
//! The phone application marks blocks too, and its markers are richer. But
//! every app stream is stamped on `mono_ns` and reaches the CSI timeline only
//! through the affine fit registered in the pre-registration §3.5:
//!
//! ```text
//! unix_ts_ns_est = a · mono_ns + b
//! ```
//!
//! That join has a gate (G4). A fold whose residual exceeds 6.0 s loses its
//! app-derived ground truth entirely (K-G4); a fold whose residual exceeds
//! 0.25 s loses T3. **A block boundary carried only by an app marker is a block
//! boundary that can be gated away.**
//!
//! A marker written here is stamped by [`crate::util::now_unix_ns`] — the same
//! `CLOCK_REALTIME` call site the CSI receive path and the BLE scanner use, in
//! the same process, on the node that holds the capture. There is **no join and
//! no transform**: the marker is already on the timebase the analysis window
//! grid is defined on.
//!
//! So the contract is stated in one direction and printed on every marker:
//!
//! > **The fleet marker is the boundary timestamp of record. The app marker is
//! > the cross-check.**
//!
//! This matters more than "redundancy". The pre-registration's cycling ramps
//! are **30 s** (§2: plateau 1.5 min, ramp 0.5 min) and the cycling event set is
//! T3-CHANGE's *primary* event set (§6.1). A 6 s boundary error is 20% of a
//! ramp; the peak-to-plateau contrast is defined on a ±0.5 s band. Boundaries
//! therefore need G4b precision — 250 ms — and the only stream that has it
//! without a fit is this one.
//!
//! ## Reconciliation with the app
//!
//! The field names are the app's, verbatim: `block_id, zone_id, level,
//! sub_condition, block_kind, phase`. A join on `block_id` + `phase` pairs the
//! two sources directly, and the difference
//!
//! ```text
//! app_marker.unix_ts_ns_est − fleet_marker.unix_ts_ns
//! ```
//!
//! is an *independent* estimate of the same residual G4 measures at sync
//! markers — computed at block boundaries, which is where it is spent.
//!
//! ## What this deliberately is not
//!
//! Not a ground-truth channel. A marker records what the *operator commanded*
//! at an instant, not what the room contained; occupancy level is still the
//! cumulative sum of QR `direction` events per the pre-registration §3.5. The
//! `level` field is the commanded staircase level, and is labelled as such.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Append-only marker log inside the session directory.
pub const MARKER_FILE: &str = "markers.jsonl";
/// Schema identifier. Bump on any field change — the analysis side joins on
/// these names.
pub const MARKER_SCHEMA: &str = "csid-marker/1";

/// Where a marker came from. The analysis prefers `fleet` for boundaries.
pub const SOURCE_FLEET: &str = "fleet";

/// One block marker.
///
/// Field order is the serialisation order, and the app-facing names
/// (`block_id`, `zone_id`, `level`, `sub_condition`, `block_kind`, `phase`) are
/// spelled exactly as the app spells them so the two logs join without a
/// mapping table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub schema: String,
    /// Host wallclock, nanoseconds. **The join key, and the boundary timestamp
    /// of record** — no transform has been applied and none is needed.
    pub unix_ts_ns: u64,
    pub host: String,
    /// The csid session this marker landed in, so a marker is never orphaned
    /// from the capture it annotates.
    pub session_id: String,
    /// `fleet` here. The app writes its own source label.
    pub source: String,

    // -- the app-shared block identity ------------------------------------
    pub block_id: String,
    pub zone_id: String,
    /// Commanded staircase level (0,1,2,3,4,6,8,10). `None` for a block whose
    /// level is not a number (a sync burst, a furniture move).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    /// `seated` / `walking` — the pre-registered motion sub-condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_condition: Option<String>,
    pub block_kind: String,
    pub phase: String,

    /// Free-form operator note. Never parsed by analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The block kinds the August session actually stages. Free-form is accepted
/// (an unforeseen block must not be unrecordable), but these are completed and
/// spell-checked so three nodes never disagree about what "cycling" is called.
pub const KNOWN_KINDS: &[&str] = &[
    "staircase",
    "cycling",
    "empty",
    "sync",
    "calibration",
    "drift",
    "probe",
];

/// Marker phases. `start`/`end` bracket a block; `mark` is an instant.
pub const KNOWN_PHASES: &[&str] = &["start", "end", "mark"];

impl Marker {
    /// Build a marker, stamping it now.
    #[allow(clippy::too_many_arguments)]
    pub fn now(
        host: impl Into<String>,
        session_id: impl Into<String>,
        block_id: impl Into<String>,
        zone_id: impl Into<String>,
        block_kind: impl Into<String>,
        phase: impl Into<String>,
        level: Option<u32>,
        sub_condition: Option<String>,
        note: Option<String>,
    ) -> Self {
        Marker::at(
            crate::util::now_unix_ns(),
            host,
            session_id,
            block_id,
            zone_id,
            block_kind,
            phase,
            level,
            sub_condition,
            note,
        )
    }

    /// Build a marker at an explicit stamp — used by tests and by any future
    /// backfill. Production always goes through [`Marker::now`].
    #[allow(clippy::too_many_arguments)]
    pub fn at(
        unix_ts_ns: u64,
        host: impl Into<String>,
        session_id: impl Into<String>,
        block_id: impl Into<String>,
        zone_id: impl Into<String>,
        block_kind: impl Into<String>,
        phase: impl Into<String>,
        level: Option<u32>,
        sub_condition: Option<String>,
        note: Option<String>,
    ) -> Self {
        Marker {
            schema: MARKER_SCHEMA.to_string(),
            unix_ts_ns,
            host: host.into(),
            session_id: session_id.into(),
            source: SOURCE_FLEET.to_string(),
            block_id: block_id.into(),
            zone_id: zone_id.into(),
            level,
            sub_condition,
            block_kind: block_kind.into(),
            phase: phase.into(),
            note,
        }
    }

    /// Reject what cannot be reconciled later.
    ///
    /// An empty `block_id` is the one failure that is silent *and* fatal: the
    /// marker lands in the log, the app's marker lands in its log, and nothing
    /// pairs them. Everything else here is a typo guard.
    pub fn validate(&self) -> Result<()> {
        if self.block_id.trim().is_empty() {
            anyhow::bail!("--block-id is required and must not be blank: it is the join key to the app's marker");
        }
        if self.zone_id.trim().is_empty() {
            anyhow::bail!("--zone is required and must not be blank");
        }
        if self.block_kind.trim().is_empty() {
            anyhow::bail!("--kind is required and must not be blank");
        }
        if !KNOWN_PHASES.contains(&self.phase.as_str()) {
            anyhow::bail!(
                "--phase must be one of {:?} (got {:?})",
                KNOWN_PHASES,
                self.phase
            );
        }
        if self.unix_ts_ns == 0 {
            anyhow::bail!("the host clock returned no time; refusing to write an unstamped marker");
        }
        Ok(())
    }

    /// One NDJSON line, no trailing newline.
    pub fn to_line(&self) -> Result<String> {
        serde_json::to_string(self).context("serialising the marker")
    }

    /// The operator-facing confirmation. It states the precision contract every
    /// time, because the one place that claim needs to be visible is the bench.
    pub fn render(&self) -> String {
        let level = self
            .level
            .map(|l| l.to_string())
            .unwrap_or_else(|| "-".into());
        let sub = self.sub_condition.as_deref().unwrap_or("-");
        format!(
            "marked {block} {phase} @ {ts} ns (native unix_ts_ns — no clock join)\n  \
             host={host} session={session} zone={zone} level={level} sub={sub} kind={kind}\n  \
             this is the boundary timestamp of record; the app marker for {block} is the cross-check",
            block = self.block_id,
            phase = self.phase,
            ts = self.unix_ts_ns,
            host = self.host,
            session = self.session_id,
            zone = self.zone_id,
            kind = self.block_kind,
        )
    }
}

/// Append a marker to `<session>/markers.jsonl`, creating the log if needed.
///
/// Flushed and fsynced on every write. A marker costs one syscall pair a few
/// times a block, and losing the boundary of a 90-second cycling plateau to a
/// buffered write is not a trade worth making.
pub fn append(session_dir: &Path, marker: &Marker) -> Result<PathBuf> {
    marker.validate()?;
    let path = session_dir.join(MARKER_FILE);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let line = marker.to_line()?;
    f.write_all(line.as_bytes())
        .and_then(|()| f.write_all(b"\n"))
        .and_then(|()| f.flush())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Marker {
        Marker::at(
            1_786_000_000_123_456_789,
            "monad04",
            "monad04_lab-anchor_20260810-101500",
            "S1-ZA-CYC-03",
            "ZONE-A",
            "cycling",
            "start",
            Some(4),
            Some("walking".into()),
            None,
        )
    }

    /// The field names are a contract with the app side. If this test is edited
    /// to match a rename, the app and the analysis both have to be edited too.
    #[test]
    fn the_wire_form_carries_the_apps_field_names_verbatim() {
        let line = sample().to_line().unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        for field in [
            "block_id",
            "zone_id",
            "level",
            "sub_condition",
            "block_kind",
            "phase",
        ] {
            assert!(v.get(field).is_some(), "missing app-shared field {field}");
        }
        assert_eq!(v["block_id"], "S1-ZA-CYC-03");
        assert_eq!(v["zone_id"], "ZONE-A");
        assert_eq!(v["level"], 4);
        assert_eq!(v["sub_condition"], "walking");
        assert_eq!(v["block_kind"], "cycling");
        assert_eq!(v["phase"], "start");
        assert_eq!(v["source"], "fleet");
        assert_eq!(v["schema"], MARKER_SCHEMA);
    }

    /// The stamp must serialise as a JSON *number* in nanoseconds — not a
    /// string, not milliseconds. A silent unit change here would move every
    /// boundary by six orders of magnitude.
    #[test]
    fn the_stamp_is_a_nanosecond_number_not_a_string_or_milliseconds() {
        let line = sample().to_line().unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let ts = v["unix_ts_ns"].as_u64().expect("must be a JSON number");
        assert_eq!(ts, 1_786_000_000_123_456_789);
        // ~1.79e18 ns is 2026 in nanoseconds; in ms it would be year 58,000,000.
        assert!(ts > 1_700_000_000_000_000_000 && ts < 2_000_000_000_000_000_000);
        assert!(!line.contains("\"unix_ts_ns\":\""), "must not be quoted");
    }

    #[test]
    fn it_round_trips() {
        let m = sample();
        let back: Marker = serde_json::from_str(&m.to_line().unwrap()).unwrap();
        assert_eq!(m, back);
    }

    /// Optional fields are omitted rather than written as null, so a sync
    /// marker's line does not claim `"level": null` is a level.
    #[test]
    fn absent_optionals_are_omitted_not_nulled() {
        let m = Marker::at(
            1,
            "monad01",
            "s",
            "SYNC-01",
            "ZONE-B",
            "sync",
            "mark",
            None,
            None,
            None,
        );
        let line = m.to_line().unwrap();
        assert!(!line.contains("null"), "{line}");
        assert!(!line.contains("level"), "{line}");
        assert!(!line.contains("sub_condition"), "{line}");
        assert!(!line.contains("note"), "{line}");
        // ...and it still round-trips with the optionals back as None.
        let back: Marker = serde_json::from_str(&line).unwrap();
        assert_eq!(back.level, None);
        assert_eq!(back.sub_condition, None);
    }

    #[test]
    fn a_marker_without_a_join_key_is_refused() {
        let mut m = sample();
        m.block_id = "  ".into();
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("join key"), "{err}");

        let mut m = sample();
        m.zone_id = String::new();
        assert!(m.validate().is_err());

        let mut m = sample();
        m.block_kind = String::new();
        assert!(m.validate().is_err());

        let mut m = sample();
        m.phase = "begin".into();
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("--phase"), "{err}");

        let mut m = sample();
        m.unix_ts_ns = 0;
        assert!(m.validate().is_err(), "an unstamped marker is worthless");
    }

    /// The precision claim is printed, not merely documented — the operator
    /// reading the confirmation is the person who needs to know which stream is
    /// the reference.
    #[test]
    fn the_confirmation_states_which_stream_is_the_reference() {
        let text = sample().render();
        assert!(text.contains("no clock join"), "{text}");
        assert!(text.contains("boundary timestamp of record"), "{text}");
        assert!(text.contains("cross-check"), "{text}");
        assert!(text.contains("1786000000123456789"), "{text}");
    }

    #[test]
    fn appending_creates_the_log_and_keeps_one_line_per_marker() {
        let dir = std::env::temp_dir().join(format!("csid-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let a = sample();
        let mut b = sample();
        b.phase = "end".into();
        b.unix_ts_ns += 90_000_000_000;

        let path = append(&dir, &a).unwrap();
        append(&dir, &b).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let back: Marker = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(back.phase, "end");
        // 90 s apart — a cycling plateau.
        assert_eq!(back.unix_ts_ns - a.unix_ts_ns, 90_000_000_000);

        std::fs::remove_dir_all(&dir).ok();
    }
}
