//! Time transfer over the illumination stream — `time_transfer.parquet`.
//!
//! ## The asset that was already on the air
//!
//! Both of this lab's transmitters stamp their transmit time **inside the
//! payload**, and nothing read it back. A fleet node that records, for every
//! stamped frame it receives, its own `unix_ts_ns` beside the transmitter's
//! stamp and sequence number, turns the illumination stream into a time-
//! transfer channel that costs nothing extra to run:
//!
//! * **Inter-node skew, to microseconds.** One frame, many receivers, one
//!   physical transmit instant that cancels *exactly* out of the difference of
//!   two nodes' receive stamps. See [`skew`] — this is a strictly better
//!   instrument than the ssh four-timestamp exchange in
//!   [`crate::fleet::clock`], which is bounded by round-trip time.
//! * **Phone → fleet affine offset, continuously.** Thousands of
//!   `(mono_ns, unix_ts_ns)` pairs per session instead of a handful of RTT
//!   bursts. See [`affine`], including what one-way delay does and does not let
//!   you claim.
//!
//! ## What this does NOT touch
//!
//! The capture hot path. The receiver is its own thread on its own `AF_PACKET`
//! socket; the CSI RX thread, the durable sink and the live sink are byte-for-
//! byte what they were. The `ftm` column is filled at **session close** by
//! walking `capture.raw` — the pass [`crate::engine`] already makes for the
//! close-time summary — rather than by wiring a third consumer into the RX
//! fan-out.
//!
//! ## Artefact layout
//!
//! ```text
//! <session>/
//!   time_transfer.jsonl     durable, append-only, written as frames arrive
//!   time_transfer.parquet   the contract artefact, written at close
//! ```
//!
//! Same split, and the same reasoning, as [`crate::ble`]: a session that lost
//! power before its close-time export is still readable from the log.
//!
//! ## The schema — a cross-repository contract
//!
//! `monad_knowledge.csi.timesync` asserts against these column names and types,
//! so a rename is a [`PARQUET_SCHEMA`] bump, not a refactor.
//!
//! | Column | Type | Null? | Meaning |
//! |---|---|---|---|
//! | `unix_ts_ns` | INT64 | required | Receiver wallclock at frame delivery |
//! | `host` | string | required | Receiving node |
//! | `session_id` | string | required | csid session this landed in |
//! | `rx_stamp_src` | string | required | `kernel` (SCM_TIMESTAMPNS) or `userspace` |
//! | `tx_kind` | string | required | `csid` or `app` |
//! | `tx_id` | string | required | Sentinel MAC, or the app's session UUID |
//! | `tx_mac` | string | required | 802.11 `addr2` — who transmitted |
//! | `seq` | INT64 | required | Payload sequence number |
//! | `tx_stamp_ns` | INT64 | required | The payload's transmit stamp |
//! | `tx_clock` | string | required | `unix` or `mono` — **never mix these** |
//! | `tx_wall_ns` | INT64 | null ok | App `wallMillis` × 1e6; null for `csid` |
//! | `ftm` | INT64 | null ok | Paired CSI 320 MHz counter; null when unpaired |
//! | `ftm_lag_ns` | INT64 | null ok | `csi.unix_ts_ns − unix_ts_ns` of that pairing |
//!
//! `rx_stamp_src` is not decoration. A userspace stamp carries the scheduler's
//! wake-up jitter, which is the same order as the quantity being measured — the
//! lesson `collectord` already learned. A session that fell back must not be
//! pooled with kernel-stamped ones, so the fallback is recorded per row rather
//! than assumed.

pub mod affine;
pub mod payload;
pub mod rx;
pub mod skew;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use parquet::basic::{Compression, LogicalType, Repetition, Type as PhysicalType};
use parquet::data_type::{ByteArray, ByteArrayType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;
use serde::{Deserialize, Serialize};

use crate::config::TimesyncConfig;
pub use payload::{TxClock, TxKind};

/// Crash-safe durable log, written as frames arrive.
pub const NDJSON_NAME: &str = "time_transfer.jsonl";
/// The contract artefact the analysis side consumes.
pub const PARQUET_NAME: &str = "time_transfer.parquet";
/// Schema identifier, mirrored into the sidecar. Bump on any column change.
pub const PARQUET_SCHEMA: &str = "time-transfer/1";
/// Rows per parquet row group.
const ROW_GROUP_ROWS: usize = 65_536;

/// Rows held in memory while streaming the time-transfer log at session close.
///
/// 64 Ki rows is ~12 MB of `Row` and one parquet row group, so the batch costs
/// about what the writer was going to buffer anyway.
pub const TIMESYNC_BATCH_ROWS: usize = 65_536;

/// Ticks the close-time pairing index may hold, as a memory budget.
///
/// A tick is 12 bytes in [`TickIndex`] (parallel `u64` + `u32`
/// vectors), so this is ~384 MB at the cap. The node has 2.07 GB and NO SWAP,
/// and the rest of teardown — one 64 Ki-row batch, the parquet writer's
/// buffers, the raw reader — is tens of megabytes, so the cap is the only term
/// that scales with session length and this leaves roughly a gigabyte of head
/// room.
///
/// For scale: the 16 h / 250 Hz night of 2026-08-17 stamped ~14 M ticks per
/// node, a third of the budget. A session that exceeds it does not crash and
/// does not pair silently — see [`TickIndex::pair`].
pub const TIMESYNC_TICK_BUDGET: usize = (384 << 20) / 12;

/// Where the receive stamp came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StampSource {
    /// `SCM_TIMESTAMPNS` — taken in the kernel, before the scheduler.
    Kernel,
    /// `clock_gettime` after the read returned. Carries wake-up jitter.
    Userspace,
}

impl StampSource {
    pub fn as_str(self) -> &'static str {
        match self {
            StampSource::Kernel => "kernel",
            StampSource::Userspace => "userspace",
        }
    }
}

/// One received frame that carried a recognised transmit stamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub unix_ts_ns: u64,
    pub rx_stamp_src: StampSource,
    pub tx_kind: TxKind,
    pub tx_id: String,
    pub tx_mac: String,
    pub seq: u64,
    pub tx_stamp_ns: u64,
    pub tx_clock: TxClock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_wall_ns: Option<u64>,
    /// Filled at session close from `capture.raw`; `None` when no CSI record
    /// could be attributed to this frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ftm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ftm_lag_ns: Option<i64>,
}

