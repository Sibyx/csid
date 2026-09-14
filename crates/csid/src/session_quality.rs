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
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(spool)? {
        let dir = entry?.path();
        let meta = dir.join("metadata.json");
        if !meta.is_file() {
            continue;
        }
        let doc: serde_json::Value = serde_json::from_reader(std::fs::File::open(meta)?)?;
        if doc["run_id"] == run_id && doc["experiment"] == profile {
            matches.push((dir, doc));
        }
    }
    anyhow::ensure!(
        matches.len() == 1,
        "expected one exact run/profile session, found {}",
        matches.len()
    );
    let (dir, meta) = matches.pop().context("session not found")?;
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
}
