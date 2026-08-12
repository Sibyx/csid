//! The two sinks a session fans out to.
//!
//! **The invariant this module exists to enforce:** the live path can never
//! block, slow, or lose data for the durable path. They are separate channels
//! with separate policies —
//!
//! - [`DurableSink`] is lossless. It receives over an unbounded channel; the
//!   only backpressure that can reach the RX thread is the kernel netlink
//!   receive buffer, which at the measured ~0.5 MB/s ceiling never fills.
//! - [`LiveSink`] is best-effort. It receives over a *bounded* channel whose
//!   producer uses `try_send`, so a slow or absent subscriber shows up as a
//!   rising dropped counter, never as a stalled capture.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::UdpSocket;
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::source::RawCsiMessage;

/// Shared counters, readable by the session loop for logging/metrics.
#[derive(Debug, Default)]
pub struct Counters {
    /// Records durably written.
    pub records: AtomicU64,
    /// Bytes durably written.
    pub bytes: AtomicU64,
    /// Records dropped on the live path (bounded queue full).
    pub live_dropped: AtomicU64,
    /// Live datagrams successfully sent.
    pub live_sent: AtomicU64,
}

impl Counters {
    /// Convenience snapshot: `(records, bytes, live_sent, live_dropped)`.
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.records.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.live_sent.load(Ordering::Relaxed),
            self.live_dropped.load(Ordering::Relaxed),
        )
    }
}

/// Lossless writer of the driver-native raw stream.
///
/// The on-disk framing is byte-compatible with the upstream `iaxcsi` output, so
/// every existing reader (`csi_parse.py`, `csiq::raw`) works unchanged:
///
/// ```text
/// [be32 msg_len][be32 hdr_len][hdr][be32 csi_len][csi]
/// ```
pub struct DurableSink {
    out: BufWriter<File>,
    path: PathBuf,
    counters: Arc<Counters>,
}

impl DurableSink {
    /// Create `capture.raw` inside the session directory.
    pub fn create(dir: &Path, counters: Arc<Counters>) -> Result<Self> {
        let path = dir.join("capture.raw");
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(DurableSink {
            // 1 MiB buffer: two seconds of headroom at the measured ceiling.
            out: BufWriter::with_capacity(1024 * 1024, file),
            path,
            counters,
        })
    }

    /// Append one message in the raw framing.
    pub fn write(&mut self, msg: &RawCsiMessage) -> Result<()> {
        let msg_len = (4 + msg.hdr.len() + 4 + msg.csi.len()) as u32;
        self.out.write_all(&msg_len.to_be_bytes())?;
        self.out.write_all(&(msg.hdr.len() as u32).to_be_bytes())?;
        self.out.write_all(&msg.hdr)?;
        self.out.write_all(&(msg.csi.len() as u32).to_be_bytes())?;
        self.out.write_all(&msg.csi)?;

        self.counters.records.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes
            .fetch_add(4 + msg_len as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Path of the file currently being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Seal the current `capture.raw` and begin a new one under `dir`.
    ///
    /// Returns the path of the file just sealed. The durable stream is
    /// flushed and `fsync`ed before the handle is dropped, so the sealed file
    /// is complete and readable the instant this returns — that is what lets a
    /// sealed segment be exported and uploaded while the capture continues.
    ///
    /// This is the only blocking work on the durable thread, and it is
    /// deliberately kept to flush + fsync + create: the expensive part
    /// (CSIQ export) happens on the sealer thread. Records that arrive during
    /// the rotation queue in the unbounded durable channel and drain
    /// immediately after, so a rotation costs latency, never data.
    pub fn rotate(&mut self, dir: &Path) -> Result<PathBuf> {
        self.out.flush().context("flushing capture.raw before rotation")?;
        self.out
            .get_ref()
            .sync_all()
            .context("fsync on capture.raw before rotation")?;

        let next = dir.join("capture.raw");
        let file = File::create(&next).with_context(|| format!("creating {}", next.display()))?;
        self.out = BufWriter::with_capacity(1024 * 1024, file);
        Ok(std::mem::replace(&mut self.path, next))
    }

    /// Flush and sync to disk. Called once at session close.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.out.flush().context("flushing capture.raw")?;
        self.out
            .get_ref()
            .sync_all()
            .context("fsync on capture.raw")?;
        Ok(self.path)
    }
}

/// Best-effort transport for live CSIQ datagrams.
pub enum LiveTransport {
    /// On-node consumers (the v1 default).
    #[cfg(unix)]
    Unix { sock: UnixDatagram, path: PathBuf },
    /// Opt-in network transport.
    Udp {
        sock: UdpSocket,
        targets: Vec<String>,
    },
}

/// Publishes parsed records as CSIQ live datagrams. Never fatal.
pub struct LiveSink {
    transport: LiveTransport,
    session_uid: u64,
    seq: u32,
    counters: Arc<Counters>,
    /// Suppress repeated "nobody listening" noise in the journal.
    warned: bool,
}

impl LiveSink {
    /// Bind a Unix-domain datagram sender aimed at `path`.
    ///
    /// The socket is *unbound* — we only send. If no consumer has bound the
    /// path yet, sends fail with `ENOENT`/`ECONNREFUSED` and are counted as
    /// drops; a consumer may appear at any time and start receiving.
    #[cfg(unix)]
    pub fn unix(path: &Path, session_uid: u64, counters: Arc<Counters>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let sock = UnixDatagram::unbound().context("creating live Unix datagram socket")?;
        Ok(LiveSink {
            transport: LiveTransport::Unix {
                sock,
                path: path.to_path_buf(),
            },
            session_uid,
            seq: 0,
            counters,
            warned: false,
        })
    }