impl Row {
    pub fn from_stamp(s: payload::Stamp, unix_ts_ns: u64, rx_stamp_src: StampSource) -> Self {
        Row {
            unix_ts_ns,
            rx_stamp_src,
            tx_kind: s.kind,
            tx_id: s.tx_id,
            tx_mac: payload::mac_string(&s.tx_mac),
            seq: s.seq,
            tx_stamp_ns: s.tx_stamp_ns,
            tx_clock: s.tx_clock,
            tx_wall_ns: s.tx_wall_ns,
            ftm: None,
            ftm_lag_ns: None,
        }
    }
}

// -- counters -----------------------------------------------------------------

/// Liveness and diagnosis counters. The *mix* is what tells an operator why a
/// session produced no rows — an encrypted SSID, the wrong channel, or nothing
/// on the air at all are three different failures with three different fixes.
#[derive(Debug, Default)]
pub struct TimesyncCounters {
    /// Frames handed to the recogniser (excludes our own transmissions).
    pub frames_seen: AtomicU64,
    /// Frames this node transmitted, looped back by `AF_PACKET`, and skipped.
    /// A node must never "receive" its own injector.
    pub own_transmissions: AtomicU64,
    pub rows_csid: AtomicU64,
    pub rows_app: AtomicU64,
    /// Data frames encrypted over the air. A large count with zero app rows
    /// means the experiment SSID is not open — see `payload`'s module docs.
    pub protected: AtomicU64,
    /// Data frames carrying no format this build recognises (ordinary traffic).
    pub no_stamp: AtomicU64,
    /// Not a data frame at all (beacons, ACKs).
    pub not_data: AtomicU64,
    /// Socket read errors.
    pub errors: AtomicU64,
    pub first_row_ns: AtomicU64,
    pub last_row_ns: AtomicU64,
}

