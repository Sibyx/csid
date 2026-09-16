//! Small, bounded qualification read over a sealed channel-hopping capture.
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Quality {
    pub session_dir: String,
    pub nonempty_records: u64,
    pub usable_window_fraction: f64,
    pub max_gap_s: f64,
    pub clock_errors: u64,
    pub beacons: BTreeSet<String>,
    pub passed: bool,
    pub csi_source_attribution: &'static str,
}

/// Six-second windows, including empty tail windows. Thresholds are an
/// exploratory availability gate, not a validated occupancy/Doppler floor.
fn window_quality(times: &[u64], seconds: u64) -> (f64, f64, u64) {
    let windows = (seconds / 6) as usize;
    if times.is_empty() {
        return (0.0, seconds as f64, 0);
    }
    let first = times[0];
    let mut bins = vec![0u64; windows];
    let mut previous = first;
    let mut errors = 0;
    let mut max_gap = 0u64;
    for &ts in times {
        if ts == 0 || ts < previous || ts < first {
            errors += 1;
            continue;
        }
        max_gap = max_gap.max(ts - previous);
        previous = ts;
        let i = ((ts - first) / 6_000_000_000) as usize;
        if i < bins.len() {
            bins[i] += 1;
        }
    }
    max_gap = max_gap.max((seconds * 1_000_000_000).saturating_sub(previous - first));
    (
        bins.iter().filter(|&&n| n >= 150).count() as f64 / windows as f64,
        max_gap as f64 / 1e9,
        errors,
    )
}

