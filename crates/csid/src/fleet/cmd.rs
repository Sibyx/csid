//! The cockpit's subcommand implementations — the thin layer between `clap`
//! and the tested logic.
//!
//! Exit status is load-bearing here. Every command that adjudicates something
//! exits non-zero when the adjudication is not a clean pass, so the cockpit can
//! gate a shell script (`csid fleet status && csid fleet marker …`) rather than
//! merely inform a human. **UNKNOWN exits non-zero too**: a script must not
//! proceed on an unmeasured fleet any more than an operator should.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use super::gates::{self, GateVerdict};
use super::health::State;
use super::probe::{self, ProbeOptions};
use super::render::Palette;
use super::{clock, ssh, FleetConfig, StatusOptions};
use crate::marker::Marker;

/// The exit code for "measured, and it failed".
pub const EXIT_FAIL: i32 = 1;
/// The exit code for "not measured". Distinct from a failure so a wrapper can
/// tell "the fleet is broken" from "the cockpit could not see the fleet".
pub const EXIT_UNKNOWN: i32 = 3;

fn exit_for(state: State) -> i32 {
    match state {
        State::Ok => 0,
        State::Warn => 0,
        State::Unknown => EXIT_UNKNOWN,
        State::Fail => EXIT_FAIL,
    }
}

fn exit_for_gate(v: GateVerdict) -> i32 {
    match v {
        GateVerdict::Pass => 0,
        GateVerdict::Fail => EXIT_FAIL,
        GateVerdict::Unknown => EXIT_UNKNOWN,
    }
}

fn require_nodes(nodes: &[ssh::NodeSpec]) -> Result<()> {
    if nodes.is_empty() {
        anyhow::bail!(
            "no nodes selected.\n\
             Either pass --nodes monad01,monad02 or configure the fleet:\n\n  \
             # ~/.config/csid/config.toml (bench laptop) or /etc/csid/config.toml\n  \
             [fleet]\n  \
             nodes = [\"monad01\", \"monad02\", \"monad03\"]\n  \
             user  = \"monad\"\n"
        );
    }
    Ok(())
}

/// `csid fleet probe` — the node-local health readout.
pub fn probe_cmd(
    spool: PathBuf,
    window_s: f64,
    src_mac: Option<String>,
    class: Option<String>,
    session: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let report = probe::probe(&ProbeOptions {
        spool,
        window_s,
        src_mac,
        class,
        session,
        ..ProbeOptions::default()
    })?;

    if json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }

    let mut health = report.health.clone();
    let state = health.grade(&super::health::Budgets::default());
    println!("{} {}  {}", state.mark(), health.host, state.label());
    println!(
        "  session   : {}",
        health.session_id.as_deref().unwrap_or("(none)")
    );
    println!("  capturing : {}", health.capture_alive);
    if let Some(s) = &health.scope {
        println!(
            "  scope     : {} from {} ({} of {} records in the window)",
            s.class, s.src_mac, s.scoped_records, s.window_records
        );
    }
    match &health.delivered_hz {
        Some(c) => println!(
            "  rate      : {} Hz (n = {} one-second bins)",
            c.render(1),
            c.n
        ),
        None => println!("  rate      : not measurable"),
    }
    match &health.interarrival_cv {
        Some(c) => println!("  gap CV    : {} (n = {} gaps)", c.render(3), c.n),
        None => println!("  gap CV    : not measurable"),
    }
    if let Some(b) = &health.ble {
        println!(
            "  ble       : {} — {:.1} obs/s, {} observation(s), max gap {:.1} s",
            b.status, b.rate_hz, b.observations, b.max_gap_s
        );
    }
    if let Some(d) = &health.disk {
        let left = d
            .hours_left
            .map(|h| format!(", ~{h:.1} h left at this rate"))
            .unwrap_or_default();
        println!(
            "  disk      : {:.1} of {:.1} GB free{left}",
            d.free_gb, d.total_gb
        );
    }
    if let Some(t) = &health.thermal {
        println!(
            "  thermal   : {} — {}",
            t.temp_c
                .map(|c| format!("{c:.1} °C"))
                .unwrap_or_else(|| "unavailable".into()),
            t.detail
        );
    }
    for n in &health.notes {
        println!("  -> {n}");
    }
    std::process::exit(exit_for(state));
}

/// `csid fleet status` — the sweep. The single most-used command of a session.
pub fn status_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    o: StatusOptions,
    json: bool,
    no_color: bool,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let report = super::status(cfg, &nodes, &o);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            super::render::fleet_table(&report, &Palette::detect(no_color))
        );
    }
    std::process::exit(exit_for(report.state()));
}