    /// Unix-domain sockets don't exist here; the caller falls back to counting
    /// the session as stream-disabled.
    #[cfg(not(unix))]
    pub fn unix(_path: &Path, _session_uid: u64, _counters: Arc<Counters>) -> Result<Self> {
        anyhow::bail!(
            "Unix-socket live transport is unavailable on this platform; use `transport = \"udp\"`"
        )
    }

    /// Bind a UDP sender aimed at one or more `host:port` targets.
    pub fn udp(targets: &[String], session_uid: u64, counters: Arc<Counters>) -> Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0").context("binding live UDP socket")?;
        Ok(LiveSink {
            transport: LiveTransport::Udp {
                sock,
                targets: targets.to_vec(),
            },
            session_uid,
            seq: 0,
            counters,
            warned: false,
        })
    }

    /// Encode and publish one record. Failures are counted, never propagated.
    pub fn publish(&mut self, record: &csiq::CsiRecord) {
        let datagram = csiq::live::encode(self.session_uid, self.seq, record);
        self.seq = self.seq.wrapping_add(1);

        let result = match &self.transport {
            #[cfg(unix)]
            LiveTransport::Unix { sock, path } => sock.send_to(&datagram, path).map(|_| ()),
            LiveTransport::Udp { sock, targets } => {
                let mut last = Ok(());
                for t in targets {
                    if let Err(e) = sock.send_to(&datagram, t) {
                        last = Err(e);
                    }
                }
                last
            }
        };

        match result {
            Ok(()) => {
                self.counters.live_sent.fetch_add(1, Ordering::Relaxed);
                self.warned = false;
            }
            Err(e) => {
                self.counters.live_dropped.fetch_add(1, Ordering::Relaxed);
                if !self.warned {
                    tracing::warn!(error = %e, "live publish failing (no subscriber?); counting drops");
                    self.warned = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::RawCsiMessage;

    #[test]
    fn durable_framing_is_readable_by_csiq_raw() {
        let dir = std::env::temp_dir().join(format!("csid-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counters = Arc::new(Counters::default());

        // Minimal but structurally valid header + CSI payload.
        let mut hdr = vec![0u8; 272];
        hdr[46] = 2; // nrx
        hdr[47] = 1; // ntx
        hdr[52..54].copy_from_slice(&52u16.to_le_bytes());
        // ntone(52) x nrx(2) x ntx(1) x I/Q x i16
        let csi = vec![0u8; 52 * 2 * 2 * 2];

        let mut sink = DurableSink::create(&dir, counters.clone()).unwrap();
        sink.write(&RawCsiMessage {
            hdr: hdr.clone(),
            csi: csi.clone(),
            unix_ts_ns: 1,
        })
        .unwrap();
        let path = sink.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let mut rr = csiq::raw::RawReader::new(&bytes[..], csiq::Width::Ht20);
        let rec = rr.next_record().unwrap().unwrap();
        assert_eq!(rec.nrx, 2);
        assert_eq!(rec.ntone, 52);
        assert_eq!(counters.records.load(Ordering::Relaxed), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Build a structurally valid single-record message.
    fn sample_msg() -> RawCsiMessage {
        let mut hdr = vec![0u8; 272];
        hdr[46] = 2; // nrx
        hdr[47] = 1; // ntx
        hdr[52..54].copy_from_slice(&52u16.to_le_bytes());
        RawCsiMessage {
            hdr,
            csi: vec![0u8; 52 * 2 * 2 * 2],
            unix_ts_ns: 1,
        }
    }

    /// The property segment rotation depends on: the file handed back by
    /// `rotate` is closed, complete and parseable *immediately* — that is what
    /// makes it safe to export and upload while the capture keeps running —
    /// and records written afterwards land in the new segment, not the old one.
    #[test]
    fn rotation_seals_a_readable_file_and_keeps_writing_into_the_next() {
        let root = std::env::temp_dir().join(format!("csid-rot-{}", std::process::id()));
        let seg1 = root.join("seg0001");
        let seg2 = root.join("seg0002");
        std::fs::create_dir_all(&seg1).unwrap();
        std::fs::create_dir_all(&seg2).unwrap();
        let counters = Arc::new(Counters::default());

        let mut sink = DurableSink::create(&seg1, counters.clone()).unwrap();
        sink.write(&sample_msg()).unwrap();
        sink.write(&sample_msg()).unwrap();

        let sealed = sink.rotate(&seg2).unwrap();
        assert_eq!(sealed, seg1.join("capture.raw"));
        assert_eq!(sink.path(), seg2.join("capture.raw"));

        // Two records readable from the sealed segment before the session ends.
        let bytes = std::fs::read(&sealed).unwrap();
        let mut rr = csiq::raw::RawReader::new(&bytes[..], csiq::Width::Ht20);
        let mut sealed_count = 0;
        while let Ok(Some(_)) = rr.next_record() {
            sealed_count += 1;
        }
        assert_eq!(sealed_count, 2, "sealed segment must be complete on rotate");

        // The next record goes to the new segment only.
        sink.write(&sample_msg()).unwrap();
        let last = sink.finish().unwrap();
        let bytes2 = std::fs::read(&last).unwrap();
        let mut rr2 = csiq::raw::RawReader::new(&bytes2[..], csiq::Width::Ht20);
        let mut next_count = 0;
        while let Ok(Some(_)) = rr2.next_record() {
            next_count += 1;
        }
        assert_eq!(next_count, 1, "post-rotation records belong to the new segment");

        // The sealed file is untouched by later writes.
        assert_eq!(std::fs::read(&sealed).unwrap().len(), bytes.len());

        // Counters are session-wide, spanning the rotation.
        assert_eq!(counters.records.load(Ordering::Relaxed), 3);

        std::fs::remove_dir_all(&root).ok();
    }
}
