//! `/run/csid/status.json` — what the capture is doing, right now.
//!
//! The live stream carries records. That is the wrong shape for the question an
//! operator asks mid-experiment, because the most important answer is the one
//! where **no records exist**: on 2026-08-17 a ch11 session received 3915 frames
//! and produced 0 CSI records, and a consumer of the live stream alone cannot
//! tell that apart from a quiet room, a mistuned radio or a dead driver.
//!
//! So the session publishes a small document instead:
//!
//! ```text
//! /run/csid/status.json
//! ```
//!
//! Three properties are deliberate.
//!
//! **It is a file, not a push.** A reader that attaches late gets the current
//! state immediately rather than waiting for the next event. `csiscope` polls
//! it once a second, and node_exporter's textfile collector could scrape a
//! derived form of it without csid growing an exporter.
//!
//! **It is written atomically** — temp file plus rename — so a reader can never
//! observe half a document. The same reasoning as the config writer in the
//! console: a partial read of a status file is indistinguishable from a real
//! reading of a broken capture.
//!
//! **It fails soft.** `/run/csid` may not exist outside systemd, the filesystem
//! may be read-only, and neither is a reason to lose a capture. A failure is
//! logged once and never again, and the absence of the file is a missing panel
//! rather than a dead console.
//!
//! ## What it is not
//!
//! It is not the sidecar. The sidecar is the durable, closed-session record and
//! it is authoritative; this is a volatile snapshot that disappears with the
//! process. Where they overlap they agree by construction — both read the same
//! counters — but a disagreement is always resolved in the sidecar's favour.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// Schema tag, so a reader can refuse a document it does not understand rather
/// than silently mis-reading a renamed field.
pub const SCHEMA: &str = "csid-status/1";

/// Tuned and wired, but the RX loop has not been supervised yet.
pub const STATE_STARTING: &str = "starting";
/// The supervise loop is running. Says nothing about whether records flow —
/// that is what `records`, `frames_seen` and `rate_hz` are for.
pub const STATE_CAPTURING: &str = "capturing";
/// Teardown has begun. Sealing a segmented capture takes seconds, and during
/// them a `capturing` snapshot would describe a session that has stopped.
pub const STATE_STOPPING: &str = "stopping";

/// The BLE half, present only when co-capture is enabled.
///
/// Absent — rather than zeroed — on a session without it, so "BLE is off" and
/// "BLE is on and receiving nothing" are different documents. They are very
/// different facts: the second one voids an arm.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BleStatus {
    pub observations: u64,
    pub rate_hz: f64,
}

/// One instant of a running capture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub schema: String,
    pub session_id: String,
    /// Fleet run identifier. Invisible to the operator today, and it is the
    /// thing that makes N nodes one addressable capture.
    pub run_id: String,
    /// True when csid invented the run id. A generated id groups nothing.
    pub run_id_generated: bool,
    pub experiment: String,
    pub host: String,
    /// One of [`STATE_STARTING`], [`STATE_CAPTURING`], [`STATE_STOPPING`].
    ///
    /// A `String` rather than a `&'static str` because the document has to
    /// round-trip: a borrowed field can only be deserialised from data that
    /// outlives the value, which a file read into a temporary buffer does not.
    pub state: String,
    pub started_unix_ns: u64,
    pub uptime_s: u64,

    pub channel: u32,
    pub width: String,
    pub band: String,
    pub control_freq_mhz: u32,
    pub center_freq_mhz: Option<u32>,
    /// Commanded inter-frame interval in microseconds; 0 means unthrottled.
    ///
    /// This is what makes the console's metronome panel exact rather than
    /// inferred: with it, the nominal slot is declared, and the delivery
    /// deficit is measured against the commanded rate instead of against a
    /// rate estimated from the arrivals it is trying to judge.
    pub interval_us: u32,

    /// CSI records the parser produced.
    pub records: u64,
    /// Frames the radio delivered, whether or not they carried a channel
    /// estimate. The denominator of the yield.
    pub frames_seen: u64,
    /// Records per second over the last reporting interval — never since
    /// session start, or a capture that flowed for an hour and then stalled
    /// averages back into looking healthy.
    pub rate_hz: f64,
    pub capture_bytes: u64,
    pub live_sent: u64,
    pub live_dropped: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ble: Option<BleStatus>,
}

impl Snapshot {
    /// `records / frames_seen`, or `None` when the radio has delivered nothing
    /// at all — which is a different fact from a yield of zero.
    pub fn yield_ratio(&self) -> Option<f64> {
        (self.frames_seen > 0).then(|| self.records as f64 / self.frames_seen as f64)
    }
}

