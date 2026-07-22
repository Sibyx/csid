//! Live-stream ingest: bind the transport, decode CSIQ datagrams, push samples.
//!
//! Runs on a plain OS thread rather than a tokio task for the same reason
//! `csid`'s RX thread does: the work per datagram is a decode and a push, and
//! the loop wants a predictable stack, not a work-stealing executor.
//!
//! ## One subscriber per socket
//!
//! A Unix **datagram** socket has exactly one owner: the process that binds the
//! path. While `csiscope` is bound to `/run/csid/live.sock`, `csid stream`
//! cannot also attach — they are alternative subscribers, not concurrent ones.
//! To watch from a second machine (or alongside the CLI), configure the
//! experiment's `[stream] transport = "udp"` with a target and point csiscope
//! at `--udp-bind`.

use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::state::{now_ns, Hub, Sample};

/// Receive buffer. The largest record `csid` can emit is 1992 tones × 4 chains
/// × 4 bytes ≈ 128 KiB of I/Q plus TLV overhead; 256 KiB is comfortable and
/// matches the `csid stream` subscriber.
const BUF: usize = 256 * 1024;

/// Where the live stream comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Bind a Unix datagram socket at this path (the `csid` v1 default).
    Unix(std::path::PathBuf),
    /// Bind UDP and accept datagrams from any `csid` targeting this address.
    Udp(String),
}

impl Source {
    /// Human-readable label, shown in the console header.
    pub fn label(&self) -> String {
        match self {
            Source::Unix(p) => format!("unix:{}", p.display()),
            Source::Udp(a) => format!("udp:{a}"),
        }
    }
}

/// Spawn the ingest thread. Returns once the socket is bound, so a bind failure
/// is reported at startup rather than silently as "no data".
pub fn spawn(source: Source, hub: Arc<Hub>) -> Result<()> {
    match source {
        Source::Unix(path) => spawn_unix(path, hub),
        Source::Udp(addr) => spawn_udp(addr, hub),
    }
}

#[cfg(unix)]
fn spawn_unix(path: std::path::PathBuf, hub: Arc<Hub>) -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A datagram receiver must own the path. A leftover socket file from a
    // previous run would make bind fail with EADDRINUSE even though nothing
    // holds it.
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }
    let sock = UnixDatagram::bind(&path).with_context(|| {
        format!(
            "binding {} — is `csid stream` already attached? \
             (a Unix datagram socket has exactly one subscriber)",
            path.display()
        )
    })?;
    // The daemon runs as root and csiscope may not; let any local consumer in.
    // This socket carries CSI, not credentials, and the console is explicitly
    // unauthenticated.
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666));

    tracing::info!(path = %path.display(), "live ingest bound (unix datagram)");
    std::thread::Builder::new()
        .name("csiscope-ingest".into())
        .spawn(move || {
            let mut buf = vec![0u8; BUF];
            loop {
                match sock.recv(&mut buf) {
                    Ok(n) => accept(&hub, &buf[..n]),
                    Err(e) => {
                        tracing::error!(error = %e, "live ingest recv failed; ingest stopping");
                        return;
                    }
                }
            }
        })
        .context("spawning ingest thread")?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_unix(_path: std::path::PathBuf, _hub: Arc<Hub>) -> Result<()> {
    anyhow::bail!("Unix-datagram ingest is not available on this platform; use --udp-bind")
}

fn spawn_udp(addr: String, hub: Arc<Hub>) -> Result<()> {
    let sock = UdpSocket::bind(&addr).with_context(|| format!("binding UDP {addr}"))?;
    tracing::info!(addr, "live ingest bound (udp)");
    std::thread::Builder::new()
        .name("csiscope-ingest".into())
        .spawn(move || {
            let mut buf = vec![0u8; BUF];
            loop {
                match sock.recv(&mut buf) {
                    Ok(n) => accept(&hub, &buf[..n]),
                    Err(e) => {
                        tracing::error!(error = %e, "live ingest recv failed; ingest stopping");
                        return;
                    }
                }
            }
        })
        .context("spawning ingest thread")?;
    Ok(())
}

