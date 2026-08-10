//! The bench cockpit: drive ten Raspberry Pis from the laptop at the bench.
//!
//! ## What this is for
//!
//! During a staged lab session the operator has ninety seconds between blocks
//! and ten nodes. The question they have to answer, repeatedly, is not "is
//! `monad04` up?" — it is **"is this session good?"**. Answering it by ssh-ing
//! into ten Pis one at a time is not slow, it is *unreliable*: the operator
//! checks four, the block starts, and node seven has been writing zero records
//! for twenty minutes.
//!
//! So the cockpit is a small number of commands that fan out over the fleet's
//! existing ssh control channel and answer in one screen:
//!
//! | Command | Question |
//! |---|---|
//! | [`status`] | is every node capturing, at rate, on cadence, with disk, cool, and on time? |
//! | [`clock`] | is the fleet one timebase, or ten clocks? |
//! | `gate g1/g2/g3` | does the *registered* gate pass, on the bound it is registered on? |
//! | [`start`] / [`stop`] | did the capture actually start **on every node**? |
//! | `marker` | stamp a block boundary in native `unix_ts_ns` |
//! | [`runbook_day0`] | walk the documented bring-up, stop at the first failure |
//!
//! ## The two invariants
//!
//! 1. **A node that could not be reached is UNKNOWN, never healthy.** Enforced
//!    in [`health`], tested there, and repeated in every roll-up.
//! 2. **No bare point estimates.** Every gate is stated as an interval against
//!    a threshold, and the deciding bound is named. Enforced in [`gates`].
//!
//! ## What the cockpit never does
//!
//! It never touches the capture hot path. Every remote call is a read of the
//! spool or a `systemctl` verb; the probe does not open the live socket, does
//! not parse the whole capture, and does not write into a session directory
//! except to append a marker line.

pub mod clock;
pub mod cmd;
pub mod gates;
pub mod health;
pub mod probe;
pub mod render;
pub mod ssh;
pub mod stats;

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use health::{Budgets, FleetReport, NodeHealth, State, Unreachable};
use ssh::{NodeSpec, SshRunner};

/// Cockpit configuration — a `[fleet]` section in the node-global config file.
///
/// On a capture node this section is simply absent. On the bench laptop it is
/// the whole configuration, which is why [`load`] also looks in the user's
/// config directory: the operator's laptop has no `/etc/csid`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// Fleet identities, e.g. `["monad01", …, "monad10"]`. These are both the
    /// node names and (per `docs/FLEET-WIFI.md`) their MagicDNS addresses.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// Per-node address override, for a node not yet enrolled in the tailnet.
    #[serde(default)]
    pub addresses: std::collections::BTreeMap<String, String>,
    /// ssh login user. The fleet image bakes in `monad`.
    #[serde(default = "default_user")]
    pub user: String,
    /// Path to `csid` on the nodes.
    #[serde(default = "default_remote_csid")]
    pub remote_csid: String,
    /// Prefix for privileged remote verbs (`systemctl start`, marker writes
    /// into a root-owned session directory).
    #[serde(default = "default_sudo")]
    pub sudo: String,
    #[serde(default = "default_connect_timeout_s")]
    pub connect_timeout_s: u64,
    #[serde(default = "default_command_timeout_s")]
    pub command_timeout_s: u64,
    /// Extra `ssh` arguments, passed through verbatim.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Clock coherence budget, milliseconds. Defaults to gate G4b's 250 ms.
    #[serde(default = "default_clock_budget_ms")]
    pub clock_budget_ms: u64,
    #[serde(default = "default_disk_min_free_gb")]
    pub disk_min_free_gb: f64,
    /// The AP node and its unit, for the G3 5 GHz probe.
    #[serde(default)]
    pub ap_node: Option<String>,
    #[serde(default)]
    pub ap_unit: Option<String>,
    #[serde(default)]
    pub ap_interface: Option<String>,
}

fn default_user() -> String {
    "monad".into()
}
fn default_remote_csid() -> String {
    "csid".into()
}
fn default_sudo() -> String {
    "sudo -n".into()
}
fn default_connect_timeout_s() -> u64 {
    5
}
fn default_command_timeout_s() -> u64 {
    30
}
fn default_clock_budget_ms() -> u64 {
    250
}
fn default_disk_min_free_gb() -> f64 {
    5.0
}

impl Default for FleetConfig {
    fn default() -> Self {
        FleetConfig {
            nodes: Vec::new(),
            addresses: Default::default(),
            user: default_user(),
            remote_csid: default_remote_csid(),
            sudo: default_sudo(),
            connect_timeout_s: default_connect_timeout_s(),
            command_timeout_s: default_command_timeout_s(),
            ssh_options: Vec::new(),
            clock_budget_ms: default_clock_budget_ms(),
            disk_min_free_gb: default_disk_min_free_gb(),
            ap_node: None,
            ap_unit: None,
            ap_interface: None,
        }
    }
}

impl FleetConfig {
    /// Resolve the node list, honouring an explicit `--nodes` selection.
    ///
    /// A `--nodes` name that is not in the configured fleet is still accepted
    /// and addressed by its own name: at a bench, being able to point the
    /// cockpit at a node the config has not caught up with is worth more than
    /// the typo protection.
    pub fn select(&self, selection: Option<&str>) -> Vec<NodeSpec> {
        let names: Vec<String> = match selection {
            None | Some("all") | Some("") => self.nodes.clone(),
            Some(list) => list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        };
        names
            .into_iter()
            .map(|name| {
                let addr = self.addresses.get(&name).cloned().unwrap_or_else(|| name.clone());
                NodeSpec {
                    name,
                    addr,
                    user: None,
                }
            })
            .collect()
    }