/// `csid fleet clock` — is the fleet one timebase?
pub fn clock_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    samples: usize,
    json: bool,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let budget_ns = cfg.clock_budget_ms * 1_000_000;
    let estimates = super::clock_sweep(cfg, &nodes, samples);
    let skew = clock::fleet_skew(&estimates, budget_ns);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "nodes": estimates.iter().map(|(n, c)| serde_json::json!({
                    "host": n, "clock": c
                })).collect::<Vec<_>>(),
                "skew": skew,
            }))?
        );
        std::process::exit(if skew.within_budget() {
            0
        } else if skew.unmeasured.is_empty() {
            EXIT_FAIL
        } else {
            EXIT_UNKNOWN
        });
    }

    println!(
        "csid fleet clock — {} node(s), {samples} round trip(s) each\n",
        nodes.len()
    );
    println!(
        "{:<3} {:<9} {:>12} {:>12} {:>8}  time daemon",
        "", "node", "offset ms", "± ms", "samples"
    );
    println!("{}", "-".repeat(72));

    for (host, est) in &estimates {
        match est {
            None => println!(
                "{:<3} {:<9} {:>12} {:>12} {:>8}  not measured",
                "??", host, "—", "—", 0
            ),
            Some(c) => {
                let inside = c.worst_case_ns() <= budget_ns;
                let daemon = c
                    .ntp
                    .as_ref()
                    .map(|n| {
                        format!(
                            "{} {}{}",
                            n.daemon,
                            if n.synchronised {
                                "synced"
                            } else {
                                "NOT SYNCED"
                            },
                            n.stratum
                                .map(|s| format!(" stratum {s}"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "none".into());
                println!(
                    "{:<3} {:<9} {:>12.3} {:>12.3} {:>8}  {}",
                    if inside { "OK" } else { "XX" },
                    host,
                    c.offset_ns as f64 / 1e6,
                    c.uncertainty_ns as f64 / 1e6,
                    c.samples,
                    daemon,
                );
            }
        }
    }

    println!();
    println!("Offsets are node-minus-cockpit from a four-timestamp exchange, minimum-delay");
    println!("sample kept; ± is half the round-trip delay, widened to the node's own root");
    println!("dispersion where that is worse. The COCKPIT clock cancels out of the node-to-node");
    println!("skew below, so it never has to be right — only stable across the sweep.");
    println!();
    println!("THIS IS THE FALLBACK INSTRUMENT. Its uncertainty is bounded by ssh round-trip time,");
    println!("which no amount of sampling improves. When a stamped transmitter is on the air, run");
    println!("`csid fleet skew` instead: it differences two nodes' receive stamps for the SAME");
    println!("frame, so the transmit instant cancels exactly and the bound is µs, not ms. Where");
    println!("both have a number for the same window, `csid fleet skew` decides.");
    println!();
    println!("{}", skew.render());
    println!();
    if skew.within_budget() {
        println!(
            "=== FLEET IS TIME-COHERENT within the {} ms budget ===",
            cfg.clock_budget_ms
        );
    } else {
        println!(
            "=== FLEET IS NOT CERTIFIED TIME-COHERENT ===\n\
             A skewed fleet corrupts every cross-node comparison, and nothing in the capture\n\
             path would notice: the captures look immaculate and the analysis is wrong.\n\
             Remedy: on the offending node, `chronyc tracking` (or `timedatectl status`), confirm\n\
             it reaches its NTP source over the tailnet, then re-run. Do NOT proceed to the\n\
             delivered-rate gate until this passes — every later gate would measure nothing."
        );
    }
    std::process::exit(if skew.within_budget() {
        0
    } else if skew.unmeasured.is_empty() {
        EXIT_FAIL
    } else {
        EXIT_UNKNOWN
    });
}

/// `csid timesync report` — the node-local time-transfer readout.
///
/// Runs ON a node (the cockpit calls it over ssh), so it must not need the
/// cockpit's fleet configuration.
pub fn timesync_report_cmd(
    spool: PathBuf,
    session: Option<PathBuf>,
    window_s: f64,
    json: bool,
) -> Result<()> {
    let (dir, sidecar) = match session {
        Some(d) => {
            let text = std::fs::read_to_string(d.join("metadata.json"))
                .with_context(|| format!("reading {}", d.join("metadata.json").display()))?;
            (d.clone(), serde_json::from_str(&text)?)
        }
        None => probe::newest_session(&spool).with_context(|| {
            format!(
                "no session found under {} — time transfer is recorded inside a capture. \
                 Start the capture first.",
                spool.display()
            )
        })?,
    };

    let log = dir.join(crate::timesync::NDJSON_NAME);
    if !log.is_file() {
        anyhow::bail!(
            "{} has no {} — was `[timesync].enabled = true` in the experiment profile?\n\
             Without it this node records no transmit stamps and cannot contribute to \
             `csid fleet skew`.",
            dir.display(),
            crate::timesync::NDJSON_NAME
        );
    }
    let (rows, malformed) = crate::timesync::read_log(&log)?;
    let floor_ns = sidecar
        .timesync
        .as_ref()
        .map(|m| m.one_way_floor_us * 1_000)
        .unwrap_or(crate::timesync::affine::D_FLOOR_DEFAULT_NS);

    let report = crate::timesync::report(
        sidecar
            .environment
            .hostname
            .as_deref()
            .unwrap_or(&sidecar.session_id),
        &sidecar.session_id,
        &rows,
        window_s,
        crate::util::now_unix_ns(),
        floor_ns,
    );

    if json {
        println!("{}", serde_json::to_string(&report)?);
        // A node with no rows in the window is not a failure of the command —
        // the cockpit needs the empty report to mark the node silent.
        return Ok(());
    }

    println!("{} — session {}", report.host, report.session_id);
    println!(
        "  window    : {:.0} s ({} row(s) of {} in the log)",
        report.window_s,
        report.rows,
        rows.len()
    );
    println!(
        "  stamps    : {}",
        if report.stamp_sources.is_empty() {
            "none".into()
        } else {
            report.stamp_sources.join("+")
        }
    );
    println!("  csid / app: {} / {}", report.rows_csid, report.rows_app);
    if malformed > 0 {
        println!("  malformed : {malformed} log line(s) skipped");
    }
    for (tx, arrivals) in &report.arrivals {
        println!("  tx {tx}: {} packet(s)", arrivals.len());
    }
    for f in &report.app_fits {
        println!("  {}", f.render().replace('\n', "\n  "));
    }
    if report.rows == 0 {
        println!(
            "\n  NO STAMPED FRAMES in this window. Check, in order: the illuminator is running \
             (`csid\n  fleet status`), this node is tuned to its channel, and the experiment \
             SSID is OPEN —\n  an encrypted SSID hides the phone's stamps from a monitor-mode \
             receiver entirely."
        );
    }
    Ok(())
}

/// `csid timesync export` — rebuild `time_transfer.parquet` from the durable log.
///
/// The recovery path for a session that lost power before its close-time
/// export, and the way to re-pair `ftm` after a `capture.raw` was restored.
pub fn timesync_export_cmd(session: PathBuf, tolerance_us: u64) -> Result<()> {
    let log = session.join(crate::timesync::NDJSON_NAME);
    let (mut rows, malformed) = crate::timesync::read_log(&log)?;
    println!(
        "{}: {} row(s){}",
        log.display(),
        rows.len(),
        if malformed > 0 {
            format!(", {malformed} malformed line(s) skipped")
        } else {
            String::new()
        }
    );

    let sidecar_path = session.join("metadata.json");
    let sidecar: crate::sidecar::Sidecar = serde_json::from_str(
        &std::fs::read_to_string(&sidecar_path)
            .with_context(|| format!("reading {}", sidecar_path.display()))?,
    )?;

    // ftm pairing needs the raw capture. A session whose raw was pruned after a
    // verified sync still exports — with every `ftm` null, which is the honest
    // outcome rather than a silent failure.
    let raw = session.join("capture.raw");
    let paired = if raw.is_file() {
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path)?)?;
        let width = crate::export::width_from_sidecar(&value).unwrap_or(csiq::Width::Unknown(0));
        let macs: std::collections::HashSet<[u8; 6]> = rows
            .iter()
            .filter_map(|r| crate::timesync::payload::parse_mac(&r.tx_mac))
            .collect();
        let file = std::fs::File::open(&raw)?;
        let mut rr = csiq::raw::RawReader::new(std::io::BufReader::new(file), width);
        let mut ticks = Vec::new();
        while let Ok(Some(rec)) = rr.next_record() {
            if rec.unix_ts_ns != 0 && macs.contains(&rec.src_mac) {
                ticks.push(crate::timesync::CsiTick {
                    unix_ts_ns: rec.unix_ts_ns,
                    ftm: rec.ftm,
                    src_mac: rec.src_mac,
                });
            }
        }
        crate::timesync::pair_ftm(&mut rows, &ticks, tolerance_us.saturating_mul(1000))
    } else {
        println!(
            "  {} is absent (pruned after sync?) — every ftm will be null",
            raw.display()
        );
        0
    };

    let out = session.join(crate::timesync::PARQUET_NAME);
    let stats = crate::timesync::write_parquet(
        &rows,
        &out,
        &crate::timesync::ParquetContext {
            host: sidecar
                .environment
                .hostname
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            session_id: sidecar.session_id.clone(),
        },
    )?;
    println!(
        "wrote {} — {} row(s) ({} csid, {} app), {} with a paired ftm, {} transmitter(s)",
        out.display(),
        stats.rows,
        stats.rows_csid,
        stats.rows_app,
        paired,
        stats.distinct_transmitters
    );
    Ok(())
}

/// `csid fleet skew` — inter-node skew through the illumination stream.
pub fn fleet_skew_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    window_s: f64,
    min_common: usize,
    tx: Option<String>,
    json: bool,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let budget_ns = cfg.clock_budget_ms * 1_000_000;

    let reports = super::timesync_sweep(cfg, &nodes, window_s);
    let Some(tx_id) = tx.or_else(|| super::dominant_transmitter(&reports)) else {
        // Nothing to measure on. That is a real answer, and it is not "coherent".
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "window_s": window_s,
                    "transmitter": null,
                    "nodes": reports.iter().map(|(h, r)| serde_json::json!({
                        "host": h,
                        "report": r.as_ref().ok(),
                        "error": r.as_ref().err(),
                    })).collect::<Vec<_>>(),
                }))?
            );
        } else {
            println!(
                "csid fleet skew — {} node(s), {window_s:.0} s window\n\n\
                 No node reported a stamped transmitter in this window, so there is nothing to\n\
                 difference and the fleet is UNMEASURED (which is not the same as coherent).\n\n\
                 Check, in order:\n  \
                 1. an illuminator is running          `csid fleet status`\n  \
                 2. `[timesync].enabled = true`        in the experiment profile on each node\n  \
                 3. the nodes are on its channel       `csid fleet probe` scope line\n  \
                 4. the experiment SSID is OPEN        an encrypted SSID hides the phone's\n     \
                    stamps from a monitor-mode receiver entirely (`collectord` still sees them)\n\n\
                 Fallback instrument: `csid fleet clock` (ssh round trip, millisecond-bounded).",
                nodes.len()
            );
        }
        std::process::exit(EXIT_UNKNOWN);
    };

    let (report, unreachable) = super::transfer_report(&reports, &tx_id, budget_ns, min_common);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "window_s": window_s,
                "transmitter": tx_id,
                "skew": report,
                "unreachable": unreachable,
                "app_fits": reports.iter().filter_map(|(h, r)| r.as_ref().ok().map(|r| {
                    serde_json::json!({ "host": h, "fits": r.app_fits })
                })).collect::<Vec<_>>(),
                "authority": super::SKEW_AUTHORITY,
            }))?
        );
    } else {
        print!(
            "{}",
            super::render_transfer(&reports, &report, &unreachable, window_s)
        );
        println!();
        if report.within_budget() && unreachable.is_empty() {
            println!(
                "=== FLEET IS TIME-COHERENT within the {} ms budget (measured on the air) ===",
                cfg.clock_budget_ms
            );
        } else {
            println!(
                "=== FLEET IS NOT CERTIFIED TIME-COHERENT ===\n\
                 A skewed fleet corrupts every cross-node comparison and nothing in the capture\n\
                 path would notice. Remedy: `chronyc tracking` on the offending node, confirm it\n\
                 reaches the fleet reference (roles/chrony — one local reference, not the public\n\
                 pool), then re-run."
            );
        }
    }

    let certified = report.within_budget() && unreachable.is_empty();
    std::process::exit(if certified {
        0
    } else if report.pairs.is_empty() || !unreachable.is_empty() || !report.silent.is_empty() {
        EXIT_UNKNOWN
    } else {
        EXIT_FAIL
    });
}

