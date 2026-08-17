//! Reading `csid`'s status document, and turning it into a verdict.
//!
//! ## Why the console needs a second source at all
//!
//! csiscope is a consumer of the live stream, and the live stream carries
//! records. The question that matters most during an experiment is what
//! happens when there are **none**.
//!
//! Measured on 2026-08-17: `smoke-bench-ch11` received 3915 frames and produced
//! 0 CSI records. On the live stream alone that is indistinguishable from a
//! quiet room, a mistuned radio, a dead driver or a stopped unit. It is none of
//! those — on 2.4 GHz the usual cause is DSSS/CCK traffic, which has no OFDM
//! preamble and therefore no channel estimate to report. The channel was busy.
//! The capture saw all of it. None of it could become CSI.
//!
//! `csid` publishes `records` and `frames_seen` to `/run/csid/status.json` once
//! a second, and this module reads it. The ratio is the **capture yield**, and
//! it is the first number on the page.
//!
//! ## Staleness is not an error
//!
//! A killed `csid` never unlinks its file, so a document on disk is not proof
//! of a running capture. Every read carries the file's age and the console
//! degrades to "unknown" past [`STALE_AFTER`] rather than showing a number that
//! stopped being true.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use csid::status::Snapshot;

/// How often the file is re-read. It is written once a second; polling at the
/// same period would alias, so this is deliberately faster.
pub const POLL_EVERY: Duration = Duration::from_millis(500);

/// Past this age the document describes a capture that may no longer exist.
/// Five seconds is five missed writes — long enough that a loaded Pi cannot
/// trip it, short enough that an operator does not act on a dead session.
pub const STALE_AFTER: Duration = Duration::from_secs(5);

/// How to read a capture's yield.
///
/// The bands are the measured fleet distribution, not round numbers: over the
/// 2026-08-17 channel survey, 5 GHz yielded a median 99.4% across 57 sessions
/// while 2.4 GHz yielded 3.5% across 18. A yield that would be alarming on
/// 5 GHz is the ordinary condition on 2.4 GHz, so the verdict is banded per
/// band or it cries wolf all day on the band the thesis actually needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Yield {
    /// As good as the band gets.
    Ok,
    /// Below what the band usually gives, but not evidence of a fault.
    Low,
    /// The radio is delivering frames and almost none become CSI.
    Bad,
    /// No frames at all: nothing to take a ratio of. Distinct from a yield of
    /// zero, which means the frames arrived and were unusable.
    NoFrames,
}

impl Yield {
    pub fn label(self) -> &'static str {
        match self {
            Yield::Ok => "ok",
            Yield::Low => "low",
            Yield::Bad => "bad",
            Yield::NoFrames => "no frames",
        }
    }
}

/// Classify a yield for a band. `band` is the sidecar spelling: `"2.4"`, `"5"`,
/// `"6"`.
pub fn classify(ratio: Option<f64>, band: &str) -> Yield {
    let Some(r) = ratio else {
        return Yield::NoFrames;
    };
    let is_24 = band.starts_with("2.4");
    if is_24 {
        // The measured median is 3.5%. Anything above 20% is a good day; below
        // that is normal and only a literal zero is worth a hard flag — and
        // even then the cause is usually CCK rather than a fault.
        match r {
            r if r >= 0.20 => Yield::Ok,
            r if r > 0.0 => Yield::Low,
            _ => Yield::Bad,
        }
    } else {
        // 5 and 6 GHz have no DSSS/CCK at all, so anything below ~80% is a real
        // signal that something is wrong with the capture path.
        match r {
            r if r >= 0.80 => Yield::Ok,
            r if r >= 0.20 => Yield::Low,
            _ => Yield::Bad,
        }
    }
}

/// The sentence a low yield deserves, or `None` when there is nothing to
/// explain.
///
/// It exists because "0 records" is the single most misread state on this
/// fleet, and a number without this sentence has been misread before.
pub fn note(y: Yield, band: &str) -> Option<&'static str> {
    match (y, band.starts_with("2.4")) {
        (Yield::Ok, _) => None,
        (Yield::NoFrames, _) => Some(
            "The radio has delivered no frames at all. That is a tuning, driver \
             or monitor-interface question, not a channel one.",
        ),
        (_, true) => Some(
            "Expected on 2.4 GHz: the measured fleet median is 3.5%. A DSSS/CCK \
             frame has no OFDM preamble, so it produces no channel estimate. \
             This is traffic you cannot see, NOT an empty room.",
        ),
        (_, false) => Some(
            "Unexpected above 2.4 GHz, where there is no CCK to lose. The \
             measured fleet median at 5 GHz is 99.4%. Check the driver ABI and \
             the monitor interface.",
        ),
    }
}

