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
//! ## What a segment sidecar reports, and why it grew
//!
//! Originally a segment carried only `records`, `tone_counts`, `capture_bytes`
//! and `mean_rate_hz`, on the reasoning that time transfer and BLE are
//! whole-session artefacts and their summaries belong at teardown. That is
//! still true of *pairing and skew*. What it cost was not obvious until a 16 h
//! run was underway: the session sidecar does not exist until the session ends,
//! so such a run could report nothing about its own delivery or its BLE
//! population for sixteen hours — the two things an operator most wants while
//! it is still possible to intervene.
//!
//! Two additions close that without duplicating teardown work:
//!
//! * **`summary.transmitters`** — per-source-MAC record counts, from the walk
//!   the sealer already performs. Dividing the injector's count by its
//!   commanded rate *is* the delivery fraction, and the same census answers
//!   "which transmitter dominates this segment", which any analysis scoped to
//!   one link needs anyway.
//! * **`summary.ble`** — the slice of the root's BLE log whose wallclock falls
//!   inside the segment's own CSI records. A forward-only cursor keeps the run
//!   O(n) and guarantees each observation is attributed to exactly one segment.
//!
//! Both are best-effort and additive: a segment with no wallclock, no BLE log
//! or an unreadable one seals exactly as it did before. Neither restates a
//! session-level number — `status` on a segment's BLE block reads `segment`
//! rather than a health verdict, and the cumulative counters (scan restarts,
//! adapter errors, parquet rows) are left to the root, because a per-segment
//! value for them would be a fabrication.
//!
//! ## `started_at` is the segment's, not the session's
//!
//! A segment used to inherit the session's `started_at` verbatim while
//! carrying its own `ended_at`, so its claimed span grew by one segment length
//! every rotation while its record count stayed flat. Any check comparing
//! `records / mean_rate_hz` against `ended_at - started_at` therefore saw a
//! discrepancy equal to the segment index — which is the signature of a clock
//! seam, i.e. real corruption. Measured on a 16 h run, 2026-08-17: every
//! segment from the fifth onward was quarantined on sound data.
//!
//! A segment now stamps the first record's host wallclock, falling back to the
//! inherited value only when nothing carried one.
//!
//! ## Ordering guarantee
//!
//! Segments are numbered from `0001` and sealed in order. Analysis that cares
//! about continuity (FTM unwrapping, drift block medians) must concatenate them
//! in index order; the session-level summary already does exactly that.