/// `csid fleet gate g1|g2` — evaluate a registered rate/cadence gate.
pub fn gate_rate_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    which: &str,
    o: StatusOptions,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let runner = cfg.runner();
    let cmd = format!(
        "{} {} fleet probe --json --window {}{}{}",
        cfg.sudo,
        cfg.remote_csid,
        o.window_s,
        o.src_mac
            .as_ref()
            .map(|m| format!(" --src-mac {}", ssh::shell_quote(m)))
            .unwrap_or_default(),
        o.class
            .as_ref()
            .map(|c| format!(" --class {}", ssh::shell_quote(c)))
            .unwrap_or_default(),
    );

    println!(
        "Gate {} — pre-registration v2 §3 (frozen 2026-08-08, \
         SHA-256 542e0d88…1aae021c4)\n",
        which.to_uppercase()
    );

    let mut worst = GateVerdict::Pass;
    for (node, out) in runner.run_all(&nodes, &cmd) {
        let outcome =
            match out.require().map_err(|e| e.summary()).and_then(|s| {
                serde_json::from_str::<probe::ProbeReport>(s).map_err(|e| e.to_string())
            }) {
                Err(why) => {
                    let mut g = if which == "g1" {
                        gates::g1_delivered_rate(&node.name, &[], gates::G1_FLOOR_HZ)
                    } else {
                        gates::g2_interarrival_cv(&node.name, &[], gates::G2_CV_CEILING)
                    };
                    g.verdict = GateVerdict::Unknown;
                    g.detail.insert(0, format!("node not measured — {why}"));
                    g
                }
                Ok(r) => {
                    let scope = r
                        .health
                        .scope
                        .as_ref()
                        .map(|s| format!("{} / {} {}", node.name, s.class, s.src_mac))
                        .unwrap_or_else(|| node.name.clone());
                    if which == "g1" {
                        gates::g1_delivered_rate(&scope, &r.arrival_s, gates::G1_FLOOR_HZ)
                    } else {
                        gates::g2_interarrival_cv(&scope, &r.arrival_s, gates::G2_CV_CEILING)
                    }
                }
            };
        print!("{}", outcome.render(if which == "g1" { 1 } else { 3 }));
        println!();
        worst = worst.worse(outcome.verdict);
    }

    println!(
        "=== {} overall: {} ===",
        which.to_uppercase(),
        worst.label()
    );
    std::process::exit(exit_for_gate(worst));
}

