//! The CSI source: nl80211 **vendor event** consumption over generic netlink.
//!
//! This is the one place `csid` talks to the kernel on the hot path, and the
//! reason the daemon exists at all — it replaces the upstream `iaxcsi` reader.
//!
//! ## How delivery actually works
//!
//! CSI is **not** broadcast on a multicast group. The driver
//! (`iwl_mvm_send_csi_event`) sends each event *unicast* to a single registered
//! netlink portid:
//!
//! ```text
//!  1. userspace sends NL80211_CMD_VENDOR                    ─┐  registration
//!       vendor_id = INTEL_OUI, subcmd = CSI_EVENT            │
//!  2. iwl_mvm_vendor_csi_register() records the sender's     │
//!       portid in mvm->csi_portid                           ─┘
//!  3. every CSI event → cfg80211_vendor_event_alloc_ucast(…, csi_portid, …)
//! ```
//!
//! Two consequences shape this module: the registration **must be sent from the
//! very socket that will receive** (the portid is the socket's), and there is
//! exactly **one CSI consumer per node** — `csid` owns that registration for the
//! session's lifetime.
//!
//! The event carries the header and matrix as attributes nested inside
//! `NL80211_ATTR_VENDOR_DATA`.
//!
//! Implemented on raw `AF_NETLINK` sockets rather than a netlink crate: the
//! protocol surface used here is small (resolve one family, send one command,
//! walk attributes) and every byte on the hot path stays explicit and auditable.

use anyhow::Result;

/// One CSI vendor event: the driver's fixed header blob plus the CSI matrix
/// blob, exactly as the kernel delivered them.
#[derive(Debug, Clone)]
pub struct RawCsiMessage {
    /// Driver header (272 bytes on iax/AX210).
    pub hdr: Vec<u8>,
    /// Interleaved `i16` I/Q CSI matrix.
    pub csi: Vec<u8>,
    /// Host wallclock stamped at delivery (the NTP-disciplined anchor).
    pub unix_ts_ns: u64,
}

/// A polling source of CSI messages, consumed on a dedicated thread.
pub trait CsiSource: Send {
    /// Wait up to an internal poll interval for the next CSI message.
    ///
    /// Returns `Ok(None)` when nothing arrived in that window — the caller uses
    /// those gaps to observe the stop flag, so a session always tears down
    /// promptly even on a silent channel.
    fn recv(&mut self) -> Result<Option<RawCsiMessage>>;
}

#[cfg(target_os = "linux")]
pub use linux::open;

#[cfg(not(target_os = "linux"))]
pub use portable::open;

/// Non-Linux builds keep the whole daemon compiling (config, sinks, export,
/// CLI are all portable) but cannot capture — netlink is Linux-only.
#[cfg(not(target_os = "linux"))]
mod portable {
    use super::*;
    use crate::config::DriverConfig;

