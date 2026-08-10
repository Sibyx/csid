//! The time-transfer receive thread: `AF_PACKET` on the monitor interface.
//!
//! Same split as [`crate::inject`]: everything decidable is in
//! [`super::payload`] and unit-tested on a laptop; only the syscalls live here.
//!
//! ## Three details that would each silently ruin the measurement
//!
//! 1. **`PACKET_OUTGOING` must be dropped.** `AF_PACKET` loops locally
//!    transmitted frames back to *other* `AF_PACKET` sockets, and the injector
//!    is one of those transmitters on this very interface. Without the filter,
//!    an illuminating node "receives" its own frames with a delay of
//!    microseconds while its peers see real air delay — and the inter-node skew
//!    would read as a few hundred microseconds of pure artefact.
//!
//! 2. **`SO_TIMESTAMPNS` where the kernel offers it.** A stamp taken after
//!    `recvmsg` returns carries the scheduler's wake-up jitter, which is the
//!    same order as the quantity being measured (`collectord` learned this the
//!    hard way; its docs say kernel timestamps are "not optional"). The kernel
//!    stamp is taken in the receive path instead. Where the option cannot be
//!    set the userspace stamp is used and **recorded as such per row**, because
//!    a userspace-stamped session must not be pooled with kernel-stamped ones.
//!
//! 3. **This thread must never apply backpressure.** It owns its own socket and
//!    its own file. It shares nothing with the CSI receive path but the stop
//!    flag, so it cannot slow it down, and a failure here is recorded rather
//!    than propagated.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

use super::{TimesyncCounters, TimesyncHandle};
use crate::config::TimesyncConfig;

