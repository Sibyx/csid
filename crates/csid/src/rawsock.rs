//! One `AF_PACKET` receive socket on the monitor interface, shared by every
//! sibling thread that reads raw 802.11 frames beside the CSI capture.
//!
//! Extracted from `timesync::rx` on 2026-09-04 when the frame census became
//! the second consumer. Two copies of the socket code would have been two
//! copies of the three details below, and the second copy would have lost one
//! of them.
//!
//! ## Three details that would each silently ruin a measurement
//!
//! 1. **`PACKET_OUTGOING` must be visible to the caller.** `AF_PACKET` loops
//!    locally transmitted frames back to *other* `AF_PACKET` sockets, and the
//!    injector is one of those transmitters on this very interface. Every
//!    consumer decides for itself what to do with an own frame, but none of
//!    them may mistake it for a receipt.
//! 2. **`SO_TIMESTAMPNS` where the kernel offers it.** A stamp taken after
//!    `recvmsg` returns carries the scheduler's wake-up jitter. Whether the
//!    option was accepted is decided once at open and reported, so a session
//!    that fell back can say so on every row.
//! 3. **A read must time out.** The stop flag is only observed between reads,
//!    so a blocking read on a silent channel would hold a thread past the
//!    session's own close.

#[cfg(target_os = "linux")]
pub use imp::{Frame, RxSocket, PACKET_OUTGOING};

#[cfg(target_os = "linux")]
mod imp {
    use anyhow::{Context, Result};

    /// `sll_pkttype` for a frame this host transmitted.
    pub const PACKET_OUTGOING: u8 = 4;
    /// Big enough for any 802.11 MPDU plus radiotap.
    pub const FRAME_BUF: usize = 4096;
    /// Socket receive buffer — a shock absorber across a scheduling hiccup.
    const SO_RCVBUF_BYTES: libc::c_int = 4 * 1024 * 1024;
    /// How long a read waits before returning so the stop flag is observed.
    const RECV_TIMEOUT_MS: i64 = 250;

    /// One frame off the wire: its bytes, the `sockaddr_ll` packet type, and
    /// the kernel's receive stamp when `SO_TIMESTAMPNS` gave one.
    pub type Frame<'a> = (&'a [u8], u8, Option<u64>);

    pub struct RxSocket {
        fd: libc::c_int,
        /// Whether `SO_TIMESTAMPNS` was accepted. Decided once, at open.
        pub kernel_stamps: bool,
    }

    impl RxSocket {
        /// Open and bind on `iface`. `what` names the consumer in error text.
        pub fn open(iface: &str, what: &str) -> Result<Self> {
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
                    "opening AF_PACKET socket for {what}: {} (CAP_NET_RAW required)",
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
                    "binding the {what} socket to {iface}: {}",
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

        /// One frame, or `None` on a timeout so the stop flag gets observed.
        pub fn recv<'a>(&self, buf: &'a mut [u8]) -> std::io::Result<Option<Frame<'a>>> {
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
}