impl TimesyncCounters {
    pub fn note_row(&self, kind: TxKind, unix_ts_ns: u64) {
        match kind {
            TxKind::Csid => &self.rows_csid,
            TxKind::App => &self.rows_app,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.first_row_ns
            .compare_exchange(0, unix_ts_ns, Ordering::Relaxed, Ordering::Relaxed)
            .ok();
        self.last_row_ns.store(unix_ts_ns, Ordering::Relaxed);
    }

    pub fn note_reject(&self, r: payload::Reject) {
        match r {
            payload::Reject::Protected => &self.protected,
            payload::Reject::NotDataFrame => &self.not_data,
            _ => &self.no_stamp,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// `(rows, mean rate over the observed span)`.
    pub fn snapshot(&self) -> (u64, f64) {
        let n = self.rows_csid.load(Ordering::Relaxed) + self.rows_app.load(Ordering::Relaxed);
        let first = self.first_row_ns.load(Ordering::Relaxed);
        let last = self.last_row_ns.load(Ordering::Relaxed);
        let span_s = if last > first {
            (last - first) as f64 / 1e9
        } else {
            0.0
        };
        (n, if span_s > 0.0 { n as f64 / span_s } else { 0.0 })
    }
}

// -- durable log --------------------------------------------------------------

/// Append-only NDJSON writer for the receive thread.
pub struct RowLog {
    writer: BufWriter<File>,
    path: PathBuf,
    since_flush: usize,
    flush_every: usize,
    last_flush: Instant,
}

impl RowLog {
    pub fn create(dir: &Path, flush_every: usize) -> Result<Self> {
        let path = dir.join(NDJSON_NAME);
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(RowLog {
            writer: BufWriter::with_capacity(64 * 1024, file),
            path,
            since_flush: 0,
            flush_every: flush_every.max(1),
            last_flush: Instant::now(),
        })
    }

    pub fn append(&mut self, row: &Row) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, row)?;
        self.writer.write_all(b"\n")?;
        self.since_flush += 1;
        if self.since_flush >= self.flush_every
            || self.last_flush.elapsed() >= std::time::Duration::from_secs(2)
        {
            self.writer.flush()?;
            self.since_flush = 0;
            self.last_flush = Instant::now();
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        self.writer
            .flush()
            .context("flushing the time-transfer log")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("fsyncing the time-transfer log")?;
        Ok(self.path)
    }
}

/// Read the durable log back. A truncated final line — the signature of a power
/// cut — costs that line and nothing more.
///
/// ⚠️ **This holds the whole log in memory and a long session's log is large.**
/// A 16 h session at 250 Hz produced a 2.5 GB NDJSON, which is ~12.5 M rows and
/// ~2.4 GB of `Row` once parsed — on a node with 2.07 GB and no swap. Use it in
/// tests and on logs of known size; the session-close path uses
/// [`for_each_batch`], which is bounded. See the note on that function.
pub fn read_log(path: &Path) -> Result<(Vec<Row>, u64)> {
    let mut rows = Vec::new();
    // Moderate batches rather than `usize::MAX`: appending one giant batch would
    // hold the batch AND the destination at full size for the length of the
    // memcpy, doubling the peak of the very function whose peak is the problem.
    let malformed = for_each_batch(path, ROW_GROUP_ROWS, |batch| {
        rows.append(batch);
        Ok(())
    })?;
    Ok((rows, malformed))
}

/// Read the durable log in bounded batches, calling `f` with each.
///
/// WHY THIS EXISTS. On 2026-08-17 all six nodes were OOM-killed during teardown,
/// 40 s to 2 min after sealing their last segment of a 16 h / 250 Hz run. The
/// session root was therefore never closed: its sidecar still reads
/// `status: "capturing"`, no `time_transfer.parquet` was written, `csid-sync`
/// skipped it forever, and ~14.5 GB of time transfer was left with one copy on
/// six SD cards. Teardown allocated roughly:
///
/// | what | size |
/// |---|---|
/// | `read_log` → `Vec<Row>` | ~2.4 GB |
/// | `summarize` → `Vec<CsiTick>` | ~830 MB |
/// | `pair_ftm`'s `by_mac` copy of those ticks | ~415 MB |
///
/// against 2.07 GB of RAM and no swap. Every one of those grows with session
/// length, so the daemon had a maximum run length nothing declared and nothing
/// measured — it simply died at the end of a long enough capture, losing the
/// close rather than the capture.
///
/// The batch is REUSED between calls (`f` receives `&mut Vec<Row>` and is
/// expected to drain it), so the allocation is amortised and peak memory is
/// `batch_rows` rows rather than all of them.
///
/// Returns the malformed-line count, exactly as [`read_log`] does.
pub fn for_each_batch<F>(path: &Path, batch_rows: usize, mut f: F) -> Result<u64>
where
    F: FnMut(&mut Vec<Row>) -> Result<()>,
{
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut batch: Vec<Row> = Vec::new();
    let mut malformed = 0u64;
    for line in reader.lines() {
        let line = line.context("reading the time-transfer log")?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Row>(&line) {
            Ok(r) => batch.push(r),
            Err(_) => malformed += 1,
        }
        if batch.len() >= batch_rows {
            f(&mut batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        f(&mut batch)?;
        batch.clear();
    }
    Ok(malformed)
}

/// Distinct transmitter MACs in the durable log, without holding the log.
///
/// The session-close path needs this BEFORE it walks `capture.raw`, so it can
/// keep only the ticks that could ever be paired. Deserialising one field
/// instead of thirteen keeps the extra pass cheap: serde still tokenises each
/// line, but allocates one `String` rather than a whole `Row`.
pub fn distinct_tx_macs(path: &Path) -> Result<HashSet<[u8; 6]>> {
    #[derive(Deserialize)]
    struct MacOnly {
        tx_mac: String,
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut macs = HashSet::new();
    for line in reader.lines() {
        let line = line.context("reading the time-transfer log")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<MacOnly>(&line) {
            if let Some(mac) = payload::parse_mac(&m.tx_mac) {
                macs.insert(mac);
            }
        }
    }
    Ok(macs)
}

// -- ftm pairing --------------------------------------------------------------

/// One CSI record reduced to what the pairing needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsiTick {
    pub unix_ts_ns: u64,
    pub ftm: u32,
    pub src_mac: [u8; 6],
}

/// Attribute an `ftm` to each row, by nearest CSI record from the same
/// transmitter within `tolerance_ns`.
///
/// Pairing on time rather than on the 802.11 sequence-control byte is
/// deliberate. The driver header exposes a single byte at a fixed offset whose
/// relationship to the 12-bit sequence-control field is driver-coupled and has
/// never been verified on hardware; time is unambiguous at any injection rate
/// this fleet uses (25 Hz ⇒ 40 ms between frames, against a default 2 ms
/// window). `ftm_lag_ns` is recorded so a reader can judge each pairing rather
/// than trust it.
///
/// Rows that cannot be attributed keep `ftm = None`. That is the honest outcome
/// for a frame the driver reported no CSI for — which is normal, since CSI is
/// only produced for frames the radio actually sounded.
///
/// Returns the number of rows paired.
pub fn pair_ftm(rows: &mut [Row], ticks: &[CsiTick], tolerance_ns: u64) -> usize {
    let mut index = TickIndex::new(usize::MAX);
    for t in ticks {
        index.push(t.unix_ts_ns, t.ftm, t.src_mac);
    }
    index.seal();
    index.pair(rows, tolerance_ns)
}

/// Ticks grouped by transmitter, built once and queried per row.
///
/// ## Why this is not a `Vec<CsiTick>`
///
/// It used to be two structures. `summarize` collected every candidate tick
/// into a `Vec<CsiTick>` (24 bytes each after padding) and `pair_ftm` then built
/// a second, differently-shaped copy to search. On the 2026-08-17 night that was
/// ~830 MB plus ~415 MB, on a node with 2.07 GB and no swap — and it was pure
/// duplication, since nothing ever needed the flat vector.
///
/// This is the searchable form, filled directly during the raw scan. Parallel
/// vectors rather than `Vec<(u64, u32)>` because the tuple pads to 16 bytes and
/// the pair costs 12: a quarter of the memory, for free, on the one structure
/// here that scales with session length.
#[derive(Debug, Default)]
pub struct TickIndex {
    by_mac: HashMap<[u8; 6], (Vec<u64>, Vec<u32>)>,
    len: usize,
    cap: usize,
    overflowed: bool,
}

impl TickIndex {
    /// `cap` bounds the number of ticks held. Past it the index stops growing
    /// and reports [`Self::overflowed`] — see [`Self::pair`] for why that is a
    /// refusal to pair rather than a partial pairing.
    pub fn new(cap: usize) -> Self {
        TickIndex {
            cap,
            ..Default::default()
        }
    }

    pub fn push(&mut self, unix_ts_ns: u64, ftm: u32, src_mac: [u8; 6]) {
        if self.len >= self.cap {
            self.overflowed = true;
            return;
        }
        let e = self.by_mac.entry(src_mac).or_default();
        e.0.push(unix_ts_ns);
        e.1.push(ftm);
        self.len += 1;
    }

    /// Put each transmitter's ticks in time order.
    ///
    /// Records arrive in time order, so this is normally a scan that finds
    /// nothing to do. It is not an assertion, because these nodes have no RTC
    /// and chrony can step the clock mid-session — which reorders wallclock
    /// stamps within one capture. Sorting via a permutation keeps the parallel
    /// vectors in step.
    pub fn seal(&mut self) {
        for (ts, ftm) in self.by_mac.values_mut() {
            if ts.windows(2).all(|w| w[0] <= w[1]) {
                continue;
            }
            let mut order: Vec<u32> = (0..ts.len() as u32).collect();
            order.sort_unstable_by_key(|&i| ts[i as usize]);
            *ts = order.iter().map(|&i| ts[i as usize]).collect();
            *ftm = order.iter().map(|&i| ftm[i as usize]).collect();
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True when the tick cap was hit and this index is therefore incomplete.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Attribute an `ftm` to each row. Returns how many were paired.
    ///
    /// Refuses outright when the index overflowed. A partial index pairs the
    /// rows whose ticks happened to land under the cap and silently leaves the
    /// rest unpaired, which is indistinguishable in the output from a session
    /// where those frames genuinely had no CSI. "Too large to pair on this node"
    /// and "these frames were not received" must not look the same.
    pub fn pair(&self, rows: &mut [Row], tolerance_ns: u64) -> usize {
        if self.is_empty() || self.overflowed {
            return 0;
        }
        let mut paired = 0usize;
        for row in rows.iter_mut() {
            let Some(mac) = payload::parse_mac(&row.tx_mac) else {
                continue;
            };
            let Some((ts, ftms)) = self.by_mac.get(&mac) else {
                continue;
            };
            let i = ts.partition_point(|t| *t < row.unix_ts_ns);
            // The nearest tick is one of the two straddling the row.
            let mut best: Option<(u32, i64)> = None;
            for cand in [i.checked_sub(1), Some(i)].into_iter().flatten() {
                let (Some(&t), Some(&ftm)) = (ts.get(cand), ftms.get(cand)) else {
                    continue;
                };
                let lag = t as i64 - row.unix_ts_ns as i64;
                if lag.unsigned_abs() <= tolerance_ns
                    && best.is_none_or(|(_, b)| lag.abs() < b.abs())
                {
                    best = Some((ftm, lag));
                }
            }
            if let Some((ftm, lag)) = best {
                row.ftm = Some(ftm);
                row.ftm_lag_ns = Some(lag);
                paired += 1;
            }
        }
        paired
    }
}

// -- parquet export -----------------------------------------------------------

/// Session-constant columns, repeated on every row so a ten-node session
/// concatenates into one dataframe with no bookkeeping (dictionary encoding
/// makes the repetition nearly free).
#[derive(Debug, Clone)]
pub struct ParquetContext {
    pub host: String,
    pub session_id: String,
}

/// What the export produced — folded into the sidecar summary.
#[derive(Debug, Clone, Default)]
pub struct ExportStats {
    pub rows: u64,
    pub rows_csid: u64,
    pub rows_app: u64,
    pub ftm_paired: u64,
    pub distinct_transmitters: u64,
    pub malformed_lines: u64,
}

/// The `time_transfer.parquet` schema. **This is a contract** — see the module
/// docs; `monad_knowledge.csi.timesync` asserts against it.
fn parquet_schema() -> Result<Type> {
    let s = |name: &str| -> Result<Type> {
        Ok(Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()?)
    };
    let i64_req = |name: &str| -> Result<Type> {
        Ok(Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_repetition(Repetition::REQUIRED)
            .build()?)
    };
    let i64_opt = |name: &str| -> Result<Type> {
        Ok(Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_repetition(Repetition::OPTIONAL)
            .build()?)
    };
    let fields = vec![
        Arc::new(i64_req("unix_ts_ns")?),
        Arc::new(s("host")?),
        Arc::new(s("session_id")?),
        Arc::new(s("rx_stamp_src")?),
        Arc::new(s("tx_kind")?),
        Arc::new(s("tx_id")?),
        Arc::new(s("tx_mac")?),
        Arc::new(i64_req("seq")?),
        Arc::new(i64_req("tx_stamp_ns")?),
        Arc::new(s("tx_clock")?),
        // OPTIONAL: absent rather than zero. A zero wallclock is 1970, and a
        // zero ftm is a real counter value.
        Arc::new(i64_opt("tx_wall_ns")?),
        Arc::new(i64_opt("ftm")?),
        Arc::new(i64_opt("ftm_lag_ns")?),
    ];
    Ok(Type::group_type_builder("time_transfer")
        .with_fields(fields)
        .build()?)
}

/// An open `time_transfer.parquet`, fed a batch at a time.
///
/// Parquet is a row-group format and this writer always emitted row groups, so
/// the only thing standing between it and a bounded-memory export was the
/// `&[Row]` in the signature: the caller had to materialise every row first.
/// Feeding batches instead lets session close hold one batch rather than a
/// 16-hour session — see [`for_each_batch`] for what that cost on 2026-08-17.
///
/// Stats accumulate across batches, so the sidecar numbers are identical to
/// those the all-at-once path produced.
pub struct ParquetSink {
    writer: SerializedFileWriter<File>,
    ctx: ParquetContext,
    stats: ExportStats,
    /// Distinct `tx_id`s. Owned rather than borrowed because no batch outlives
    /// the sink; bounded by the number of transmitters, not by rows.
    seen: HashSet<String>,
    wrote_any: bool,
}

impl ParquetSink {
    pub fn create(out: &Path, ctx: ParquetContext) -> Result<Self> {
        let schema = Arc::new(parquet_schema()?);
        let props = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::SNAPPY)
                .build(),
        );
        let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
        let writer = SerializedFileWriter::new(file, schema, props)
            .context("opening the time_transfer.parquet writer")?;
        Ok(ParquetSink {
            writer,
            ctx,
            stats: ExportStats::default(),
            seen: HashSet::new(),
            wrote_any: false,
        })
    }

    pub fn write_batch(&mut self, rows: &[Row]) -> Result<()> {
        for r in rows {
            if !self.seen.contains(r.tx_id.as_str()) {
                self.seen.insert(r.tx_id.clone());
            }
            match r.tx_kind {
                TxKind::Csid => self.stats.rows_csid += 1,
                TxKind::App => self.stats.rows_app += 1,
            }
            if r.ftm.is_some() {
                self.stats.ftm_paired += 1;
            }
        }
        for chunk in rows.chunks(ROW_GROUP_ROWS) {
            self.stats.rows += write_row_group(&mut self.writer, chunk, &self.ctx)? as u64;
            self.wrote_any = true;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<ExportStats> {
        // A session that received nothing still gets a schema-correct empty
        // file: "no stamped frames" and "no artefact" are different diagnoses.
        if !self.wrote_any {
            write_row_group(&mut self.writer, &[], &self.ctx)?;
        }
        self.stats.distinct_transmitters = self.seen.len() as u64;
        self.writer
            .close()
            .context("closing time_transfer.parquet")?;
        Ok(self.stats)
    }
}

/// Write `rows` to `out` in one call.
///
/// A thin wrapper over [`ParquetSink`], kept because tests and any caller with a
/// log of known size are clearer this way. The session-close path streams.
pub fn write_parquet(rows: &[Row], out: &Path, ctx: &ParquetContext) -> Result<ExportStats> {
    let mut sink = ParquetSink::create(out, ctx.clone())?;
    sink.write_batch(rows)?;
    sink.finish()
}

fn write_row_group<W: std::io::Write + Send>(
    writer: &mut SerializedFileWriter<W>,
    batch: &[Row],
    ctx: &ParquetContext,
) -> Result<usize> {
    let mut rg = writer.next_row_group()?;

    let int_col = |rg: &mut parquet::file::writer::SerializedRowGroupWriter<'_, W>,
                   vals: Vec<i64>|
     -> Result<()> {
        let mut col = rg.next_column()?.context("missing required INT64 column")?;
        col.typed::<Int64Type>().write_batch(&vals, None, None)?;
        col.close()?;
        Ok(())
    };
    let text_col = |rg: &mut parquet::file::writer::SerializedRowGroupWriter<'_, W>,
                    vals: Vec<ByteArray>|
     -> Result<()> {
        let mut col = rg
            .next_column()?
            .context("missing required string column")?;
        col.typed::<ByteArrayType>()
            .write_batch(&vals, None, None)?;
        col.close()?;
        Ok(())
    };
    let opt_int_col = |rg: &mut parquet::file::writer::SerializedRowGroupWriter<'_, W>,
                       vals: Vec<Option<i64>>|
     -> Result<()> {
        let mut col = rg.next_column()?.context("missing optional INT64 column")?;
        // Definition level 1 = present, 0 = null; only present values are in
        // the value buffer.
        let def: Vec<i16> = vals.iter().map(|v| i16::from(v.is_some())).collect();
        let present: Vec<i64> = vals.iter().flatten().copied().collect();
        col.typed::<Int64Type>()
            .write_batch(&present, Some(&def), None)?;
        col.close()?;
        Ok(())
    };

    int_col(&mut rg, batch.iter().map(|r| r.unix_ts_ns as i64).collect())?;
    for constant in [&ctx.host, &ctx.session_id] {
        text_col(
            &mut rg,
            std::iter::repeat_n(ByteArray::from(constant.as_str()), batch.len()).collect(),
        )?;
    }
    text_col(
        &mut rg,
        batch
            .iter()
            .map(|r| ByteArray::from(r.rx_stamp_src.as_str()))
            .collect(),
    )?;
    text_col(
        &mut rg,
        batch
            .iter()
            .map(|r| ByteArray::from(r.tx_kind.as_str()))
            .collect(),
    )?;
    text_col(
        &mut rg,
        batch
            .iter()
            .map(|r| ByteArray::from(r.tx_id.as_str()))
            .collect(),
    )?;
    text_col(
        &mut rg,
        batch
            .iter()
            .map(|r| ByteArray::from(r.tx_mac.as_str()))
            .collect(),
    )?;
    int_col(&mut rg, batch.iter().map(|r| r.seq as i64).collect())?;
    int_col(
        &mut rg,
        batch.iter().map(|r| r.tx_stamp_ns as i64).collect(),
    )?;
    text_col(
        &mut rg,
        batch
            .iter()
            .map(|r| ByteArray::from(r.tx_clock.as_str()))
            .collect(),
    )?;
    opt_int_col(
        &mut rg,
        batch
            .iter()
            .map(|r| r.tx_wall_ns.map(|v| v as i64))
            .collect(),
    )?;
    opt_int_col(
        &mut rg,
        batch.iter().map(|r| r.ftm.map(i64::from)).collect(),
    )?;
    opt_int_col(&mut rg, batch.iter().map(|r| r.ftm_lag_ns).collect())?;

    rg.close()?;
    Ok(batch.len())
}

// -- node-local report --------------------------------------------------------

/// What `csid timesync report --json` returns, and what `csid fleet skew`
/// aggregates. Kept small on purpose: the arrivals are the payload, and a 60 s
/// window at 25 Hz is ~1500 of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimesyncReport {
    pub host: String,
    pub session_id: String,
    pub window_s: f64,
    pub schema: String,
    pub rows: usize,
    pub rows_csid: usize,
    pub rows_app: usize,
    /// Distinct `rx_stamp_src` values seen. A window mixing `kernel` and
    /// `userspace` is a window whose jitter has two causes.
    pub stamp_sources: Vec<String>,
    /// Per `csid`-kind transmitter: the arrivals the skew estimator needs.
    pub arrivals: Vec<(String, Vec<skew::Arrival>)>,
    /// Per app transmitter: the affine fit this node can make on its own.
    pub app_fits: Vec<affine::AffineFit>,
}

/// Build the report from a session's durable log.
pub fn report(
    host: &str,
    session_id: &str,
    rows: &[Row],
    window_s: f64,
    now_ns: u64,
    d_floor_ns: u64,
) -> TimesyncReport {
    let cutoff = if window_s > 0.0 {
        now_ns.saturating_sub((window_s * 1e9) as u64)
    } else {
        0
    };
    let scoped: Vec<&Row> = rows.iter().filter(|r| r.unix_ts_ns >= cutoff).collect();

    let mut arrivals: HashMap<&str, Vec<skew::Arrival>> = HashMap::new();
    let mut app: HashMap<&str, Vec<affine::Sample>> = HashMap::new();
    let mut sources: Vec<String> = Vec::new();
    for r in &scoped {
        let src = r.rx_stamp_src.as_str().to_string();
        if !sources.contains(&src) {
            sources.push(src);
        }
        match r.tx_kind {
            TxKind::Csid => arrivals.entry(&r.tx_id).or_default().push(skew::Arrival {
                seq: r.seq,
                unix_ts_ns: r.unix_ts_ns,
            }),
            TxKind::App => app.entry(&r.tx_id).or_default().push(affine::Sample {
                mono_ns: r.tx_stamp_ns,
                rx_unix_ns: r.unix_ts_ns,
                tx_wall_ns: r.tx_wall_ns,
            }),
        }
    }

    let mut arrivals: Vec<(String, Vec<skew::Arrival>)> = arrivals
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    arrivals.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    let mut app_fits: Vec<affine::AffineFit> = app
        .into_iter()
        .filter_map(|(k, v)| affine::fit(k, &v, d_floor_ns))
        .collect();
    app_fits.sort_by(|a, b| b.n.cmp(&a.n).then(a.tx_id.cmp(&b.tx_id)));

    TimesyncReport {
        host: host.to_string(),
        session_id: session_id.to_string(),
        window_s,
        schema: PARQUET_SCHEMA.to_string(),
        rows: scoped.len(),
        rows_csid: scoped.iter().filter(|r| r.tx_kind == TxKind::Csid).count(),
        rows_app: scoped.iter().filter(|r| r.tx_kind == TxKind::App).count(),
        stamp_sources: sources,
        arrivals,
        app_fits,
    }
}

// -- lifecycle ----------------------------------------------------------------

/// A running time-transfer receiver.
pub struct TimesyncHandle {
    thread: JoinHandle<()>,
    pub ndjson: PathBuf,
}

impl TimesyncHandle {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn new(thread: JoinHandle<()>, ndjson: PathBuf) -> Self {
        TimesyncHandle { thread, ndjson }
    }

    /// Join the receive thread. A panicking receiver must never fail the CSI
    /// session — the log up to that point is intact.
    pub fn join(self) -> PathBuf {
        if self.thread.join().is_err() {
            tracing::error!(
                "time-transfer thread panicked; {} up to that point is intact",
                NDJSON_NAME
            );
        }
        self.ndjson
    }
}

/// Start the time-transfer receiver on `monitor` (already up and tuned).
///
/// The socket is opened on the **caller's** thread, so a missing interface or a
/// missing `CAP_NET_RAW` fails at setup rather than silently producing nothing.
pub fn spawn(
    dir: &Path,
    monitor: &str,
    cfg: &TimesyncConfig,
    stop: Arc<AtomicBool>,
    counters: Arc<TimesyncCounters>,
) -> Result<TimesyncHandle> {
    rx::spawn(dir, monitor, cfg, stop, counters)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_786_000_000_000_000_000;
    const SENTINEL: &str = "ef:be:ad:de:ad:de";

    fn csid_row(seq: u64, rx_ns: u64) -> Row {
        Row {
            unix_ts_ns: rx_ns,
            rx_stamp_src: StampSource::Kernel,
            tx_kind: TxKind::Csid,
            tx_id: SENTINEL.into(),
            tx_mac: SENTINEL.into(),
            seq,
            tx_stamp_ns: rx_ns - 300_000,
            tx_clock: TxClock::Unix,
            tx_wall_ns: None,
            ftm: None,
            ftm_lag_ns: None,
        }
    }

    fn app_row(seq: u64, rx_ns: u64, mono: u64) -> Row {
        Row {
            unix_ts_ns: rx_ns,
            rx_stamp_src: StampSource::Userspace,
            tx_kind: TxKind::App,
            tx_id: "abababab-abab-abab-abab-abababababab".into(),
            tx_mac: "02:11:22:33:44:55".into(),
            seq,
            tx_stamp_ns: mono,
            tx_clock: TxClock::Mono,
            tx_wall_ns: Some(rx_ns / 1_000_000 * 1_000_000),
            ftm: None,
            ftm_lag_ns: None,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "csid-timesync-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_log_round_trips_and_survives_a_truncated_tail() {
        let dir = tmpdir("log");
        let mut log = RowLog::create(&dir, 1).unwrap();
        for i in 0..5u64 {
            log.append(&csid_row(i, T0 + i * 40_000_000)).unwrap();
        }
        log.append(&app_row(0, T0, 900_000_000_000)).unwrap();
        let path = log.finish().unwrap();

        let (rows, malformed) = read_log(&path).unwrap();
        assert_eq!(rows.len(), 6);
        assert_eq!(malformed, 0);
        assert_eq!(rows[0], csid_row(0, T0));
        assert_eq!(rows[5].tx_clock, TxClock::Mono);
        assert_eq!(rows[5].tx_wall_ns, Some(T0 / 1_000_000 * 1_000_000));

        // A power cut mid-write costs that line and nothing more.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"unix_ts_ns\":1,\"tx_ki");
        std::fs::write(&path, text).unwrap();
        let (rows, malformed) = read_log(&path).unwrap();
        assert_eq!(rows.len(), 6);
        assert_eq!(malformed, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The two clocks must stay distinguishable on disk. A serialisation that
    /// lost `tx_clock` would let the affine fit be applied to a unix stamp.
    #[test]
    fn the_wire_form_keeps_the_clock_kind_and_a_nanosecond_number() {
        let line = serde_json::to_string(&app_row(7, T0, 900_000_000_000)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["tx_clock"], "mono");
        assert_eq!(v["tx_kind"], "app");
        assert_eq!(v["rx_stamp_src"], "userspace");
        let ts = v["unix_ts_ns"].as_u64().unwrap();
        assert!(ts > 1_700_000_000_000_000_000 && ts < 2_000_000_000_000_000_000);
        assert!(!line.contains("\"unix_ts_ns\":\""), "must not be quoted");
        // Absent optionals are omitted, not nulled.
        let csid = serde_json::to_string(&csid_row(1, T0)).unwrap();
        assert!(!csid.contains("ftm"), "{csid}");
        assert!(!csid.contains("tx_wall_ns"), "{csid}");
    }

    /// A brute-force nearest-within-tolerance search, written independently of
    /// the index. If `TickIndex` and this ever disagree, the index is wrong —
    /// the whole point of streaming is that it changes memory, not results.
    fn reference_pair(rows: &mut [Row], ticks: &[CsiTick], tol: u64) -> usize {
        let mut paired = 0;
        for row in rows.iter_mut() {
            let Some(mac) = payload::parse_mac(&row.tx_mac) else {
                continue;
            };
            let mut best: Option<(u32, i64)> = None;
            for t in ticks.iter().filter(|t| t.src_mac == mac) {
                let lag = t.unix_ts_ns as i64 - row.unix_ts_ns as i64;
                if lag.unsigned_abs() <= tol && best.is_none_or(|(_, b)| lag.abs() < b.abs()) {
                    best = Some((t.ftm, lag));
                }
            }
            if let Some((ftm, lag)) = best {
                row.ftm = Some(ftm);
                row.ftm_lag_ns = Some(lag);
                paired += 1;
            }
        }
        paired
    }

    /// Streaming the log must yield exactly what reading it whole yielded, at
    /// every batch size — including one that divides the row count exactly, one
    /// that does not, and one larger than the file.
    #[test]
    fn batched_reads_are_indistinguishable_from_whole_ones() {
        let dir = tmpdir("batched");
        let mut log = RowLog::create(&dir, 1).unwrap();
        for i in 0..50u64 {
            log.append(&csid_row(i, T0 + i * 40_000_000)).unwrap();
        }
        let path = log.finish().unwrap();
        // A torn tail, so the malformed count is exercised too.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"unix_ts_ns\":1,\"tx_ki");
        std::fs::write(&path, text).unwrap();

        let (whole, whole_malformed) = read_log(&path).unwrap();
        assert_eq!(whole.len(), 50);
        assert_eq!(whole_malformed, 1);

        for batch_rows in [1usize, 7, 10, 49, 50, 51, 10_000] {
            let mut seen = Vec::new();
            let mut sizes = Vec::new();
            let malformed = for_each_batch(&path, batch_rows, |b| {
                sizes.push(b.len());
                seen.append(b);
                Ok(())
            })
            .unwrap();
            assert_eq!(seen, whole, "batch_rows={batch_rows}");
            assert_eq!(malformed, whole_malformed, "batch_rows={batch_rows}");
            assert!(
                sizes.iter().all(|n| *n <= batch_rows),
                "a batch exceeded its bound: {sizes:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The index is a memory optimisation. It must pair identically to a
    /// brute-force search, including when ticks arrive out of order — which
    /// these nodes can produce, having no RTC and a chrony that steps.
    #[test]
    fn the_tick_index_pairs_exactly_as_a_brute_force_search_does() {
        let mac = payload::parse_mac(SENTINEL).unwrap();
        let other = [0x02, 0x6d, 0x6f, 0x6e, 0x00, 0x13];

        let mut ticks = Vec::new();
        for i in 0..200u64 {
            ticks.push(CsiTick {
                unix_ts_ns: T0 + i * 4_000_000,
                ftm: 1000 + i as u32,
                src_mac: if i % 5 == 0 { other } else { mac },
            });
        }
        // A clock step: a block of ticks lands out of order.
        ticks[120..140].reverse();

        let mut rows: Vec<Row> = (0..60u64)
            .map(|i| csid_row(i, T0 + i * 13_000_000 + 500_000))
            .collect();
        let mut expect = rows.clone();

        let tol = 2_000_000u64;
        let want = reference_pair(&mut expect, &ticks, tol);

        let mut index = TickIndex::new(usize::MAX);
        for t in &ticks {
            index.push(t.unix_ts_ns, t.ftm, t.src_mac);
        }
        index.seal();
        let got = index.pair(&mut rows, tol);

        assert_eq!(got, want, "paired count");
        assert_eq!(rows, expect, "per-row ftm attribution");
        assert!(want > 0, "the fixture must actually pair something");
    }

    /// An index that hit its budget must pair NOTHING. A partial pairing is
    /// indistinguishable in the output from frames that had no CSI, and those
    /// are different facts.
    #[test]
    fn an_overflowed_index_refuses_to_pair_rather_than_pairing_some() {
        let mac = payload::parse_mac(SENTINEL).unwrap();
        let mut index = TickIndex::new(4);
        for i in 0..50u64 {
            index.push(T0 + i * 1_000_000, i as u32, mac);
        }
        index.seal();
        assert!(index.overflowed());
        assert_eq!(index.len(), 4);

        let mut rows = vec![csid_row(0, T0), csid_row(1, T0 + 1_000_000)];
        assert_eq!(index.pair(&mut rows, 2_000_000), 0);
        assert!(rows.iter().all(|r| r.ftm.is_none()));

        // ...and an index inside its budget still pairs.
        let mut ok = TickIndex::new(1000);
        for i in 0..50u64 {
            ok.push(T0 + i * 1_000_000, i as u32, mac);
        }
        ok.seal();
        assert!(!ok.overflowed());
        assert!(ok.pair(&mut rows, 2_000_000) > 0);
    }

    /// The sink fed in batches must produce the same file and the same stats as
    /// one all-at-once call. This is what lets session close stream.
    #[test]
    fn the_batched_sink_matches_the_all_at_once_writer() {
        let dir = tmpdir("sink");
        let ctx = ParquetContext {
            host: "monad05".into(),
            session_id: "monad05_test_20260818-000000".into(),
        };
        let mut rows: Vec<Row> = (0..300u64)
            .map(|i| csid_row(i, T0 + i * 40_000_000))
            .collect();
        rows.extend((0..120u64).map(|i| app_row(i, T0 + i * 50_000_000, 900_000_000_000 + i)));
        rows[7].ftm = Some(4242);
        rows[7].ftm_lag_ns = Some(-1234);

        let whole_path = dir.join("whole.parquet");
        let whole = write_parquet(&rows, &whole_path, &ctx).unwrap();

        let batched_path = dir.join("batched.parquet");
        let mut sink = ParquetSink::create(&batched_path, ctx.clone()).unwrap();
        for chunk in rows.chunks(37) {
            sink.write_batch(chunk).unwrap();
        }
        let batched = sink.finish().unwrap();

        assert_eq!(batched.rows, whole.rows);
        assert_eq!(batched.rows_csid, whole.rows_csid);
        assert_eq!(batched.rows_app, whole.rows_app);
        assert_eq!(batched.ftm_paired, whole.ftm_paired);
        assert_eq!(batched.distinct_transmitters, whole.distinct_transmitters);
        assert_eq!(batched.rows, 420);
        assert_eq!(batched.ftm_paired, 1);

        // Both files must be readable and carry every row.
        for path in [&whole_path, &batched_path] {
            let f = File::open(path).unwrap();
            let r = parquet::file::reader::SerializedFileReader::new(f).unwrap();
            use parquet::file::reader::FileReader;
            let total: i64 = r.metadata().row_groups().iter().map(|g| g.num_rows()).sum();
            assert_eq!(total, 420, "{path:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A streamed writer opens its file before it knows the read will succeed.
    /// A failure must not leave a parquet behind: a footerless one cannot be
    /// opened, and a footered short one is opened happily and is wrong. Both
    /// read as "this session exported" to anything that checks for the file.
    #[test]
    fn a_failed_stream_leaves_no_parquet_to_mistake_for_an_export() {
        let dir = tmpdir("partial");
        let out = dir.join(PARQUET_NAME);
        let ctx = ParquetContext {
            host: "monad05".into(),
            session_id: "monad05_test_20260818-000000".into(),
        };

        // The sink opens the file...
        let mut sink = ParquetSink::create(&out, ctx).unwrap();
        sink.write_batch(&[csid_row(0, T0)]).unwrap();
        assert!(out.is_file(), "the sink must have opened its file");

        // ...and a stream that never finishes leaves it without a footer.
        drop(sink);
        let f = File::open(&out).unwrap();
        assert!(
            parquet::file::reader::SerializedFileReader::new(f).is_err(),
            "a dropped sink should leave an unreadable file — if this ever \
             starts passing, the cleanup in finish_timesync is still right but \
             this test no longer proves why"
        );

        // Which is why the caller removes it. Reading a log that does not exist
        // is the cheapest way to reach that path.
        assert!(for_each_batch(&dir.join("absent.jsonl"), 10, |_| Ok(())).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Distinct MACs must be recoverable without holding the log.
    #[test]
    fn the_mac_scan_finds_every_transmitter_without_reading_rows() {
        let dir = tmpdir("macs");
        let mut log = RowLog::create(&dir, 1).unwrap();
        for i in 0..10u64 {
            log.append(&csid_row(i, T0 + i)).unwrap();
        }
        let mut other = csid_row(99, T0);
        other.tx_mac = "02:6d:6f:6e:00:13".into();
        log.append(&other).unwrap();
        let path = log.finish().unwrap();

        let macs = distinct_tx_macs(&path).unwrap();
        assert_eq!(macs.len(), 2);
        assert!(macs.contains(&payload::parse_mac(SENTINEL).unwrap()));
        assert!(macs.contains(&[0x02, 0x6d, 0x6f, 0x6e, 0x00, 0x13]));

        // It must agree with what a full read would have found.
        let (rows, _) = read_log(&path).unwrap();
        let from_rows: HashSet<[u8; 6]> = rows
            .iter()
            .filter_map(|r| payload::parse_mac(&r.tx_mac))
            .collect();
        assert_eq!(macs, from_rows);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ftm_pairs_on_the_nearest_record_from_the_same_transmitter() {
        let mac = payload::parse_mac(SENTINEL).unwrap();
        let other = payload::parse_mac("aa:bb:cc:dd:ee:ff").unwrap();
        let mut rows = vec![
            csid_row(0, T0),
            csid_row(1, T0 + 40_000_000),
            csid_row(2, T0 + 80_000_000),
        ];
        let ticks = vec![
            // 120 µs after row 0 — inside the window.
            CsiTick {
                unix_ts_ns: T0 + 120_000,
                ftm: 111,
                src_mac: mac,
            },
            // A closer record, but from a different transmitter: must not win.
            CsiTick {
                unix_ts_ns: T0 + 40_000_010,
                ftm: 999,
                src_mac: other,
            },
            // 300 µs before row 1.
            CsiTick {
                unix_ts_ns: T0 + 39_700_000,
                ftm: 222,
                src_mac: mac,
            },
            // Row 2 has nothing within tolerance.
            CsiTick {
                unix_ts_ns: T0 + 200_000_000,
                ftm: 333,
                src_mac: mac,
            },
        ];
        let paired = pair_ftm(&mut rows, &ticks, 2_000_000);
        assert_eq!(paired, 2);
        assert_eq!(rows[0].ftm, Some(111));
        assert_eq!(rows[0].ftm_lag_ns, Some(120_000));
        assert_eq!(rows[1].ftm, Some(222));
        assert_eq!(rows[1].ftm_lag_ns, Some(-300_000));
        assert_eq!(
            rows[2].ftm, None,
            "no record in tolerance is None, not a guess"
        );
        assert_eq!(rows[2].ftm_lag_ns, None);
    }

    /// The frame arrived; the driver reported no CSI for it. That is normal and
    /// must not be papered over with the nearest record from a second ago.
    #[test]
    fn an_unsounded_frame_stays_unpaired() {
        let mut rows = vec![csid_row(0, T0)];
        assert_eq!(pair_ftm(&mut rows, &[], 2_000_000), 0);
        assert_eq!(rows[0].ftm, None);

        let mac = payload::parse_mac(SENTINEL).unwrap();
        let far = vec![CsiTick {
            unix_ts_ns: T0 + 1_000_000_000,
            ftm: 5,
            src_mac: mac,
        }];
        assert_eq!(pair_ftm(&mut rows, &far, 2_000_000), 0);
    }

    #[test]
    fn parquet_writes_the_contract_columns_including_the_null_ones() {
        let dir = tmpdir("parquet");
        let mut rows = vec![csid_row(0, T0), app_row(1, T0 + 1_000_000, 900_000_000_000)];
        rows[0].ftm = Some(0); // a real counter value of zero, not a null
        rows[0].ftm_lag_ns = Some(-5);

        let out = dir.join(PARQUET_NAME);
        let stats = write_parquet(
            &rows,
            &out,
            &ParquetContext {
                host: "monad02".into(),
                session_id: "monad02_lab-anchor_20260808-101500".into(),
            },
        )
        .unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(stats.rows_csid, 1);
        assert_eq!(stats.rows_app, 1);
        assert_eq!(stats.ftm_paired, 1);
        assert_eq!(stats.distinct_transmitters, 2);
        assert!(out.metadata().unwrap().len() > 0);

        // The column contract, read back off the file itself.
        let file = File::open(&out).unwrap();
        let reader = parquet::file::serialized_reader::SerializedFileReader::new(file).unwrap();
        let schema = parquet::file::reader::FileReader::metadata(&reader)
            .file_metadata()
            .schema_descr();
        let names: Vec<String> = (0..schema.num_columns())
            .map(|i| schema.column(i).name().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "unix_ts_ns",
                "host",
                "session_id",
                "rx_stamp_src",
                "tx_kind",
                "tx_id",
                "tx_mac",
                "seq",
                "tx_stamp_ns",
                "tx_clock",
                "tx_wall_ns",
                "ftm",
                "ftm_lag_ns",
            ],
            "the column contract with monad_knowledge.csi.timesync"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// "The session received nothing" and "the session wrote no artefact" are
    /// different diagnoses and must not look alike to a reader.
    #[test]
    fn an_empty_session_still_produces_a_schema_correct_file() {
        let dir = tmpdir("empty");
        let out = dir.join(PARQUET_NAME);
        let stats = write_parquet(
            &[],
            &out,
            &ParquetContext {
                host: "monad02".into(),
                session_id: "s".into(),
            },
        )
        .unwrap();
        assert_eq!(stats.rows, 0);
        assert!(out.exists());
        assert!(out.metadata().unwrap().len() > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_report_windows_and_splits_the_two_transmitter_kinds() {
        let mut rows: Vec<Row> = (0..200u64)
            .map(|i| csid_row(i, T0 + i * 40_000_000))
            .collect();
        for i in 0..100u64 {
            rows.push(app_row(
                i,
                T0 + i * 50_000_000,
                900_000_000_000 + i * 50_000_000,
            ));
        }
        let now = T0 + 8_000_000_000;

        // Whole session.
        let all = report("monad02", "s", &rows, 0.0, now, affine::D_FLOOR_DEFAULT_NS);
        assert_eq!(all.rows, 300);
        assert_eq!(all.rows_csid, 200);
        assert_eq!(all.rows_app, 100);
        assert_eq!(all.arrivals.len(), 1);
        assert_eq!(all.arrivals[0].0, SENTINEL);
        assert_eq!(all.arrivals[0].1.len(), 200);
        assert_eq!(all.app_fits.len(), 1);
        assert_eq!(all.schema, PARQUET_SCHEMA);
        assert_eq!(all.stamp_sources, vec!["kernel", "userspace"]);

        // Last 2 s only.
        let win = report("monad02", "s", &rows, 2.0, now, affine::D_FLOOR_DEFAULT_NS);
        assert!(win.rows < all.rows && win.rows > 0);
        assert!(win.arrivals[0]
            .1
            .iter()
            .all(|a| a.unix_ts_ns >= now - 2_000_000_000));
    }

    /// A window with too few app packets must produce NO fit rather than a
    /// slope invented from noise.
    #[test]
    fn a_thin_app_stream_produces_no_fit() {
        let rows: Vec<Row> = (0..4u64)
            .map(|i| app_row(i, T0 + i * 50_000_000, 900_000_000_000 + i * 50_000_000))
            .collect();
        let r = report(
            "monad02",
            "s",
            &rows,
            0.0,
            T0 + 1_000_000_000,
            affine::D_FLOOR_DEFAULT_NS,
        );
        assert_eq!(r.rows_app, 4);
        assert!(r.app_fits.is_empty());
    }

    #[test]
    fn counters_separate_the_reasons_a_session_produced_nothing() {
        let c = TimesyncCounters::default();
        c.note_reject(payload::Reject::Protected);
        c.note_reject(payload::Reject::Protected);
        c.note_reject(payload::Reject::NotDataFrame);
        c.note_reject(payload::Reject::NoStamp);
        c.note_reject(payload::Reject::Short);
        assert_eq!(c.protected.load(Ordering::Relaxed), 2);
        assert_eq!(c.not_data.load(Ordering::Relaxed), 1);
        assert_eq!(c.no_stamp.load(Ordering::Relaxed), 2);

        c.note_row(TxKind::Csid, T0);
        c.note_row(TxKind::App, T0 + 1_000_000_000);
        c.note_row(TxKind::Csid, T0 + 2_000_000_000);
        let (n, rate) = c.snapshot();
        assert_eq!(n, 3);
        assert!((rate - 1.5).abs() < 1e-9);
        assert_eq!(c.first_row_ns.load(Ordering::Relaxed), T0);
    }
}
