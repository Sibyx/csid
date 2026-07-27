//! The receive path: a UDP socket that stamps arrival in the **kernel**.
//!
//! This is the whole reason the collector is a Rust service on the node rather than a script
//! somewhere convenient. The experiment measures inter-arrival regularity and a clock offset at
//! millisecond-or-better resolution; a timestamp taken in userspace after the scheduler has
//! decided to wake the process carries that scheduler's jitter, which is the same magnitude as the
//! quantity being measured. `SO_TIMESTAMPNS` makes the kernel stamp the datagram as it is received
//! and hand the stamp over as a control message, so the number describes the packet rather than
//! the process.
//!
//! On non-Linux hosts the socket falls back to a userspace stamp so the crate still builds and can
//! be exercised on a development machine — the fallback is reported, never silently substituted.

use std::io;
use std::net::{SocketAddr, UdpSocket};

use anyhow::{Context, Result};

/// One received datagram plus the arrival instant.
pub struct Received {
    pub len: usize,
    pub peer: SocketAddr,
    /// Nanoseconds since the Unix epoch, from the kernel where available.
    pub arrival_ns: u64,
    /// False when the stamp came from userspace after the read, which is materially worse.
    pub kernel_stamped: bool,
}

pub struct RxSocket {
    socket: UdpSocket,
    kernel_timestamps: bool,
}

impl RxSocket {
    pub fn bind(bind_addr: &str) -> Result<Self> {
        let socket =
            UdpSocket::bind(bind_addr).with_context(|| format!("binding UDP {bind_addr}"))?;
        // A read timeout keeps the loop responsive to shutdown and to the systemd watchdog even on
        // a silent channel — an experiment AP with nobody on it is the normal idle state.
        socket.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;

        let kernel_timestamps = enable_kernel_timestamps(&socket);
        if !kernel_timestamps {
            tracing::warn!(
                "kernel receive timestamps unavailable; falling back to userspace stamps, \
                 which carry scheduler jitter of the same order as the quantity being measured"
            );
        }

        Ok(Self {
            socket,
            kernel_timestamps,
        })
    }

    pub fn kernel_timestamps(&self) -> bool {
        self.kernel_timestamps
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub fn send_to(&self, bytes: &[u8], peer: SocketAddr) -> Result<usize> {
        Ok(self.socket.send_to(bytes, peer)?)
    }

    /// Receive one datagram. `Ok(None)` on read timeout, which is an ordinary idle outcome.
    pub fn recv(&self, buf: &mut [u8]) -> Result<Option<Received>> {
        match self.recv_stamped(buf) {
            Ok(Some(r)) => Ok(Some(r)),
            Ok(None) => Ok(None),
            Err(e) if is_timeout(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    #[cfg(target_os = "linux")]
    fn recv_stamped(&self, buf: &mut [u8]) -> io::Result<Option<Received>> {
        use std::os::fd::AsRawFd;

        let mut peer_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        // Room for a single SCM_TIMESTAMPNS control message.
        let mut cmsg_space = [0u8; 128];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = &mut peer_storage as *mut _ as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_space.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space.len();

        let n = unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut msg, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut arrival_ns = None;
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg).cmsg_type == libc::SCM_TIMESTAMPNS
                {
                    let ts = std::ptr::read_unaligned(
                        libc::CMSG_DATA(cmsg) as *const libc::timespec
                    );
                    arrival_ns =
                        Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64);
                    break;
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }

        let kernel_stamped = arrival_ns.is_some();
        Ok(Some(Received {
            len: n as usize,
            peer: sockaddr_to_socketaddr(&peer_storage)?,
            arrival_ns: arrival_ns.unwrap_or_else(now_unix_ns),
            kernel_stamped,
        }))
    }

    #[cfg(not(target_os = "linux"))]
    fn recv_stamped(&self, buf: &mut [u8]) -> io::Result<Option<Received>> {
        let (len, peer) = self.socket.recv_from(buf)?;
        Ok(Some(Received {
            len,
            peer,
            arrival_ns: now_unix_ns(),
            kernel_stamped: false,
        }))
    }
}

#[cfg(target_os = "linux")]
fn enable_kernel_timestamps(socket: &UdpSocket) -> bool {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPNS,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    rc == 0
}

#[cfg(not(target_os = "linux"))]
fn enable_kernel_timestamps(_socket: &UdpSocket) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn sockaddr_to_socketaddr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let addr: &libc::sockaddr_in = unsafe { std::mem::transmute(storage) };
            let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
            Ok(SocketAddr::V4(SocketAddrV4::new(
                ip,
                u16::from_be(addr.sin_port),
            )))
        }
        libc::AF_INET6 => {
            let addr: &libc::sockaddr_in6 = unsafe { std::mem::transmute(storage) };
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(addr.sin6_addr.s6_addr),
                u16::from_be(addr.sin6_port),
                addr.sin6_flowinfo,
                addr.sin6_scope_id,
            )))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected address family {other}"),
        )),
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Nanoseconds since the Unix epoch — the collector's reference clock, and the one every phone
/// offset is expressed against. The node is disciplined by chrony/NTP, so this is the plane's
/// shared time base.
pub fn now_unix_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
