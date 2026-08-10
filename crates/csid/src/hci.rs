//! The Linux side of BLE co-capture: a raw HCI socket driving a passive LE scan.
//!
//! `csid` talks to the controller the way `btmon` and `hcitool lescan` do —
//! `AF_BLUETOOTH`/`BTPROTO_HCI` on `HCI_CHANNEL_RAW` — rather than through
//! BlueZ's D-Bus API. Three reasons:
//!
//! 1. **Per-advertisement RSSI.** The D-Bus `Device1` interface exposes a
//!    device's *current* RSSI as a property; the calibration arm needs the
//!    time series of every received advertisement, which only the HCI LE
//!    Advertising Report carries.
//! 2. **No duplicate filtering.** `LE Set Scan Enable` is issued with
//!    `Filter_Duplicates = 0`, so a device advertising at 10 Hz produces 10
//!    observations a second. D-Bus deduplicates.
//! 3. **No new dependency.** `csid` already speaks raw netlink and `AF_PACKET`
//!    through `libc`; a D-Bus client would have added an async stack to a
//!    daemon whose whole design argument is that it has none.
//!
//! The scan is passive: the controller never transmits `SCAN_REQ`, so the node
//! stays radio-silent on 2.4 GHz apart from the Wi-Fi capture itself. That is
//! both the privacy posture and a measurement requirement — an active scanner
//! would inject energy into the band the CSI capture is measuring.
//!
//! **Operational coupling with `bluetoothd`.** A `HCI_CHANNEL_RAW` socket shares
//! the adapter with anything else that has it open. If `bluetooth.service` is
//! running and performing its own discovery, it will change the scan parameters
//! underneath us. On a capture node, mask the service or leave the adapter
//! unmanaged. `csid doctor` reports what it can see.

#[cfg(target_os = "linux")]
pub use linux::{probe, spawn};

#[cfg(not(target_os = "linux"))]
pub use portable::{probe, spawn};

/// What a BLE readiness probe found. Shown by `csid doctor`.
#[derive(Debug, Clone)]
pub struct BleProbe {
    pub adapter: String,
    /// `/sys/class/bluetooth/<adapter>` exists.
    pub present: bool,
    /// The adapter accepted a socket bind (i.e. it is up and we have the caps).
    pub usable: bool,
    pub detail: String,
}

#[cfg(not(target_os = "linux"))]
mod portable {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use anyhow::Result;

    use super::BleProbe;
    use crate::ble::{BleCounters, BleHandle};
    use crate::config::BleConfig;

    pub fn spawn(
        _dir: &Path,
        _cfg: &BleConfig,
        _stop: Arc<AtomicBool>,
        _counters: Arc<BleCounters>,
    ) -> Result<BleHandle> {
        anyhow::bail!(
            "BLE co-capture requires Linux with BlueZ (AF_BLUETOOTH/BTPROTO_HCI); \
             this build is for development only"
        )
    }