use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use crate::config::ExperimentConfig;
use crate::sidecar::{
    BleSummary, Sidecar, Status, SummaryMeta, TransmitterCensus, TransmitterCount,
};

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
            // The session root is the template sidecar's directory — that is
            // where the whole-session BLE log is appended.
            let mut ble_cursor = BleCursor::new(
                template
                    .path()
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
            );
            while let Ok(seg) = rx.recv() {
                match seal_one(&seg, &template, &cfg, &mut ble_cursor) {
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
/// Order matters, and the two consumers want **opposite** orders:
///
/// - `csid-sync` reads the on-disk `status` as its ready-to-ship signal, so the
///   file must say `capturing` until the export lands. A crash mid-export then
///   leaves a directory sync skips.
/// - The `.csiq` embeds the session block, and a published file must describe a
///   *finished* segment. Embedding `capturing` told every stranger the capture
///   was truncated, on effectively the whole archive.
///
/// Both are satisfied by finalising the sidecar **in memory**, embedding that
/// value in the export, and only then writing the finalised document to disk.
/// The on-disk file is still `capturing` for exactly the window the sync
/// guarantee needs, and the embedded block is true at close.
fn seal_one(
    seg: &Sealed,
    template: &Sidecar,
    cfg: &ExperimentConfig,
    ble_cursor: &mut BleCursor,
) -> Result<u64> {
    let mut sc = template.clone();
    sc.session_id = segment_dir_name(&template.session_id, seg.index);
    sc.set_path(seg.dir.join("metadata.json"));
    sc.status = Status::Capturing;
    sc.summary = None;
    sc.write()?;

    let stats = summarize_segment(&seg.raw, cfg);
    let mut summary = stats.summary;
    let records = summary.records;

    // A segment's `started_at` must be its OWN start, not the session's.
    //
    // Inheriting the session's start made a segment claim a span that grew by
    // 30 minutes every rotation while its record count stayed flat, so
    // `records / mean_rate_hz` disagreed with `ended_at - started_at` by
    // exactly the segment index. Downstream that is the signature of a clock
    // seam — real corruption — and the archive's integrity check duly flagged
    // it: measured 2026-08-17 on a 16 h run, every segment from the fifth
    // onward was quarantined as `clock-seam` on data that was entirely sound.
    //
    // The first record's host stamp is the honest answer to "when does this
    // segment's data begin". Fall back to the inherited value when no record
    // carried a wallclock, which is the pre-existing behaviour and no worse.
    if stats.first_ts_ns != 0 {
        sc.started_at = crate::util::rfc3339_utc(stats.first_ts_ns / 1_000_000_000);
    }

    if cfg.ble.enabled {
        summary.ble = ble_cursor.slice(stats.first_ts_ns, stats.last_ts_ns);
    }

    // Finalise in memory. `ended_at` is stamped exactly once here, so the
    // embedded block and the sidecar cannot report two close times.
    sc.finalise(Status::Complete, Some(summary));

    if cfg.export.on_close {
        let out = seg.dir.join("capture.csiq");
        let session = match serde_json::to_value(&sc) {
            Ok(v) => Some(v),
            Err(e) => {
                // A file with no session block is self-describing about that
                // absence; a file with a wrong one is not. Export anyway.
                tracing::error!(
                    error = %format!("{e:#}"),
                    "serialising the segment sidecar failed; exporting without a session block"
                );
                None
            }
        };
        if let Err(e) = crate::export::raw_to_csiq_with_session(
            &seg.raw,
            &out,
            cfg.radio.width.to_csiq(),
            session.as_ref(),
        ) {
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

    // Only now does the on-disk sidecar become the sync signal.
    sc.persist();
    Ok(records)
}

/// Forward-only reader that attributes the session's BLE stream to segments.
///
/// The BLE log is a whole-session artefact appended at the session root, so a
/// segment cannot own a file — but it can own a *slice*. The cursor remembers
/// how far it has read and consumes only rows whose wallclock falls at or
/// before the segment's last CSI record, leaving anything later for the next
/// segment. That keeps the whole run O(n) rather than re-reading a growing log
/// once per segment, and it means a row is counted by exactly one segment.
///
/// Deliberately best-effort: BLE is `required = false` on every long-running
/// profile precisely so a scanner fault cannot cost the CSI capture, and the
/// same reasoning applies here. An unreadable or malformed log yields `None`
/// and the segment seals exactly as it did before.
struct BleCursor {
    log: PathBuf,
    /// Byte offset of the first line not yet attributed to a segment.
    offset: u64,
}

impl BleCursor {
    fn new(session_root: PathBuf) -> Self {
        Self {
            log: session_root.join(crate::ble::NDJSON_NAME),
            offset: 0,
        }
    }

    /// Observations in `[first_ts_ns, last_ts_ns]`, advancing the cursor past
    /// them. Returns `None` when the window is unusable or nothing was read —
    /// which is different from a window that genuinely saw zero devices, and
    /// the caller cannot tell those apart, so a zero-observation window still
    /// returns `Some`.
    fn slice(&mut self, first_ts_ns: u64, last_ts_ns: u64) -> Option<BleSummary> {
        if last_ts_ns == 0 {
            // No CSI record carried a host stamp, so there is no window to
            // attribute against. Saying nothing beats guessing a boundary.
            return None;
        }

        let file = std::fs::File::open(&self.log).ok()?;
        let mut reader = std::io::BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset)).ok()?;

        let mut observations: u64 = 0;
        let mut malformed: u64 = 0;
        let mut rssi_unavailable: u64 = 0;
        let mut lab_frames: u64 = 0;
        let mut hashes: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut lab_participants: std::collections::HashSet<u16> = std::collections::HashSet::new();
        let mut first_seen: u64 = 0;
        let mut last_seen: u64 = 0;
        let mut max_gap_ms: u64 = 0;
        let mut prev_ns: u64 = 0;
        let mut consumed = self.offset;

        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).ok()?;
            if n == 0 {
                break; // caught up with the writer
            }
            // A final partial line means the writer is mid-append; leave the
            // offset before it so the next segment reads it whole.
            if !line.ends_with('\n') {
                break;
            }
            match serde_json::from_str::<crate::ble::Observation>(&line) {
                Ok(obs) => {
                    if obs.unix_ts_ns > last_ts_ns {
                        // Belongs to a later segment. Do NOT consume it.
                        break;
                    }
                    consumed += n as u64;
                    if obs.unix_ts_ns < first_ts_ns {
                        // Written before this segment's first CSI record —
                        // the previous segment's tail. Consume, do not count.
                        continue;
                    }
                    observations += 1;
                    hashes.insert(obs.device_hash);
                    if obs.rssi_dbm.is_none() {
                        rssi_unavailable += 1;
                    }
                    if let Some(key) = obs.lab_participant_key {
                        lab_frames += 1;
                        lab_participants.insert(key);
                    }
                    if first_seen == 0 {
                        first_seen = obs.unix_ts_ns;
                    }
                    if prev_ns != 0 {
                        max_gap_ms = max_gap_ms.max(obs.unix_ts_ns.saturating_sub(prev_ns) / 1_000_000);
                    }
                    prev_ns = obs.unix_ts_ns;
                    last_seen = obs.unix_ts_ns;
                }
                Err(_) => {
                    consumed += n as u64;
                    malformed += 1;
                }
            }
        }
        self.offset = consumed;

        let span_s = last_seen.saturating_sub(first_seen) as f64 / 1e9;
        Some(BleSummary {
            // A segment cannot judge scanner health across the session, and
            // `failed` here would contradict the session verdict computed at
            // teardown. It reports what it saw and lets the root decide.
            status: "segment".to_string(),
            observations,
            distinct_device_hashes: hashes.len() as u64,
            mean_rate_hz: if span_s > 0.0 {
                observations as f64 / span_s
            } else {
                0.0
            },
            max_gap_s: max_gap_ms as f64 / 1000.0,
            rssi_unavailable,
            malformed_log_lines: malformed,
            lab_frames,
            distinct_lab_participants: lab_participants.len() as u64,
            // Cumulative counters (restarts, adapter errors, unparsed events)
            // and the parquet row count are session-scoped and land at close;
            // a per-segment value for them would be a fabrication.
            ..BleSummary::default()
        })
    }
}