    /// Always fails: capture requires Linux.
    pub fn open(_driver: &DriverConfig, _wiphy: u32) -> Result<Box<dyn CsiSource>> {
        anyhow::bail!(
            "CSI capture requires Linux (nl80211 vendor events over netlink); \
             this build is for development only"
        )
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::config::DriverConfig;
    use crate::util::now_unix_ns;
    use std::io;
    use std::os::unix::io::RawFd;

    // -- generic netlink ------------------------------------------------------
    const GENL_ID_CTRL: u16 = 16;
    const CTRL_CMD_GETFAMILY: u8 = 3;
    const CTRL_ATTR_FAMILY_ID: u16 = 1;
    const CTRL_ATTR_FAMILY_NAME: u16 = 2;

    const NLM_F_REQUEST: u16 = 0x01;
    const NLM_F_ACK: u16 = 0x04;
    const NLMSG_ERROR: u16 = 0x02;
    const NLMSG_DONE: u16 = 0x03;

    // -- nl80211 (values computed from the target's uapi/linux/nl80211.h) -----
    const NL80211_CMD_VENDOR: u8 = 103;
    const NL80211_ATTR_WIPHY: u16 = 1;
    const NL80211_ATTR_VENDOR_ID: u16 = 195;
    const NL80211_ATTR_VENDOR_SUBCMD: u16 = 196;
    const NL80211_ATTR_VENDOR_DATA: u16 = 197;

    const NLA_TYPE_MASK: u16 = 0x3fff;
    const NLMSG_HDR_LEN: usize = 16;
    const GENL_HDR_LEN: usize = 4;

    /// Receive buffer: sized well above the largest CSI message (1992 tones ×
    /// 4 chains × 4 bytes ≈ 32 KB) so a single `recv` never truncates.
    const RECV_BUF: usize = 256 * 1024;

    /// Socket receive buffer. This is the shock absorber that lets the RX
    /// thread hand off without the kernel dropping events during a scheduling
    /// hiccup (measured p99.9 delivery stall: 5.4 ms).
    const SO_RCVBUF_BYTES: libc::c_int = 8 * 1024 * 1024;

    /// How long `recv` waits before returning `Ok(None)` so the capture thread
    /// can check its stop flag.
    const RECV_TIMEOUT_MS: i64 = 250;

    #[inline]
    fn nla_align(len: usize) -> usize {
        (len + 3) & !3
    }

    /// Iterate a netlink attribute stream, yielding `(type, payload)`.
    fn iter_attrs(mut buf: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
        std::iter::from_fn(move || {
            if buf.len() < 4 {
                return None;
            }
            let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
            let ty = u16::from_ne_bytes([buf[2], buf[3]]) & NLA_TYPE_MASK;
            if len < 4 || len > buf.len() {
                return None;
            }
            let payload = &buf[4..len];
            let advance = nla_align(len).min(buf.len());
            buf = &buf[advance..];
            Some((ty, payload))
        })
    }

    fn push_attr(out: &mut Vec<u8>, ty: u16, payload: &[u8]) {
        let len = 4 + payload.len();
        out.extend_from_slice(&(len as u16).to_ne_bytes());
        out.extend_from_slice(&ty.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(nla_align(out.len()), 0);
    }

    fn push_u32(out: &mut Vec<u8>, ty: u16, value: u32) {
        push_attr(out, ty, &value.to_ne_bytes());
    }

    /// Build `nlmsghdr + genlmsghdr + payload`.
    fn build_message(ty: u16, flags: u16, seq: u32, cmd: u8, payload: &[u8]) -> Vec<u8> {
        let total = NLMSG_HDR_LEN + GENL_HDR_LEN + payload.len();
        let mut m = Vec::with_capacity(total);
        m.extend_from_slice(&(total as u32).to_ne_bytes());
        m.extend_from_slice(&ty.to_ne_bytes());
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&seq.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes()); // pid: kernel fills in
        m.push(cmd);
        m.push(1); // genl version
        m.extend_from_slice(&0u16.to_ne_bytes()); // reserved
        m.extend_from_slice(payload);
        m
    }

    /// The netlink socket, the resolved nl80211 family id, and the driver ABI.
    pub struct NetlinkSource {
        fd: RawFd,
        family_id: u16,
        driver: DriverConfig,
        buf: Vec<u8>,
    }

    impl NetlinkSource {
        /// Open the socket, resolve `nl80211`, and register for CSI events.
        pub fn new(driver: &DriverConfig, wiphy: u32) -> Result<Self> {
            let fd = unsafe {
                libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                    libc::NETLINK_GENERIC,
                )
            };
            if fd < 0 {
                anyhow::bail!(
                    "opening AF_NETLINK/NETLINK_GENERIC socket: {}",
                    io::Error::last_os_error()
                );
            }

            // Enlarge the kernel-side receive buffer before anything can flow.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &SO_RCVBUF_BYTES as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }

            // nl_pid = 0 asks the kernel to assign this socket's portid — the
            // very portid the driver will later unicast CSI events to.
            let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            let rc = unsafe {
                libc::bind(
                    fd,
                    &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                )
            };
            if rc < 0 {
                let e = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                anyhow::bail!("binding netlink socket: {e}");
            }

            // Bounded receive wait so the capture thread can observe its stop
            // flag between polls.
            let tv = libc::timeval {
                tv_sec: RECV_TIMEOUT_MS / 1000,
                tv_usec: (RECV_TIMEOUT_MS % 1000) * 1000,
            };
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &tv as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
            }

            let mut src = NetlinkSource {
                fd,
                family_id: 0,
                driver: driver.clone(),
                buf: vec![0u8; RECV_BUF],
            };

            src.family_id = src.resolve_family("nl80211")?;
            src.register_csi(wiphy)?;

            tracing::info!(
                family_id = src.family_id,
                wiphy,
                oui = format_args!("0x{:06x}", driver.vendor_oui),
                subcmd = format_args!("0x{:02x}", driver.csi_event_subcmd),
                "registered for CSI vendor events (unicast to this socket)"
            );
            Ok(src)
        }

