//! Rotating BLE-only archive. Rotation changes neither the HCI socket nor scan
//! parameters. Closed logs export on one background thread; the seal appears
//! only after parquet is durable. A lagging exporter fails loudly with raw logs
//! intact, instead of building an unbounded queue.
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ble::{self, DeviceHasher, LabMatcher, ObservationLog, ParquetContext, RawAdv};
use crate::config::BleConfig;
use crate::util::{now_unix_ns, rfc3339_utc};
use anyhow::{Context, Result};

pub struct ContinuousLog {
    root: PathBuf,
    cfg: BleConfig,
    segment: Duration,
    opened: Instant,
    started_ns: u64,
    log: Option<ObservationLog>,
    hasher: DeviceHasher,
    context: ParquetContext,
    exporting: Option<JoinHandle<Result<()>>>,
}

impl ContinuousLog {
    pub fn open(root: &Path, cfg: &BleConfig, segment: Duration, host: &str) -> Result<Self> {
        let started_ns = now_unix_ns();
        let context = ParquetContext {
            host: host.to_string(),
            session_id: format!("{host}_ble-continuous_{started_ns}"),
            adapter: cfg.adapter.clone(),
            lab_namespace_uuid: cfg.lab_matcher()?.map(|m| m.namespace().to_string()),
            scan_interval_ms: cfg.scan_interval_ms,
            scan_window_ms: cfg.scan_window_ms,
            hash_bytes: cfg.hash_bytes,
        };
        let dir = root.join(&context.session_id);
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            root: root.to_owned(),
            cfg: cfg.clone(),
            segment,
            opened: Instant::now(),
            started_ns,
            log: Some(ObservationLog::create(&dir, cfg.flush_every)?),
            hasher: DeviceHasher::new_random(cfg.hash_bytes)?,
            context,
            exporting: None,
        })
    }

    pub fn observe(&mut self, adv: &RawAdv, ts: u64, matcher: Option<&LabMatcher>) -> Result<()> {
        self.log
            .as_mut()
            .context("BLE log already closed")?
            .append(&self.hasher.observe(adv, ts, matcher))?;
        Ok(())
    }

    pub fn rotate_if_due(&mut self) -> Result<()> {
        if self.opened.elapsed() < self.segment {
            return Ok(());
        }
        self.join_export(true)?;
        let next = Self::open(&self.root, &self.cfg, self.segment, &self.context.host)?;
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
        std::thread::Builder::new().name("ble-export".into()).spawn(move || {
            let dir = path.parent().context("BLE log parent")?;
            let stats = ble::export_parquet(&path, &dir.join(ble::PARQUET_NAME), &ctx)?;
            std::fs::File::open(dir.join(ble::PARQUET_NAME))?.sync_all()?;
            let seal = serde_json::json!({
                "schema": "ble-continuous/1", "observations_schema": "ble-rssi/2",
                "session_id": ctx.session_id, "host": ctx.host, "adapter": ctx.adapter,
                "started_utc": rfc3339_utc(started_ns / 1_000_000_000),
                "ended_utc": rfc3339_utc(ended_ns / 1_000_000_000),
                "started_unix_ns": started_ns, "ended_unix_ns": ended_ns,
                "observations": stats.rows, "distinct_device_hashes": stats.distinct_device_hashes,
                "scan_interval_ms": ctx.scan_interval_ms, "scan_window_ms": ctx.scan_window_ms,
                "passive": true, "duplicate_filtering": false, "hash_scope": "segment",
                "raw_addresses_stored": false, "raw_payload_stored": false,
                "csid_version": crate::VERSION
            });
            let tmp = dir.join("session.json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&seal)?)?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(tmp, dir.join("session.json"))?;
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
    #[test]
    fn rotation_seals_both_segments_and_changes_the_pseudonym() {
        let root = std::env::temp_dir().join(format!("csid-ble-rotation-{}", now_unix_ns()));
        let cfg = BleConfig::default();
        let mut log =
            ContinuousLog::open(&root, &cfg, Duration::from_secs(60), "test-node").unwrap();
        let adv = RawAdv {
            event_type: 0,
            addr_type: 0,
            addr: [1, 2, 3, 4, 5, 6],
            rssi: -50,
            data: vec![],
        };
        let first = root.join(&log.context.session_id);
        log.observe(&adv, now_unix_ns(), None).unwrap();
        assert!(
            !first.join("session.json").exists(),
            "an open segment must not ship"
        );
        log.opened -= Duration::from_secs(61);
        log.rotate_if_due().unwrap();
        let second = root.join(&log.context.session_id);
        log.observe(&adv, now_unix_ns(), None).unwrap();
        log.finish().unwrap();
        let read = |dir: &Path| -> serde_json::Value {
            assert!(dir.join("session.json").is_file());
            assert!(dir.join(ble::PARQUET_NAME).is_file());
            serde_json::from_str(
                std::fs::read_to_string(dir.join(ble::NDJSON_NAME))
                    .unwrap()
                    .trim(),
            )
            .unwrap()
        };
        assert_ne!(read(&first)["device_hash"], read(&second)["device_hash"]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