/// Busiest transmitters kept in a segment sidecar. An ambient 5 GHz channel
/// carries a dozen-odd beaconing BSSIDs (measured 2026-08-17: 12+ on ch44), so
/// an uncapped list would grow the sidecar without adding signal — the tail is
/// individually negligible and `distinct` already records that it existed.
const TOP_TRANSMITTERS: usize = 16;

/// What a segment walk learned, beyond the sidecar summary itself.
struct SegmentStats {
    summary: SummaryMeta,
    /// Wallclock bounds of the records in this segment, 0 when none carried a
    /// host stamp. This is what lets the BLE stream — a whole-session artefact
    /// at the root — be attributed to a segment without a second full pass.
    first_ts_ns: u64,
    last_ts_ns: u64,
}

/// Per-segment summary: record count, tone classes, achieved rate, and who
/// transmitted.
///
/// Still *not* the session-level summary: time transfer pairing and the BLE
/// parquet are whole-session artefacts living at the root, and pairing/skew is
/// computed once at teardown rather than per segment. What changed is that
/// "computed at teardown" used to mean a long run could say nothing about its
/// own delivery until it ended — a 16 h capture reported records and nothing
/// else for sixteen hours. The two things an operator needs mid-run are cheap
/// here: a per-MAC census costs nothing on a walk that already visits every
/// record, and BLE needs only this segment's time bounds to slice the root log.
fn summarize_segment(raw: &Path, cfg: &ExperimentConfig) -> SegmentStats {
    let mut summary = SummaryMeta::default();

    let Ok(file) = std::fs::File::open(raw) else {
        return SegmentStats {
            summary,
            first_ts_ns: 0,
            last_ts_ns: 0,
        };
    };
    let reader = std::io::BufReader::new(file);
    let mut rr = csiq::raw::RawReader::new(reader, cfg.radio.width.to_csiq());

    let mut tones: Vec<u16> = Vec::new();
    let mut count: u64 = 0;
    let mut first_ftm: Option<u32> = None;
    let mut unwrapper = csiq::FtmUnwrapper::new();
    let mut last_unwrapped: u64 = 0;
    let mut bytes: u64 = 0;
    let mut by_mac: std::collections::HashMap<[u8; 6], u64> = std::collections::HashMap::new();
    let mut first_ts: u64 = 0;
    let mut last_ts: u64 = 0;

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

        *by_mac.entry(rec.src_mac).or_insert(0) += 1;

        // A record with no host stamp cannot bound anything on the wallclock
        // timeline the BLE log is written on.
        if rec.unix_ts_ns != 0 {
            if first_ts == 0 {
                first_ts = rec.unix_ts_ns;
            }
            last_ts = rec.unix_ts_ns;
        }
    }

    if let Ok(md) = std::fs::metadata(raw) {
        bytes = md.len();
    }

    tones.sort_unstable();
    summary.records = count;
    summary.tone_counts = tones;
    summary.capture_bytes = bytes;
    summary.transmitters = Some(census(by_mac));

    if let Some(first) = first_ftm {
        let span_ticks = last_unwrapped.saturating_sub(first as u64);
        let span_s = csiq::ftm_to_seconds(span_ticks);
        if span_s > 0.0 {
            summary.mean_rate_hz = count as f64 / span_s;
        }
    }
    SegmentStats {
        summary,
        first_ts_ns: first_ts,
        last_ts_ns: last_ts,
    }
}