/// Start the receiver. See [`super::spawn`].
pub fn spawn(
    dir: &Path,
    monitor: &str,
    cfg: &TimesyncConfig,
    stop: Arc<AtomicBool>,
    counters: Arc<TimesyncCounters>,
) -> Result<TimesyncHandle> {
    imp::spawn(dir, monitor, cfg, stop, counters)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use super::super::{payload, Row, RowLog, StampSource, TimesyncCounters, TimesyncHandle};
    use crate::config::TimesyncConfig;
    use crate::util;

    /// `sll_pkttype` for a frame this host transmitted.
    const PACKET_OUTGOING: u8 = 4;
    /// Big enough for any 802.11 MPDU plus radiotap.
    const FRAME_BUF: usize = 4096;
    /// Socket receive buffer — a shock absorber across a scheduling hiccup.
    const SO_RCVBUF_BYTES: libc::c_int = 4 * 1024 * 1024;
    /// How long a read waits before returning so the stop flag is observed.
    const RECV_TIMEOUT_MS: i64 = 250;

    struct RxSocket {
        fd: libc::c_int,
        /// Whether `SO_TIMESTAMPNS` was accepted. Decided once, at open.
        kernel_stamps: bool,
    }

    impl RxSocket {
        fn open(iface: &str) -> Result<Self> {
            let ifindex = {
                let name = std::ffi::CString::new(iface).context("interface name")?;
                let idx = unsafe { libc::if_nametoindex(name.as_ptr()) };
                if idx == 0 {
                    anyhow::bail!(
                        "monitor interface {iface} not found: {}",
                        std::io::Error::last_os_error()
                    );
                }
                idx
            };

            let fd = unsafe {
                libc::socket(
                    libc::AF_PACKET,
                    libc::SOCK_RAW,
                    (libc::ETH_P_ALL as u16).to_be() as libc::c_int,
                )
            };
            if fd < 0 {
                anyhow::bail!(
                    "opening AF_PACKET socket for time transfer: {} (CAP_NET_RAW required)",
                    std::io::Error::last_os_error()
                );
            }
            let mut sock = RxSocket {
                fd,
                kernel_stamps: false,
            };

            let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            addr.sll_family = libc::AF_PACKET as u16;
            addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
            addr.sll_ifindex = ifindex as libc::c_int;
            let rc = unsafe {
                libc::bind(
                    sock.fd,
                    &addr as *const libc::sockaddr_ll as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                anyhow::bail!(
                    "binding the time-transfer socket to {iface}: {}",
                    std::io::Error::last_os_error()
                );
            }

            sock.set_int(libc::SOL_SOCKET, libc::SO_RCVBUF, SO_RCVBUF_BYTES);
            let tv = libc::timeval {
                tv_sec: RECV_TIMEOUT_MS / 1000,
                tv_usec: (RECV_TIMEOUT_MS % 1000) * 1000,
            };
            unsafe {
                libc::setsockopt(
                    sock.fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &tv as *const libc::timeval as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
            }

            // Decided once, at open, and then recorded on every row.
            sock.kernel_stamps = sock.set_int(libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, 1);
            Ok(sock)
        }

        fn set_int(&self, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> bool {
            let rc = unsafe {
                libc::setsockopt(
                    self.fd,
                    level,
                    name,
                    &value as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            rc == 0
        }

        /// One frame: `(bytes, pkttype, kernel stamp if the kernel gave one)`.
        fn recv<'a>(
            &self,
            buf: &'a mut [u8],
        ) -> std::io::Result<Option<(&'a [u8], u8, Option<u64>)>> {
            let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            let mut cbuf = [0u8; 128];
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            };
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_name = &mut addr as *mut libc::sockaddr_ll as *mut libc::c_void;
            msg.msg_namelen = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cbuf.len() as _;

            let n = unsafe { libc::recvmsg(self.fd, &mut msg, 0) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                return match e.kind() {
                    // The SO_RCVTIMEO expiry — the stop-flag check window.
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Ok(None),
                    _ => Err(e),
                };
            }

            // Walk the control messages for SCM_TIMESTAMPNS.
            let mut stamp = None;
            let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
            while !cmsg.is_null() {
                let hdr = unsafe { &*cmsg };
                if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_TIMESTAMPNS {
                    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            libc::CMSG_DATA(cmsg),
                            &mut ts as *mut libc::timespec as *mut u8,
                            std::mem::size_of::<libc::timespec>(),
                        );
                    }
                    stamp = Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64);
                    break;
                }
                cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
            }

            Ok(Some((&buf[..n as usize], addr.sll_pkttype, stamp)))
        }
    }

    impl Drop for RxSocket {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }

    pub fn spawn(
        dir: &Path,
        monitor: &str,
        cfg: &TimesyncConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<TimesyncCounters>,
    ) -> Result<TimesyncHandle> {
        // Open on the caller's thread so a missing interface or capability
        // fails the session at setup rather than silently mid-run.
        let sock = RxSocket::open(monitor)?;
        if !sock.kernel_stamps {
            tracing::warn!(
                monitor,
                "SO_TIMESTAMPNS refused; time-transfer rows will carry USERSPACE stamps \
                 (scheduler jitter included). They are marked as such and must not be pooled \
                 with kernel-stamped sessions."
            );
        }
        let log = RowLog::create(dir, cfg.flush_every)?;
        let ndjson = dir.join(super::super::NDJSON_NAME);
        let monitor = monitor.to_string();

        let thread = std::thread::Builder::new()
            .name("csid-timesync".into())
            .spawn(move || run_loop(sock, log, &monitor, stop, counters))
            .context("spawning the time-transfer thread")?;
        Ok(TimesyncHandle::new(thread, ndjson))
    }

    fn run_loop(
        sock: RxSocket,
        mut log: RowLog,
        monitor: &str,
        stop: Arc<AtomicBool>,
        counters: Arc<TimesyncCounters>,
    ) {
        let stamp_src = if sock.kernel_stamps {
            StampSource::Kernel
        } else {
            StampSource::Userspace
        };
        tracing::info!(
            monitor,
            kernel_stamps = sock.kernel_stamps,
            "time transfer running"
        );

        let mut buf = vec![0u8; FRAME_BUF];
        let mut last_log = Instant::now();
        let mut last_rows: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            match sock.recv(&mut buf) {
                Ok(None) => {} // read timeout: loop and re-check the stop flag
                Err(e) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, "time-transfer read failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(Some((frame, pkttype, kernel_ns))) => {
                    // Our own injector, looped back. Counting it as a receipt
                    // would fabricate a sub-millisecond skew against every peer.
                    if pkttype == PACKET_OUTGOING {
                        counters.own_transmissions.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    counters.frames_seen.fetch_add(1, Ordering::Relaxed);

                    // Prefer the kernel's stamp; fall back to now.
                    let unix_ts_ns = kernel_ns.unwrap_or_else(util::now_unix_ns);
                    match payload::recognise(frame) {
                        Ok(stamp) => {
                            let row = Row::from_stamp(stamp, unix_ts_ns, stamp_src);
                            counters.note_row(row.tx_kind, unix_ts_ns);
                            if let Err(e) = log.append(&row) {
                                counters.errors.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(error = %e, "time-transfer log append failed");
                            }
                        }
                        Err(r) => counters.note_reject(r),
                    }
                }
            }

            if last_log.elapsed() >= Duration::from_secs(30) {
                let (rows, _) = counters.snapshot();
                let window = last_log.elapsed().as_secs_f64();
                tracing::info!(
                    rows,
                    rate_hz = (rows - last_rows) as f64 / window,
                    frames_seen = counters.frames_seen.load(Ordering::Relaxed),
                    protected = counters.protected.load(Ordering::Relaxed),
                    own_transmissions = counters.own_transmissions.load(Ordering::Relaxed),
                    "time transfer"
                );
                last_rows = rows;
                last_log = Instant::now();
            }
        }

        if let Err(e) = log.finish() {
            tracing::error!(error = %e, "closing the time-transfer log failed");
        }
        let (rows, rate) = counters.snapshot();
        tracing::info!(rows, rate_hz = rate, "time transfer stopped");
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use anyhow::Result;

    use super::super::{TimesyncCounters, TimesyncHandle};
    use crate::config::TimesyncConfig;

    pub fn spawn(
        _dir: &Path,
        _monitor: &str,
        _cfg: &TimesyncConfig,
        _stop: Arc<AtomicBool>,
        _counters: Arc<TimesyncCounters>,
    ) -> Result<TimesyncHandle> {
        anyhow::bail!(
            "[timesync].enabled requires Linux (AF_PACKET on the monitor interface); \
             this build is for development only"
        )
    }
}
