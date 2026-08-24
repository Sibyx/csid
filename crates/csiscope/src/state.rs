//! The shared live buffer.
//!
//! `csiscope` inherits `csid`'s posture towards the capture: it is a *reader* of
//! a best-effort stream and must never be able to influence it. The ingest
//! thread does one thing — decode a datagram and push it — and every analysis
//! runs off a cheap `Arc` snapshot taken under a short read lock, so a slow
//! browser stalls only itself.
//!
//! The ring is bounded twice: by record count *and* by a coefficient budget.
//! A 996-tone 2×2 record is 16 KiB of I/Q, so an unbounded "last 8192 records"
//! would be 128 MiB on a node whose whole job is capture. The budget keeps the
//! console's footprint flat across tone counts by trimming history instead.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use csiq::CsiRecord;

/// One received live datagram plus the receiver-side context a record cannot
/// carry: the sender's sequence number and csiscope's own arrival stamp.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Session identity from the live datagram; a change means `csid` restarted.
    pub session_uid: u64,
    /// Sender-side monotonic datagram counter. Gaps are sender-side drops.
    pub seq: u32,
    /// Monotonic 320 MHz ticks — `ftm` unwrapped across its ~13.42 s wrap.
    pub ftm_ticks: u64,
    /// Arrival wallclock at csiscope (ns). Distinct from `rec.unix_ts_ns`,
    /// which `csid` stamped at netlink delivery; the difference is transport.
    pub recv_ns: u64,
    pub rec: Arc<CsiRecord>,
}

impl Sample {
    /// Complex coefficients in this record (`ntone * nrx * ntx`).
    pub fn coeffs(&self) -> usize {
        self.rec.coeff_count()
    }
}

/// Ingest-side counters, all monotonic since process start.
#[derive(Debug, Default)]
pub struct Counters {
    /// Datagrams decoded into records.
    pub received: AtomicU64,
    /// Datagrams that failed to decode (wrong magic, truncated, version skew).
    pub decode_errors: AtomicU64,
    /// Records the *sender* dropped, inferred from gaps in `seq`.
    pub sender_gaps: AtomicU64,
    /// Times the session uid changed — i.e. `csid` restarted under us.
    pub session_changes: AtomicU64,
    /// Datagram bytes received.
    pub bytes: AtomicU64,
    /// Wallclock (ns) of the most recent datagram, 0 if none yet.
    pub last_ns: AtomicU64,
}

/// A bounded, index-addressed ring of samples.
///
/// Absolute indices (rather than positions) let a WebSocket client say "give me
/// everything after what I last drew" without holding a cursor into a buffer
/// that may have rotated underneath it.
struct Ring {
    buf: VecDeque<Sample>,
    /// Absolute index of `buf[0]`.
    first_index: u64,
    max_records: usize,
    max_coeffs: usize,
    coeffs: usize,
}

impl Ring {
    fn new(max_records: usize, max_coeffs: usize) -> Self {
        Ring {
            buf: VecDeque::with_capacity(max_records.min(4096)),
            first_index: 0,
            max_records,
            max_coeffs,
            coeffs: 0,
        }
    }

    fn push(&mut self, s: Sample) {
        self.coeffs += s.coeffs();
        self.buf.push_back(s);
        while self.buf.len() > self.max_records
            || (self.coeffs > self.max_coeffs && self.buf.len() > 2)
        {
            if let Some(old) = self.buf.pop_front() {
                self.coeffs -= old.coeffs();
                self.first_index += 1;
            }
        }
    }

    fn total(&self) -> u64 {
        self.first_index + self.buf.len() as u64
    }
}

/// The process-wide live state: one ring, one set of counters.
pub struct Hub {
    ring: RwLock<Ring>,
    pub counters: Counters,
    /// Human-readable description of where the stream comes from.
    pub source: String,
    pub started: Instant,
    /// `csid`'s own view of the capture, polled from its status file.
    ///
    /// It lives here rather than beside the HTTP surface because the analysis
    /// needs it: the yield, the tuned channel and the commanded frame interval
    /// all belong in the same frame as the numbers derived from the records, or
    /// an operator is left correlating two panels that refreshed at different
    /// instants. It is `None` when csiscope is watching a UDP stream from
    /// another host, where there is no local status file to read.
    pub capture: Option<Arc<crate::capture::CaptureStatus>>,
}

impl Hub {
    /// Build a hub bounded by `max_records` and a total I/Q coefficient budget.
    pub fn new(source: String, max_records: usize, max_coeffs: usize) -> Arc<Self> {
        Arc::new(Hub {
            ring: RwLock::new(Ring::new(max_records, max_coeffs)),
            counters: Counters::default(),
            source,
            started: Instant::now(),
            capture: None,
        })
    }

    /// Build a hub that also reads `csid`'s status file.
    pub fn with_capture_status(
        source: String,
        max_records: usize,
        max_coeffs: usize,
        status: Arc<crate::capture::CaptureStatus>,
    ) -> Arc<Self> {
        Arc::new(Hub {
            ring: RwLock::new(Ring::new(max_records, max_coeffs)),
            counters: Counters::default(),
            source,
            started: Instant::now(),
            capture: Some(status),
        })
    }

    /// Append one decoded sample. Called only by the ingest thread.
    pub fn push(&self, s: Sample) {
        self.counters.received.fetch_add(1, Ordering::Relaxed);
        self.counters.last_ns.store(s.recv_ns, Ordering::Relaxed);
        if let Ok(mut r) = self.ring.write() {
            r.push(s);
        }
    }

