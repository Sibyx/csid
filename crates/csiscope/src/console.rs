//! The read-only operator surface: what this node is, and what it is running.
//!
//! ## Read-only, by construction rather than by flag
//!
//! `csiscope` used to serve an editor for `/etc/csid/config.toml` and the
//! per-experiment TOML, buttons that started and stopped capture units, a
//! session browser and an export trigger — all unauthenticated, with a
//! `--read-only` flag as the only thing standing between the port and the
//! radio. The flag was the wrong shape: it made the safe configuration the
//! explicit one, and it meant every handler carried a `writable()` guard that
//! had to be remembered.
//!
//! The write surface is gone. A capture is armed by merging a plan and letting
//! the control host's timer act on it (IP-136); a session is read out of the
//! spool by the tools that own the archive; configuration is Ansible's. None of
//! those wanted a text box on a lab network. What is left here is a description
//! of the node — versions, kernel, resolved experiment tuning, unit states,
//! journal, `csid doctor` — and none of it can change anything.
//!
//! **Validation is not reimplemented.** An experiment is parsed with
//! [`csid::config::ExperimentConfig`] and checked with the same
//! [`csid::caps::validate_radio`] the daemon runs, so the console describes
//! exactly what `csid validate` would say. A second implementation would drift,
//! and the drift would surface four hours into an unattended run.
//!
//! **No authentication is still not the same as no input validation.** Every
//! name that reaches the filesystem or `systemctl` is checked against a strict
//! pattern first, and the journal is readable only for units this project owns.
//! Nothing here interpolates into a shell.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use csid::config::ExperimentConfig;

/// Units `csiscope` is willing to *read* the journal of.
///
/// Nothing here is controllable any more — the console has no write surface —
/// but the allow-list stays, because `journalctl -u <anything>` on an
/// unauthenticated port is its own disclosure. A console on a lab network may
/// describe this project's capture units and nothing else.
const READABLE: &[&str] = &["csid-sync", "csid-prune", "csid-driver-guard"];

// -- names --------------------------------------------------------------------

/// Accept a slug used as a filename, a systemd instance, or a session id.
///
/// Deliberately narrow: alphanumerics, dot, dash, underscore. That excludes
/// `/` and `..`, which is the whole point — these strings become paths under
/// `/etc/csid/experiments` and `/var/lib/csid`.
pub fn safe_name(name: &str) -> Result<&str> {
    if name.is_empty() || name.len() > 96 {
        bail!("name must be 1–96 characters");
    }
    if name.starts_with('.') {
        bail!("name may not start with a dot");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("name may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(name)
}

/// Map a name to the systemd unit it denotes, refusing anything outside the
/// project's own units.
pub fn resolve_unit(unit: &str) -> Result<String> {
    if let Some(exp) = unit.strip_prefix("csid@") {
        let exp = exp.strip_suffix(".service").unwrap_or(exp);
        safe_name(exp)?;
        return Ok(format!("csid@{exp}.service"));
    }
    let base = unit
        .strip_suffix(".service")
        .or_else(|| unit.strip_suffix(".timer"))
        .unwrap_or(unit);
    if READABLE.contains(&base) {
        return Ok(if unit.contains('.') {
            unit.to_string()
        } else {
            format!("{base}.service")
        });
    }
    bail!("{unit} is not a unit csiscope reports on")
}

// -- shelling out -------------------------------------------------------------

/// Result of running a helper binary. Non-zero exit is data, not an error:
/// `csid doctor` failing its checks is exactly what the operator wants to read.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub argv: Vec<String>,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub ok: bool,
}

/// Run a command with no shell involved.
pub fn run(program: &str, args: &[&str]) -> Run {
    let mut argv = vec![program.to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));

    match Command::new(program).args(args).output() {
        Ok(out) => Run {
            argv,
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            ok: out.status.success(),
        },
        Err(e) => Run {
            argv,
            code: -1,
            stdout: String::new(),
            stderr: format!("could not run {program}: {e}"),
            ok: false,
        },
    }
}

// -- experiments --------------------------------------------------------------

/// One experiment file as the console sees it: its text, its parse/validation
/// outcome, and the tuning it resolves to.
#[derive(Debug, Clone, Serialize)]
pub struct Experiment {
    pub name: String,
    pub path: String,
    pub toml: String,
    #[serde(flatten)]
    pub check: Check,
}