/// Publishes [`Snapshot`]s to a path, atomically, best-effort.
pub struct StatusWriter {
    path: PathBuf,
    tmp: PathBuf,
    /// One complaint per process. A status file that cannot be written fails
    /// every second for the length of the session, and a log full of that
    /// buries the capture's own lines.
    warned: AtomicBool,
}

impl StatusWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let tmp = path.with_extension("json.tmp");
        StatusWriter {
            path,
            tmp,
            warned: AtomicBool::new(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write one snapshot. Never fails the caller.
    pub fn publish(&self, snap: &Snapshot) {
        if let Err(e) = self.try_publish(snap) {
            if !self.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "cannot publish the status file; the capture is unaffected and \
                     this will not be reported again"
                );
            }
        }
    }

    fn try_publish(&self, snap: &Snapshot) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(snap)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        {
            let mut f = std::fs::File::create(&self.tmp)?;
            f.write_all(&body)?;
            f.write_all(b"\n")?;
            // The rename below is what makes the swap atomic, but only for a
            // reader — it does not order the bytes against a power loss. This
            // file is volatile state under /run, so durability is not wanted;
            // visibility is.
            f.flush()?;
        }
        std::fs::rename(&self.tmp, &self.path)
    }

    /// Remove the file. A stale status file outlives its capture and would be
    /// read as a live one, so teardown unlinks rather than leaving a last
    /// snapshot behind. Readers still carry an age, because a killed process
    /// never reaches this.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// The file's lifetime is the writer's lifetime.
///
/// `run_session` has several early returns — a required time-transfer receiver
/// that will not start, a required BLE scanner, a failed injector — and each
/// one leaves a session that never captured. Unlinking in `Drop` covers all of
/// them at once, including the ones added later, which an explicit call at each
/// exit does not.
impl Drop for StatusWriter {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot {
            schema: SCHEMA.to_string(),
            session_id: "monad06_x_20260817-103351".into(),
            run_id: "explore-ble-coex-01".into(),
            run_id_generated: false,
            experiment: "x".into(),
            host: "monad06".into(),
            state: STATE_CAPTURING.to_string(),
            started_unix_ns: 1_755_000_000_000_000_000,
            uptime_s: 122,
            channel: 6,
            width: "HT20".into(),
            band: "2.4".into(),
            control_freq_mhz: 2437,
            center_freq_mhz: None,
            interval_us: 10_000,
            records: 7791,
            frames_seen: 8854,
            rate_hz: 63.8,
            capture_bytes: 5_452_800,
            live_sent: 7791,
            live_dropped: 0,
            ble: None,
        }
    }

    #[test]
    fn round_trips() {
        let s = snap();
        let text = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn ble_absent_and_present_are_different_documents() {
        let mut s = snap();
        assert!(!serde_json::to_string(&s).unwrap().contains("\"ble\""));
        s.ble = Some(BleStatus {
            observations: 0,
            rate_hz: 0.0,
        });
        // A scanner that is on and silent must be visible; it voids an arm.
        assert!(serde_json::to_string(&s).unwrap().contains("\"ble\""));
    }

    #[test]
    fn yield_of_zero_differs_from_no_frames() {
        let mut s = snap();
        s.records = 0;
        s.frames_seen = 3915;
        assert_eq!(s.yield_ratio(), Some(0.0));
        s.frames_seen = 0;
        assert_eq!(s.yield_ratio(), None);
    }

    #[test]
    fn publish_replaces_atomically_and_clear_unlinks() {
        let dir = std::env::temp_dir().join(format!("csid-status-{}", std::process::id()));
        let path = dir.join("status.json");
        let w = StatusWriter::new(&path);

        w.publish(&snap());
        let raw = std::fs::read(&path).unwrap();
        let first: Snapshot = serde_json::from_slice(&raw).unwrap();
        assert_eq!(first.records, 7791);

        let mut s = snap();
        s.records = 9000;
        w.publish(&s);
        let raw = std::fs::read(&path).unwrap();
        let second: Snapshot = serde_json::from_slice(&raw).unwrap();
        assert_eq!(second.records, 9000);
        // The temp file must not survive a successful publish.
        assert!(!path.with_extension("json.tmp").exists());

        w.clear();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_path_does_not_panic() {
        // /proc is not a place a file can be created; the writer must swallow it.
        let w = StatusWriter::new("/proc/csid-status-should-not-exist/status.json");
        w.publish(&snap());
        w.publish(&snap()); // and must not warn twice
        w.clear();
    }
}