/// `csid fleet gate g3` — the Day-0 5 GHz `iax` AP stability probe.
///
/// The command brings up the AP unit, runs the capture for the registered
/// window, then adjudicates all four conditions. It **cannot** verify the
/// firmware survived without reading the journal, so a journal it could not
/// read is reported as UNKNOWN, never as zero resets.
#[allow(clippy::too_many_arguments)]
pub fn gate_g3_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    experiment: &str,
    duration: Duration,
    ap_node: Option<String>,
    ap_unit: Option<String>,
    ap_interface: Option<String>,
    assume_ap_up: bool,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let runner = cfg.runner();

    let ap_name = ap_node
        .or_else(|| cfg.ap_node.clone())
        .context("G3 needs an AP node: pass --ap-node or set [fleet] ap_node")?;
    let ap = ssh::NodeSpec {
        addr: cfg
            .addresses
            .get(&ap_name)
            .cloned()
            .unwrap_or_else(|| ap_name.clone()),
        name: ap_name.clone(),
        user: None,
    };
    let unit = ap_unit
        .or_else(|| cfg.ap_unit.clone())
        .unwrap_or_else(|| "hostapd".to_string());
    let iface = ap_interface
        .or_else(|| cfg.ap_interface.clone())
        .unwrap_or_else(|| "wlan1".to_string());

    println!(
        "G3 — Day-0 5 GHz iax AP stability probe\n\
         AP node   : {ap_name} (unit {unit}, interface {iface})\n\
         Rx nodes  : {}\n\
         Window    : {:.0} min (registered minimum {} min)\n",
        nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        duration.as_secs_f64() / 60.0,
        gates::G3_MIN_PROBE_S / 60,
    );

    // 0. Mark the journal cursor before anything starts, so the reset count is
    //    scoped to the probe and not to the node's whole uptime.
    let cursor_cmd = "date -u +%Y-%m-%d\\ %H:%M:%S";
    let cursors: Vec<(String, String)> = runner
        .run_all(&nodes, cursor_cmd)
        .into_iter()
        .map(|(n, o)| (n.name, o.stdout.trim().to_string()))
        .collect();

    // 1. Bring the AP up.
    if assume_ap_up {
        println!("[skip] AP bring-up (--assume-ap-up): the operator has the AP running already");
    } else {
        let out = runner.run(
            &ap,
            &format!("{} systemctl start {}", cfg.sudo, ssh::shell_quote(&unit)),
        );
        if !out.ok() {
            anyhow::bail!(
                "could not start the 5 GHz AP unit {unit} on {ap_name}: {}",
                out.require().err().map(|e| e.summary()).unwrap_or_default()
            );
        }
        println!("[ok]   AP unit {unit} started on {ap_name}");
    }

    // 2. Start the captures.
    let started = super::start(cfg, &nodes, experiment, Duration::from_secs(30));
    print!("{}", super::render_lifecycle("start", &started));
    let confirmed: Vec<ssh::NodeSpec> = nodes
        .iter()
        .filter(|n| {
            started
                .iter()
                .any(|(h, o)| h == &n.name && o.is_confirmed())
        })
        .cloned()
        .collect();
    if confirmed.is_empty() {
        anyhow::bail!("no node confirmed a capture; the probe cannot run");
    }

    // 3. Hold the window, sampling the AP's associated-station count. A phone
    //    that drops off mid-probe fails condition (i-b), and the minimum over
    //    the samples is what catches it — a final check would not.
    println!("\nholding the probe window; sampling AP associations every 30 s…");
    let station_cmd = format!(
        "iw dev {} station dump | grep -c '^Station' || true",
        ssh::shell_quote(&iface)
    );
    let mut min_stations: Option<u32> = None;
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(
            Duration::from_secs(30)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
        let out = runner.run(&ap, &station_cmd);
        if let Ok(s) = out.require() {
            if let Ok(n) = s.trim().parse::<u32>() {
                min_stations = Some(min_stations.map_or(n, |m: u32| m.min(n)));
            }
        }
    }

    // 4. Read the journal for iax resets, scoped to the probe.
    let mut resets: std::collections::BTreeMap<String, Option<u32>> = Default::default();
    for (node, cursor) in &cursors {
        let spec = ssh::NodeSpec {
            addr: cfg
                .addresses
                .get(node)
                .cloned()
                .unwrap_or_else(|| node.clone()),
            name: node.clone(),
            user: None,
        };
        let cmd = format!(
            "{} journalctl -k --since {} --no-pager 2>/dev/null | \
             grep -icE 'iwl(wifi|mvm).*(fw error|hw error|restart|reset|microcode SW error|\
             failed to load|NMI)' || true",
            cfg.sudo,
            ssh::shell_quote(cursor)
        );
        let out = runner.run(&spec, &cmd);
        resets.insert(
            node.clone(),
            out.require()
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok()),
        );
    }

    // 5. G1 and G2 on the probe capture, then adjudicate.
    let probe_cmd = format!(
        "{} {} fleet probe --json --window 60",
        cfg.sudo, cfg.remote_csid
    );
    let mut worst = GateVerdict::Pass;
    for (node, out) in runner.run_all(&confirmed, &probe_cmd) {
        let (g1, g2) = match out
            .require()
            .ok()
            .and_then(|s| serde_json::from_str::<probe::ProbeReport>(s).ok())
        {
            None => (None, None),
            Some(r) => (
                Some(
                    gates::g1_delivered_rate(&node.name, &r.arrival_s, gates::G1_FLOOR_HZ).verdict,
                ),
                Some(
                    gates::g2_interarrival_cv(&node.name, &r.arrival_s, gates::G2_CV_CEILING)
                        .verdict,
                ),
            ),
        };
        let outcome = gates::g3_ap_stability(
            &format!("{ap_name} (AP) -> {}", node.name),
            &gates::G3Conditions {
                probe_seconds: Some(duration.as_secs()),
                min_associated_stations: min_stations,
                driver_resets: resets.get(&node.name).copied().flatten(),
                g1,
                g2,
            },
        );
        print!("{}", outcome.render(1));
        println!();
        worst = worst.worse(outcome.verdict);
    }

    print!(
        "{}",
        super::render_lifecycle("stop", &super::stop(cfg, &nodes, experiment))
    );
    println!("\n=== G3 overall: {} ===", worst.label());
    if worst != GateVerdict::Pass {
        println!("\n{}", gates::G3_FALLBACK);
    }
    std::process::exit(exit_for_gate(worst));
}

