//! Rotating BLE-only archive. Rotation changes neither the HCI socket nor scan
//! parameters. Closed logs export on one background thread; the seal appears
//! only after parquet is durable. A lagging exporter fails loudly with raw logs
//! intact, instead of building an unbounded queue.
//!
//! Segments are wall-clock aligned (`index = floor(unix_s / segment_s)`) and
//! salted with the fleet salt of their index, derived from that UTC day's key
//! (see [`ble::FleetKeyChain`]), so every node rotates at the same instant and
//! hashes one address to one pseudonym inside a segment. The index is taken
//! from each observation's own timestamp, so a frame is never hashed with a
//! neighbouring segment's salt. The first segment after a start is shorter
//! than `segment_s`; a clock step (these nodes have no RTC) changes the index
//! and so rotates the log. A segment whose day has no key on this node (the
//! clock sits before the key's day) gets a random salt and says so in its seal.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ble::{
    self, DeviceHasher, FleetKeyChain, IntervalSnapshot, IntervalStats, LabMatcher, ObservationLog,
    ParquetContext, RawAdv, SaltScope,
};
use crate::config::BleConfig;
use crate::util::{now_unix_ns, rfc3339_utc};
use anyhow::{Context, Result};

/// Seal schema. `/2` (csid 2026-09-24): fleet-segment salt from a daily key,
/// wall-clock aligned segments and the `salt` block. `/1` segments carry a
/// random salt per node and segment and must never be joined across nodes.
pub const SEAL_SCHEMA: &str = "ble-continuous/2";

/// The wall-clock segment a timestamp falls in.
fn segment_index(unix_ns: u64, segment_s: u64) -> u64 {
    unix_ns / 1_000_000_000 / segment_s
}

pub struct ContinuousLog {
    root: PathBuf,
    cfg: BleConfig,
    segment_s: u64,
    index: u64,
    /// Shared with the next segment; a segment on a new day moves it forward.
    chain: Arc<Mutex<FleetKeyChain>>,
    started_ns: u64,
    log: Option<ObservationLog>,
    hasher: DeviceHasher,
    /// Heartbeat statistics. Owned by the segment because the salt is: a
    /// rotation replaces the whole log, so the cumulative label count restarts
    /// with the pseudonyms it counts. The heartbeat that straddles a rotation
    /// reports only the part after it.
    stats: IntervalStats,
    context: ParquetContext,
    exporting: Option<JoinHandle<Result<()>>>,
}

impl ContinuousLog {
    pub fn open(
        root: &Path,
        cfg: &BleConfig,
        segment: Duration,
        host: &str,
        chain: FleetKeyChain,
    ) -> Result<Self> {
        let segment_s = segment.as_secs();
        anyhow::ensure!(segment_s >= 60, "BLE segments must be at least 60 seconds");
        // A segment must never straddle midnight UTC, or its salt would need
        // two days' keys.
        anyhow::ensure!(
            86_400 % segment_s == 0,
            "BLE segment length must divide a day (e.g. 5m, 15m, 30m, 1h); got {segment_s} s"
        );
        let chain = Arc::new(Mutex::new(chain));
        Self::open_at(root, cfg, segment_s, host, chain, now_unix_ns())
    }

