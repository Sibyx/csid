//! `sd_notify` — the systemd `Type=notify` protocol, implemented on the
//! standard library (no `libsystemd` dependency).
//!
//! `csid` reports `READY=1` once the radio is tuned and the source is live,
//! pings `WATCHDOG=1` while records flow, and sends `STOPPING=1` on teardown.
//! When `NOTIFY_SOCKET` is unset (interactive runs) every call is a no-op.

use std::io;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

/// Send a raw notification string. No-op when not started by systemd.
pub fn notify(state: &str) -> io::Result<()> {
    let Some(raw) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let path = raw.to_string_lossy().to_string();
    let sock = UnixDatagram::unbound()?;

    // A leading '@' denotes a Linux abstract socket, whose address begins with
    // a NUL byte — not expressible through the std path API, so use sendto.
    if let Some(name) = path.strip_prefix('@') {
        #[cfg(target_os = "linux")]
        {
            return send_abstract(&sock, name, state.as_bytes());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            return Ok(());
        }
    }

    sock.send_to(state.as_bytes(), &path).map(|_| ())
}

#[cfg(target_os = "linux")]
fn send_abstract(sock: &UnixDatagram, name: &str, payload: &[u8]) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    if bytes.len() + 1 > addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NOTIFY_SOCKET abstract name too long",
        ));
    }
    // sun_path[0] stays NUL (abstract namespace marker).
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i + 1] = *b as libc::c_char;
    }
    let addr_len = (std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len()) as libc::socklen_t;

    let sent = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            0,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `READY=1` — the unit is up (radio tuned, CSI source registered).
pub fn ready() {
    log_err(notify("READY=1"));
}

/// `WATCHDOG=1` — liveness ping for `WatchdogSec=`.
pub fn watchdog() {
    log_err(notify("WATCHDOG=1"));
}

/// `STOPPING=1` — teardown has begun.
pub fn stopping() {
    log_err(notify("STOPPING=1"));
}

/// Free-form status line shown by `systemctl status`.
pub fn status(text: &str) {
    log_err(notify(&format!("STATUS={text}")));
}

/// The interval at which to ping the watchdog, derived from `WATCHDOG_USEC`.
///
/// systemd requires pings at least twice per `WatchdogSec`; we use a third of
/// the interval for margin. Returns `None` when the watchdog is disabled.
pub fn watchdog_interval() -> Option<Duration> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    if usec == 0 {
        return None;
    }
    Some(Duration::from_micros(usec / 3))
}

fn log_err(r: io::Result<()>) {
    if let Err(e) = r {
        tracing::warn!(error = %e, "sd_notify failed");
    }
}