/// `csid marker` — the node-local marker write.
#[allow(clippy::too_many_arguments)]
pub fn marker_cmd(
    spool: PathBuf,
    block_id: String,
    zone: String,
    kind: String,
    phase: String,
    level: Option<u32>,
    sub_condition: Option<String>,
    note: Option<String>,
    json: bool,
) -> Result<()> {
    let (dir, sidecar) = probe::newest_session(&spool).with_context(|| {
        format!(
            "no session found under {} — a marker has to land in a capture. \
             Start the capture first.",
            spool.display()
        )
    })?;

    let host = sidecar
        .environment
        .hostname
        .clone()
        .or_else(|| crate::util::run_opt("hostname", &[]))
        .unwrap_or_else(|| "unknown".into());

    let marker = Marker::now(
        host,
        sidecar.session_id.clone(),
        block_id,
        zone,
        kind,
        phase,
        level,
        sub_condition,
        note,
    );
    crate::marker::append(&dir, &marker)?;

    if json {
        println!("{}", marker.to_line()?);
    } else {
        println!("{}", marker.render());
        println!("  -> {}", dir.join(crate::marker::MARKER_FILE).display());
    }
    Ok(())
}

/// `csid fleet marker` — broadcast a marker to every node.
#[allow(clippy::too_many_arguments)]
pub fn fleet_marker_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    block_id: &str,
    zone: &str,
    kind: &str,
    phase: &str,
    level: Option<u32>,
    sub_condition: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;

    let mut args = format!(
        "--block-id {} --zone {} --kind {} --phase {}",
        ssh::shell_quote(block_id),
        ssh::shell_quote(zone),
        ssh::shell_quote(kind),
        ssh::shell_quote(phase),
    );
    if let Some(l) = level {
        args.push_str(&format!(" --level {l}"));
    }
    if let Some(s) = sub_condition {
        args.push_str(&format!(" --sub-condition {}", ssh::shell_quote(s)));
    }
    if let Some(n) = note {
        args.push_str(&format!(" --note {}", ssh::shell_quote(n)));
    }

    let results = super::broadcast_marker(cfg, &nodes, &args);
    let all_marked = results.iter().all(|(_, r)| r.is_ok());
    print!(
        "{}",
        super::render_broadcast(&results, cfg.clock_budget_ms * 1_000_000)
    );
    std::process::exit(if all_marked { 0 } else { EXIT_UNKNOWN });
}

