//! Live burst statistic of the illuminator's frames, for the zone map (2026-10-26).
//!
//! The lake decoder reduces every 2-s window to `inj_rssi_sd`: the sample SD of the strongest-chain
//! RSSI of the injector's frames (`02:6d:6f:6e:*`), when at least 100 of them arrived. On the
//! library fleet the upper quantiles of that statistic over a minute track the in-room device count
//! at least as well as the median amplitude level and keep a floor half as noisy against their
//! response (`monad-knowledge`, study `2026-10-26-csi-burst-features`). This module computes the
//! same window statistic on the node, live, and writes the minute's p75 and p90 to a node_exporter
//! textfile, so the fleet's minute-by-minute burst state reaches Mimir without the lake's batch
//! delay.
//!
//! Two deliberate differences from the decoder, both stated here so nobody compares the two as
//! equal: windows are aligned to the Unix epoch (the decoder aligns them to its block grid), and
//! the decoder's further gate on a scored phase step is not applied.
//!
//! ## Why a textfile
//!
//! The same reason as `csid thermal --write`: node_exporter reads the directory on each scrape, the
//! write is an atomic rename, and nothing here can block the RX thread. The writer runs on the live
//! publisher thread, which is already best-effort.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Source-MAC prefix of the fleet's injected frames (`02:6d:6f:6e:GG:NN`).
pub const INJECTOR_PREFIX: [u8; 4] = [0x02, 0x6d, 0x6f, 0x6e];
/// File name inside the textfile directory.
pub const FILE_NAME: &str = "csid_burst.prom";

/// Is this source address one of the fleet's injectors?
pub fn is_injector(mac: &[u8; 6]) -> bool {
    mac[..4] == INJECTOR_PREFIX
}

/// The strongest chain's RSSI in dBm, or `None` when no chain reported (`0` means no measurement).
pub fn rssi_max(rssi: &[i16], nrx: u8) -> Option<i16> {
    let chains = &rssi[..rssi.len().min(nrx as usize)];
    chains.iter().copied().filter(|&v| v != 0).max()
}

