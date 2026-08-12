//! Segment rotation — turning one long capture into a stream of shippable
//! session-shaped directories.
//!
//! ## Why a segment is a *session directory* and not a subdirectory
//!
//! `csid-sync` ships any spool directory whose `metadata.json` reads
//! `complete` / `stopped` / `failed` and that carries no `.synced` marker;
//! `csid-prune` reclaims `capture.raw` a grace window after that marker
//! appears. Both walk `"$SPOOL"/*/`. By emitting each segment as a *sibling*
//! directory `<session_id>-segNNNN/` — with its own `capture.raw`,
//! `metadata.json` and `capture.csiq` — a segment becomes eligible for both
//! the moment it is sealed, with **no change to either script**:
//!
//! * uploads happen *during* the run, not after it;
//! * a node that is offline simply queues on disk and catches up on reconnect,
//!   because that is already what an unmarked directory means;
//! * shipped segments are pruned mid-run, which is what makes a high-rate
//!   profile survivable on a card far smaller than the run's total output.
//!
//! Nesting segments under the session root would have required teaching both
//! scripts a second layout, and would have made "is this session shipped?"
//! ambiguous. A flat namespace keeps one answer to that question.
//!
//! ## What stays at the session root
//!
//! The root directory keeps the session-level sidecar and the artefacts that
//! are inherently whole-session: `time_transfer.jsonl` / `.parquet` and the
//! BLE stream. Only the CSI record stream rotates. Consequently a segmented
//! session's root has no `capture.raw`, and its sidecar summary aggregates
//! across the segments.
//!
//! ## Ordering guarantee
//!
//! Segments are numbered from `0001` and sealed in order. Analysis that cares
//! about continuity (FTM unwrapping, drift block medians) must concatenate them
//! in index order; the session-level summary already does exactly that.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use crate::config::ExperimentConfig;
use crate::sidecar::{Sidecar, Status, SummaryMeta};

/// Directory name for segment `index` of `session_id`.
pub fn segment_dir_name(session_id: &str, index: u32) -> String {
    format!("{session_id}-seg{index:04}")
}

/// One sealed segment, handed from the durable thread to the sealer.
pub struct Sealed {
    /// The segment's own directory.
    pub dir: PathBuf,
    /// The sealed `capture.raw` inside it.
    pub raw: PathBuf,
    /// 1-based segment index.
    pub index: u32,
}

/// Sealer thread handle.
pub struct Sealer {
    tx: Option<Sender<Sealed>>,
    handle: Option<JoinHandle<Vec<PathBuf>>>,
}

impl Sealer {
    /// Queue a sealed segment for sidecar + CSIQ export. Never blocks the
    /// caller for longer than a channel send.
    pub fn submit(&self, sealed: Sealed) {
        if let Some(tx) = &self.tx {
            if tx.send(sealed).is_err() {
                tracing::error!("segment sealer went away; segments will not be finalised");
            }
        }
    }

    /// Close the queue and wait for outstanding segments to finish sealing.
    /// Returns every sealed `capture.raw`, in index order.
    pub fn finish(mut self) -> Vec<PathBuf> {
        drop(self.tx.take());
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(paths)) => paths,
            Some(Err(_)) => {
                tracing::error!("segment sealer panicked; sealed segments may be incomplete");
                Vec::new()
            }
            None => Vec::new(),
        }
    }
}

/// Spawn the sealer.
///
/// `template` is the session's open-time sidecar: each segment inherits the
/// radio / inject / timesync / environment blocks from it verbatim, so a
/// segment is self-describing without the reader needing the session root.
///
/// Sealing runs off the durable thread on purpose. A CSIQ export walks the
/// whole segment and at high rates that is seconds of work — doing it inline
/// would stall the writer and push the backlog into the durable channel.
pub fn spawn(template: Sidecar, cfg: ExperimentConfig) -> Result<Sealer> {
    let (tx, rx): (Sender<Sealed>, Receiver<Sealed>) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("csid-sealer".into())
        .spawn(move || {
            let mut sealed_raws: Vec<PathBuf> = Vec::new();
            while let Ok(seg) = rx.recv() {
                match seal_one(&seg, &template, &cfg) {
                    Ok(records) => {
                        tracing::info!(
                            index = seg.index,
                            dir = %seg.dir.display(),
                            records,
                            "segment sealed — eligible for sync"
                        );
                    }
                    Err(e) => {
                        // A failed seal costs the segment's sidecar/export, not
                        // its data: capture.raw is already flushed and fsynced.
                        // Leaving it unsealed keeps csid-sync away from a
                        // half-written directory, which is the safe failure.
                        tracing::error!(
                            index = seg.index,
                            error = %format!("{e:#}"),
                            "sealing a segment failed; its capture.raw is intact but it will NOT sync"
                        );
                    }
                }
                sealed_raws.push(seg.raw);
            }
            sealed_raws
        })
        .context("spawning the segment sealer thread")?;

    Ok(Sealer {
        tx: Some(tx),
        handle: Some(handle),
    })
}