/// `csid fleet start` / `stop`.
pub fn lifecycle_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    experiment: &str,
    stop: bool,
    confirm_s: u64,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let results = if stop {
        super::stop(cfg, &nodes, experiment)
    } else {
        super::start(cfg, &nodes, experiment, Duration::from_secs(confirm_s))
    };
    print!(
        "{}",
        super::render_lifecycle(if stop { "stop" } else { "start" }, &results)
    );
    let all = results.iter().all(|(_, o)| o.is_confirmed());
    std::process::exit(if all { 0 } else { EXIT_FAIL });
}

/// `csid fleet runbook day0` — walk the documented bring-up, stop at the first
/// failure, print the plan's own remedy text.
pub fn runbook_day0_cmd(
    cfg: &FleetConfig,
    selection: Option<&str>,
    experiment: &str,
    adapter: &str,
    ble_sanity_s: u64,
) -> Result<()> {
    let nodes = cfg.select(selection);
    require_nodes(&nodes)?;
    let runner = cfg.runner();
    let steps = super::day0_steps(cfg, experiment, adapter);

    println!(
        "Day-0 runbook — {} node(s): {}\n\
         Source: experiments/Lab Session Plan 2026-08.md (Day 0) + prereg v2 §3, §11.\n\
         This stops at the FIRST failure. Fix it, then re-run.\n",
        nodes.len(),
        nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    for (i, step) in steps.iter().enumerate() {
        println!("──────────────────────────────────────────────────────────────");
        println!("{}", step.title);

        let failures: Vec<String> = match (i, &step.command) {
            // Remote command steps: every node must pass.
            (_, Some(cmd)) => runner
                .run_all(&nodes, cmd)
                .into_iter()
                .filter_map(|(n, out)| match out.require() {
                    Ok(_) => {
                        println!("  OK  {}", n.name);
                        None
                    }
                    Err(why) => {
                        println!("  XX  {} — {}", n.name, why.summary());
                        Some(n.name)
                    }
                })
                .collect(),

            // Step 4: the BLE sanity window.
            (3, None) => {
                println!("  holding {ble_sanity_s} s while the scanner runs…");
                std::thread::sleep(Duration::from_secs(ble_sanity_s));
                let cmd = format!(
                    "{} {} fleet probe --json --window {ble_sanity_s}",
                    cfg.sudo, cfg.remote_csid
                );
                runner
                    .run_all(&nodes, &cmd)
                    .into_iter()
                    .filter_map(|(n, out)| {
                        let ble = out
                            .require()
                            .ok()
                            .and_then(|s| serde_json::from_str::<probe::ProbeReport>(s).ok())
                            .and_then(|r| r.health.ble);
                        match ble {
                            Some(b) if b.rate_hz > 0.0 && b.status != "failed" => {
                                println!(
                                    "  OK  {} — {:.1} obs/s, {} observation(s)",
                                    n.name, b.rate_hz, b.observations
                                );
                                None
                            }
                            Some(b) => {
                                println!(
                                    "  XX  {} — BLE {} at {:.1} obs/s ({} observation(s))",
                                    n.name, b.status, b.rate_hz, b.observations
                                );
                                Some(n.name)
                            }
                            None => {
                                println!("  ??  {} — no BLE stream to read", n.name);
                                Some(n.name)
                            }
                        }
                    })
                    .collect()
            }

            // Step 5: the fleet clock check — before the rate gate, on purpose.
            (4, None) => {
                let budget_ns = cfg.clock_budget_ms * 1_000_000;
                let est = super::clock_sweep(cfg, &nodes, 5);
                for (host, c) in &est {
                    match c {
                        Some(c) => println!(
                            "  {}  {} — {:+.3} ms ± {:.3} ms ({} sample(s))",
                            if c.worst_case_ns() <= budget_ns {
                                "OK"
                            } else {
                                "XX"
                            },
                            host,
                            c.offset_ns as f64 / 1e6,
                            c.uncertainty_ns as f64 / 1e6,
                            c.samples
                        ),
                        None => println!("  ??  {host} — clock not measured"),
                    }
                }
                let skew = clock::fleet_skew(&est, budget_ns);
                println!("  {}", skew.render());
                if skew.within_budget() {
                    Vec::new()
                } else {
                    est.iter()
                        .filter(|(_, c)| c.as_ref().is_none_or(|c| c.worst_case_ns() > budget_ns))
                        .map(|(h, _)| h.clone())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .chain(std::iter::once("fleet".to_string()))
                        .collect()
                }
            }

            // Step 6: the delivered-rate gate.
            (5, None) => {
                let cmd = format!(
                    "{} {} fleet probe --json --window 60",
                    cfg.sudo, cfg.remote_csid
                );
                runner
                    .run_all(&nodes, &cmd)
                    .into_iter()
                    .filter_map(|(n, out)| {
                        let arrivals = out
                            .require()
                            .ok()
                            .and_then(|s| serde_json::from_str::<probe::ProbeReport>(s).ok())
                            .map(|r| r.arrival_s)
                            .unwrap_or_default();
                        let g1 = gates::g1_delivered_rate(&n.name, &arrivals, gates::G1_FLOOR_HZ);
                        let g2 =
                            gates::g2_interarrival_cv(&n.name, &arrivals, gates::G2_CV_CEILING);
                        print!("{}", g1.render(1));
                        print!("{}", g2.render(3));
                        (g1.verdict != GateVerdict::Pass || g2.verdict != GateVerdict::Pass)
                            .then_some(n.name)
                    })
                    .collect()
            }

            (_, None) => Vec::new(),
        };

        if !failures.is_empty() {
            println!("\n  FAILED on: {}\n", failures.join(", "));
            println!("  REMEDY (from the plan of record):");
            for line in wrap(step.remedy, 76) {
                println!("    {line}");
            }
            println!("\n=== Day-0 runbook STOPPED at step {} ===", i + 1);
            std::process::exit(EXIT_FAIL);
        }
        println!();
    }

    println!("=== Day-0 runbook COMPLETE — all steps passed on every node ===");
    println!("Next: `csid fleet gate g3` for the 5 GHz probe, if the band matrix needs it.");
    Ok(())
}

/// Wrap prose to a width, so a long remedy is readable in a terminal.
///
/// Lines that already fit are emitted verbatim, and a wrapped line keeps its
/// own indentation on the continuation. The remedy strings are quoted from the
/// plan and several are already laid out (the A1/A2 abort text is an indented
/// list); re-flowing those from scratch turned "STOP AND FIX; no staged capture
/// proceeds" into a ragged mess at the exact moment the operator most needs to
/// read it.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.len() <= width {
            out.push(para.to_string());
            continue;
        }
        let indent: String = para.chars().take_while(|c| c.is_whitespace()).collect();
        let mut line = indent.clone();
        let mut has_word = false;
        for word in para.split_whitespace() {
            if has_word && line.len() + 1 + word.len() > width {
                out.push(std::mem::replace(&mut line, indent.clone()));
                has_word = false;
            }
            if has_word {
                line.push(' ');
            }
            line.push_str(word);
            has_word = true;
        }
        if has_word {
            out.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_distinguish_broken_from_unseen() {
        assert_eq!(exit_for(State::Ok), 0);
        assert_eq!(exit_for(State::Warn), 0);
        assert_eq!(exit_for(State::Fail), EXIT_FAIL);
        assert_eq!(
            exit_for(State::Unknown),
            EXIT_UNKNOWN,
            "a script must be able to tell 'the fleet is broken' from \
             'the cockpit could not see the fleet'"
        );
        assert_ne!(EXIT_FAIL, EXIT_UNKNOWN);

        assert_eq!(exit_for_gate(GateVerdict::Pass), 0);
        assert_eq!(exit_for_gate(GateVerdict::Fail), EXIT_FAIL);
        assert_eq!(exit_for_gate(GateVerdict::Unknown), EXIT_UNKNOWN);
    }

    /// An empty fleet must produce a usable error, not an empty table that
    /// reads as "everything is fine".
    #[test]
    fn an_unconfigured_cockpit_says_how_to_configure_itself() {
        let err = require_nodes(&[]).unwrap_err().to_string();
        assert!(err.contains("--nodes"), "{err}");
        assert!(err.contains("[fleet]"), "{err}");
        assert!(err.contains("config.toml"), "{err}");
        assert!(require_nodes(&[ssh::NodeSpec::new("monad01")]).is_ok());
    }

    #[test]
    fn remedy_text_wraps_without_losing_words() {
        let wrapped = wrap(gates::G1_ABORT, 76);
        let flat: String = wrapped.join(" ");
        for word in ["STOP", "AND", "FIX", "A1", "A2", "Illuminator"] {
            assert!(flat.contains(word), "{word} lost in wrapping");
        }
        assert!(wrapped.iter().all(|l| l.len() <= 80), "{wrapped:?}");
    }

    /// The abort text is already laid out as an indented list. Re-flowing it
    /// from scratch broke "STOP AND FIX; no staged capture proceeds" across a
    /// ragged line at the moment the operator most needs to read it.
    #[test]
    fn already_formatted_remedy_lines_are_left_alone() {
        let wrapped = wrap(gates::G1_ABORT, 76);
        assert!(
            wrapped
                .iter()
                .any(|l| l.contains("A1 — G1 fails on the empty-room block after remediation")),
            "{wrapped:#?}"
        );
        assert!(
            wrapped
                .iter()
                .any(|l| l.starts_with("       capture proceeds.")),
            "the list indentation must survive: {wrapped:#?}"
        );
    }

    /// A genuinely long single line still wraps, and the continuation keeps the
    /// original indentation rather than jumping to column zero.
    #[test]
    fn a_long_line_wraps_and_keeps_its_indentation() {
        let long = format!("    {}", "word ".repeat(40));
        let wrapped = wrap(&long, 40);
        assert!(wrapped.len() > 1, "{wrapped:?}");
        for l in &wrapped {
            assert!(l.starts_with("    "), "{l:?}");
            assert!(l.len() <= 44, "{l:?}");
        }
    }
}