    /// Total samples ever accepted (the absolute index of the next one).
    pub fn total(&self) -> u64 {
        self.ring.read().map(|r| r.total()).unwrap_or(0)
    }

    /// The most recent `n` samples, oldest first.
    pub fn tail(&self, n: usize) -> Vec<Sample> {
        let mut out = Vec::new();
        self.tail_into(n, &mut out);
        out
    }

    /// [`Hub::tail`], into a buffer the caller keeps between frames.
    ///
    /// The copy itself was never the problem — a `Sample` is four words and an
    /// `Arc` bump — but allocating and freeing a 256-element `Vec` twenty
    /// times a second, per client, was. Refilling a buffer the caller owns
    /// makes the steady state allocation-free.
    ///
    /// The read lock is held only for the copy. Ingest must never wait on
    /// analysis: the whole point of `csid`'s best-effort live path is that a
    /// slow consumer stalls itself and nothing else.
    pub fn tail_into(&self, n: usize, out: &mut Vec<Sample>) {
        out.clear();
        let Ok(r) = self.ring.read() else {
            return;
        };
        let skip = r.buf.len().saturating_sub(n);
        out.reserve(r.buf.len() - skip);
        out.extend(r.buf.iter().skip(skip).cloned());
    }

    /// Samples with absolute index `>= since`, capped at `max` (newest kept).
    ///
    /// Returns the new cursor, the samples, and how many were skipped because
    /// the caller fell behind the ring or the cap — the client needs the skip
    /// count to label the waterfall's real time compression honestly.
    pub fn since(&self, since: u64, max: usize) -> (u64, Vec<Sample>, u64) {
        let mut out = Vec::new();
        let (cursor, skipped) = self.since_into(since, max, &mut out);
        (cursor, out, skipped)
    }

    /// [`Hub::since`], into a buffer the caller keeps. Returns the new cursor
    /// and the skip count.
    pub fn since_into(&self, since: u64, max: usize, out: &mut Vec<Sample>) -> (u64, u64) {
        out.clear();
        let Ok(r) = self.ring.read() else {
            return (since, 0);
        };
        let total = r.total();
        if total <= since {
            return (total, 0);
        }
        let start = since.max(r.first_index);
        let lost = start.saturating_sub(since);
        let avail = (total - start) as usize;
        let take = avail.min(max);
        let dropped = (avail - take) as u64;
        let from = (start - r.first_index) as usize + (avail - take);
        out.reserve(take);
        out.extend(r.buf.iter().skip(from).cloned());
        (total, lost + dropped)
    }

    /// How many samples the ring is holding right now.
    pub fn depth(&self) -> usize {
        self.ring.read().map(|r| r.buf.len()).unwrap_or(0)
    }
}

/// Wallclock nanoseconds since the epoch.
pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: u64, ntone: u16) -> Sample {
        Sample {
            session_uid: 1,
            seq: i as u32,
            ftm_ticks: i * 1000,
            recv_ns: i,
            rec: Arc::new(CsiRecord {
                ftm: i as u32,
                us: 0,
                unix_ts_ns: 0,
                rnf: 0,
                phy: None,
                bw_antsel: None,
                mono_us: None,
                vendor_hdr: None,
                node: Default::default(),
                seq: 0,
                nrx: 1,
                ntx: 1,
                ntone,
                rssi: vec![-40],
                src_mac: [0; 6],
                channel: 36,
                width: csiq::Width::Ht20,
                iq: vec![0; ntone as usize * 2],
            }),
        }
    }

    #[test]
    fn ring_evicts_on_record_cap() {
        let hub = Hub::new("test".into(), 4, usize::MAX);
        for i in 0..10 {
            hub.push(sample(i, 52));
        }
        assert_eq!(hub.depth(), 4);
        assert_eq!(hub.total(), 10);
        let tail = hub.tail(10);
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[0].seq, 6);
    }

    #[test]
    fn ring_evicts_on_coefficient_budget() {
        // Budget for ~3 records of 996 tones.
        let hub = Hub::new("test".into(), 10_000, 3 * 996);
        for i in 0..20 {
            hub.push(sample(i, 996));
        }
        assert!(hub.depth() <= 4, "depth was {}", hub.depth());
    }

    #[test]
    fn since_reports_what_the_caller_missed() {
        let hub = Hub::new("test".into(), 4, usize::MAX);
        for i in 0..10 {
            hub.push(sample(i, 52));
        }
        // A client that last drew index 0 has fallen 6 behind the ring.
        let (cursor, got, lost) = hub.since(0, 100);
        assert_eq!(cursor, 10);
        assert_eq!(got.len(), 4);
        assert_eq!(lost, 6);

        // Caught up: nothing new.
        let (cursor2, got2, lost2) = hub.since(cursor, 100);
        assert_eq!(cursor2, 10);
        assert!(got2.is_empty());
        assert_eq!(lost2, 0);
    }

    #[test]
    fn since_caps_and_keeps_the_newest() {
        let hub = Hub::new("test".into(), 100, usize::MAX);
        for i in 0..10 {
            hub.push(sample(i, 52));
        }
        let (cursor, got, lost) = hub.since(0, 3);
        assert_eq!(cursor, 10);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[2].seq, 9,
            "the cap must drop the oldest, not the newest"
        );
        assert_eq!(lost, 7);
    }
}