/// Decode one datagram and push it, maintaining the sequence/session bookkeeping.
fn accept(hub: &Hub, bytes: &[u8]) {
    hub.counters
        .bytes
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);

    let dg = match csiq::live::decode(bytes) {
        Ok(d) => d,
        Err(e) => {
            let n = hub.counters.decode_errors.fetch_add(1, Ordering::Relaxed);
            // One line per decode failure would drown the journal at 600 Hz.
            if n.is_power_of_two() {
                tracing::warn!(error = %e, bytes = bytes.len(), count = n + 1, "undecodable live datagram");
            }
            return;
        }
    };

    // The `ftm` clock wraps every ~13.42 s and the unwrapper is stateful, so it
    // lives here — the one place that sees every record in arrival order.
    let (ftm_ticks, gap, session_change) = {
        let mut st = TRACKER.lock().expect("ingest tracker poisoned");
        st.observe(dg.session_uid, dg.seq, dg.record.ftm)
    };
    if gap > 0 {
        hub.counters.sender_gaps.fetch_add(gap, Ordering::Relaxed);
    }
    if session_change {
        hub.counters.session_changes.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            session_uid = dg.session_uid,
            "new capture session on the wire"
        );
    }

    hub.push(Sample {
        session_uid: dg.session_uid,
        seq: dg.seq,
        ftm_ticks,
        recv_ns: now_ns(),
        rec: Arc::new(dg.record),
    });
}

/// Per-stream continuity state. A `Mutex` is honest here: there is exactly one
/// ingest thread, so it is never contended, and it keeps the unwrapper's state
/// out of the read-mostly `Hub`.
static TRACKER: std::sync::Mutex<Tracker> = std::sync::Mutex::new(Tracker::new());

struct Tracker {
    session_uid: u64,
    last_seq: Option<u32>,
    ftm_last: Option<u32>,
    ftm_wraps: u64,
}

impl Tracker {
    const fn new() -> Self {
        Tracker {
            session_uid: 0,
            last_seq: None,
            ftm_last: None,
            ftm_wraps: 0,
        }
    }

    /// Returns `(unwrapped ftm ticks, sender-side gap, session changed)`.
    fn observe(&mut self, session_uid: u64, seq: u32, ftm: u32) -> (u64, u64, bool) {
        let changed = session_uid != self.session_uid;
        if changed {
            // A restarted capture resets both the datagram counter and the
            // baseband clock; carrying either across would fabricate a gap and
            // a 13-second time jump.
            self.session_uid = session_uid;
            self.last_seq = None;
            self.ftm_last = None;
            self.ftm_wraps = 0;
        }

        let gap = match self.last_seq {
            Some(prev) => seq.wrapping_sub(prev.wrapping_add(1)) as u64,
            None => 0,
        };
        self.last_seq = Some(seq);

        if let Some(prev) = self.ftm_last {
            if ftm < prev {
                self.ftm_wraps += 1;
            }
        }
        self.ftm_last = Some(ftm);
        let ticks = self.ftm_wraps * (1u64 << 32) + ftm as u64;

        (ticks, gap, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_unwraps_and_counts_gaps() {
        let mut t = Tracker::new();
        let (ticks, gap, changed) = t.observe(7, 0, u32::MAX - 5);
        assert_eq!(ticks, (u32::MAX - 5) as u64);
        assert_eq!(gap, 0);
        assert!(changed, "the first datagram establishes the session");

        // Two datagrams later (one dropped) and past the ftm wrap.
        let (ticks, gap, changed) = t.observe(7, 2, 100);
        assert_eq!(ticks, (1u64 << 32) + 100);
        assert_eq!(gap, 1);
        assert!(!changed);
    }

    #[test]
    fn tracker_resets_across_sessions() {
        let mut t = Tracker::new();
        t.observe(1, 900, 4_000_000_000);
        let (ticks, gap, changed) = t.observe(2, 0, 12);
        assert_eq!(ticks, 12, "a new session restarts the baseband clock");
        assert_eq!(gap, 0, "a session change is not a dropped datagram");
        assert!(changed);
    }
}