/// One read of the status file.
#[derive(Debug, Clone)]
pub struct Reading {
    pub snap: Snapshot,
    /// Wall time since the file was last successfully read into this value.
    pub age: Duration,
}

impl Reading {
    pub fn stale(&self) -> bool {
        self.age > STALE_AFTER
    }
}

/// Polls the status file on its own thread and holds the newest reading.
///
/// A thread rather than a read-on-demand because the read happens on the
/// analysis path, which runs at the client's frame rate — up to 40 times a
/// second, against a file that changes once.
pub struct CaptureStatus {
    path: PathBuf,
    current: RwLock<Option<(Snapshot, Instant)>>,
    stop: AtomicBool,
}

impl CaptureStatus {
    pub fn new(path: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(CaptureStatus {
            path: path.into(),
            current: RwLock::new(None),
            stop: AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start the poller. Returns immediately; a missing file is the normal
    /// state between captures and is not reported.
    pub fn spawn(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("csiscope-status".into())
            .spawn(move || {
                while !me.stop.load(Ordering::Relaxed) {
                    me.poll_once();
                    std::thread::sleep(POLL_EVERY);
                }
            })
            .ok();
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn poll_once(&self) {
        let next = read_snapshot(&self.path);
        let mut cur = match self.current.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match next {
            // A successful read always replaces, even by an identical document:
            // the timestamp is the point. csid rewrites the file every second,
            // so an unchanged document that keeps arriving is a *live* capture
            // that is simply not moving, which is exactly the state an operator
            // must be able to tell from a dead one.
            Some(s) => *cur = Some((s, Instant::now())),
            // A vanished file means the session ended and unlinked it. Drop the
            // reading rather than letting it age out: "no capture" is a
            // different sentence from "the last capture is 40 s old".
            None if !self.path.exists() => *cur = None,
            // Present but unreadable: a torn read, or a schema this build does
            // not understand. Keep the previous value and let it age.
            None => {}
        }
    }

    /// The newest reading, with its age.
    pub fn get(&self) -> Option<Reading> {
        let cur = match self.current.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        cur.as_ref().map(|(s, at)| Reading {
            snap: s.clone(),
            age: at.elapsed(),
        })
    }
}

/// Read and parse one status document. `None` on any failure — absent, torn,
/// or a schema this build does not know.
fn read_snapshot(path: &Path) -> Option<Snapshot> {
    let bytes = std::fs::read(path).ok()?;
    let snap: Snapshot = serde_json::from_slice(&bytes).ok()?;
    // A future schema may have moved a field this build reads by name, and a
    // silently mis-read counter is worse than a missing panel.
    (snap.schema == csid::status::SCHEMA).then_some(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bands_are_judged_differently() {
        // The measured 2.4 GHz median would be a hard fault at 5 GHz.
        assert_eq!(classify(Some(0.035), "2.4"), Yield::Low);
        assert_eq!(classify(Some(0.035), "5"), Yield::Bad);
        // And the measured 5 GHz median is fine everywhere.
        assert_eq!(classify(Some(0.994), "5"), Yield::Ok);
        assert_eq!(classify(Some(0.994), "2.4"), Yield::Ok);
        // The measured 2.4 GHz coex arm, which was healthy.
        assert_eq!(classify(Some(0.880), "2.4"), Yield::Ok);
    }

    /// The ch11 smoke bench: 3915 frames, 0 records. Not the same state as a
    /// radio that received nothing.
    #[test]
    fn zero_records_from_real_frames_is_not_no_frames() {
        assert_eq!(classify(Some(0.0), "2.4"), Yield::Bad);
        assert_eq!(classify(None, "2.4"), Yield::NoFrames);
        assert_ne!(note(Yield::Bad, "2.4"), note(Yield::NoFrames, "2.4"));
    }

    #[test]
    fn a_healthy_capture_gets_no_lecture() {
        assert!(note(Yield::Ok, "2.4").is_none());
        assert!(note(Yield::Ok, "5").is_none());
        assert!(note(Yield::Low, "2.4").unwrap().contains("CCK"));
    }

    #[test]
    fn a_missing_file_reads_as_no_capture() {
        let s = CaptureStatus::new("/nonexistent/csid/status.json");
        s.poll_once();
        assert!(s.get().is_none());
    }

    #[test]
    fn a_foreign_schema_is_refused_rather_than_misread() {
        let dir = std::env::temp_dir().join(format!("csiscope-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.json");
        std::fs::write(&path, br#"{"schema":"csid-status/99","records":1}"#).unwrap();
        assert!(read_snapshot(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