/// Write one segment's sidecar and (optionally) its CSIQ export.
///
/// Order matters: the CSIQ export embeds the sidecar, and `csid-sync` treats
/// the sidecar's `status` as the "ready to ship" signal. So the sidecar is
/// written **twice** — once as `capturing` so a crash mid-export leaves a
/// directory sync will skip, then as `complete` only after the export lands.
fn seal_one(seg: &Sealed, template: &Sidecar, cfg: &ExperimentConfig) -> Result<u64> {
    let mut sc = template.clone();
    sc.session_id = segment_dir_name(&template.session_id, seg.index);
    sc.set_path(seg.dir.join("metadata.json"));
    sc.status = Status::Capturing;
    sc.summary = None;
    sc.write()?;

    let summary = summarize_segment(&seg.raw, cfg);
    let records = summary.records;

    if cfg.export.on_close {
        let out = seg.dir.join("capture.csiq");
        if let Err(e) = crate::export::raw_to_csiq(&seg.raw, &out, cfg, sc.path()) {
            // Non-fatal: the raw is the source of truth and `csid export` can
            // regenerate the container later. Still mark the segment complete
            // so it ships — a segment with raw but no csiq is worth far more
            // off the node than on it.
            tracing::error!(
                error = %format!("{e:#}"),
                "segment CSIQ export failed; shipping the raw anyway"
            );
        }
    }

    sc.close(Status::Complete, Some(summary));
    Ok(records)
}

/// Per-segment summary: record count, tone classes and achieved rate.
///
/// Deliberately *not* the session-level summary — time transfer and BLE are
/// whole-session artefacts living at the root, so pairing/skew is computed
/// once at teardown rather than per segment.
fn summarize_segment(raw: &Path, cfg: &ExperimentConfig) -> SummaryMeta {
    let mut summary = SummaryMeta::default();

    let Ok(file) = std::fs::File::open(raw) else {
        return summary;
    };
    let reader = std::io::BufReader::new(file);
    let mut rr = csiq::raw::RawReader::new(reader, cfg.radio.width.to_csiq());

    let mut tones: Vec<u16> = Vec::new();
    let mut count: u64 = 0;
    let mut first_ftm: Option<u32> = None;
    let mut unwrapper = csiq::FtmUnwrapper::new();
    let mut last_unwrapped: u64 = 0;
    let mut bytes: u64 = 0;

    while let Ok(Some(rec)) = rr.next_record() {
        count += 1;
        if !tones.contains(&rec.ntone) {
            tones.push(rec.ntone);
        }
        let u = unwrapper.push(rec.ftm);
        if first_ftm.is_none() {
            first_ftm = Some(rec.ftm);
        }
        last_unwrapped = u;
    }

    if let Ok(md) = std::fs::metadata(raw) {
        bytes = md.len();
    }

    tones.sort_unstable();
    summary.records = count;
    summary.tone_counts = tones;
    summary.capture_bytes = bytes;

    if let Some(first) = first_ftm {
        let span_ticks = last_unwrapped.saturating_sub(first as u64);
        let span_s = csiq::ftm_to_seconds(span_ticks);
        if span_s > 0.0 {
            summary.mean_rate_hz = count as f64 / span_s;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Segment directories must sort lexicographically into index order —
    /// `csid-sync` walks the spool with a glob and analysis concatenates the
    /// results, so `seg10` sorting before `seg2` would silently reorder a
    /// capture. Zero-padding is the contract, not cosmetics.
    #[test]
    fn segment_names_sort_into_index_order() {
        let mut names: Vec<String> = [11u32, 2, 1, 10, 3]
            .iter()
            .map(|i| segment_dir_name("monad03_drift_20260811-145705", *i))
            .collect();
        names.sort();

        let indices: Vec<u32> = names
            .iter()
            .map(|n| n.rsplit("-seg").next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(indices, vec![1, 2, 3, 10, 11]);
    }

    /// A segment lives beside its session, not inside it — that flat namespace
    /// is what lets the existing `"$SPOOL"/*/` sync and prune globs pick it up
    /// with no change.
    #[test]
    fn a_segment_is_a_sibling_of_the_session_not_a_child() {
        let name = segment_dir_name("monad03_drift_20260811-145705", 7);
        assert_eq!(name, "monad03_drift_20260811-145705-seg0007");
        assert!(
            !name.contains('/'),
            "a segment name must be a single path component"
        );
    }
}