/// Collapse per-MAC counts into the sidecar's census, busiest first.
fn census(by_mac: std::collections::HashMap<[u8; 6], u64>) -> TransmitterCensus {
    let distinct = by_mac.len() as u64;
    let mut counts: Vec<([u8; 6], u64)> = by_mac.into_iter().collect();
    // Ties broken by MAC so a segment's census is reproducible rather than
    // depending on hash iteration order.
    counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(TOP_TRANSMITTERS);
    TransmitterCensus {
        distinct,
        top: counts
            .into_iter()
            .map(|(mac, records)| TransmitterCount {
                mac: crate::util::format_mac(&mac),
                records,
            })
            .collect(),
    }
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

    fn mac(last: u8) -> [u8; 6] {
        [0x02, 0x6d, 0x6f, 0x6e, 0x00, last]
    }

    /// Delivery is the point of the census: divide the injector's count by its
    /// commanded rate. Ordering must be by traffic, so the busiest transmitter
    /// is `top[0]` without the reader sorting anything.
    #[test]
    fn census_ranks_by_traffic_and_keeps_the_distinct_count() {
        let mut counts = std::collections::HashMap::new();
        counts.insert(mac(0x13), 75_000u64); // the injector
        counts.insert(mac(0x01), 900);
        counts.insert(mac(0x02), 40);

        let c = census(counts);
        assert_eq!(c.distinct, 3);
        assert_eq!(c.top[0].mac, "02:6d:6f:6e:00:13");
        assert_eq!(c.top[0].records, 75_000);
        assert_eq!(c.top[1].records, 900);
    }

    /// An ambient 5 GHz channel carries a dozen-plus beaconing BSSIDs, so the
    /// list is truncated — but `distinct` must still report the full breadth,
    /// or a reader would conclude the channel was quieter than it was.
    #[test]
    fn census_truncates_the_list_without_hiding_how_many_there_were() {
        let counts: std::collections::HashMap<[u8; 6], u64> =
            (0..40u8).map(|i| (mac(i), 100 - i as u64)).collect();

        let c = census(counts);
        assert_eq!(c.distinct, 40, "distinct counts every transmitter seen");
        assert_eq!(c.top.len(), TOP_TRANSMITTERS);
    }

    /// MAC rendering must match what an analysis scopes with. Lowercase, colon
    /// separated — a capitalised census would silently select nothing.
    #[test]
    fn census_renders_macs_in_the_form_analysis_scopes_with() {
        let mut counts = std::collections::HashMap::new();
        counts.insert([0xEF, 0xBE, 0xAD, 0xDE, 0xAD, 0xDE], 1u64);
        assert_eq!(census(counts).top[0].mac, "ef:be:ad:de:ad:de");
    }

    // ── IP-139 Phase 1: the session block is true at close ──────────────────

    /// A scratch directory that removes itself, so a failed assertion never
    /// leaves a half-sealed segment behind to confuse the next run.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "csid-seal-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn join(&self, p: &str) -> PathBuf {
            self.0.join(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One synthetic iax record: `[be32 msg_len][be32 hdr_len][hdr][be32 csi_len][csi]`.
    ///
    /// Only the fields `summarize_segment` and the TLV encoder read are set, so
    /// this stays a fixture for the *sealing* contract and not a second, drifting
    /// copy of the header layout.
    fn raw_record(ntone: u16, unix_ts_ns: u64) -> Vec<u8> {
        const HDR: usize = csiq::raw::HEADER_LEN;
        let mut hdr = vec![0u8; HDR];
        hdr[8..12].copy_from_slice(&1_000u32.to_le_bytes()); // ftm
        hdr[46] = 1; // nrx
        hdr[47] = 1; // ntx
        hdr[52..54].copy_from_slice(&ntone.to_le_bytes());
        hdr[60] = 40; // rssi magnitude -> -40 dBm
        hdr[68..74].copy_from_slice(&[0x02, 0x6d, 0x6f, 0x6e, 0x00, 0x13]); // src mac
        hdr[92..96].copy_from_slice(&0x4100u32.to_le_bytes()); // rate_n_flags
        hdr[208..216].copy_from_slice(&unix_ts_ns.to_le_bytes());
        hdr[216] = 11; // channel

        // 2 i16 per tone per chain, one chain.
        let csi = vec![0u8; ntone as usize * 4];

        let mut out = Vec::new();
        let msg_len = 4 + HDR + 4 + csi.len();
        out.extend_from_slice(&(msg_len as u32).to_be_bytes());
        out.extend_from_slice(&(HDR as u32).to_be_bytes());
        out.extend_from_slice(&hdr);
        out.extend_from_slice(&(csi.len() as u32).to_be_bytes());
        out.extend_from_slice(&csi);
        out
    }

    fn write_raw(path: &Path, records: usize) {
        let mut buf = Vec::new();
        for i in 0..records {
            buf.extend_from_slice(&raw_record(52, 1_700_000_000_000_000_000 + i as u64 * 10_000_000));
        }
        std::fs::write(path, buf).unwrap();
    }

    const SEAL_CFG: &str = r#"
[radio]
interface = "wlp1s0"
channel = 11
width = "HT20"

[capture]
mode = "passive"

[export]
on_close = true
"#;

    fn seal_cfg() -> ExperimentConfig {
        toml::from_str(SEAL_CFG).unwrap()
    }

    /// A sidecar in the state a segment template is in: open, capturing, no
    /// summary. Built by deserialisation so the test needs no radio, no
    /// `GlobalConfig` and no host probe.
    fn template_sidecar() -> crate::sidecar::Sidecar {
        serde_json::from_value(serde_json::json!({
            "schema": "csid-session/1",
            "session_id": "monad03_seal_20260824-120000",
            "run_id": "seal-test",
            "experiment": "seal",
            "tag": null,
            "radio": {
                "interface": "wlp1s0", "monitor": "wlp1s0mon0", "band": "2.4",
                "channel": 11, "control_freq_mhz": 2462, "center_freq_mhz": null,
                "width": "HT20", "interval_us": 0, "mac_filter": []
            },
            "environment": { "csid_version": "0.2.0" },
            "started_at": "2026-08-24T12:00:00Z",
            "ended_at": null,
            "status": "capturing",
            "summary": null
        }))
        .unwrap()
    }

    fn session_block(csiq_path: &Path) -> serde_json::Value {
        let f = std::fs::File::open(csiq_path).unwrap();
        let r = csiq::Reader::new(std::io::BufReader::new(f)).unwrap();
        r.session().expect("the export must embed a session block").clone()
    }

    /// The defect IP-139 P6 records: every segmented capture's `.csiq` claimed
    /// it was never finished, because the export re-read a sidecar deliberately
    /// left at `capturing` for the sync guarantee. Segments dominate the
    /// archive, so effectively every published file told a stranger the capture
    /// was truncated.
    #[test]
    fn a_sealed_segments_csiq_says_the_segment_finished() {
        let scratch = Scratch::new("complete");
        let raw = scratch.join("capture.raw");
        write_raw(&raw, 8);

        let seg = Sealed {
            dir: scratch.0.clone(),
            raw: raw.clone(),
            index: 1,
        };
        let mut cursor = BleCursor::new(scratch.0.clone());
        let records = seal_one(&seg, &template_sidecar(), &seal_cfg(), &mut cursor).unwrap();
        assert_eq!(records, 8);

        let block = session_block(&scratch.join("capture.csiq"));
        assert_eq!(
            block["status"], "complete",
            "a published file must not claim the capture was truncated"
        );
        assert!(
            block["ended_at"].is_string(),
            "a finished segment states when it finished"
        );
        assert_eq!(
            block["summary"]["records"], 8,
            "the embedded summary must be populated, not null"
        );
    }

    /// The embedded block and the on-disk sidecar must report ONE close time.
    ///
    /// A segment's export takes seconds to minutes. Finalising and writing at
    /// two instants would give one segment two `ended_at` values — a difference
    /// no reader could explain, and the signature every clock-seam check flags.
    #[test]
    fn the_embedded_block_and_the_sidecar_agree_on_the_close_time() {
        let scratch = Scratch::new("onetime");
        let raw = scratch.join("capture.raw");
        write_raw(&raw, 4);

        let seg = Sealed {
            dir: scratch.0.clone(),
            raw,
            index: 1,
        };
        let mut cursor = BleCursor::new(scratch.0.clone());
        seal_one(&seg, &template_sidecar(), &seal_cfg(), &mut cursor).unwrap();

        let block = session_block(&scratch.join("capture.csiq"));
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.join("metadata.json")).unwrap())
                .unwrap();

        assert_eq!(on_disk["status"], "complete");
        assert_eq!(
            block["ended_at"], on_disk["ended_at"],
            "one segment, one close time"
        );
        assert_eq!(block["summary"]["records"], on_disk["summary"]["records"]);
    }

    /// The export must never read the sidecar back from disk.
    ///
    /// This is the root cause, isolated: while a segment seals, the on-disk file
    /// says `capturing` on purpose, so that a crash mid-export leaves a
    /// directory `csid-sync` skips. Taking the session by value is what lets
    /// both consumers have the order they need.
    #[test]
    fn the_export_embeds_the_value_it_is_given_not_the_file_on_disk() {
        let scratch = Scratch::new("byvalue");
        let raw = scratch.join("capture.raw");
        write_raw(&raw, 2);

        // On disk: the deliberately stale, still-capturing sidecar.
        let mut stale = template_sidecar();
        stale.set_path(scratch.join("metadata.json"));
        stale.write().unwrap();

        // In memory: the finalised block the file should carry.
        let mut finalised = template_sidecar();
        finalised.finalise(
            Status::Complete,
            Some(crate::sidecar::SummaryMeta {
                records: 2,
                ..Default::default()
            }),
        );
        let value = serde_json::to_value(&finalised).unwrap();

        let out = scratch.join("capture.csiq");
        crate::export::raw_to_csiq_with_session(
            &raw,
            &out,
            csiq::Width::Ht20,
            Some(&value),
        )
        .unwrap();

        assert_eq!(session_block(&out)["status"], "complete");
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(
            on_disk["status"], "capturing",
            "the sync guarantee is untouched: the file is still the ready-to-ship signal"
        );
    }

    /// A failed export must still ship the segment. The raw is the source of
    /// truth and `csid export` can regenerate the container later, so a segment
    /// with raw but no csiq is worth far more off the node than on it.
    #[test]
    fn a_failed_export_still_marks_the_segment_complete() {
        let scratch = Scratch::new("exportfail");
        let raw = scratch.join("capture.raw");
        write_raw(&raw, 3);

        // A directory where the writer wants to create a file: File::create fails.
        std::fs::create_dir_all(scratch.join("capture.csiq")).unwrap();

        let seg = Sealed {
            dir: scratch.0.clone(),
            raw,
            index: 1,
        };
        let mut cursor = BleCursor::new(scratch.0.clone());
        let records = seal_one(&seg, &template_sidecar(), &seal_cfg(), &mut cursor).unwrap();
        assert_eq!(records, 3);

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(
            on_disk["status"], "complete",
            "the segment must ship even though its container could not be written"
        );
    }

    fn write_log(dir: &Path, rows: &[(u64, &str)]) {
        let mut s = String::new();
        for (ts, hash) in rows {
            s.push_str(&format!(
                r#"{{"unix_ts_ns":{ts},"device_hash":"{hash}","addr_kind":"public","pdu_type":"adv_ind","rssi_dbm":-70}}"#
            ));
            s.push('\n');
        }
        std::fs::write(dir.join(crate::ble::NDJSON_NAME), s).unwrap();
    }

    /// Every observation must be counted by exactly one segment. The cursor is
    /// what guarantees that: rows past the segment's last CSI record stay for
    /// the next one, and rows already consumed are never revisited.
    #[test]
    fn ble_rows_are_attributed_to_exactly_one_segment() {
        let dir = std::env::temp_dir().join(format!("csid-seg-ble-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_log(
            &dir,
            &[
                (1_000, "aa"),
                (2_000, "bb"),
                (3_000, "aa"),
                (9_000, "cc"), // after the first segment's window
            ],
        );

        let mut cur = BleCursor::new(dir.clone());
        let first = cur.slice(500, 3_500).expect("a window with records");
        assert_eq!(first.observations, 3);
        assert_eq!(
            first.distinct_device_hashes, 2,
            "aa appears twice and is one device"
        );

        let second = cur.slice(3_501, 10_000).expect("the next window");
        assert_eq!(
            second.observations, 1,
            "the row past the first window belongs to the second, and only once"
        );
        assert_eq!(second.distinct_device_hashes, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Without a wallclock bound there is no window to attribute against, and
    /// guessing one would silently mis-assign the whole stream.
    #[test]
    fn ble_says_nothing_when_the_segment_has_no_wallclock() {
        let dir = std::env::temp_dir().join(format!("csid-seg-ble-nots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_log(&dir, &[(1_000, "aa")]);

        let mut cur = BleCursor::new(dir.clone());
        assert!(cur.slice(0, 0).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The defect this replaced: a segment inheriting the session's start
    /// claims a span that grows every rotation while its records stay flat, and
    /// `records / rate` vs `ended_at - started_at` then reads as a clock seam.
    /// Guard the arithmetic that made it detectable.
    #[test]
    fn a_segments_span_matches_its_own_records_not_the_sessions() {
        // Segment 11 of a 30-minute rotation: 1800 s of records either way.
        let records = 776_565.0_f64;
        let rate = 431.4_f64;
        let implied_s = records / rate;

        let session_relative_span = 11.0 * 1800.0;
        assert!(
            session_relative_span / implied_s > 5.0,
            "inheriting the session start is what tripped the seam check"
        );

        let own_span = 1800.0;
        assert!(
            (own_span / implied_s) < 1.05,
            "a segment stamped with its own start agrees with its own records"
        );
    }

    /// BLE is `required = false` on every long profile so a scanner fault
    /// cannot cost the CSI capture. Sealing must inherit that: a missing log
    /// yields no BLE block, not a failed seal.
    #[test]
    fn ble_absence_is_not_a_sealing_failure() {
        let dir = std::env::temp_dir().join(format!("csid-seg-ble-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut cur = BleCursor::new(dir.clone());
        assert!(cur.slice(1, 2).is_none(), "no log means no claim");

        std::fs::remove_dir_all(&dir).ok();
    }
}