        /// `CTRL_CMD_GETFAMILY` → the nl80211 family id.
        fn resolve_family(&mut self, name: &str) -> Result<u16> {
            let mut payload = Vec::new();
            let mut name_z = name.as_bytes().to_vec();
            name_z.push(0);
            push_attr(&mut payload, CTRL_ATTR_FAMILY_NAME, &name_z);

            let msg = build_message(GENL_ID_CTRL, NLM_F_REQUEST, 1, CTRL_CMD_GETFAMILY, &payload);
            self.send(&msg)?;

            let n = self
                .recv_raw()?
                .ok_or_else(|| anyhow::anyhow!("timed out resolving the nl80211 family"))?;
            let genl_payload = parse_single_message(&self.buf[..n])?;

            for (ty, val) in iter_attrs(genl_payload) {
                if ty == CTRL_ATTR_FAMILY_ID && val.len() >= 2 {
                    return Ok(u16::from_ne_bytes([val[0], val[1]]));
                }
            }
            anyhow::bail!("kernel returned no nl80211 family id — is cfg80211 loaded?")
        }

        /// Send the vendor command that registers this socket's portid as the
        /// CSI recipient, and wait for the kernel's ACK.
        fn register_csi(&mut self, wiphy: u32) -> Result<()> {
            let mut payload = Vec::new();
            push_u32(&mut payload, NL80211_ATTR_WIPHY, wiphy);
            push_u32(&mut payload, NL80211_ATTR_VENDOR_ID, self.driver.vendor_oui);
            push_u32(
                &mut payload,
                NL80211_ATTR_VENDOR_SUBCMD,
                self.driver.csi_event_subcmd,
            );

            let msg = build_message(
                self.family_id,
                NLM_F_REQUEST | NLM_F_ACK,
                2,
                NL80211_CMD_VENDOR,
                &payload,
            );
            self.send(&msg)?;

            // Expect an ACK (NLMSG_ERROR with error == 0).
            let n = self
                .recv_raw()?
                .ok_or_else(|| anyhow::anyhow!("timed out awaiting the CSI registration ACK"))?;
            expect_ack(&self.buf[..n]).map_err(|e| {
                anyhow::anyhow!(
                    "CSI registration rejected by the driver ({e}) — check [driver] \
                     vendor_oui/csi_event_subcmd against the loaded driver"
                )
            })
        }

        fn send(&self, data: &[u8]) -> Result<()> {
            let sent =
                unsafe { libc::send(self.fd, data.as_ptr() as *const libc::c_void, data.len(), 0) };
            if sent < 0 {
                anyhow::bail!("netlink send: {}", io::Error::last_os_error());
            }
            Ok(())
        }