/// The verdict on a candidate configuration.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Check {
    pub valid: bool,
    pub error: Option<String>,
    /// Resolved radio parameters — the same numbers `csid validate` prints.
    pub tuning: Option<Tuning>,
    /// Parsed configuration, for the form editor.
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tuning {
    pub band: String,
    pub control_freq_mhz: u32,
    pub center_freq_mhz: Option<u32>,
    pub width: String,
    pub interval_us: u32,
    /// Rate the configured interval implies, or 0 when unthrottled.
    pub interval_rate_hz: f64,
    pub duration_s: Option<u64>,
    pub stream: Option<String>,
    pub export_on_close: bool,
}

/// Parse and validate a candidate experiment TOML without touching the disk.
///
/// This is the function behind both the "validate" button and every write: a
/// configuration that `csid` would reject never reaches `/etc/csid`.
pub fn check_experiment(text: &str) -> Check {
    let cfg: ExperimentConfig = match toml::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            return Check {
                valid: false,
                error: Some(format!("TOML: {e}")),
                ..Default::default()
            }
        }
    };

    let config = serde_json::to_value(&cfg).ok();

    if let Err(e) = cfg.validate() {
        return Check {
            valid: false,
            error: Some(format!("{e:#}")),
            config,
            ..Default::default()
        };
    }

    let tuning = match csid::radio::resolve(&cfg.radio) {
        Ok(t) => t,
        Err(e) => {
            return Check {
                valid: false,
                error: Some(format!("{e:#}")),
                config,
                ..Default::default()
            }
        }
    };

    Check {
        valid: true,
        error: None,
        config,
        tuning: Some(Tuning {
            band: format!("{:?}", tuning.band),
            control_freq_mhz: tuning.freq,
            center_freq_mhz: tuning.center,
            width: cfg.radio.width.iw_token().to_string(),
            interval_us: cfg.radio.interval_us,
            interval_rate_hz: if cfg.radio.interval_us > 0 {
                1e6 / cfg.radio.interval_us as f64
            } else {
                0.0
            },
            duration_s: cfg.capture.duration.map(|d| d.as_secs()),
            stream: cfg.stream.enabled.then(|| {
                if cfg.stream.transport == "udp" {
                    format!("udp -> {}", cfg.stream.targets.join(", "))
                } else {
                    format!("unix -> {}", cfg.stream.unix_socket.display())
                }
            }),
            export_on_close: cfg.export.on_close,
        }),
    }
}

/// List every `*.toml` under the experiment directory, checked.
pub fn list_experiments(dir: &Path) -> Vec<Experiment> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Experiment> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_stem()?.to_string_lossy().to_string();
            safe_name(&name).ok()?;
            let toml = std::fs::read_to_string(&path).ok()?;
            Some(Experiment {
                check: check_experiment(&toml),
                name,
                path: path.display().to_string(),
                toml,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Read one experiment.
pub fn read_experiment(dir: &Path, name: &str) -> Result<Experiment> {
    safe_name(name)?;
    let path = dir.join(format!("{name}.toml"));
    let toml =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Experiment {
        check: check_experiment(&toml),
        name: name.to_string(),
        path: path.display().to_string(),
        toml,
    })
}

// -- systemd ------------------------------------------------------------------

/// A unit's state, as `systemctl show` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct UnitStatus {
    pub unit: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub result: String,
    pub since: String,
    pub main_pid: String,
    /// False when systemd itself is unreachable (not running as PID 1, a
    /// container, a developer laptop) — the console shows the difference
    /// between "stopped" and "cannot tell".
    pub available: bool,
}

/// Query one unit. Never fails: an unreachable systemd is reported, not raised.
pub fn unit_status(unit: &str) -> UnitStatus {
    let props = [
        "LoadState",
        "ActiveState",
        "SubState",
        "Result",
        "ActiveEnterTimestamp",
        "MainPID",
    ];
    let mut args = vec!["show", unit];
    for p in &props {
        args.push("-p");
        args.push(p);
    }
    let out = run("systemctl", &args);

    let get = |key: &str| -> String {
        let prefix = format!("{key}=");
        out.stdout
            .lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .unwrap_or_default()
            .to_string()
    };

    UnitStatus {
        unit: unit.to_string(),
        load_state: get("LoadState"),
        active_state: get("ActiveState"),
        sub_state: get("SubState"),
        result: get("Result"),
        since: get("ActiveEnterTimestamp"),
        main_pid: get("MainPID"),
        available: out.ok || !out.stdout.is_empty(),
    }
}

