//! Subcommand implementations.

#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;

use crate::caps::{self, Envelope};
use crate::config::{ExperimentConfig, GlobalConfig};
use crate::{debugfs, engine, export, radio, util};

/// `csid run <experiment>` — the systemd `ExecStart`.
pub fn run(
    global: &GlobalConfig,
    experiment: &str,
    experiment_dir: &Path,
    duration_override: Option<std::time::Duration>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut cfg = ExperimentConfig::resolve(experiment, experiment_dir)?;
    if let Some(d) = duration_override {
        cfg.capture.duration = Some(d);
    }
    let outcome = engine::run_session(global, &cfg, stop)?;
    println!(
        "{} {:?} — {} records, {} bytes -> {}",
        outcome.session_id,
        outcome.status,
        outcome.summary.records,
        outcome.summary.capture_bytes,
        outcome.dir.display()
    );
    Ok(())
}

/// `csid validate <experiment>` — config + radio-resolution dry run.
pub fn validate(experiment: &str, experiment_dir: &Path, probe: bool) -> Result<()> {
    let cfg = ExperimentConfig::resolve(experiment, experiment_dir)?;
    cfg.validate()?;
    let tuning = radio::resolve(&cfg.radio)?;

    println!("experiment    : {}", cfg.slug());
    println!(
        "interface     : {} (monitor {})",
        cfg.radio.interface, cfg.radio.monitor
    );
    println!("band          : {:?}", tuning.band);
    println!(
        "channel       : {} ({} MHz)",
        cfg.radio.channel, tuning.freq
    );
    println!("width         : {}", cfg.radio.width.iw_token());
    match tuning.center {
        Some(c) => println!("center freq   : {c} MHz"),
        None => println!("center freq   : (not required at this width)"),
    }
    println!(
        "interval      : {} µs{}",
        cfg.radio.interval_us,
        if cfg.radio.interval_us == 0 {
            " (unthrottled)"
        } else {
            ""
        }
    );
    println!("mac filter    : {:?}", cfg.radio.mac_filter);
    println!("mode          : {}", cfg.capture.mode);
    if cfg.capture.mode == "inject" {
        println!(
            "inject        : {} Hz, {} B frames, {} -> {}, {} Mbps legacy OFDM",
            cfg.inject.rate_hz,
            cfg.inject.frame_bytes,
            cfg.inject.src_mac,
            cfg.inject.dst_mac,
            cfg.inject.bitrate_mbps
        );
    }
    println!(
        "duration      : {}",
        cfg.capture
            .duration
            .map(|d| format!("{}s", d.as_secs()))
            .unwrap_or_else(|| "until stopped".into())
    );
    println!(
        "live stream   : {}",
        if cfg.stream.enabled {
            format!(
                "{} -> {}",
                cfg.stream.transport,
                cfg.stream.unix_socket.display()
            )
        } else {
            "disabled".into()
        }
    );
    println!("csiq on close : {}", cfg.export.on_close);

    if probe {
        println!("\n-- hardware probe --");
        if !radio::interface_exists(&cfg.radio.interface) {
            anyhow::bail!("interface {} does not exist", cfg.radio.interface);
        }
        println!("interface     : present");
        let knobs = debugfs::Knobs::for_interface(&cfg.radio.interface)?;
        println!("debugfs       : {}", knobs.dir().display());
    }

    println!("\nconfiguration is valid.");
    Ok(())
}