        /// `Ok(None)` on poll timeout (`SO_RCVTIMEO` expiry).
        fn recv_raw(&mut self) -> Result<Option<usize>> {
            loop {
                let n = unsafe {
                    libc::recv(
                        self.fd,
                        self.buf.as_mut_ptr() as *mut libc::c_void,
                        self.buf.len(),
                        0,
                    )
                };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    match e.kind() {
                        io::ErrorKind::Interrupted => continue, // EINTR: retry
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => return Ok(None),
                        _ => anyhow::bail!("netlink recv: {e}"),
                    }
                }
                return Ok(Some(n as usize));
            }
        }

        /// Extract the CSI header/matrix blobs from one vendor-event message.
        ///
        /// The blobs live in attributes nested inside `NL80211_ATTR_VENDOR_DATA`
        /// (the nest `cfg80211_vendor_event_alloc_ucast` opens).
        fn extract_csi(&self, genl_payload: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
            let mut vendor_id = None;
            let mut subcmd = None;
            let mut data: Option<&[u8]> = None;

            for (ty, val) in iter_attrs(genl_payload) {
                match ty {
                    NL80211_ATTR_VENDOR_ID if val.len() >= 4 => {
                        vendor_id = Some(u32::from_ne_bytes([val[0], val[1], val[2], val[3]]));
                    }
                    NL80211_ATTR_VENDOR_SUBCMD if val.len() >= 4 => {
                        subcmd = Some(u32::from_ne_bytes([val[0], val[1], val[2], val[3]]));
                    }
                    NL80211_ATTR_VENDOR_DATA => data = Some(val),
                    _ => {}
                }
            }

            if vendor_id? != self.driver.vendor_oui || subcmd? != self.driver.csi_event_subcmd {
                return None;
            }

            let mut hdr = None;
            let mut csi = None;
            for (ty, val) in iter_attrs(data?) {
                if ty == self.driver.attr_csi_hdr {
                    hdr = Some(val.to_vec());
                } else if ty == self.driver.attr_csi_data {
                    csi = Some(val.to_vec());
                }
            }
            Some((hdr?, csi?))
        }
    }

    impl CsiSource for NetlinkSource {
        fn recv(&mut self) -> Result<Option<RawCsiMessage>> {
            loop {
                let Some(n) = self.recv_raw()? else {
                    return Ok(None); // poll timeout — let the caller check its stop flag
                };
                // Stamp as close to delivery as possible.
                let unix_ts_ns = now_unix_ns();

                // A netlink datagram may carry several messages back to back.
                let mut off = 0usize;
                while off + NLMSG_HDR_LEN <= n {
                    let buf = &self.buf[off..n];
                    let len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                    let ty = u16::from_ne_bytes([buf[4], buf[5]]);
                    if len < NLMSG_HDR_LEN || len > buf.len() {
                        break;
                    }
                    if ty == self.family_id && len >= NLMSG_HDR_LEN + GENL_HDR_LEN {
                        let cmd = buf[NLMSG_HDR_LEN];
                        if cmd == NL80211_CMD_VENDOR {
                            let payload = &buf[NLMSG_HDR_LEN + GENL_HDR_LEN..len];
                            if let Some((hdr, csi)) = self.extract_csi(payload) {
                                return Ok(Some(RawCsiMessage {
                                    hdr,
                                    csi,
                                    unix_ts_ns,
                                }));
                            }
                        }
                    }
                    off += nla_align(len);
                }
                // Nothing of ours in this datagram — keep listening.
            }
        }
    }

    impl Drop for NetlinkSource {
        fn drop(&mut self) {
            // Closing the socket drops the portid; the driver notices via its
            // netlink notifier and clears mvm->csi_portid.
            unsafe { libc::close(self.fd) };
        }
    }

    /// Validate one response message and return its generic-netlink payload.
    fn parse_single_message(buf: &[u8]) -> Result<&[u8]> {
        if buf.len() < NLMSG_HDR_LEN {
            anyhow::bail!("short netlink response ({} bytes)", buf.len());
        }
        let len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let ty = u16::from_ne_bytes([buf[4], buf[5]]);
        if ty == NLMSG_ERROR {
            return Err(netlink_error(buf));
        }
        if ty == NLMSG_DONE {
            anyhow::bail!("netlink DONE where a payload was expected");
        }
        let end = len.min(buf.len());
        if end < NLMSG_HDR_LEN + GENL_HDR_LEN {
            anyhow::bail!("netlink message too short for a generic-netlink header");
        }
        Ok(&buf[NLMSG_HDR_LEN + GENL_HDR_LEN..end])
    }

    /// Accept an ACK (`NLMSG_ERROR` carrying error code 0).
    fn expect_ack(buf: &[u8]) -> Result<()> {
        if buf.len() < NLMSG_HDR_LEN + 4 {
            anyhow::bail!("short ACK ({} bytes)", buf.len());
        }
        let ty = u16::from_ne_bytes([buf[4], buf[5]]);
        if ty != NLMSG_ERROR {
            // Some kernels answer a vendor command with a reply message rather
            // than a bare ACK; either means the command was accepted.
            return Ok(());
        }
        let code = i32::from_ne_bytes([
            buf[NLMSG_HDR_LEN],
            buf[NLMSG_HDR_LEN + 1],
            buf[NLMSG_HDR_LEN + 2],
            buf[NLMSG_HDR_LEN + 3],
        ]);
        if code == 0 {
            return Ok(());
        }
        Err(anyhow::anyhow!("{}", io::Error::from_raw_os_error(-code)))
    }

    fn netlink_error(buf: &[u8]) -> anyhow::Error {
        if buf.len() < NLMSG_HDR_LEN + 4 {
            return anyhow::anyhow!("truncated netlink error");
        }
        let code = i32::from_ne_bytes([
            buf[NLMSG_HDR_LEN],
            buf[NLMSG_HDR_LEN + 1],
            buf[NLMSG_HDR_LEN + 2],
            buf[NLMSG_HDR_LEN + 3],
        ]);
        if code == 0 {
            anyhow::anyhow!("netlink ACK where a payload was expected")
        } else {
            anyhow::anyhow!("netlink error: {}", io::Error::from_raw_os_error(-code))
        }
    }

    /// Open the platform CSI source for a given wiphy index.
    pub fn open(driver: &DriverConfig, wiphy: u32) -> Result<Box<dyn CsiSource>> {
        Ok(Box::new(NetlinkSource::new(driver, wiphy)?))
    }
}
