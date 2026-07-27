//! Per-session record writing and the sidecar.
//!
//! A session's on-disk shape deliberately mirrors a `csid` capture session, because the fleet's
//! sync and prune machinery keys on that shape: a directory containing `metadata.json` whose
//! `status` field gates shipping. Matching it means the collector inherits a working, tested
//! upload path instead of inventing a second one.
//!
//! ```text
//! <spool>/<session-uuid>/
//!   arrivals.tsv    one row per received datagram, kernel arrival stamp first
//!   exchanges.tsv   one row per answered clock exchange
//!   metadata.json   the sidecar; its `status` is what makes the session shippable
//! ```

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::proto::Hello;

pub const SCHEMA: &str = "monad-collector-session/1";

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Receiving. Not shippable — the sync script skips it, which is what prevents a half-written
    /// session being uploaded and then treated as complete.
    Receiving,
    Complete,
    Stopped,
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub schema: String,
    pub session_id: String,
    pub identity: Identity,
    pub environment: Environment,
    pub lifecycle: Lifecycle,
    pub summary: Summary,
    pub status: Status,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Identity {
    pub participant_id: String,
    pub site: String,
    pub ap_id: String,
    pub platform: String,
    pub app_version: String,
    pub peer: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Environment {
    pub node: String,
    pub collector_version: String,
    /// False when arrival stamps came from userspace. Load-bearing for anyone reading the
    /// inter-arrival statistics: a userspace-stamped session measures the scheduler as much as the
    /// channel, and must not be pooled with kernel-stamped ones.
    pub kernel_timestamps: bool,
    pub bind: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Lifecycle {
    pub first_packet_unix_ns: u64,
    pub last_packet_unix_ns: u64,
    pub closed_unix_ns: u64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Summary {
    pub commanded_rate_hz: f64,
    /// Packets actually received, which is the number the illuminator contract turns on.
    pub packets: u64,
    /// Gaps in the phone's sequence numbers — packets it says it sent and we never saw.
    pub lost: u64,
    pub duplicates: u64,
    pub delivered_rate_hz: f64,
    pub mean_interval_ms: f64,
    pub interval_cv: f64,
    pub max_gap_ms: f64,
    pub clock_exchanges: u64,
    pub bytes: u64,
}

/// A session being written.
pub struct Session {
    pub dir: PathBuf,
    pub uuid: String,
    arrivals: BufWriter<File>,
    exchanges: BufWriter<File>,
    identity: Identity,
    environment: Environment,
    first_ns: u64,
    last_ns: u64,
    packets: u64,
    bytes: u64,
    exchange_count: u64,
    highest_seq: Option<u32>,
    seen_seq: u64,
    duplicates: u64,
    last_arrival_ns: u64,
    interval_count: u64,
    interval_sum: f64,
    interval_sum_sq: f64,
    max_gap_ns: u64,
    commanded_rate_hz: f64,
}

impl Session {
    pub fn create(
        spool: &Path,
        uuid: &str,
        peer: SocketAddr,
        environment: Environment,
    ) -> Result<Self> {
        let dir = spool.join(uuid);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;

        let mut arrivals = BufWriter::new(File::create(dir.join("arrivals.tsv"))?);
        writeln!(
            arrivals,
            "arrival_unix_ns\tsequence\tphone_mono_ns\tphone_wall_ms\tbytes\tkernel_stamped"
        )?;
        let mut exchanges = BufWriter::new(File::create(dir.join("exchanges.tsv"))?);
        writeln!(exchanges, "t1_phone_mono_ns\tt2_unix_ns\tt3_unix_ns\tsequence")?;

        let mut identity = Identity::default();
        identity.peer = peer.to_string();

        let mut session = Self {
            dir,
            uuid: uuid.to_string(),
            arrivals,
            exchanges,
            identity,
            environment,
            first_ns: 0,
            last_ns: 0,
            packets: 0,
            bytes: 0,
            exchange_count: 0,
            highest_seq: None,
            seen_seq: 0,
            duplicates: 0,
            last_arrival_ns: 0,
            interval_count: 0,
            interval_sum: 0.0,
            interval_sum_sq: 0.0,
            max_gap_ns: 0,
            commanded_rate_hz: 0.0,
        };
        // Write the sidecar immediately in `receiving` state: a session that dies mid-flight then
        // still leaves an explanation on disk rather than an unlabelled directory.
        session.write_sidecar(Status::Receiving)?;
        Ok(session)
    }

    pub fn apply_hello(&mut self, hello: &Hello) {
        if !hello.participant_id.is_empty() {
            self.identity.participant_id = hello.participant_id.clone();
        }
        if !hello.site.is_empty() {
            self.identity.site = hello.site.clone();
        }
        if !hello.ap_id.is_empty() {
            self.identity.ap_id = hello.ap_id.clone();
        }
        if !hello.platform.is_empty() {
            self.identity.platform = hello.platform.clone();
        }
        if !hello.app_version.is_empty() {
            self.identity.app_version = hello.app_version.clone();
        }
        if hello.commanded_rate_hz > 0.0 {
            self.commanded_rate_hz = hello.commanded_rate_hz;
        }
    }

    pub fn record_data(
        &mut self,
        arrival_ns: u64,
        sequence: u32,
        phone_mono_ns: u64,
        phone_wall_ms: u64,
        bytes: usize,
        kernel_stamped: bool,
    ) -> Result<()> {
        writeln!(
            self.arrivals,
            "{arrival_ns}\t{sequence}\t{phone_mono_ns}\t{phone_wall_ms}\t{bytes}\t{}",
            if kernel_stamped { 1 } else { 0 }
        )?;

        if self.first_ns == 0 {
            self.first_ns = arrival_ns;
        }
        self.last_ns = arrival_ns;
        self.packets += 1;
        self.bytes += bytes as u64;

        // Loss is inferred from the phone's own sequence numbering rather than from timing: the
        // phone knows what it emitted, and the difference is exactly what the channel dropped.
        match self.highest_seq {
            None => self.highest_seq = Some(sequence),
            Some(h) if sequence > h => self.highest_seq = Some(sequence),
            Some(_) => self.duplicates += 1,
        }
        self.seen_seq += 1;

        if self.last_arrival_ns != 0 {
            let gap = (arrival_ns - self.last_arrival_ns) as f64;
            self.interval_count += 1;
            self.interval_sum += gap;
            self.interval_sum_sq += gap * gap;
            if gap as u64 > self.max_gap_ns {
                self.max_gap_ns = gap as u64;
            }
        }
        self.last_arrival_ns = arrival_ns;
        Ok(())
    }

    pub fn record_exchange(&mut self, t1: u64, t2: u64, t3: u64, sequence: u32) -> Result<()> {
        writeln!(self.exchanges, "{t1}\t{t2}\t{t3}\t{sequence}")?;
        self.exchange_count += 1;
        Ok(())
    }

    pub fn idle_for(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.last_ns.max(self.first_ns))
    }

    pub fn summary(&self) -> Summary {
        let elapsed_s = if self.last_ns > self.first_ns {
            (self.last_ns - self.first_ns) as f64 / 1e9
        } else {
            0.0
        };
        let mean = if self.interval_count > 0 {
            self.interval_sum / self.interval_count as f64
        } else {
            0.0
        };
        let variance = if self.interval_count > 1 {
            (self.interval_sum_sq / self.interval_count as f64 - mean * mean).max(0.0)
        } else {
            0.0
        };
        let expected = self.highest_seq.map(|h| h as u64 + 1).unwrap_or(0);
        Summary {
            commanded_rate_hz: self.commanded_rate_hz,
            packets: self.packets,
            lost: expected.saturating_sub(self.seen_seq - self.duplicates),
            duplicates: self.duplicates,
            delivered_rate_hz: if elapsed_s > 0.0 {
                self.packets as f64 / elapsed_s
            } else {
                0.0
            },
            mean_interval_ms: mean / 1e6,
            interval_cv: if mean > 0.0 { variance.sqrt() / mean } else { 0.0 },
            max_gap_ms: self.max_gap_ns as f64 / 1e6,
            clock_exchanges: self.exchange_count,
            bytes: self.bytes,
        }
    }

    pub fn write_sidecar(&mut self, status: Status) -> Result<()> {
        let sidecar = Sidecar {
            schema: SCHEMA.to_string(),
            session_id: self.uuid.clone(),
            identity: Identity {
                participant_id: self.identity.participant_id.clone(),
                site: self.identity.site.clone(),
                ap_id: self.identity.ap_id.clone(),
                platform: self.identity.platform.clone(),
                app_version: self.identity.app_version.clone(),
                peer: self.identity.peer.clone(),
            },
            environment: Environment {
                node: self.environment.node.clone(),
                collector_version: self.environment.collector_version.clone(),
                kernel_timestamps: self.environment.kernel_timestamps,
                bind: self.environment.bind.clone(),
            },
            lifecycle: Lifecycle {
                first_packet_unix_ns: self.first_ns,
                last_packet_unix_ns: self.last_ns,
                closed_unix_ns: if status == Status::Receiving {
                    0
                } else {
                    crate::rx::now_unix_ns()
                },
            },
            summary: self.summary(),
            status,
        };

        // Write-then-rename: the sync script reads `metadata.json` on a timer, and must never see
        // a half-written file whose status happens to parse as complete.
        let tmp = self.dir.join("metadata.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&sidecar)?)?;
        fs::rename(tmp, self.dir.join("metadata.json"))?;
        Ok(())
    }

    pub fn close(&mut self, status: Status) -> Result<()> {
        self.arrivals.flush()?;
        self.exchanges.flush()?;
        self.write_sidecar(status)?;
        Ok(())
    }

    pub fn participant(&self) -> &str {
        if self.identity.participant_id.is_empty() {
            "unknown-participant"
        } else {
            &self.identity.participant_id
        }
    }
}

/// Sessions currently being received, keyed by session UUID.
#[derive(Default)]
pub struct SessionTable {
    inner: HashMap<String, Session>,
}

impl SessionTable {
    pub fn get_or_create(
        &mut self,
        spool: &Path,
        uuid: &str,
        peer: SocketAddr,
        environment: &Environment,
    ) -> Result<&mut Session> {
        if !self.inner.contains_key(uuid) {
            let session = Session::create(
                spool,
                uuid,
                peer,
                Environment {
                    node: environment.node.clone(),
                    collector_version: environment.collector_version.clone(),
                    kernel_timestamps: environment.kernel_timestamps,
                    bind: environment.bind.clone(),
                },
            )?;
            tracing::info!(session = uuid, peer = %peer, "session opened");
            self.inner.insert(uuid.to_string(), session);
        }
        Ok(self.inner.get_mut(uuid).expect("just inserted"))
    }

    /// Close and drop sessions that have gone quiet.
    ///
    /// A phone never announces the end of a session — it may run out of battery, walk out of
    /// range, or be killed by the OS — so quiet is the only end-of-session signal there is.
    pub fn expire(&mut self, now_ns: u64, idle_timeout_ns: u64) -> Vec<(String, Summary)> {
        let expired: Vec<String> = self
            .inner
            .iter()
            .filter(|(_, s)| s.idle_for(now_ns) > idle_timeout_ns)
            .map(|(k, _)| k.clone())
            .collect();

        let mut closed = Vec::new();
        for key in expired {
            if let Some(mut session) = self.inner.remove(&key) {
                let summary = session.summary();
                if let Err(e) = session.close(Status::Complete) {
                    tracing::error!(session = %key, error = %e, "closing session failed");
                }
                tracing::info!(
                    session = %key,
                    participant = session.participant(),
                    packets = summary.packets,
                    lost = summary.lost,
                    delivered_hz = summary.delivered_rate_hz,
                    cv = summary.interval_cv,
                    "session closed"
                );
                closed.push((key, summary));
            }
        }
        closed
    }

    pub fn close_all(&mut self, status: Status) {
        for (key, session) in self.inner.iter_mut() {
            if let Err(e) = session.close(status) {
                tracing::error!(session = %key, error = %e, "closing session failed");
            }
        }
        self.inner.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}