    pub fn runner(&self) -> SshRunner {
        SshRunner {
            user: Some(self.user.clone()),
            connect_timeout: Duration::from_secs(self.connect_timeout_s),
            command_timeout: Duration::from_secs(self.command_timeout_s),
            extra_args: self.ssh_options.clone(),
            ..SshRunner::default()
        }
    }

    pub fn budgets(&self) -> Budgets {
        Budgets {
            disk_min_free_gb: self.disk_min_free_gb,
            clock_budget_ns: self.clock_budget_ms * 1_000_000,
            ..Budgets::default()
        }
    }
}

/// Load the cockpit's configuration.
///
/// Order: the explicit `--config` path, then the user config directory. A
/// capture node has `/etc/csid/config.toml`; a laptop has
/// `~/.config/csid/config.toml`. Neither is required — `--nodes` alone is
/// enough to drive a fleet from a machine that has never been configured.
pub fn load(explicit: &std::path::Path) -> FleetConfig {
    #[derive(Deserialize)]
    struct Outer {
        #[serde(default)]
        fleet: FleetConfig,
    }

    let mut candidates = vec![explicit.to_path_buf()];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            std::path::PathBuf::from(home)
                .join(".config")
                .join("csid")
                .join("config.toml"),
        );
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match toml::from_str::<Outer>(&text) {
            Ok(o) if !o.fleet.nodes.is_empty() => return o.fleet,
            Ok(o) => return o.fleet,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unparseable config");
            }
        }
    }
    FleetConfig::default()
}

// -- status -------------------------------------------------------------------

/// Options for a fleet sweep.
#[derive(Debug, Clone)]
pub struct StatusOptions {
    pub window_s: f64,
    pub src_mac: Option<String>,
    pub class: Option<String>,
    /// Also run the clock round trips. Costs a few extra RTTs per node.
    pub with_clock: bool,
    pub clock_samples: usize,
}

impl Default for StatusOptions {
    fn default() -> Self {
        StatusOptions {
            window_s: 30.0,
            src_mac: None,
            class: None,
            with_clock: true,
            clock_samples: 5,
        }
    }
}

/// Build the remote probe command line.
fn probe_command(cfg: &FleetConfig, o: &StatusOptions) -> String {
    let mut c = format!(
        "{} {} fleet probe --json --window {}",
        cfg.sudo, cfg.remote_csid, o.window_s
    );
    if let Some(m) = &o.src_mac {
        c.push_str(&format!(" --src-mac {}", ssh::shell_quote(m)));
    }
    if let Some(k) = &o.class {
        c.push_str(&format!(" --class {}", ssh::shell_quote(k)));
    }
    c
}