/// `csid caps` — the measured capability envelope plus the tunable matrix.
pub fn caps_cmd(json: bool) -> Result<()> {
    let env = Envelope::default();
    if json {
        println!("{}", serde_json::to_string_pretty(&env)?);
        return Ok(());
    }

    println!("AX210 / iax measured capability envelope");
    println!("  tone counts         : {:?}", env.tone_counts);
    println!("  max MIMO            : {}", env.max_mimo);
    println!(
        "  sustained rate      : {} Hz (~{} KB/s)",
        env.sustained_rate_hz,
        env.sustained_bytes_per_sec / 1024
    );
    println!(
        "  ftm clock           : {} Hz ({} ns, wraps {:.2} s)",
        env.ftm_clock_hz, env.ftm_resolution_ns, env.ftm_wrap_seconds
    );
    println!("\nnotes:");
    for n in env.notes {
        println!("  - {n}");
    }

    println!("\nwide-capture centre frequencies (from this build's tables):");
    for (band, chans) in [
        (caps::Band::Ghz5, vec![36u32, 52, 100, 149]),
        (caps::Band::Ghz6, vec![1u32, 33, 65]),
    ] {
        for ch in chans {
            for w in [caps::WidthCfg::W80, caps::WidthCfg::W160] {
                if let Ok(Some(c)) = caps::center_freq(band, ch, w) {
                    let ctrl = caps::channel_to_freq(band, ch).unwrap_or(0);
                    println!(
                        "  {:?} ch {:>3} {:>6} : control {} MHz, centre {} MHz",
                        band,
                        ch,
                        w.iw_token(),
                        ctrl,
                        c
                    );
                }
            }
        }
    }
    Ok(())
}