/// Sample standard deviation (ddof 1), `None` below two values.
fn sd1(v: &[f64]) -> Option<f64> {
    if v.len() < 2 {
        return None;
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let ss = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>();
    Some((ss / (n - 1.0)).sqrt())
}

/// Linear-interpolation quantile of an unsorted slice (numpy's default, DuckDB's `quantile_cont`).
pub fn quantile(v: &[f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let pos = q.clamp(0.0, 1.0) * (s.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    Some(s[lo] + (s[hi] - s[lo]) * (pos - lo as f64))
}

/// One finished minute.
#[derive(Debug, Clone, PartialEq)]
pub struct Minute {
    /// Unix seconds of the minute's start.
    pub start_s: i64,
    /// Windows that met the frame minimum.
    pub windows: usize,
    pub p75: Option<f64>,
    pub p90: Option<f64>,
}

/// Accumulates injector RSSI per window and window SDs per minute.
pub struct BurstStats {
    window_ns: i64,
    min_frames: usize,
    cur_window: Option<i64>,
    rssi: Vec<f64>,
    cur_minute: Option<i64>,
    sds: Vec<f64>,
}

impl BurstStats {
    pub fn new(window_s: f64, min_frames: usize) -> Self {
        BurstStats {
            window_ns: (window_s * 1e9) as i64,
            min_frames,
            cur_window: None,
            rssi: Vec::new(),
            cur_minute: None,
            sds: Vec::new(),
        }
    }

    fn close_window(&mut self) {
        if self.rssi.len() >= self.min_frames {
            if let Some(sd) = sd1(&self.rssi) {
                self.sds.push(sd);
            }
        }
        self.rssi.clear();
    }

    fn close_minute(&mut self) -> Option<Minute> {
        let start = self.cur_minute?;
        let m = Minute {
            start_s: start * 60,
            windows: self.sds.len(),
            p75: quantile(&self.sds, 0.75),
            p90: quantile(&self.sds, 0.90),
        };
        self.sds.clear();
        Some(m)
    }

    /// Feed one record's wallclock, source and RSSI. Returns the minute that just ended, if any.
    ///
    /// A window belongs to the minute in which it starts, so a 2-s window never straddles two
    /// reported minutes.
    pub fn observe(
        &mut self,
        unix_ts_ns: u64,
        src_mac: &[u8; 6],
        rssi: &[i16],
        nrx: u8,
    ) -> Option<Minute> {
        if !is_injector(src_mac) {
            return None;
        }
        let t = unix_ts_ns as i64;
        let window = t.div_euclid(self.window_ns);
        let minute = (window * self.window_ns).div_euclid(60_000_000_000);
        let mut done = None;
        if self.cur_window != Some(window) {
            if self.cur_window.is_some() {
                self.close_window();
            }
            self.cur_window = Some(window);
            if self.cur_minute != Some(minute) {
                done = self.close_minute();
                self.cur_minute = Some(minute);
            }
        }
        if let Some(r) = rssi_max(rssi, nrx) {
            self.rssi.push(r as f64);
        }
        done
    }
}

/// Render one minute as node_exporter textfile input. Absent quantiles emit no sample.
pub fn prometheus_text(m: &Minute) -> String {
    let mut s = String::new();
    s.push_str("# HELP csid_burst_minute_start_seconds Unix start of the minute the burst samples describe.\n");
    s.push_str("# TYPE csid_burst_minute_start_seconds gauge\n");
    s.push_str(&format!("csid_burst_minute_start_seconds {}\n", m.start_s));
    s.push_str("# HELP csid_burst_windows 2-s windows in the minute with at least the minimum injector frames.\n");
    s.push_str("# TYPE csid_burst_windows gauge\n");
    s.push_str(&format!("csid_burst_windows {}\n", m.windows));
    for (q, v) in [("0.75", m.p75), ("0.9", m.p90)] {
        if let Some(v) = v {
            s.push_str(&format!(
                "# HELP csid_burst_inj_rssi_sd_db Quantile over the minute's 2-s windows of the injector-frame RSSI SD (dB), the decoder's inj_rssi_sd.\n# TYPE csid_burst_inj_rssi_sd_db gauge\ncsid_burst_inj_rssi_sd_db{{quantile=\"{q}\"}} {v:.4}\n"
            ));
        }
    }
    s
}

/// Writes finished minutes to `<dir>/csid_burst.prom` by atomic rename.
pub struct BurstWriter {
    stats: BurstStats,
    path: PathBuf,
}

impl BurstWriter {
    pub fn new(dir: &Path, window_s: f64, min_frames: usize) -> Self {
        BurstWriter {
            stats: BurstStats::new(window_s, min_frames),
            path: dir.join(FILE_NAME),
        }
    }

    pub fn observe(
        &mut self,
        unix_ts_ns: u64,
        src_mac: &[u8; 6],
        rssi: &[i16],
        nrx: u8,
    ) -> Result<()> {
        if let Some(m) = self.stats.observe(unix_ts_ns, src_mac, rssi, nrx) {
            write_atomic(&self.path, &prometheus_text(&m))?;
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("prom.tmp");
    std::fs::write(&tmp, text.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INJ: [u8; 6] = [0x02, 0x6d, 0x6f, 0x6e, 0x01, 0x01];
    const OTHER: [u8; 6] = [0x02, 0x6d, 0x6f, 0x6f, 0x01, 0x01];

    #[test]
    fn quantile_matches_numpy_linear() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile(&v, 0.75), Some(3.25));
        assert_eq!(quantile(&v, 0.9), Some(3.7));
        assert_eq!(quantile(&[], 0.5), None);
    }

    #[test]
    fn rssi_max_ignores_absent_chains_and_extra_entries() {
        assert_eq!(rssi_max(&[-60, 0], 2), Some(-60));
        assert_eq!(rssi_max(&[0, 0], 2), None);
        assert_eq!(rssi_max(&[-70, -55, -10], 2), Some(-55));
    }

    #[test]
    fn a_minute_reports_window_sds_and_skips_thin_windows() {
        let mut b = BurstStats::new(2.0, 3);
        // A minute boundary.
        let base: u64 = 1_800_000_000 * 1_000_000_000;
        // Window 0: RSSI -60, -62, -64 -> sd 2.0. Window 1: two frames only -> skipped.
        for (i, r) in [-60i16, -62, -64].iter().enumerate() {
            assert!(b
                .observe(base + i as u64 * 1_000_000, &INJ, &[*r, 0], 2)
                .is_none());
        }
        for i in 0..2u64 {
            b.observe(base + 2_000_000_000 + i * 1_000_000, &INJ, &[-50, 0], 2);
        }
        // A foreign frame in the next minute changes nothing.
        assert!(b
            .observe(base + 61_000_000_000, &OTHER, &[-40, 0], 2)
            .is_none());
        // The first injector frame of the next minute closes the previous one.
        let m = b
            .observe(base + 60_000_000_000, &INJ, &[-50, 0], 2)
            .expect("minute closed");
        assert_eq!(m.start_s, 1_800_000_000);
        assert_eq!(m.windows, 1);
        assert!((m.p75.unwrap() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn the_text_omits_absent_quantiles_rather_than_writing_zero() {
        let m = Minute {
            start_s: 60,
            windows: 0,
            p75: None,
            p90: None,
        };
        let t = prometheus_text(&m);
        assert!(t.contains("csid_burst_windows 0"));
        assert!(!t.contains("csid_burst_inj_rssi_sd_db{"));
        let m = Minute {
            start_s: 60,
            windows: 30,
            p75: Some(1.5),
            p90: Some(2.0),
        };
        let t = prometheus_text(&m);
        assert!(t.contains("csid_burst_inj_rssi_sd_db{quantile=\"0.75\"} 1.5000"));
        assert!(t.contains("csid_burst_inj_rssi_sd_db{quantile=\"0.9\"} 2.0000"));
    }
}