/// Sweep the fleet.
///
/// Every node is probed concurrently; a node that does not answer becomes an
/// UNKNOWN row with the reason attached, and the sweep still returns.
pub fn status(cfg: &FleetConfig, nodes: &[NodeSpec], o: &StatusOptions) -> FleetReport {
    let runner = cfg.runner();
    let budgets = cfg.budgets();
    let raw = runner.run_all(nodes, &probe_command(cfg, o));

    let clocks = if o.with_clock {
        clock_sweep(cfg, nodes, o.clock_samples)
    } else {
        Vec::new()
    };

    let mut graded: Vec<(NodeHealth, State)> = Vec::with_capacity(raw.len());
    for (node, out) in raw {
        let mut health = match out.require() {
            Err(why) => NodeHealth::unreachable(&node.name, why),
            Ok(stdout) => match serde_json::from_str::<probe::ProbeReport>(stdout) {
                Ok(r) => {
                    let mut h = r.health;
                    // Trust the cockpit's inventory name over the node's own
                    // `hostname`: a node mid-rename would otherwise appear
                    // under a name that is in no table the operator has.
                    h.host = node.name.clone();
                    h
                }
                Err(e) => NodeHealth::unreachable(
                    &node.name,
                    Unreachable::BadOutput(format!("{e}; got {:?}", truncate(stdout, 160))),
                ),
            },
        };
        health.clock = clocks
            .iter()
            .find(|(n, _)| *n == node.name)
            .and_then(|(_, c)| c.clone());
        let state = health.grade(&budgets);
        graded.push((health, state));
    }

    let skew = o.with_clock.then(|| {
        clock::fleet_skew(
            &clocks
                .iter()
                .map(|(n, c)| (n.clone(), c.clone()))
                .collect::<Vec<_>>(),
            budgets.clock_budget_ns,
        )
    });

    FleetReport {
        taken_at_unix_ns: crate::util::now_unix_ns(),
        window_s: o.window_s,
        budgets,
        nodes: graded,
        fleet_skew: skew,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// -- clock --------------------------------------------------------------------

/// The remote half of the four-timestamp exchange.
///
/// `date +%s%N` twice brackets the node's own execution, so the node-side
/// processing time drops out of the offset. The daemon state is read in the
/// same round trip — a second ssh call would cost another RTT for information
/// that does not move.
const CLOCK_PROBE: &str = "date +%s%N; chronyc -c tracking 2>/dev/null | head -1; \
                           timedatectl show -p NTPSynchronized --value 2>/dev/null; \
                           date +%s%N";

/// Parse the probe's four lines: `t2`, chrony CSV (or blank), timedatectl
/// value (or blank), `t3`.
fn parse_clock_probe(stdout: &str) -> Option<(u64, u64, Option<health::NtpState>)> {
    let lines: Vec<&str> = stdout.lines().collect();
    let t2 = lines.first()?.trim().parse::<u64>().ok()?;
    let t3 = lines.last()?.trim().parse::<u64>().ok()?;
    let ntp = lines
        .get(1)
        .and_then(|l| clock::parse_chrony_tracking(l))
        .or_else(|| lines.get(2).and_then(|l| clock::parse_timedatectl(l)));
    Some((t2, t3, ntp))
}

/// Measure every node's clock offset against the cockpit.
///
/// `samples` round trips per node; the minimum-delay one wins. Nodes are probed
/// concurrently, but the samples within a node are sequential — a concurrent
/// burst to one node would queue behind itself and inflate every delay.
pub fn clock_sweep(
    cfg: &FleetConfig,
    nodes: &[NodeSpec],
    samples: usize,
) -> Vec<(String, Option<health::ClockHealth>)> {
    let runner = cfg.runner();
    std::thread::scope(|scope| {
        let handles: Vec<_> = nodes
            .iter()
            .map(|node| {
                let runner = &runner;
                scope.spawn(move || {
                    let mut exchanges = Vec::new();
                    let mut ntp = None;
                    for _ in 0..samples.max(1) {
                        let t1 = crate::util::now_unix_ns();
                        let out = runner.run(node, CLOCK_PROBE);
                        let t4 = crate::util::now_unix_ns();
                        let Ok(stdout) = out.require() else { continue };
                        let Some((t2, t3, n)) = parse_clock_probe(stdout) else {
                            continue;
                        };
                        if ntp.is_none() {
                            ntp = n;
                        }
                        exchanges.push(clock::Exchange {
                            t1_ns: t1,
                            t2_ns: t2,
                            t3_ns: t3,
                            t4_ns: t4,
                        });
                    }
                    (node.name.clone(), clock::estimate(&exchanges, ntp))
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

// -- time transfer ------------------------------------------------------------

/// Ask every node for its time-transfer report over the last `window_s`.
///
/// The node-local command reads only its own spool, so this is the same shape
/// as [`status`]: concurrent, and a node that does not answer becomes a named
/// absence rather than a silently shorter fleet.
pub fn timesync_sweep(
    cfg: &FleetConfig,
    nodes: &[NodeSpec],
    window_s: f64,
) -> Vec<(String, std::result::Result<crate::timesync::TimesyncReport, String>)> {
    let runner = cfg.runner();
    let cmd = format!(
        "{} {} timesync report --json --window {window_s}",
        cfg.sudo, cfg.remote_csid
    );
    runner
        .run_all(nodes, &cmd)
        .into_iter()
        .map(|(node, out)| {
            let parsed = match out.require() {
                Err(why) => Err(why.summary()),
                Ok(stdout) => stdout
                    .lines()
                    .find(|l| l.trim_start().starts_with('{'))
                    .ok_or_else(|| "no report JSON on stdout".to_string())
                    .and_then(|l| {
                        serde_json::from_str::<crate::timesync::TimesyncReport>(l)
                            .map_err(|e| e.to_string())
                    }),
            };
            (node.name.clone(), parsed)
        })
        .collect()
}

/// The transmitter the fleet should be measured on: the one the most nodes
/// heard, breaking ties on total packets.
///
/// Picking automatically matters because a lab often has more than one stamped
/// transmitter on the air (an injector plus a phone), and a skew measured on a
/// transmitter only two nodes could hear is a measurement of those two nodes.
pub fn dominant_transmitter(
    reports: &[(String, std::result::Result<crate::timesync::TimesyncReport, String>)],
) -> Option<String> {
    let mut tally: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for (_, r) in reports {
        let Ok(r) = r else { continue };
        for (tx, arrivals) in &r.arrivals {
            let e = tally.entry(tx.as_str()).or_insert((0, 0));
            e.0 += 1;
            e.1 += arrivals.len();
        }
    }
    tally
        .into_iter()
        .max_by_key(|(_, (nodes, packets))| (*nodes, *packets))
        .map(|(tx, _)| tx.to_string())
}

/// Fold the sweep into one fleet-level skew report on one transmitter.
///
/// Nodes that answered but heard nothing from that transmitter are carried
/// through as **silent** rather than dropped; a node the cockpit could not
/// reach at all is returned separately, because "unreachable" and "heard
/// nothing" are different failures.
pub fn transfer_report(
    reports: &[(String, std::result::Result<crate::timesync::TimesyncReport, String>)],
    tx_id: &str,
    budget_ns: u64,
    min_common: usize,
) -> (crate::timesync::skew::TransferReport, Vec<(String, String)>) {
    let mut nodes: Vec<(String, Vec<crate::timesync::skew::Arrival>)> = Vec::new();
    let mut unreachable: Vec<(String, String)> = Vec::new();
    for (host, r) in reports {
        match r {
            Err(why) => unreachable.push((host.clone(), why.clone())),
            Ok(r) => {
                let arrivals = r
                    .arrivals
                    .iter()
                    .find(|(t, _)| t == tx_id)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_default();
                nodes.push((host.clone(), arrivals));
            }
        }
    }
    (
        crate::timesync::skew::fleet_transfer_skew(tx_id, &nodes, budget_ns, min_common),
        unreachable,
    )
}

/// The line that settles which instrument decides when both have run.
pub const SKEW_AUTHORITY: &str = "\
AUTHORITY — when both `csid fleet skew` and `csid fleet clock` have a number for the
same window, THIS ONE DECIDES.

  `csid fleet clock` estimates each node's offset against the COCKPIT through an ssh
  four-timestamp exchange. Its error is bounded by half the round-trip asymmetry, which
  it cannot observe, so it reports half the round-trip delay as the uncertainty:
  milliseconds over a tailnet, on a good day.

  `csid fleet skew` differences two nodes' receive stamps for ONE transmitted frame.
  The transmit instant is the same physical event on both sides and cancels exactly, so
  there is no round trip to be asymmetric. What is left is per-node RX pipeline jitter,
  which averages out over thousands of packets.

Use `csid fleet clock` when no stamped transmitter is on the air (no illumination, no
phone), when a node's own chronyd state is the question, or as the cross-check. Its
UNCERTAINTY is never better than the node's chrony root dispersion, and that widening is
what makes it safe to fall back to.";

/// Render the full time-transfer readout: per-node liveness, the pairwise skew
/// table, the phone fits, and the authority statement.
pub fn render_transfer(
    reports: &[(String, std::result::Result<crate::timesync::TimesyncReport, String>)],
    report: &crate::timesync::skew::TransferReport,
    unreachable: &[(String, String)],
    window_s: f64,
) -> String {
    let mut out = format!(
        "csid fleet skew — {} node(s), {:.0} s window\n\n",
        reports.len(),
        window_s
    );

    out.push_str(&format!(
        "{:<3} {:<9} {:>8} {:>8} {:>8}  {}\n",
        "", "node", "csid", "app", "tx", "receive stamps"
    ));
    out.push_str(&"-".repeat(60));
    out.push('\n');
    for (host, r) in reports {
        match r {
            Err(why) => out.push_str(&format!(
                "{:<3} {:<9} {:>8} {:>8} {:>8}  {}\n",
                "??", host, "—", "—", "—", why
            )),
            Ok(r) => out.push_str(&format!(
                "{:<3} {:<9} {:>8} {:>8} {:>8}  {}\n",
                if r.rows_csid > 0 { "OK" } else { "XX" },
                host,
                r.rows_csid,
                r.rows_app,
                r.arrivals.len(),
                if r.stamp_sources.is_empty() {
                    "none".to_string()
                } else {
                    r.stamp_sources.join("+")
                },
            )),
        }
    }
    out.push('\n');
    out.push_str(&report.render());

    if !unreachable.is_empty() {
        out.push_str(&format!(
            "\n  UNREACHABLE: {} — the fleet skew below covers the nodes that answered and \
             says nothing\n  about these.\n",
            unreachable
                .iter()
                .map(|(h, w)| format!("{h} ({w})"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let fits: Vec<(&String, &crate::timesync::affine::AffineFit)> = reports
        .iter()
        .filter_map(|(h, r)| r.as_ref().ok().map(|r| (h, r)))
        .flat_map(|(h, r)| r.app_fits.iter().map(move |f| (h, f)))
        .collect();
    if fits.is_empty() {
        out.push_str(
            "\nphone → fleet: no app packets carried a readable stamp in this window.\n  \
             If the phone WAS transmitting, check `protected_frames` in the session sidecar: \
             a\n  WPA2 experiment SSID encrypts the payload over the air and only `collectord` \
             — which\n  receives the datagram as a real UDP peer — can read it.\n",
        );
    } else {
        out.push_str("\nphone → fleet affine fit (unix_ts_ns ~ a*mono_ns + b)\n");
        for (host, f) in fits {
            out.push_str(&format!("\n  [{host}] {}\n", f.render().replace('\n', "\n  ")));
        }
    }

    out.push('\n');
    out.push_str(SKEW_AUTHORITY);
    out.push('\n');
    out
}

// -- session lifecycle --------------------------------------------------------

/// What happened to one node in a start/stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleOutcome {
    /// The unit is active **and** the node is writing records.
    Confirmed(String),
    /// The command was accepted but the node never confirmed. This is the
    /// dangerous one, and it is deliberately not the same value as Confirmed.
    NotConfirmed(String),
    Refused(String),
    Unreachable(String),
}

impl LifecycleOutcome {
    pub fn mark(&self) -> &'static str {
        match self {
            LifecycleOutcome::Confirmed(_) => "OK",
            LifecycleOutcome::NotConfirmed(_) => "XX",
            LifecycleOutcome::Refused(_) => "XX",
            LifecycleOutcome::Unreachable(_) => "??",
        }
    }

    pub fn is_confirmed(&self) -> bool {
        matches!(self, LifecycleOutcome::Confirmed(_))
    }

    pub fn detail(&self) -> &str {
        match self {
            LifecycleOutcome::Confirmed(s)
            | LifecycleOutcome::NotConfirmed(s)
            | LifecycleOutcome::Refused(s)
            | LifecycleOutcome::Unreachable(s) => s,
        }
    }
}

/// Render a start/stop report.
///
/// The failure this exists to design against is a **partial start that looks
/// successful**. So the report is not "the command was sent" — it is a
/// per-node confirmation that the capture is actually producing records, and
/// the summary line refuses to say "started" unless every node did.
pub fn render_lifecycle(verb: &str, results: &[(String, LifecycleOutcome)]) -> String {
    let mut out = format!("csid fleet {verb} — {} node(s)\n\n", results.len());
    let mut ok = 0;
    for (host, r) in results {
        if r.is_confirmed() {
            ok += 1;
        }
        out.push_str(&format!("{:<3} {:<9} {}\n", r.mark(), host, r.detail()));
    }
    out.push('\n');
    if ok == results.len() && !results.is_empty() {
        out.push_str(&format!("=== ALL {ok} NODE(S) {} ===\n", verb.to_uppercase()));
    } else {
        out.push_str(&format!(
            "=== {} — {ok} of {} node(s) confirmed. The rest did NOT {verb}. \
             Do not start a staged block on a partial fleet. ===\n",
            if verb == "start" {
                "PARTIAL START"
            } else {
                "PARTIAL STOP"
            },
            results.len()
        ));
    }
    out
}

/// Start a capture on every node and **confirm** it.
pub fn start(
    cfg: &FleetConfig,
    nodes: &[NodeSpec],
    experiment: &str,
    confirm_timeout: Duration,
) -> Vec<(String, LifecycleOutcome)> {
    let runner = cfg.runner();
    let unit = format!("csid@{experiment}");
    let cmd = format!("{} systemctl start {}", cfg.sudo, ssh::shell_quote(&unit));

    let mut results: Vec<(String, LifecycleOutcome)> = Vec::new();
    let mut pending: Vec<NodeSpec> = Vec::new();
    for (node, out) in runner.run_all(nodes, &cmd) {
        match out.require() {
            Err(Unreachable::SshFailed(e)) | Err(Unreachable::BadOutput(e)) => {
                results.push((node.name.clone(), LifecycleOutcome::Unreachable(e)))
            }
            Err(Unreachable::TimedOut) => results.push((
                node.name.clone(),
                LifecycleOutcome::Unreachable("timed out".into()),
            )),
            Err(Unreachable::RemoteFailed { code, stderr }) => results.push((
                node.name.clone(),
                LifecycleOutcome::Refused(format!("systemctl exit {code:?}: {}", stderr.trim())),
            )),
            Ok(_) => pending.push(node),
        }
    }

    // Confirmation loop: a unit that is `active` is not a capture that is
    // producing records. Poll the probe until each node reports a live session
    // with a scope, or the deadline passes.
    let deadline = Instant::now() + confirm_timeout;
    let probe_cmd = probe_command(
        cfg,
        &StatusOptions {
            window_s: 10.0,
            with_clock: false,
            ..StatusOptions::default()
        },
    );
    while !pending.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1500));
        let mut still: Vec<NodeSpec> = Vec::new();
        for (node, out) in runner.run_all(&pending, &probe_cmd) {
            let confirmed = out
                .require()
                .ok()
                .and_then(|s| serde_json::from_str::<probe::ProbeReport>(s).ok())
                .filter(|r| r.health.capture_alive)
                .map(|r| {
                    (
                        r.health.session_id.unwrap_or_default(),
                        r.health.scope.map(|s| s.scoped_records).unwrap_or(0),
                    )
                });
            match confirmed {
                Some((session, records)) => results.push((
                    node.name.clone(),
                    LifecycleOutcome::Confirmed(format!(
                        "{session} — capturing, {records} scoped record(s) in the last 10 s"
                    )),
                )),
                None => still.push(node),
            }
        }
        pending = still;
    }
    for node in pending {
        results.push((
            node.name.clone(),
            LifecycleOutcome::NotConfirmed(format!(
                "systemctl accepted the start, but no session is producing records after {:.0} s \
                 — check `journalctl -u csid@{experiment}` on this node",
                confirm_timeout.as_secs_f64()
            )),
        ));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Stop a capture on every node and confirm the session closed.
pub fn stop(cfg: &FleetConfig, nodes: &[NodeSpec], experiment: &str) -> Vec<(String, LifecycleOutcome)> {
    let runner = cfg.runner();
    let unit = format!("csid@{experiment}");
    // `systemctl stop` is synchronous, so by the time it returns the session
    // has closed its sidecar. One extra probe reports what it produced.
    let cmd = format!(
        "{sudo} systemctl stop {unit}; {sudo} {csid} fleet probe --json --window 5",
        sudo = cfg.sudo,
        unit = ssh::shell_quote(&unit),
        csid = cfg.remote_csid,
    );
    let mut results: Vec<(String, LifecycleOutcome)> = runner
        .run_all(nodes, &cmd)
        .into_iter()
        .map(|(node, out)| {
            let outcome = match out.require() {
                Err(why) => LifecycleOutcome::Unreachable(why.summary()),
                Ok(stdout) => match serde_json::from_str::<probe::ProbeReport>(
                    stdout.lines().find(|l| l.starts_with('{')).unwrap_or(stdout),
                ) {
                    Ok(r) if !r.health.capture_alive => LifecycleOutcome::Confirmed(format!(
                        "stopped — last session {}",
                        r.health.session_id.unwrap_or_else(|| "(none)".into())
                    )),
                    Ok(r) => LifecycleOutcome::NotConfirmed(format!(
                        "still producing records after stop — {}",
                        r.health.session_id.unwrap_or_default()
                    )),
                    Err(e) => LifecycleOutcome::NotConfirmed(format!(
                        "stop issued but the node's state is unreadable: {e}"
                    )),
                },
            };
            (node.name.clone(), outcome)
        })
        .collect();
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

// -- markers ------------------------------------------------------------------

/// Broadcast a block marker to every node.
///
/// Each node stamps with **its own** `now_unix_ns` at the moment it executes,
/// so the markers are not identical — they differ by the fleet skew plus the
/// ssh fan-out spread. That is a feature: the spread across the returned
/// stamps is a direct, in-band measurement of exactly the quantity
/// [`clock_sweep`] estimates, taken at the boundary where it is spent. The
/// command reports it.
pub fn broadcast_marker(
    cfg: &FleetConfig,
    nodes: &[NodeSpec],
    args: &str,
) -> Vec<(String, std::result::Result<crate::marker::Marker, String>)> {
    let runner = cfg.runner();
    let cmd = format!("{} {} marker --json {args}", cfg.sudo, cfg.remote_csid);
    runner
        .run_all(nodes, &cmd)
        .into_iter()
        .map(|(node, out)| {
            let parsed = match out.require() {
                Err(why) => Err(why.summary()),
                Ok(stdout) => stdout
                    .lines()
                    .find(|l| l.trim_start().starts_with('{'))
                    .ok_or_else(|| "no marker JSON on stdout".to_string())
                    .and_then(|l| {
                        serde_json::from_str::<crate::marker::Marker>(l).map_err(|e| e.to_string())
                    }),
            };
            (node.name.clone(), parsed)
        })
        .collect()
}

/// Render a broadcast marker result, including the observed stamp spread.
pub fn render_broadcast(
    results: &[(String, std::result::Result<crate::marker::Marker, String>)],
    budget_ns: u64,
) -> String {
    let mut out = String::from("csid fleet marker\n\n");
    let mut stamps: Vec<(String, u64)> = Vec::new();
    for (host, r) in results {
        match r {
            Ok(m) => {
                out.push_str(&format!(
                    "OK  {:<9} {} ns  {} {}\n",
                    host, m.unix_ts_ns, m.block_id, m.phase
                ));
                stamps.push((host.clone(), m.unix_ts_ns));
            }
            Err(e) => out.push_str(&format!("??  {host:<9} NOT MARKED — {e}\n")),
        }
    }
    out.push('\n');

    let missing = results.len() - stamps.len();
    if stamps.len() >= 2 {
        let min = stamps.iter().min_by_key(|(_, t)| *t).unwrap();
        let max = stamps.iter().max_by_key(|(_, t)| *t).unwrap();
        let spread = max.1 - min.1;
        out.push_str(&format!(
            "stamp spread across nodes: {:.1} ms ({} … {}) against a {:.0} ms budget\n",
            spread as f64 / 1e6,
            min.0,
            max.0,
            budget_ns as f64 / 1e6,
        ));
        out.push_str(
            "  this spread is fleet clock skew PLUS ssh fan-out latency, so it is an upper\n  \
             bound on the skew, not the skew itself — `csid fleet clock` separates them.\n",
        );
        if spread > budget_ns {
            out.push_str(
                "  ABOVE BUDGET: use each node's own marker for that node's records, and run\n  \
                 `csid fleet clock` before trusting any cross-node boundary.\n",
            );
        }
    }
    if missing > 0 {
        out.push_str(&format!(
            "\n=== {missing} of {} NODE(S) NOT MARKED. Those nodes' captures have no fleet-side\n\
             boundary for this block and fall back to the app marker, which is gated by G4. ===\n",
            results.len()
        ));
    } else {
        out.push_str("\n=== ALL NODES MARKED — these stamps are the boundary of record ===\n");
    }
    out
}

// -- the Day-0 runbook --------------------------------------------------------

/// One step of the documented bring-up.
pub struct RunbookStep {
    pub title: &'static str,
    /// Remote command, or `None` for a cockpit-side step.
    pub command: Option<String>,
    /// The exact remedy text from the lab plan / pre-registration.
    pub remedy: &'static str,
}

/// The Day-0 sequence, in order, with the remedy text quoted from
/// `experiments/Lab Session Plan 2026-08.md` §"Day-0 BLE bring-up" and the
/// pre-registration §11.
///
/// The clock check sits **before** the delivered-rate gate on purpose: a fleet
/// that is not time-coherent makes every later gate a measurement of nothing,
/// so there is no point paying for a 30-minute rate probe until the nodes agree
/// what time it is.
pub fn day0_steps(cfg: &FleetConfig, experiment: &str, adapter: &str) -> Vec<RunbookStep> {
    vec![
        RunbookStep {
            title: "1. mask bluetoothd and bring the HCI adapter up",
            command: Some(format!(
                "{sudo} systemctl mask bluetooth.service >/dev/null 2>&1; \
                 {sudo} systemctl stop bluetooth.service >/dev/null 2>&1; \
                 {sudo} hciconfig {adapter} up && echo masked-and-up",
                sudo = cfg.sudo
            )),
            remedy: "`sudo systemctl mask bluetooth.service` — MANDATORY. bluetoothd shares the \
                     adapter and will change scan parameters underneath csid. Then \
                     `sudo hciconfig hci0 up`.",
        },
        RunbookStep {
            title: "2. csid doctor",
            command: Some(format!("{} {} doctor", cfg.sudo, cfg.remote_csid)),
            remedy: "Expect `[ok] BLE adapter: hci0 — passive LE scan started and stopped \
                     cleanly`. A CAP_NET_RAW complaint means `systemctl daemon-reload` was not \
                     run after the unit change.",
        },
        RunbookStep {
            title: "3. csid validate --probe",
            command: Some(format!(
                "{} {} validate {} --probe",
                cfg.sudo,
                cfg.remote_csid,
                ssh::shell_quote(experiment)
            )),
            remedy: "With `ble.required = true` this fails loudly if the adapter is unusable, \
                     rather than producing a session with an empty BLE stream. Fix the adapter \
                     before proceeding — a calibration session without BLE anchors cannot test \
                     the thing it exists to test.",
        },
        RunbookStep {
            title: "4. BLE sanity — is the scanner actually seeing advertisements?",
            command: None,
            remedy: "Confirm the journal shows `ble scanning` with a NON-ZERO rate_hz. If it is \
                     zero while `sudo btmon -i hci0` shows traffic, suspect the `hci_filter` \
                     sockopt length or the `HCI_CHANNEL_RAW` bind — the HCI socket is the one \
                     layer that could not be tested off-device. If `unparsed_events` is large, \
                     the controller is reporting BT 5.0 EXTENDED advertising rather than legacy \
                     subevent 0x02; extended-scan support would be a follow-on, not a config \
                     change.",
        },
        RunbookStep {
            title: "5. fleet clock coherence (G4b budget)",
            command: None,
            remedy: "The nodes are NOT aligned to each other unless chrony is healthy. Ten \
                     nodes each individually fine but mutually skewed by seconds corrupt every \
                     cross-node comparison, and nothing in the capture path would notice. \
                     Remedy: check `chronyc -c tracking` on the offending node and confirm it \
                     is following the FLEET REFERENCE (roles/chrony points every node at one \
                     local reference, on purpose — a node that has been 'fixed' back to \
                     0.pool.ntp.org is coherent with nobody), that it reaches it over the \
                     tailnet, and re-run. Do NOT proceed to the rate gate on a skewed fleet — \
                     every later gate would be measuring nothing. This step uses the ssh \
                     round-trip instrument, whose bound is milliseconds; once the illuminator \
                     is up, `csid fleet skew` measures the same quantity on the air to \
                     microseconds and is authoritative.",
        },
        RunbookStep {
            title: "6. delivered-rate gate G1 (and G2)",
            command: None,
            remedy: gates::G1_ABORT,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_selection_falls_back_to_the_configured_fleet_and_honours_an_override() {
        let mut cfg = FleetConfig {
            nodes: vec!["monad01".into(), "monad02".into(), "monad03".into()],
            ..FleetConfig::default()
        };
        assert_eq!(cfg.select(None).len(), 3);
        assert_eq!(cfg.select(Some("all")).len(), 3);
        let picked = cfg.select(Some("monad02, monad03"));
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].name, "monad02");
        assert_eq!(picked[0].addr, "monad02");

        // A node the config has never heard of is still addressable.
        let ad_hoc = cfg.select(Some("monad11"));
        assert_eq!(ad_hoc.len(), 1);
        assert_eq!(ad_hoc[0].addr, "monad11");

        // An address override wins (a node not yet enrolled in the tailnet).
        cfg.addresses
            .insert("monad04".into(), "monad.local".into());
        let ovr = cfg.select(Some("monad04"));
        assert_eq!(ovr[0].name, "monad04");
        assert_eq!(ovr[0].addr, "monad.local");
    }

    #[test]
    fn the_clock_budget_defaults_to_g4b() {
        let b = FleetConfig::default().budgets();
        assert_eq!(b.clock_budget_ns, gates::G4B_BUDGET_NS);
        assert_eq!(b.clock_budget_ns, 250_000_000);
    }

    #[test]
    fn the_probe_command_carries_the_window_and_the_scope() {
        let cfg = FleetConfig::default();
        let c = probe_command(
            &cfg,
            &StatusOptions {
                window_s: 45.0,
                src_mac: Some("ef:be:ad:de:ad:de".into()),
                class: Some("52:legacyofdm".into()),
                ..StatusOptions::default()
            },
        );
        assert!(c.contains("fleet probe --json"), "{c}");
        assert!(c.contains("--window 45"), "{c}");
        assert!(c.contains("'ef:be:ad:de:ad:de'"), "{c}");
        assert!(c.contains("'52:legacyofdm'"), "{c}");
    }

    #[test]
    fn the_clock_probe_output_is_parsed_including_the_no_daemon_case() {
        let with_chrony = "1786000000123456789\n\
             C0248F81,192.36.143.130,2,1786000000.0,0.000000345,-0.0000001,0.000012,\
             -1.2,0.001,0.05,0.012,0.002,64.0,Normal\n\
             yes\n\
             1786000000133456789\n";
        let (t2, t3, ntp) = parse_clock_probe(with_chrony).unwrap();
        assert_eq!(t2, 1_786_000_000_123_456_789);
        assert_eq!(t3, 1_786_000_000_133_456_789);
        let ntp = ntp.unwrap();
        assert_eq!(ntp.daemon, "chronyd");
        assert!(ntp.synchronised);

        // No chrony: the timedatectl line answers, with no error bound.
        let timesyncd = "1786000000000000000\n\nyes\n1786000000010000000\n";
        let (_, _, ntp) = parse_clock_probe(timesyncd).unwrap();
        let ntp = ntp.unwrap();
        assert_eq!(ntp.daemon, "systemd-timesyncd");
        assert_eq!(ntp.root_dispersion_s, None);

        // Neither daemon: still a usable offset, just no daemon cross-check.
        let bare = "1786000000000000000\n\n\n1786000000010000000\n";
        let (_, _, ntp) = parse_clock_probe(bare).unwrap();
        assert!(ntp.is_none());

        assert!(parse_clock_probe("").is_none());
        assert!(parse_clock_probe("not-a-number\n\n\nalso-not\n").is_none());
    }

    /// The failure this command exists to design against.
    #[test]
    fn a_partial_start_is_never_reported_as_a_success() {
        let results = vec![
            (
                "monad01".to_string(),
                LifecycleOutcome::Confirmed("s1 — capturing, 1200 record(s)".into()),
            ),
            (
                "monad02".to_string(),
                LifecycleOutcome::Confirmed("s2 — capturing, 1180 record(s)".into()),
            ),
            (
                "monad03".to_string(),
                LifecycleOutcome::NotConfirmed("no session producing records after 30 s".into()),
            ),
        ];
        let text = render_lifecycle("start", &results);
        assert!(text.contains("PARTIAL START"), "{text}");
        assert!(text.contains("2 of 3 node(s) confirmed"), "{text}");
        assert!(text.contains("Do not start a staged block"), "{text}");
        assert!(!text.contains("ALL"), "{text}");

        // A clean start says so unambiguously.
        let all_ok: Vec<_> = results[..2].to_vec();
        let text = render_lifecycle("start", &all_ok);
        assert!(text.contains("=== ALL 2 NODE(S) START ==="), "{text}");
    }

    /// "systemctl accepted it" is not "it is capturing". Those are different
    /// outcomes and must render differently.
    #[test]
    fn an_accepted_but_unconfirmed_start_marks_as_failed_not_unknown() {
        let o = LifecycleOutcome::NotConfirmed("x".into());
        assert_eq!(o.mark(), "XX");
        assert!(!o.is_confirmed());
        assert_eq!(LifecycleOutcome::Unreachable("x".into()).mark(), "??");
        assert_eq!(LifecycleOutcome::Confirmed("x".into()).mark(), "OK");
    }

    #[test]
    fn a_broadcast_marker_reports_the_stamp_spread_as_an_upper_bound() {
        let mk = |ts: u64| {
            crate::marker::Marker::at(
                ts,
                "h",
                "s",
                "S1-ZA-CYC-03",
                "ZONE-A",
                "cycling",
                "start",
                Some(4),
                None,
                None,
            )
        };
        let base = 1_786_000_000_000_000_000u64;
        let results = vec![
            ("monad01".to_string(), Ok(mk(base))),
            ("monad02".to_string(), Ok(mk(base + 40_000_000))),
            ("monad03".to_string(), Ok(mk(base + 12_000_000))),
        ];
        let text = render_broadcast(&results, gates::G4B_BUDGET_NS);
        assert!(text.contains("stamp spread across nodes: 40.0 ms"), "{text}");
        assert!(text.contains("upper"), "{text}");
        assert!(text.contains("boundary of record"), "{text}");
        assert!(!text.contains("ABOVE BUDGET"), "{text}");

        // Over budget: the operator is told what to do about it.
        let results = vec![
            ("monad01".to_string(), Ok(mk(base))),
            ("monad02".to_string(), Ok(mk(base + 900_000_000))),
        ];
        let text = render_broadcast(&results, gates::G4B_BUDGET_NS);
        assert!(text.contains("ABOVE BUDGET"), "{text}");
        assert!(text.contains("csid fleet clock"), "{text}");
    }

    #[test]
    fn an_unmarked_node_is_called_out_with_its_consequence() {
        let results = vec![
            (
                "monad01".to_string(),
                Ok(crate::marker::Marker::at(
                    1,
                    "h",
                    "s",
                    "B",
                    "Z",
                    "cycling",
                    "start",
                    None,
                    None,
                    None,
                )),
            ),
            ("monad02".to_string(), Err("timed out".into())),
        ];
        let text = render_broadcast(&results, gates::G4B_BUDGET_NS);
        assert!(text.contains("?? "), "{text}");
        assert!(text.contains("NOT MARKED"), "{text}");
        assert!(text.contains("gated by G4"), "{text}");
    }

    /// The correction that produced this ordering: clock before rate.
    #[test]
    fn the_day0_runbook_checks_the_clock_before_the_rate_gate() {
        let steps = day0_steps(&FleetConfig::default(), "lab-anchor", "hci0");
        let titles: Vec<&str> = steps.iter().map(|s| s.title).collect();
        let clock = titles.iter().position(|t| t.contains("clock")).unwrap();
        let rate = titles.iter().position(|t| t.contains("delivered-rate")).unwrap();
        assert!(clock < rate, "{titles:?}");

        // And the documented order of the BLE bring-up is preserved.
        let idx = |needle: &str| titles.iter().position(|t| t.contains(needle)).unwrap();
        assert!(idx("mask bluetoothd") < idx("doctor"));
        assert!(idx("doctor") < idx("validate"));
        assert!(idx("validate") < idx("BLE sanity"));
        assert!(idx("BLE sanity") < clock);
    }

    /// The remedy text has to be the plan's text, not a paraphrase — the
    /// operator acts on it at 11pm without opening the vault.
    #[test]
    fn every_runbook_step_carries_an_actionable_remedy_quoted_from_the_plan() {
        let steps = day0_steps(&FleetConfig::default(), "lab-anchor", "hci0");
        for s in &steps {
            assert!(s.remedy.len() > 40, "{}: remedy too thin", s.title);
        }
        let all: String = steps.iter().map(|s| s.remedy).collect::<Vec<_>>().join(" ");
        assert!(all.contains("MANDATORY"));
        assert!(all.contains("CAP_NET_RAW"));
        assert!(all.contains("systemctl daemon-reload"));
        assert!(all.contains("btmon"));
        assert!(all.contains("HCI_CHANNEL_RAW"));
        assert!(all.contains("EXTENDED advertising"));
        assert!(all.contains("STOP AND FIX"));
        assert!(all.contains("A1"));
    }
}