/// Recent journal lines for a unit.
pub fn journal(unit: &str, lines: usize) -> Result<String> {
    let unit = resolve_unit(unit)?;
    let n = lines.clamp(1, 2000).to_string();
    let out = run(
        "journalctl",
        &["-u", &unit, "-n", &n, "--no-pager", "-o", "short-iso"],
    );
    Ok(if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
tag = "console test"

[radio]
interface = "wlp1s0"
monitor = "wlp1s0mon0"
channel = 36
width = "80MHz"
interval_us = 10000

[capture]
mode = "passive"
duration = "60s"

[stream]
enabled = true
transport = "unix"

[export]
on_close = true
"#;

    #[test]
    fn names_that_escape_the_directory_are_refused() {
        assert!(safe_name("smoke").is_ok());
        assert!(safe_name("drift-24h").is_ok());
        assert!(safe_name("node_01.a").is_ok());
        for bad in [
            "",
            "../../etc/passwd",
            "a/b",
            ".hidden",
            "with space",
            "semi;colon",
            "back\\slash",
            "new\nline",
        ] {
            assert!(safe_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn only_project_units_are_readable() {
        assert_eq!(resolve_unit("csid@smoke").unwrap(), "csid@smoke.service");
        assert_eq!(
            resolve_unit("csid@drift-24h.service").unwrap(),
            "csid@drift-24h.service"
        );
        assert_eq!(resolve_unit("csid-sync").unwrap(), "csid-sync.service");
        assert_eq!(resolve_unit("csid-sync.timer").unwrap(), "csid-sync.timer");

        for bad in ["sshd", "csid@../root", "csid@a b", "systemd-logind", ""] {
            assert!(resolve_unit(bad).is_err(), "{bad:?} should be refused");
        }
    }

    /// The write surface is gone from the module, not merely unrouted.
    ///
    /// A handler can be un-wired and a function left behind for "later"; the
    /// next person wires it back. This is the cheapest way to state that the
    /// console cannot write, and it fails the moment one of them returns.
    #[test]
    fn nothing_in_this_module_can_change_the_node() {
        // Matched at the start of a line, so this test's own list of names —
        // which is quoted and indented — does not satisfy its own assertion.
        let src = include_str!("console.rs");
        for gone in [
            "\npub fn write_experiment",
            "\npub fn delete_experiment",
            "\npub fn write_global",
            "\nfn atomic_write",
            "\npub fn unit_action",
            "\npub fn list_sessions",
            "\npub fn session_dir",
        ] {
            assert!(
                !src.contains(gone),
                "{} is back; the console is meant to be read-only",
                gone.trim()
            );
        }
    }

    #[test]
    fn check_resolves_the_same_tuning_csid_would() {
        let c = check_experiment(GOOD);
        assert!(c.valid, "{:?}", c.error);
        let t = c.tuning.unwrap();
        assert_eq!(t.control_freq_mhz, 5180);
        assert_eq!(t.center_freq_mhz, Some(5210));
        assert_eq!(t.width, "80MHz");
        assert_eq!(t.duration_s, Some(60));
        assert_eq!(t.interval_rate_hz, 100.0);
        assert!(t.export_on_close);
        assert!(t.stream.unwrap().starts_with("unix"));
    }

    #[test]
    fn check_rejects_what_the_daemon_rejects() {
        // 160 MHz on 2.4 GHz — the case csid's own test pins.
        let bad = GOOD
            .replace("channel = 36", "channel = 6")
            .replace("\"80MHz\"", "\"160MHz\"");
        let c = check_experiment(&bad);
        assert!(!c.valid);
        assert!(c.error.unwrap().contains("2.4 GHz"));

        // An unknown key is a typo, and a typo silently ignored is how an
        // unattended run ends up capturing the wrong thing.
        let typo = format!("{GOOD}\n[bogus]\nx = 1\n");
        assert!(!check_experiment(&typo).valid);

        // Channel 132 is a legal 20/80 MHz control channel but belongs to no
        // 160 MHz group, so there is no centre frequency to tune to.
        let orphan = GOOD
            .replace("channel = 36", "channel = 132")
            .replace("\"80MHz\"", "\"160MHz\"");
        let c = check_experiment(&orphan);
        assert!(!c.valid);
        assert!(c.error.unwrap().contains("160MHz"));
    }

}