/// Is `name` a segment directory (`<session_id>-segNNNN`) rather than a
/// session root? Mirrors [`crate::segment::segment_dir_name`], which is the
/// one place the suffix is minted.
fn is_segment_dir(name: &str) -> bool {
    match name.rsplit_once("-seg") {
        Some((_, digits)) => digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Find the one session root in `spool` whose sidecar carries this exact
/// `run_id` and `experiment`.
///
/// THREE THINGS THIS TOLERATES, each of which has cost an arm.
///
/// * **A sidecar that does not parse is skipped, not fatal.** The first
///   version did `serde_json::from_reader(..)?` on every `metadata.json` in the
///   spool, so one bad file anywhere failed every locate on that node. monad05
///   carried a zero-byte `metadata.json` from a session opened on a full disk on
///   2026-08-18 (`fs::write` created the file and then hit ENOSPC), and from
///   2026-09-15 20:12 UTC every matrix arm whose comparison transmitter was
///   monad05 failed at its `--locate-only` step — three entries in fourteen
///   hours — while both cohorts were capturing. The junk is logged by path so
///   it can be removed; it no longer decides anything.
/// * **Only directories named for this profile are opened.** A session id is
///   `<host>_<experiment>_<stamp>`, so `_<profile>_` in the directory name is a
///   necessary condition, and a spool holds 1,200 to 1,900 directories (the
///   scan took 3.7 s on monad05). Parsing a dozen files instead of two
///   thousand is the difference between a locate and a spool audit.
/// * **Segment directories are not candidates.** A sealed segment carries the
///   root's `run_id` and `experiment` verbatim, so once the first segment seals
///   (30 min into a matrix arm) the old scan would have found two matches and
///   refused. The root is what the caller wants: its directory is where the
///   protocol evidence lives.
fn locate(spool: &Path, run_id: &str, profile: &str) -> Result<(std::path::PathBuf, serde_json::Value)> {
    let needle = format!("_{profile}_");
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(spool).with_context(|| format!("reading spool {}", spool.display()))? {
        let dir = entry?.path();
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.contains(&needle) || is_segment_dir(name) {
            continue;
        }
        let meta = dir.join("metadata.json");
        if !meta.is_file() {
            continue;
        }
        let doc: serde_json::Value = match std::fs::File::open(&meta)
            .map_err(anyhow::Error::from)
            .and_then(|f| serde_json::from_reader(f).map_err(anyhow::Error::from))
        {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(
                    path = %meta.display(),
                    error = %e,
                    "sidecar unreadable; skipped while locating a session (remove or repair it)"
                );
                continue;
            }
        };
        if doc["run_id"] == run_id && doc["experiment"] == profile {
            matches.push((dir, doc));
        }
    }
    anyhow::ensure!(
        matches.len() == 1,
        "expected one exact run/profile session for run_id={run_id} profile={profile}, found {}",
        matches.len()
    );
    matches.pop().context("session not found")
}

pub fn report(
    spool: &Path,
    run_id: &str,
    profile: &str,
    seconds: u64,
    locate_only: bool,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        (6..=900).contains(&seconds) && seconds % 6 == 0,
        "quality duration must be 6..900 seconds in complete six-second windows"
    );
    let (dir, meta) = locate(spool, run_id, profile)?;
    if locate_only {
        return Ok(serde_json::json!({"session_dir": dir}));
    }
    anyhow::ensure!(
        meta["status"] == "complete",
        "qualification session did not complete"
    );
    let mut reader = csiq::raw::RawReader::new(
        BufReader::new(std::fs::File::open(dir.join("capture.raw"))?),
        csiq::Width::Ht20,
    );
    let mut times = Vec::new();
    while let Some(rec) = reader.next_record()? {
        if !rec.iq.is_empty() && rec.iq.iter().any(|&v| v != 0) {
            times.push(rec.unix_ts_ns);
        }
        anyhow::ensure!(
            times.len() <= 2_000_000,
            "qualification capture exceeds record budget"
        );
    }
    let (fraction, gap, errors) = window_quality(&times, seconds);
    let mut beacons = BTreeSet::new();
    for line in BufReader::new(std::fs::File::open(dir.join("frame_census.jsonl"))?).lines() {
        let row: serde_json::Value = serde_json::from_str(&line?)?;
        if row["kind"] == "mgmt" && row["subtype"] == 8 {
            if let Some(bssid) = row["ta"].as_str() {
                beacons.insert(bssid.to_lowercase());
            }
        }
    }
    Ok(serde_json::to_value(Quality {
        session_dir: dir.display().to_string(),
        nonempty_records: times.len() as u64,
        usable_window_fraction: fraction,
        max_gap_s: gap,
        clock_errors: errors,
        beacons,
        passed: fraction >= 0.9 && gap <= 1.0 && errors == 0,
        csi_source_attribution: "mixed; beacon presence does not attribute CSI to eduroam",
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_short_burst_cannot_pass_as_a_sustained_source() {
        let burst: Vec<_> = (0..3000).map(|i| 1_000_000_000 + i * 100_000).collect();
        let (fraction, gap, _) = window_quality(&burst, 60);
        assert!(fraction < 0.9 && gap > 1.0);
    }
    #[test]
    fn sustained_nonempty_stream_passes_but_clock_reversal_is_visible() {
        let mut times: Vec<_> = (0..6000).map(|i| 1_000_000_000 + i * 10_000_000).collect();
        let (fraction, gap, errors) = window_quality(&times, 60);
        assert_eq!(fraction, 1.0);
        assert!(gap < 0.02);
        assert_eq!(errors, 0);
        times[40] = times[10];
        assert!(window_quality(&times, 60).2 > 0);
    }
    #[test]
    fn no_data_is_not_a_good_channel() {
        assert_eq!(window_quality(&[], 60), (0.0, 60.0, 0));
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csid-session-quality-{tag}-{}-{}",
            std::process::id(),
            crate::util::now_unix_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session(spool: &Path, name: &str, body: &str) {
        let dir = spool.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("metadata.json"), body).unwrap();
    }

    #[test]
    fn segment_suffix_is_recognised_exactly() {
        assert!(is_segment_dir("monad05_explore-lib-matrix-wide-3h-tx_20260916-052142-seg0003"));
        assert!(!is_segment_dir("monad05_explore-lib-matrix-wide-3h-tx_20260916-052142"));
        assert!(!is_segment_dir("monad05_seg-profile_20260916-052142"));
        assert!(!is_segment_dir("monad05_x_20260916-seg12"));
    }

    /// The 2026-09-15/16 failure: a zero-byte sidecar from a full-disk session a
    /// month earlier sat beside the live session and every locate on that node
    /// died with `EOF while parsing a value at line 1 column 0`.
    #[test]
    fn an_unreadable_sidecar_elsewhere_in_the_spool_does_not_fail_the_locate() {
        let spool = scratch("junk");
        session(&spool, "monad05_console_20260818-124713", "");
        session(&spool, "monad05_other_20260901-000000", "{ not json");
        session(
            &spool,
            "monad05_explore-lib-matrix-wide-3h-tx_20260916-052142",
            r#"{"run_id":"matrix-wide-3h-day-2026-09-16-02","experiment":"explore-lib-matrix-wide-3h-tx","status":"capturing"}"#,
        );
        let out = report(
            &spool,
            "matrix-wide-3h-day-2026-09-16-02",
            "explore-lib-matrix-wide-3h-tx",
            60,
            true,
        )
        .unwrap();
        assert!(out["session_dir"]
            .as_str()
            .unwrap()
            .ends_with("monad05_explore-lib-matrix-wide-3h-tx_20260916-052142"));
        std::fs::remove_dir_all(spool).unwrap();
    }

    /// A sealed segment repeats the root's run id and experiment. It must not
    /// count as a second match once the first rotation has happened.
    #[test]
    fn a_sealed_segment_is_not_a_second_session() {
        let spool = scratch("segment");
        let body = r#"{"run_id":"r1","experiment":"explore-lib-matrix-wide-3h","status":"capturing"}"#;
        session(&spool, "monad03_explore-lib-matrix-wide-3h_20260916-051548", body);
        session(&spool, "monad03_explore-lib-matrix-wide-3h_20260916-051548-seg0000", body);
        session(&spool, "monad03_explore-lib-matrix-wide-3h_20260916-051548-seg0001", body);
        // Same run id, a different profile on the same node: not a match either.
        session(
            &spool,
            "monad03_explore-lib-matrix-wide-3h-tx_20260916-051548",
            r#"{"run_id":"r1","experiment":"explore-lib-matrix-wide-3h-tx"}"#,
        );
        let out = report(&spool, "r1", "explore-lib-matrix-wide-3h", 60, true).unwrap();
        assert!(out["session_dir"]
            .as_str()
            .unwrap()
            .ends_with("monad03_explore-lib-matrix-wide-3h_20260916-051548"));
        std::fs::remove_dir_all(spool).unwrap();
    }

    #[test]
    fn no_session_and_two_sessions_are_both_refused() {
        let spool = scratch("count");
        assert!(report(&spool, "r1", "p", 60, true).is_err());
        let body = r#"{"run_id":"r1","experiment":"p"}"#;
        session(&spool, "monad01_p_20260916-000000", body);
        session(&spool, "monad01_p_20260916-000001", body);
        let err = report(&spool, "r1", "p", 60, true).unwrap_err().to_string();
        assert!(err.contains("found 2"), "{err}");
        std::fs::remove_dir_all(spool).unwrap();
    }
}
