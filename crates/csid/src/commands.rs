//! Subcommand implementations.

#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;

use crate::caps::{self, Envelope};
use crate::config::{ExperimentConfig, GlobalConfig};
use crate::thermal::{self, Verdict};
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
        // Same trap as in `bench`: shortening the session below the profile's
        // own segment length makes validate() reject the config outright. Drop
        // segmentation rather than fail — the operator asked for a short run,
        // and a session that fits in one segment does not need rotation.
        if cfg.capture.segment_duration.is_some_and(|seg| seg >= d) {
            cfg.capture.segment_duration = None;
        }
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
    // Say whether anything is KEPT. An operator reading this for the console
    // profile should not have to infer "writes nothing" from an absence, and an
    // operator reading it for a measurement profile should see the opposite
    // stated. `persist = false` on a profile whose data an experiment claims is
    // the mistake this line exists to make visible.
    println!(
        "records kept  : {}",
        if cfg.capture.persist {
            match cfg.capture.segment_duration {
                Some(seg) => format!(
                    "capture.raw, rolled every {}",
                    humantime_serde::re::humantime::format_duration(seg)
                ),
                None => "capture.raw (one file for the whole session)".to_string(),
            }
        } else {
            "NOTHING — stream only; the sidecar is the only artefact".to_string()
        }
    );
    println!("csiq on close : {}", cfg.export.on_close);
    println!(
        "ble co-capture: {}",
        if cfg.ble.enabled {
            let (i, w) = cfg.ble.hci_units();
            format!(
                "{} passive {:.0}/{:.0} ms ({i}/{w} units){} -> {}",
                cfg.ble.adapter,
                cfg.ble.scan_interval_ms,
                cfg.ble.scan_window_ms,
                if cfg.ble.required { ", REQUIRED" } else { "" },
                crate::ble::PARQUET_NAME,
            )
        } else {
            "disabled".into()
        }
    );
    println!(
        "time transfer : {}",
        if cfg.timesync.enabled {
            format!(
                "on {}{} -> {} (ftm window {} µs, one-way floor {} µs)",
                cfg.radio.monitor,
                if cfg.timesync.required {
                    ", REQUIRED"
                } else {
                    ""
                },
                crate::timesync::PARQUET_NAME,
                cfg.timesync.ftm_tolerance_us,
                cfg.timesync.one_way_floor_us,
            )
        } else {
            "disabled — `csid fleet skew` will have nothing to difference".into()
        }
    );

    if probe {
        println!("\n-- hardware probe --");
        if !radio::interface_exists(&cfg.radio.interface) {
            anyhow::bail!("interface {} does not exist", cfg.radio.interface);
        }
        if cfg.ble.enabled {
            let p = crate::hci::probe(&cfg.ble.adapter);
            println!("BLE adapter   : {} — {}", p.adapter, p.detail);
            if cfg.ble.required && !p.usable {
                anyhow::bail!(
                    "ble.required = true but {} is not usable: {}",
                    p.adapter,
                    p.detail
                );
            }
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

    // BLE co-capture readiness. Informational, never a failure: most sessions
    // do not use it, and the ones that do declare `ble.required` themselves.
    // A node that reports `usable` here will produce ble_rssi.parquet; one that
    // does not will produce a session with an honest `ble.status = failed`.
    let ble = crate::hci::probe("hci0");
    println!(
        "[{}] BLE adapter: {} — {}",
        if ble.usable { "ok" } else { "info" },
        ble.adapter,
        ble.detail
    );

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
                    crate::util::format_mac(&r.src_mac)
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

/// `csid thermal` — the node's current thermal state, for humans or for the
/// metrics pipeline.
///
/// ## Why this is a csid subcommand and not a shell script
///
/// The firmware throttle word is a bitmask whose meaning is not obvious and is
/// easy to get subtly wrong — in particular the distinction between the live
/// bits (0..3) and the sticky since-boot bits (16..19), which is exactly the
/// distinction that matters. On 2026-07-28 monad02 read `0x80000`: nothing
/// wrong at that instant, the soft temperature limit already engaged earlier.
/// A check that only tested the live half would have reported a healthy node.
///
/// That decode is implemented and unit-tested once, in [`crate::thermal`].
/// Emitting the Prometheus text from here means the metrics pipeline and the
/// bench harness cannot disagree about what a bit means.
///
/// ## Why textfile rather than an exporter
///
/// node_exporter's `textfile` collector reads a directory of `.prom` files on
/// each scrape. A timer writing this file costs two sysfs reads every 30 s,
/// entirely outside the capture process — no port, no daemon, and nothing that
/// can block, hold a lock, or share a core with the RX thread.
pub fn thermal_cmd(prometheus: bool, write: Option<PathBuf>) -> Result<()> {
    let temp = thermal::read_temp_c();
    let throttle = thermal::read_throttle();

    if !prometheus && write.is_none() {
        match temp {
            Some(c) => println!(
                "temperature: {c:.1} °C ({:+.1} °C to the {:.0} °C soft limit)",
                thermal::SOFT_LIMIT_C - c,
                thermal::SOFT_LIMIT_C
            ),
            None => println!("temperature: unavailable (no thermal zone)"),
        }
        match throttle {
            Some(t) => println!("throttle:    0x{:x} ({})", t.raw, t.describe()),
            None => println!("throttle:    unavailable"),
        }
        return Ok(());
    }

    let text = prometheus_text(temp, throttle);
    match write {
        // Atomic replace: node_exporter may scrape the directory at any moment,
        // and a half-written file is a parse error that blanks every series in
        // it rather than merely delaying them.
        Some(path) => {
            let tmp = path.with_extension("prom.tmp");
            std::fs::write(&tmp, text.as_bytes())
                .with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("installing {}", path.display()))?;
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// Render the node's thermal state as node_exporter textfile-collector input.
///
/// Absent readings emit NO sample rather than a zero. A missing series is
/// visibly missing in a dashboard; a fabricated `0 °C` looks like a working
/// sensor on an impossibly cold node.
fn prometheus_text(temp: Option<f32>, throttle: Option<thermal::Throttle>) -> String {
    use crate::thermal::Throttle;
    let mut s = String::new();

    if let Some(c) = temp {
        s.push_str("# HELP csid_node_temp_celsius SoC die temperature.\n");
        s.push_str("# TYPE csid_node_temp_celsius gauge\n");
        s.push_str(&format!("csid_node_temp_celsius {c:.3}\n"));

        s.push_str(
            "# HELP csid_node_thermal_headroom_celsius Degrees below the firmware soft throttle limit; negative means the clock is being reduced.\n",
        );
        s.push_str("# TYPE csid_node_thermal_headroom_celsius gauge\n");
        s.push_str(&format!(
            "csid_node_thermal_headroom_celsius {:.3}\n",
            thermal::SOFT_LIMIT_C - c
        ));
    }

    if let Some(t) = throttle {
        s.push_str(
            "# HELP csid_node_throttle_flags Raw firmware throttle word, as vcgencmd get_throttled.\n",
        );
        s.push_str("# TYPE csid_node_throttle_flags gauge\n");
        s.push_str(&format!("csid_node_throttle_flags {}\n", t.raw));

        s.push_str(
            "# HELP csid_node_throttled Firmware throttle conditions. window=now is live, window=since_boot is sticky.\n",
        );
        s.push_str("# TYPE csid_node_throttled gauge\n");
        for (mask, condition, window) in [
            (Throttle::UNDERVOLT_NOW, "undervolt", "now"),
            (Throttle::FREQ_CAPPED_NOW, "freq_capped", "now"),
            (Throttle::THROTTLED_NOW, "throttled", "now"),
            (Throttle::SOFT_LIMIT_NOW, "soft_limit", "now"),
            (Throttle::UNDERVOLT_SEEN, "undervolt", "since_boot"),
            (Throttle::FREQ_CAPPED_SEEN, "freq_capped", "since_boot"),
            (Throttle::THROTTLED_SEEN, "throttled", "since_boot"),
            (Throttle::SOFT_LIMIT_SEEN, "soft_limit", "since_boot"),
        ] {
            s.push_str(&format!(
                "csid_node_throttled{{condition=\"{condition}\",window=\"{window}\"}} {}\n",
                u8::from(t.bit(mask))
            ));
        }
    }
    s
}

/// How often the thermal sampler reads sysfs during a bench leg.
///
/// The die moves on a timescale of seconds under a step in load, so this is
/// comfortably faster than the thing it measures while still being nothing:
/// two small sysfs reads twice a second.
const THERMAL_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// `csid bench` — timed capture(s) reporting achieved rate, CSI mix, and the
/// thermal state the rate was achieved in.
///
/// This is the harness that produced the envelope in `csid caps`; pass several
/// channels to sweep them one after another.
///
/// ## Why a rate without a temperature is not a result
///
/// The nodes pin the `performance` governor so capture timing does not move
/// under the experiment. Above 80 °C the Pi 5 firmware takes the clock away
/// anyway, and `cpufreq` keeps reporting 2.4 GHz while it does — so the
/// governor's guarantee quietly stops holding with no OS-level symptom. A bench
/// leg that hit its rate on a node at 84 °C and one that hit it at 55 °C are
/// not the same measurement, and only one of them will still be true six hours
/// into an unattended run.
///
/// Every leg therefore carries a [`Verdict`], and the command's exit status
/// reflects the worst one — so this can gate a deployment rather than merely
/// inform it.
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

    // The state the node was in BEFORE any of this ran. A node that is already
    // at the soft limit while idle has a cooling fault, and no per-leg number
    // below will mean what it appears to mean.
    match (thermal::read_temp_c(), thermal::read_throttle()) {
        (Some(c), t) => {
            let flags = t.map(|t| t.describe()).unwrap_or_else(|| "unknown".into());
            println!("idle baseline: {c:.1} °C, throttle flags: {flags}");
            if c >= thermal::SAFE_PEAK_C {
                println!(
                    "  WARNING: {:.1} °C at idle leaves {:.1} °C to the {:.0} °C soft limit; \
                     a full-rate capture costs about 6 °C. Fix cooling before trusting these numbers.",
                    c,
                    thermal::SOFT_LIMIT_C - c,
                    thermal::SOFT_LIMIT_C,
                );
            }
        }
        (None, _) => println!("idle baseline: no thermal zone on this host"),
    }
    println!();

    println!(
        "{:<8} {:<8} {:>10} {:>12} {:>10} {:>8} {:>8} {:>10}  tones",
        "channel", "width", "records", "rate (Hz)", "bytes", "peak °C", "rise °C", "verdict"
    );

    let mut worst = Verdict::Pass;
    for ch in channels {
        let mut cfg = base.clone();
        cfg.radio.channel = ch;
        cfg.capture.duration = Some(duration);
        // A bench is a seconds-long probe, so segmentation is meaningless here —
        // and leaving the experiment's own value in place makes the run
        // UNRUNNABLE: validate() rejects `segment_duration >= duration`, which is
        // exactly what a 30 m segment against a 30 s bench is. Every profile that
        // segments (drift-24h, drift-overnight-illum, …) therefore failed
        // `csid bench` with a bare "configuration is invalid", while unsegmented
        // ones (smoke) passed — which reads as a band or width fault and is not
        // one. Observed 2026-08-12 on monad06 chasing a phantom 80 MHz bug.
        cfg.capture.segment_duration = None;
        cfg.experiment = Some(format!("{}-bench-ch{ch}", base.slug()));

        // Started before the session so the trace covers the radio setup too,
        // and finished after it so the peak is not missed by ending early.
        let sampler = thermal::Sampler::start(THERMAL_SAMPLE_INTERVAL);
        let result = engine::run_session(global, &cfg, stop.clone());
        let trace = sampler.finish();
        let verdict = trace.verdict();

        match result {
            Ok(o) => {
                let (peak, rise) = if trace.is_measured() {
                    (
                        format!("{:.1}", trace.max_c),
                        format!("{:+.1}", trace.max_c - trace.min_c),
                    )
                } else {
                    ("-".into(), "-".into())
                };
                println!(
                    "{:<8} {:<8} {:>10} {:>12.1} {:>10} {:>8} {:>8} {:>10} {:?}",
                    ch,
                    cfg.radio.width.iw_token(),
                    o.summary.records,
                    o.summary.mean_rate_hz,
                    o.summary.capture_bytes,
                    peak,
                    rise,
                    verdict.label(),
                    o.summary.tone_counts
                );
                if verdict != Verdict::Pass && verdict != Verdict::Unmeasured {
                    let after = trace
                        .throttle_after
                        .map(|t| t.describe())
                        .unwrap_or_else(|| "unknown".into());
                    println!(
                        "         headroom {:+.1} °C to the soft limit; degraded during run: {}; flags: {after}",
                        trace.headroom_c(),
                        trace.degraded_during,
                    );
                }
            }
            Err(e) => {
                println!("{:<8} {:<8} failed: {e}", ch, cfg.radio.width.iw_token());
                worst = Verdict::Fail;
            }
        }

        worst = worse_of(worst, verdict);
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }

    println!();
    println!("overall: {}", worst.label());
    // A harness whose job is to qualify a node must be usable from a script.
    // `Unmeasured` is not a failure — it is what a dev box legitimately reports.
    if worst == Verdict::Fail {
        anyhow::bail!(
            "node is not fit for an unattended capture: peak temperature reached the \
             {:.0} °C soft limit, the firmware degraded the clock, or the supply sagged",
            thermal::SOFT_LIMIT_C
        );
    }
    Ok(())
}

/// Fold two verdicts to the more serious one.
///
/// `Unmeasured` is absorbed by anything real: on a host with no thermal zone
/// every leg is unmeasured and the run reports that honestly, but one measured
/// failure among measured legs must not be diluted back to "unknown".
fn worse_of(a: Verdict, b: Verdict) -> Verdict {
    fn rank(v: Verdict) -> u8 {
        match v {
            Verdict::Unmeasured => 0,
            Verdict::Pass => 1,
            Verdict::Marginal => 2,
            Verdict::Fail => 3,
        }
    }
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// monad02's real state on 2026-07-28, rendered the way the scrape sees it.
    #[test]
    fn the_textfile_separates_live_throttling_from_its_history() {
        let text = prometheus_text(Some(81.5), Some(thermal::Throttle { raw: 0x8_0000 }));
        assert!(text.contains("csid_node_temp_celsius 81.500"));
        // 81.5 is past the soft limit, so headroom is negative.
        assert!(text.contains("csid_node_thermal_headroom_celsius -1.500"));
        assert!(text.contains("csid_node_throttle_flags 524288"));
        // The soft limit has fired at some point...
        assert!(
            text.contains("csid_node_throttled{condition=\"soft_limit\",window=\"since_boot\"} 1")
        );
        // ...but is not firing at this instant, and the supply is fine.
        assert!(text.contains("csid_node_throttled{condition=\"soft_limit\",window=\"now\"} 0"));
        assert!(
            text.contains("csid_node_throttled{condition=\"undervolt\",window=\"since_boot\"} 0")
        );
    }

    /// An unreadable sensor must produce no sample at all. A zero would render
    /// as a working sensor reporting 0 °C.
    #[test]
    fn an_absent_reading_emits_nothing_rather_than_a_zero() {
        let text = prometheus_text(None, None);
        assert!(text.is_empty(), "{text:?}");

        let only_temp = prometheus_text(Some(60.0), None);
        assert!(only_temp.contains("csid_node_temp_celsius 60.000"));
        assert!(!only_temp.contains("csid_node_throttle_flags"));
    }

    /// Every metric line must carry its HELP and TYPE, or the collector logs a
    /// parse warning and drops the file wholesale.
    #[test]
    fn every_emitted_family_is_declared() {
        let text = prometheus_text(Some(55.0), Some(thermal::Throttle { raw: 0 }));
        for family in [
            "csid_node_temp_celsius",
            "csid_node_thermal_headroom_celsius",
            "csid_node_throttle_flags",
            "csid_node_throttled",
        ] {
            assert!(text.contains(&format!("# HELP {family} ")), "{family}");
            assert!(text.contains(&format!("# TYPE {family} gauge")), "{family}");
        }
    }

    #[test]
    fn the_worst_verdict_wins_and_unmeasured_never_masks_a_real_one() {
        assert_eq!(
            worse_of(Verdict::Pass, Verdict::Marginal),
            Verdict::Marginal
        );
        assert_eq!(
            worse_of(Verdict::Marginal, Verdict::Pass),
            Verdict::Marginal
        );
        assert_eq!(worse_of(Verdict::Marginal, Verdict::Fail), Verdict::Fail);
        assert_eq!(worse_of(Verdict::Fail, Verdict::Pass), Verdict::Fail);
        // A dev box with no thermal zone reports Unmeasured throughout...
        assert_eq!(
            worse_of(Verdict::Unmeasured, Verdict::Unmeasured),
            Verdict::Unmeasured
        );
        // ...but a single measured verdict takes precedence over it.
        assert_eq!(worse_of(Verdict::Unmeasured, Verdict::Pass), Verdict::Pass);
        assert_eq!(worse_of(Verdict::Fail, Verdict::Unmeasured), Verdict::Fail);
    }
}