    pub fn probe(adapter: &str) -> BleProbe {
        BleProbe {
            adapter: adapter.to_string(),
            present: false,
            usable: false,
            detail: "BLE co-capture requires Linux with BlueZ".to_string(),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::unix::io::RawFd;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use super::BleProbe;
    use crate::ble::{
        command_status, parse_hci_event, scan_enable_command, scan_parameters_command, BleCounters,
        BleHandle, DeviceHasher, ObservationLog, HCI_EVENT_PKT, OP_LE_SET_SCAN_ENABLE,
        OP_LE_SET_SCAN_PARAMETERS,
    };
    use crate::config::BleConfig;
    use crate::util::now_unix_ns;

    // -- Bluetooth socket ABI (uapi/linux/bluetooth) --------------------------
    // libc does not expose these for Linux, so they are named here rather than
    // written as magic numbers at the call site.
    const AF_BLUETOOTH: libc::c_int = 31;
    const BTPROTO_HCI: libc::c_int = 1;
    const SOL_HCI: libc::c_int = 0;
    const HCI_FILTER: libc::c_int = 2;
    const HCI_CHANNEL_RAW: u16 = 0;

    /// An HCI event is at most 3 + 255 bytes; round up.
    const RECV_BUF: usize = 1024;
    /// `recv` wait before returning `Ok(None)` so the loop can check its stop
    /// flag — the same idiom the netlink source uses.
    const RECV_TIMEOUT_MS: i64 = 250;
    /// How long to wait for a Command Complete before calling the adapter mute.
    const CMD_TIMEOUT: Duration = Duration::from_millis(2000);

    #[repr(C)]
    struct SockaddrHci {
        hci_family: libc::sa_family_t,
        hci_dev: u16,
        hci_channel: u16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct HciFilter {
        type_mask: u32,
        event_mask: [u32; 2],
        opcode: u16,
    }
    /// Bytes of [`HciFilter`] the kernel reads — through `opcode`, excluding
    /// the struct's trailing alignment padding.
    const HCI_FILTER_LEN: libc::socklen_t = 14;

    /// A bound HCI socket with LE scanning enabled. Scanning is disabled again
    /// on drop, so a stopped session never leaves the controller scanning.
    struct Scanner {
        fd: RawFd,
        buf: Vec<u8>,
        adapter: String,
    }

    fn sys_path(adapter: &str) -> String {
        format!("/sys/class/bluetooth/{adapter}")
    }

    /// Translate the errnos this path actually produces into something an
    /// operator can act on at 2 a.m. in a lecture hall.
    fn explain(e: &io::Error, adapter: &str) -> String {
        match e.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => format!(
                "{e} — the HCI socket needs CAP_NET_RAW (add it to the systemd unit's \
                 AmbientCapabilities, or run as root)"
            ),
            Some(libc::ENETDOWN) => format!(
                "{e} — {adapter} is down; bring it up with `sudo hciconfig {adapter} up` \
                 (or `bluetoothctl power on`)"
            ),
            Some(libc::EAFNOSUPPORT) | Some(libc::EPROTONOSUPPORT) => format!(
                "{e} — this kernel has no Bluetooth stack (CONFIG_BT); BLE co-capture \
                 cannot run on this node"
            ),
            Some(libc::ENODEV) => format!("{e} — no such HCI adapter as {adapter}"),
            _ => e.to_string(),
        }
    }

    impl Scanner {
        fn open(cfg: &BleConfig) -> Result<Self> {
            let adapter = cfg.adapter.clone();
            let index = cfg.adapter_index()?;

            // Cheapest, clearest failure first: is there an adapter at all?
            if !Path::new(&sys_path(&adapter)).exists() {
                anyhow::bail!(
                    "BLE adapter {adapter} not found ({} does not exist) — no Bluetooth \
                     controller, or BlueZ has not bound it",
                    sys_path(&adapter)
                );
            }

            let fd = unsafe {
                libc::socket(
                    AF_BLUETOOTH,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                    BTPROTO_HCI,
                )
            };
            if fd < 0 {
                let e = io::Error::last_os_error();
                anyhow::bail!(
                    "opening AF_BLUETOOTH/BTPROTO_HCI socket: {}",
                    explain(&e, &adapter)
                );
            }
            let sc = Scanner {
                fd,
                buf: vec![0u8; RECV_BUF],
                adapter: adapter.clone(),
            };

            // Events only; every event code, so Command Complete for our own
            // commands arrives alongside the advertising reports.
            let filter = HciFilter {
                type_mask: 1 << HCI_EVENT_PKT,
                event_mask: [u32::MAX, u32::MAX],
                opcode: 0,
            };
            let rc = unsafe {
                libc::setsockopt(
                    sc.fd,
                    SOL_HCI,
                    HCI_FILTER,
                    &filter as *const HciFilter as *const libc::c_void,
                    HCI_FILTER_LEN,
                )
            };
            if rc != 0 {
                let e = io::Error::last_os_error();
                anyhow::bail!("setting the HCI event filter: {}", explain(&e, &adapter));
            }

            // Bounded recv so the scan loop observes the stop flag promptly.
            let tv = libc::timeval {
                tv_sec: RECV_TIMEOUT_MS / 1000,
                tv_usec: ((RECV_TIMEOUT_MS % 1000) * 1000) as libc::suseconds_t,
            };
            unsafe {
                libc::setsockopt(
                    sc.fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &tv as *const libc::timeval as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
            }

            let addr = SockaddrHci {
                hci_family: AF_BLUETOOTH as libc::sa_family_t,
                hci_dev: index,
                hci_channel: HCI_CHANNEL_RAW,
            };
            let rc = unsafe {
                libc::bind(
                    sc.fd,
                    &addr as *const SockaddrHci as *const libc::sockaddr,
                    std::mem::size_of::<SockaddrHci>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                let e = io::Error::last_os_error();
                anyhow::bail!(
                    "binding the HCI socket to {adapter}: {}",
                    explain(&e, &adapter)
                );
            }

            sc.start_scan(cfg)?;
            Ok(sc)
        }

        /// Disable → configure → enable. Parameters cannot be changed while
        /// scanning, so the first disable is mandatory and its failure benign
        /// (it fails exactly when scanning was already off).
        fn start_scan(&self, cfg: &BleConfig) -> Result<()> {
            let _ = self.command(OP_LE_SET_SCAN_ENABLE, &scan_enable_command(false));

            let (interval, window) = cfg.hci_units();
            self.command(
                OP_LE_SET_SCAN_PARAMETERS,
                &scan_parameters_command(interval, window),
            )
            .context("LE Set Scan Parameters")?;

            self.command(OP_LE_SET_SCAN_ENABLE, &scan_enable_command(true))
                .context("LE Set Scan Enable")?;

            tracing::info!(
                adapter = %self.adapter,
                scan_interval_ms = cfg.scan_interval_ms,
                scan_window_ms = cfg.scan_window_ms,
                "BLE passive scan running"
            );
            Ok(())
        }

        /// Send a framed HCI command packet and wait for its Command Complete /
        /// Command Status. `op` is only used to match the completion.
        fn command(&self, op: u16, pkt: &[u8]) -> Result<()> {
            let n =
                unsafe { libc::send(self.fd, pkt.as_ptr() as *const libc::c_void, pkt.len(), 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                anyhow::bail!(
                    "sending HCI command 0x{op:04x}: {}",
                    explain(&e, &self.adapter)
                );
            }

            let deadline = Instant::now() + CMD_TIMEOUT;
            let mut buf = vec![0u8; RECV_BUF];
            while Instant::now() < deadline {
                let n = unsafe {
                    libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
                };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    match e.kind() {
                        io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut => continue,
                        _ => anyhow::bail!("awaiting HCI command completion: {e}"),
                    }
                }
                if let Some(status) = command_status(&buf[..n as usize], op) {
                    if status == 0 {
                        return Ok(());
                    }
                    anyhow::bail!(
                        "controller rejected HCI command 0x{op:04x} with status 0x{status:02x}"
                    );
                }
                // Anything else (an advertising report that arrived first) is
                // simply not the answer; keep waiting.
            }
            anyhow::bail!(
                "timed out awaiting completion of HCI command 0x{op:04x} on {} — \
                 is another process (bluetoothd) driving the adapter?",
                self.adapter
            )
        }

        /// `Ok(None)` on receive timeout.
        fn recv(&mut self) -> Result<Option<usize>> {
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
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => return Ok(None),
                        _ => anyhow::bail!("HCI recv on {}: {e}", self.adapter),
                    }
                }
                return Ok(Some(n as usize));
            }
        }
    }

    impl Drop for Scanner {
        fn drop(&mut self) {
            // Best effort: leaving the controller scanning would burn power and
            // confuse the next session. No completion is awaited — the socket
            // is about to close.
            let pkt = scan_enable_command(false);
            unsafe {
                libc::send(self.fd, pkt.as_ptr() as *const libc::c_void, pkt.len(), 0);
                libc::close(self.fd);
            }
        }
    }

    /// Report BLE readiness without starting a session (`csid doctor`).
    pub fn probe(adapter: &str) -> BleProbe {
        let present = Path::new(&sys_path(adapter)).exists();
        if !present {
            return BleProbe {
                adapter: adapter.to_string(),
                present: false,
                usable: false,
                detail: format!("{} does not exist", sys_path(adapter)),
            };
        }
        let cfg = BleConfig {
            enabled: true,
            adapter: adapter.to_string(),
            ..BleConfig::default()
        };
        match Scanner::open(&cfg) {
            Ok(sc) => {
                drop(sc); // Drop disables scanning again immediately.
                BleProbe {
                    adapter: adapter.to_string(),
                    present: true,
                    usable: true,
                    detail: "passive LE scan started and stopped cleanly".to_string(),
                }
            }
            Err(e) => BleProbe {
                adapter: adapter.to_string(),
                present: true,
                usable: false,
                detail: format!("{e:#}"),
            },
        }
    }

    /// Open the adapter on the caller's thread, then run the scan loop.
    pub fn spawn(
        dir: &Path,
        cfg: &BleConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<BleCounters>,
    ) -> Result<BleHandle> {
        // Fail fast, on the caller's thread, so `ble.required` can act on it.
        let scanner = Scanner::open(cfg)?;
        let hasher = DeviceHasher::new_random(cfg.hash_bytes)?;
        let log = ObservationLog::create(dir, cfg.flush_every)?;
        let ndjson = dir.join(crate::ble::NDJSON_NAME);
        let cfg = cfg.clone();

        let thread: JoinHandle<()> = std::thread::Builder::new()
            .name("csid-ble".into())
            .spawn(move || run_loop(scanner, log, hasher, &cfg, stop, counters))
            .context("spawning the BLE scan thread")?;
        Ok(BleHandle::new(thread, ndjson))
    }

    fn run_loop(
        first: Scanner,
        mut log: ObservationLog,
        hasher: DeviceHasher,
        cfg: &BleConfig,
        stop: Arc<AtomicBool>,
        counters: Arc<BleCounters>,
    ) {
        let mut pending = Some(first);
        // Consecutive restarts that produced nothing. Backs the silence budget
        // off exponentially so a genuinely empty room does not churn the
        // adapter all night, while a wedged one still recovers quickly.
        let mut silent_restarts: u32 = 0;

        'session: while !stop.load(Ordering::Relaxed) {
            let mut sc = match pending.take() {
                Some(s) => s,
                None => match Scanner::open(cfg) {
                    Ok(s) => {
                        tracing::info!(adapter = %cfg.adapter, "BLE scanner re-opened");
                        s
                    }
                    Err(e) => {
                        counters.adapter_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            error = %format!("{e:#}"),
                            adapter = %cfg.adapter,
                            "BLE scanner unavailable; retrying"
                        );
                        sleep_interruptible(cfg.backoff_s, &stop);
                        continue;
                    }
                },
            };

            let mut saw_any = false;
            let silence_budget = Duration::from_secs_f64(
                cfg.restart_after_s * (1u32 << silent_restarts.min(4)) as f64,
            );
            let mut last_obs = Instant::now();
            let mut last_log = Instant::now();
            let mut last_count = counters.observations.load(Ordering::Relaxed);

            while !stop.load(Ordering::Relaxed) {
                match sc.recv() {
                    Ok(Some(n)) => {
                        let unix_ts_ns = now_unix_ns();
                        let parsed = parse_hci_event(&sc.buf[..n]);
                        if parsed.ignored || parsed.truncated {
                            counters.unparsed_events.fetch_add(1, Ordering::Relaxed);
                        }
                        for adv in &parsed.advs {
                            let obs = hasher.observe(adv, unix_ts_ns);
                            if obs.rssi_dbm.is_none() {
                                counters.rssi_unavailable.fetch_add(1, Ordering::Relaxed);
                            }
                            counters.note_observation(unix_ts_ns, cfg.gap_alert_s);
                            if let Err(e) = log.append(&obs) {
                                tracing::error!(
                                    error = %e,
                                    "BLE log write failed; stopping the scanner \
                                     (the CSI capture is unaffected)"
                                );
                                break 'session;
                            }
                            saw_any = true;
                            last_obs = Instant::now();
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        counters.adapter_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(error = %format!("{e:#}"), "BLE socket failed; restarting the scanner");
                        break;
                    }
                }

                if last_obs.elapsed() >= silence_budget {
                    tracing::warn!(
                        adapter = %cfg.adapter,
                        silent_s = last_obs.elapsed().as_secs(),
                        "no BLE advertisements; restarting the scanner"
                    );
                    break;
                }

                if last_log.elapsed() >= Duration::from_secs(10) {
                    let (obs, mean_hz) = counters.snapshot();
                    let window = last_log.elapsed().as_secs_f64();
                    let rate = (obs - last_count) as f64 / window;
                    tracing::info!(
                        observations = obs,
                        rate_hz = rate,
                        mean_rate_hz = mean_hz,
                        restarts = counters.scan_restarts.load(Ordering::Relaxed),
                        max_gap_ms = counters.max_gap_ms.load(Ordering::Relaxed),
                        "ble scanning"
                    );
                    last_count = obs;
                    last_log = Instant::now();
                }
            }

            drop(sc);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            counters.scan_restarts.fetch_add(1, Ordering::Relaxed);
            silent_restarts = if saw_any { 0 } else { silent_restarts + 1 };
            sleep_interruptible(cfg.backoff_s, &stop);
        }

        if let Err(e) = log.finish() {
            tracing::error!(error = %e, "closing the BLE log failed");
        }
        let (obs, rate) = counters.snapshot();
        tracing::info!(
            observations = obs,
            mean_rate_hz = rate,
            restarts = counters.scan_restarts.load(Ordering::Relaxed),
            adapter_errors = counters.adapter_errors.load(Ordering::Relaxed),
            "BLE scanner stopped"
        );
    }

    /// Sleep in short slices so a stop signal is not held up by the backoff.
    fn sleep_interruptible(seconds: f64, stop: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn filter_length_covers_through_the_opcode_field() {
            // The kernel copies HCI_FILTER_LEN bytes into struct hci_ufilter;
            // it must reach the end of `opcode` and no further.
            let f = HciFilter::default();
            let base = &f as *const HciFilter as usize;
            let op = &f.opcode as *const u16 as usize;
            assert_eq!(op - base, 12);
            assert_eq!(HCI_FILTER_LEN as usize, 12 + std::mem::size_of::<u16>());
        }
    }
}