    /// Open the segment `at_ns` falls in. The session id keeps the
    /// `{host}_ble-continuous_{started_ns}` form every reader parses.
    fn open_at(
        root: &Path,
        cfg: &BleConfig,
        segment_s: u64,
        host: &str,
        chain: Arc<Mutex<FleetKeyChain>>,
        at_ns: u64,
    ) -> Result<Self> {
        let started_ns = at_ns;
        let index = segment_index(at_ns, segment_s);
        let day = ble::utc_day(at_ns);
        let key = chain
            .lock()
            .map_err(|_| anyhow::anyhow!("BLE fleet key chain poisoned"))?
            .key_for_day(day)?;
        let (hasher, salt_scope) = match &key {
            Some(k) => {
                // One line per segment; every node's line for one segment
                // must carry the same key_id, which is the fleet-wide check.
                tracing::info!(
                    key_day = day,
                    key_id = %k.id(),
                    segment_index = index,
                    "BLE fleet salt armed"
                );
                (
                    DeviceHasher::for_fleet_segment(k, segment_s, index, cfg.hash_bytes),
                    SaltScope::FleetSegment {
                        segment_s,
                        index,
                        key_day: day,
                        key_id: k.id(),
                    },
                )
            }
            None => {
                tracing::warn!(
                    day,
                    "no BLE fleet key for this UTC day (clock before the key's day, or a wild \
                     clock); this segment gets a random salt and will not join across nodes"
                );
                (
                    DeviceHasher::new_random(cfg.hash_bytes)?,
                    SaltScope::Segment,
                )
            }
        };
        let context = ParquetContext {
            host: host.to_string(),
            session_id: format!("{host}_ble-continuous_{started_ns}"),
            adapter: cfg.adapter.clone(),
            lab_namespace_uuid: cfg.lab_matcher()?.map(|m| m.namespace().to_string()),
            scan_interval_ms: cfg.scan_interval_ms,
            scan_window_ms: cfg.scan_window_ms,
            hash_bytes: cfg.hash_bytes,
            salt_scope,
        };
        let dir = root.join(&context.session_id);
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            root: root.to_owned(),
            cfg: cfg.clone(),
            segment_s,
            index,
            chain,
            started_ns,
            log: Some(ObservationLog::create(&dir, cfg.flush_every)?),
            hasher,
            stats: IntervalStats::default(),
            context,
            exporting: None,
        })
    }

    pub fn observe(&mut self, adv: &RawAdv, ts: u64, matcher: Option<&LabMatcher>) -> Result<()> {
        self.rotate_at(ts)?;
        let obs = self.hasher.observe(adv, ts, matcher);
        self.stats.note(&obs);
        self.log
            .as_mut()
            .context("BLE log already closed")?
            .append(&obs)?;
        Ok(())
    }

    /// The heartbeat's view of the interval just elapsed.
    pub fn take_interval(&mut self) -> IntervalSnapshot {
        self.stats.take()
    }

    /// Rotate when the wall clock has left this segment. Called from the scan
    /// loop, so a quiet room still seals on time.
    pub fn rotate_if_due(&mut self) -> Result<()> {
        self.rotate_at(now_unix_ns())
    }

    fn rotate_at(&mut self, ts: u64) -> Result<()> {
        if segment_index(ts, self.segment_s) == self.index {
            return Ok(());
        }
        self.join_export(true)?;
        let next = Self::open_at(
            &self.root,
            &self.cfg,
            self.segment_s,
            &self.context.host,
            self.chain.clone(),
            ts,
        )?;
        let mut old = std::mem::replace(self, next);
        self.exporting = Some(old.seal_async()?);
        Ok(())
    }

    fn seal_async(&mut self) -> Result<JoinHandle<Result<()>>> {
        let path = self
            .log
            .take()
            .context("BLE log already closed")?
            .finish()?;
        let ctx = self.context.clone();
        let started_ns = self.started_ns;
        let ended_ns = now_unix_ns();
        let (segment_s, index) = (self.segment_s, self.index);
        std::thread::Builder::new().name("ble-export".into()).spawn(move || {
            let dir = path.parent().context("BLE log parent")?;
            let stats = ble::export_parquet(&path, &dir.join(ble::PARQUET_NAME), &ctx)?;
            std::fs::File::open(dir.join(ble::PARQUET_NAME))?.sync_all()?;
            let nominal_start_s = index * segment_s;
            let (hash_scope, salt) = match &ctx.salt_scope {
                SaltScope::FleetSegment { key_day, key_id, .. } => (
                    "fleet-segment",
                    serde_json::json!({
                        "scheme": ble::FLEET_SALT_SCHEME,
                        "key_schedule": ble::FLEET_RATCHET_SCHEME,
                        "persisted": false,
                        "segment_s": segment_s, "segment_index": index,
                        "key_day": key_day, "key_id": key_id,
                        "nominal_start_utc": rfc3339_utc(nominal_start_s),
                        "nominal_end_utc": rfc3339_utc(nominal_start_s + segment_s),
                    }),
                ),
                _ => (
                    "segment",
                    serde_json::json!({
                        "scheme": "random", "persisted": false,
                        "reason": "no fleet key for this UTC day on this node",
                        "segment_s": segment_s, "segment_index": index,
                    }),
                ),
            };
            let seal = serde_json::json!({
                "schema": SEAL_SCHEMA, "observations_schema": ble::PARQUET_SCHEMA,
                "session_id": ctx.session_id, "host": ctx.host, "adapter": ctx.adapter,
                "started_utc": rfc3339_utc(started_ns / 1_000_000_000),
                "ended_utc": rfc3339_utc(ended_ns / 1_000_000_000),
                "started_unix_ns": started_ns, "ended_unix_ns": ended_ns,
                "observations": stats.rows, "distinct_device_hashes": stats.distinct_device_hashes,
                "scan_interval_ms": ctx.scan_interval_ms, "scan_window_ms": ctx.scan_window_ms,
                "passive": true, "duplicate_filtering": false, "hash_scope": hash_scope,
                "salt": salt,
                "raw_addresses_stored": false, "raw_payload_stored": false,
                "csid_version": crate::VERSION
            });
            let tmp = dir.join("session.json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&seal)?)?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(tmp, dir.join("session.json"))?;
            // The handoff to blescan-sync; `monad_ble:sessions_sealed:rate1h`
            // counts this line.
            tracing::info!(
                session_id = %ctx.session_id,
                rows = stats.rows,
                devices_total = stats.distinct_device_hashes,
                "BLE segment sealed"
            );
            Ok(())
        }).context("starting BLE export")
    }

    fn join_export(&mut self, require_finished: bool) -> Result<()> {
        if require_finished {
            anyhow::ensure!(
                self.exporting.as_ref().is_none_or(|h| h.is_finished()),
                "BLE exporter exceeded one segment; raw logs retained"
            );
        }
        if let Some(h) = self.exporting.take() {
            h.join()
                .map_err(|_| anyhow::anyhow!("BLE exporter panicked"))??;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.join_export(false)?;
        self.exporting = Some(self.seal_async()?);
        self.join_export(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SEG: u64 = 60;
    /// A segment boundary well inside the valid clock range.
    const T0_NS: u64 = (1_790_000_040 / SEG) * SEG * 1_000_000_000;

    fn adv() -> RawAdv {
        RawAdv {
            event_type: 0,
            addr_type: 0,
            addr: [1, 2, 3, 4, 5, 6],
            rssi: -50,
            data: vec![],
        }
    }

    fn row(dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(ble::NDJSON_NAME)).unwrap();
        serde_json::from_str(text.lines().next().unwrap()).unwrap()
    }

    fn seal(dir: &Path) -> serde_json::Value {
        assert!(dir.join(ble::PARQUET_NAME).is_file());
        serde_json::from_str(&std::fs::read_to_string(dir.join("session.json")).unwrap()).unwrap()
    }

    fn tmp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("csid-ble-{tag}-{}", now_unix_ns()))
    }

    const DAY_NS: u64 = 86_400 * 1_000_000_000;

    /// A chain seeded with `byte` on the day of `T0_NS`, as Ansible seeds a node.
    fn chain(byte: u8) -> Arc<Mutex<FleetKeyChain>> {
        chain_on(byte, ble::utc_day(T0_NS))
    }

    fn chain_on(byte: u8, day: u64) -> Arc<Mutex<FleetKeyChain>> {
        Arc::new(Mutex::new(FleetKeyChain::in_memory(
            day,
            ble::FleetKey::from_bytes([byte; 32]),
        )))
    }

    fn key_id_of(byte: u8) -> String {
        ble::FleetKey::from_bytes([byte; 32]).id()
    }

    #[test]
    fn rotation_follows_the_frame_timestamp_and_changes_the_pseudonym() {
        let root = tmp_root("rotation");
        let cfg = BleConfig::default();
        let mut log =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-a", chain(5), T0_NS + 1).unwrap();
        let first = root.join(&log.context.session_id);
        log.observe(&adv(), T0_NS + 2, None).unwrap();
        log.observe(&adv(), T0_NS + SEG * 1_000_000_000 - 1, None)
            .unwrap();
        assert!(
            !first.join("session.json").exists(),
            "an open segment must not ship"
        );

        // The first frame of the next wall-clock segment rotates before it is hashed.
        log.observe(&adv(), T0_NS + SEG * 1_000_000_000, None)
            .unwrap();
        let second = root.join(&log.context.session_id);
        assert_ne!(first, second);
        log.finish().unwrap();

        assert_ne!(row(&first)["device_hash"], row(&second)["device_hash"]);
        let (s1, s2) = (seal(&first), seal(&second));
        assert_eq!(s1["schema"], SEAL_SCHEMA);
        assert_eq!(s1["hash_scope"], "fleet-segment");
        assert_eq!(
            s1["observations"], 2,
            "both frames of segment one stay in segment one"
        );
        assert_eq!(s2["observations"], 1);
        let i1 = s1["salt"]["segment_index"].as_u64().unwrap();
        assert_eq!(s2["salt"]["segment_index"].as_u64().unwrap(), i1 + 1);
        assert_eq!(s1["salt"]["key_id"], key_id_of(5));
        assert_eq!(s1["salt"]["key_day"], ble::utc_day(T0_NS));
        assert_eq!(s1["salt"]["persisted"], false);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_nodes_with_one_key_share_the_pseudonym_inside_a_segment() {
        let root = tmp_root("fleet");
        let cfg = BleConfig::default();
        // Two nodes, each with its own copy of the same seed, booted apart.
        let mut a =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-a", chain(5), T0_NS + 3).unwrap();
        let mut b =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-b", chain(5), T0_NS + 41_000_000_000)
                .unwrap();
        let (da, db) = (
            root.join(&a.context.session_id),
            root.join(&b.context.session_id),
        );
        a.observe(&adv(), T0_NS + 42_000_000_000, None).unwrap();
        b.observe(&adv(), T0_NS + 43_000_000_000, None).unwrap();
        a.finish().unwrap();
        b.finish().unwrap();
        assert_eq!(row(&da)["device_hash"], row(&db)["device_hash"]);

        let mut c =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-c", chain(6), T0_NS + 3).unwrap();
        let dc = root.join(&c.context.session_id);
        c.observe(&adv(), T0_NS + 44_000_000_000, None).unwrap();
        c.finish().unwrap();
        assert_ne!(row(&da)["device_hash"], row(&dc)["device_hash"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn midnight_moves_every_node_to_the_next_days_key() {
        let root = tmp_root("midnight");
        let cfg = BleConfig::default();
        let day = ble::utc_day(T0_NS);
        let midnight = (day + 1) * DAY_NS;
        let (ca, cb) = (chain(5), chain(5));
        let mut a =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-a", ca.clone(), midnight - 1).unwrap();
        let mut b = ContinuousLog::open_at(&root, &cfg, SEG, "node-b", cb, midnight - 2).unwrap();
        let a_before = root.join(&a.context.session_id);
        a.observe(&adv(), midnight - 1, None).unwrap();
        a.observe(&adv(), midnight, None).unwrap();
        b.observe(&adv(), midnight + 1, None).unwrap();
        let (a_after, b_after) = (
            root.join(&a.context.session_id),
            root.join(&b.context.session_id),
        );
        a.finish().unwrap();
        b.finish().unwrap();

        let (before, after) = (seal(&a_before), seal(&a_after));
        assert_eq!(before["salt"]["key_day"], day);
        assert_eq!(after["salt"]["key_day"], day + 1);
        assert_ne!(before["salt"]["key_id"], after["salt"]["key_id"]);
        assert_eq!(after["salt"]["key_id"], seal(&b_after)["salt"]["key_id"]);
        assert_eq!(row(&a_after)["device_hash"], row(&b_after)["device_hash"]);
        // The chain itself moved on: yesterday's key is gone.
        let mut ca = ca.lock().unwrap();
        assert_eq!(ca.day(), day + 1);
        assert!(ca.key_for_day(day).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_clock_before_the_keys_day_gets_a_random_salt_and_says_so() {
        let root = tmp_root("past");
        let cfg = BleConfig::default();
        // Seeded for tomorrow: this node booted in the past (no RTC).
        let tomorrow = ble::utc_day(T0_NS) + 1;
        let mut a =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-a", chain_on(5, tomorrow), T0_NS)
                .unwrap();
        let mut b =
            ContinuousLog::open_at(&root, &cfg, SEG, "node-b", chain_on(5, tomorrow), T0_NS)
                .unwrap();
        let (da, db) = (
            root.join(&a.context.session_id),
            root.join(&b.context.session_id),
        );
        a.observe(&adv(), T0_NS + 1, None).unwrap();
        b.observe(&adv(), T0_NS + 1, None).unwrap();
        a.finish().unwrap();
        b.finish().unwrap();
        let s = seal(&da);
        assert_eq!(s["hash_scope"], "segment");
        assert_eq!(s["salt"]["scheme"], "random");
        assert!(s["salt"].get("key_id").is_none());
        assert_ne!(
            row(&da)["device_hash"],
            row(&db)["device_hash"],
            "no accidental join"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_clock_step_rotates_the_segment() {
        let root = tmp_root("step");
        let cfg = BleConfig::default();
        // chrony steps the node forward a day in the middle of a segment.
        let mut log = ContinuousLog::open_at(&root, &cfg, SEG, "node-a", chain(5), T0_NS).unwrap();
        let before = root.join(&log.context.session_id);
        log.observe(&adv(), T0_NS + 1, None).unwrap();
        log.observe(&adv(), T0_NS + DAY_NS, None).unwrap();
        let after = root.join(&log.context.session_id);
        log.finish().unwrap();
        assert_ne!(before, after);
        assert_eq!(seal(&before)["observations"], 1);
        assert_eq!(seal(&after)["salt"]["key_day"], ble::utc_day(T0_NS) + 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_segment_that_is_short_or_straddles_midnight_is_refused() {
        let root = tmp_root("short");
        let open = |s: u64| {
            ContinuousLog::open(
                &root,
                &BleConfig::default(),
                Duration::from_secs(s),
                "n",
                FleetKeyChain::in_memory(1, ble::FleetKey::from_bytes([5u8; 32])),
            )
            .err()
            .unwrap()
            .to_string()
        };
        assert!(open(59).contains("60 seconds"));
        assert!(
            open(420).contains("divide a day"),
            "7 min does not divide 24 h"
        );
    }
}