/// `csid doctor` — is this node able to capture right now?
pub fn doctor(global: &GlobalConfig, interface: &str) -> Result<()> {
    let mut failures = 0;

    let mut check = |label: &str, ok: bool, detail: String| {
        println!("[{}] {label}: {detail}", if ok { "ok" } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    };

    // Kernel + driver provenance.
    let kernel = util::run_opt("uname", &["-r"]).unwrap_or_else(|| "unknown".into());
    check("kernel", true, kernel);

    let modpath = util::run_opt("modinfo", &["-F", "filename", "iwlwifi"]);
    match &modpath {
        Some(p) => {
            let dkms = p.contains("updates/dkms") || p.contains("/updates/");
            check(
                "iwlwifi module",
                dkms,
                format!(
                    "{p}{}",
                    if dkms {
                        ""
                    } else {
                        "  <- in-tree driver: NO CSI"
                    }
                ),
            );
        }
        None => check("iwlwifi module", false, "modinfo found no iwlwifi".into()),
    }

    let lsmod = util::run_opt("lsmod", &[]).unwrap_or_default();
    let compat = lsmod.lines().any(|l| l.starts_with("compat "));
    check(
        "backport compat module",
        compat,
        if compat {
            "loaded".into()
        } else {
            "not loaded".into()
        },
    );

    // Interface + debugfs.
    let iface_ok = radio::interface_exists(interface);
    check(
        "capture interface",
        iface_ok,
        format!("{interface}{}", if iface_ok { "" } else { " (missing)" }),
    );

    if iface_ok {
        match debugfs::Knobs::for_interface(interface) {
            Ok(k) => check("debugfs CSI knobs", true, k.dir().display().to_string()),
            Err(e) => check("debugfs CSI knobs", false, e.to_string()),
        }
    }

    // Regulatory + performance context.
    check(
        "regdomain",
        true,
        radio::regdomain().unwrap_or_else(|| "unknown".into()),
    );
    let governor = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    check(
        "cpu governor",
        governor == "performance" || governor == "unknown",
        governor,
    );

    // Spool.
    let spool = &global.node.spool;
    check(
        "spool directory",
        spool.is_dir(),
        format!(
            "{}{}",
            spool.display(),
            if spool.is_dir() { "" } else { " (missing)" }
        ),
    );

    // Driver ABI coupling actually in use.
    println!(
        "[info] driver ABI: oui=0x{:06x} subcmd=0x{:02x} hdr_attr=0x{:02x} data_attr=0x{:02x}",
        global.driver.vendor_oui,
        global.driver.csi_event_subcmd,
        global.driver.attr_csi_hdr,
        global.driver.attr_csi_data,
    );
    if iface_ok {
        match radio::phy_index(interface) {
            Ok(idx) => println!("[info] wiphy index: {idx} (registration target)"),
            Err(e) => check("wiphy index", false, e.to_string()),
        }
    }

    if failures > 0 {
        anyhow::bail!("{failures} check(s) failed");
    }
    println!("\nall checks passed.");
    Ok(())
}

/// `csid export <session-dir>` — raw → `.csiq`.
pub fn export_cmd(dir: &Path, out: Option<PathBuf>) -> Result<()> {
    let (path, n) = export::export_session(dir, out)?;
    println!("wrote {} ({n} records)", path.display());
    Ok(())
}

/// `csid stream` — attach a debug subscriber to the live socket.
#[cfg(unix)]
pub fn stream_cmd(socket: &Path, limit: Option<u64>) -> Result<()> {
    // A datagram receiver must own (bind) the path.
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let sock =
        UnixDatagram::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    println!("listening on {} (Ctrl-C to stop)", socket.display());

    let mut buf = vec![0u8; 256 * 1024];
    let mut seen: u64 = 0;
    let mut last_seq: Option<u32> = None;
    let mut gaps: u64 = 0;

    loop {
        let n = sock.recv(&mut buf).context("receiving live datagram")?;
        match csiq::live::decode(&buf[..n]) {
            Ok(dg) => {
                if let Some(prev) = last_seq {
                    let expected = prev.wrapping_add(1);
                    if dg.seq != expected {
                        gaps += dg.seq.wrapping_sub(expected) as u64;
                    }
                }
                last_seq = Some(dg.seq);
                seen += 1;
                let r = &dg.record;
                println!(
                    "seq={:<8} ftm={:<12} tones={:<5} {}x{} rssi={:?} phy={:?} src={}",
                    dg.seq,
                    r.ftm,
                    r.ntone,
                    r.nrx,
                    r.ntx,
                    r.rssi,
                    r.phy.map(|p| p.modulation),
                    fmt_mac(&r.src_mac)
                );
            }
            Err(e) => eprintln!("undecodable datagram ({n} bytes): {e}"),
        }
        if let Some(l) = limit {
            if seen >= l {
                println!("\n{seen} records, {gaps} missed (sender-side drops)");
                return Ok(());
            }
        }
    }
}

/// The live socket is Unix-domain; without it there is nothing to attach to.
#[cfg(not(unix))]
pub fn stream_cmd(_socket: &Path, _limit: Option<u64>) -> Result<()> {
    anyhow::bail!("`csid stream` requires Unix-domain sockets, unavailable on this platform")
}

#[cfg(unix)]
fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// `csid bench` — timed capture(s) reporting achieved rate and CSI mix.
///
/// This is the harness that produced the envelope in `csid caps`; pass several
/// channels to sweep them one after another.
pub fn bench(
    global: &GlobalConfig,
    experiment: &str,
    experiment_dir: &Path,
    channels: Vec<u32>,
    duration: std::time::Duration,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let base = ExperimentConfig::resolve(experiment, experiment_dir)?;
    let channels = if channels.is_empty() {
        vec![base.radio.channel]
    } else {
        channels
    };

    println!(
        "{:<8} {:<8} {:>10} {:>12} {:>10}  tones",
        "channel", "width", "records", "rate (Hz)", "bytes"
    );
    for ch in channels {
        let mut cfg = base.clone();
        cfg.radio.channel = ch;
        cfg.capture.duration = Some(duration);
        cfg.experiment = Some(format!("{}-bench-ch{ch}", base.slug()));

        match engine::run_session(global, &cfg, stop.clone()) {
            Ok(o) => println!(
                "{:<8} {:<8} {:>10} {:>12.1} {:>10} {:?}",
                ch,
                cfg.radio.width.iw_token(),
                o.summary.records,
                o.summary.mean_rate_hz,
                o.summary.capture_bytes,
                o.summary.tone_counts
            ),
            Err(e) => println!("{:<8} {:<8} failed: {e}", ch, cfg.radio.width.iw_token()),
        }
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }
    Ok(())
}
