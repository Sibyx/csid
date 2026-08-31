//! Periodic node and host state, attached to the record stream (IP-139 Phase 6).
//!
//! # Why this is in the FILE and not only in the metrics store
//!
//! The fleet already ships `csid_node_temp_celsius` and `csid_node_throttled` to
//! Mimir, and the measurement lake joins them per host and minute as `fleet_ops`.
//! For our own analysis that is enough. It is not enough for the format's own
//! promise — *a capture should be interpretable by someone who has only the file
//! and the spec* — because that reader has no Mimir, no lake and no tenant.
//!
//! So the file carries the conditions the capture was taken under. Which
//! conditions is not a guess; this project has paid for each of them:
//!
//! * **Die temperature** orders phase drift across the fleet, and the nodes do
//!   not share a thermal envelope — some have fans and some do not.
//! * **NIC die temperature** is the radio's own, and the radio is what makes the
//!   CSI. It is read separately because the SoC's cooler does not cool the card:
//!   the two run different envelopes in the same box.
//! * **Throttle flags** mean the SoC was capped. A throttled node is a different
//!   instrument, and the fact is invisible in the CSI itself.
//! * **Spool free bytes**: an overnight arm once ran out of the space the live
//!   console had quietly taken, its durable writer failed at hour 13, and the OOM
//!   killer took csid with it. The file should say the disk was closing in.
//! * **Load** separates "the radio delivered nothing" from "the host could not
//!   keep up", which look identical in a record count.
//!
//! # It is a sparse series, not a column
//!
//! The sampler ticks on an interval and the next record to be written carries
//! whatever it produced. Every other record carries nothing. That keeps the cost
//! at five small TLVs per tick rather than per record, and it is why every field
//! in [`csiq::NodeState`] is optional. A reader that treats these as a per-record
//! column will find them mostly absent and must not read that as zero.
//!
//! Every reading is best-effort and independent. A missing thermal zone is an
//! absence, never a failure — the same rule the rest of csid follows.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use csiq::record::NodeState;

/// How often the sampler produces a reading.
///
/// One minute, which is `fleet_ops`' own grain — so a published file and the
/// metrics store describe the same instants and a reader can check one against
/// the other. Faster would cost record size for a quantity that does not move
/// that fast; slower would miss a thermal excursion inside a 30-minute segment.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Samples node and host state on an interval, for attachment to records.
///
/// Deliberately synchronous and allocation-free on the hot path: [`take`] is
/// called once per record from the writer, and on all but one record in a
/// minute it does a single `Instant` comparison and returns an empty state.
///
/// [`take`]: Sampler::take
pub struct Sampler {
    spool: PathBuf,
    interval: Duration,
    next_at: Instant,
}

impl Sampler {
    /// A sampler that reads the filesystem `spool` lives on.
    ///
    /// The first reading is produced immediately, so a short session still
    /// records the conditions it ran under rather than none at all.
    pub fn new(spool: impl Into<PathBuf>, interval: Duration) -> Self {
        Sampler {
            spool: spool.into(),
            interval,
            next_at: Instant::now(),
        }
    }

    /// The state to attach to the next record, or an empty one.
    ///
    /// Returns a populated [`NodeState`] at most once per interval.
    pub fn take(&mut self) -> NodeState {
        let now = Instant::now();
        if now < self.next_at {
            return NodeState::default();
        }
        self.next_at = now + self.interval;
        read_now(&self.spool)
    }
}

/// One reading, independent of any schedule. Every field is best-effort.
pub fn read_now(spool: &Path) -> NodeState {
    NodeState {
        // Millidegrees, because the sysfs source is already millidegrees and a
        // float in the record would round-trip worse than the integer it came
        // from. `read_temp_c` divides by 1000; this multiplies back rather than
        // re-reading the file, so there is one parser for the thermal zone.
        temp_mc: crate::thermal::read_temp_c().map(|c| (c * 1000.0).round() as i32),
        throttle_flags: crate::thermal::read_throttle().map(|t| t.raw),
        spool_free_bytes: free_bytes(spool),
        load_m: load_1m_milli(),
        // The RADIO's own die temperature, from the driver's DTS. Separate from
        // `temp_mc` because the two are not the same instrument: the SoC sits
        // under the active cooler and the card sits under the HAT, so an
        // enclosure can hold one in spec while the other climbs.
        //
        // This is the only reading here that is not a file read — it is a
        // firmware round trip that can take up to a second (see
        // `debugfs::read_nic_temp_c`). At the one-minute default that is a
        // stall the supervisor loop absorbs; it is why this must not be moved
        // to a faster cadence without measuring what it costs.
        nic_temp_c: crate::debugfs::read_nic_temp_c(),
    }
}

/// Bytes free on the filesystem holding `path`.
///
/// Reuses the fleet probe's `statvfs` wrapper rather than opening a second one,
/// so "how much room is left" has a single answer in this daemon. The probe
/// reports gigabytes as a float; this converts back to bytes because a record
/// field should carry the measurement, not a rounded presentation of it.
fn free_bytes(path: &Path) -> Option<u64> {
    let d = crate::fleet::probe::disk_health(path, None)?;
    Some((d.free_gb * 1e9).round() as u64)
}

/// The 1-minute load average, times 1000.
///
/// Scaled to an integer for the same reason as the temperature: the record
/// carries a fixed-point value rather than a float whose text form varies.
fn load_1m_milli() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let first = text.split_whitespace().next()?;
    let v: f64 = first.parse().ok()?;
    Some((v * 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sampler must produce a reading at most once per interval. A record
    /// stream that carried node state on every record would pay four TLVs per
    /// record for a quantity that moves once a minute.
    #[test]
    fn a_reading_is_produced_at_most_once_per_interval() {
        let mut s = Sampler::new(std::env::temp_dir(), Duration::from_secs(3600));
        // First call is due immediately, so a short session records something.
        let first = s.take();
        // Whether the fields populate depends on the host; what must hold is
        // that the SECOND call inside the interval produces nothing at all.
        let second = s.take();
        assert!(
            second.is_empty(),
            "a second reading inside the interval must be empty, got {second:?}"
        );
        let _ = first;
    }

    /// A host with no thermal zone, no throttle file and no `/proc/loadavg` is
    /// not a failure. Absence is absence — the same rule the rest of csid keeps.
    #[test]
    fn an_unreadable_host_yields_an_empty_state_not_an_error() {
        let s = read_now(Path::new("/nonexistent-path-for-this-test"));
        // On a dev machine most of these are absent; the contract is only that
        // reading them cannot panic and cannot invent a value.
        assert!(s.spool_free_bytes.is_none(), "a missing path has no free bytes");
    }

    /// Zero is a real load and a real temperature. `None` means not measured,
    /// and the two must never collapse into one value.
    #[test]
    fn an_empty_state_is_distinguishable_from_a_zero_reading() {
        let empty = NodeState::default();
        assert!(empty.is_empty());

        let zeroed = NodeState {
            temp_mc: Some(0),
            throttle_flags: Some(0),
            spool_free_bytes: Some(0),
            load_m: Some(0),
            nic_temp_c: Some(0),
        };
        assert!(
            !zeroed.is_empty(),
            "all-zero readings are measurements, not an absent sample"
        );
    }
}
