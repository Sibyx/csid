//! The operator surface: experiment configuration, unit control, session
//! browsing.
//!
//! Two rules shape this module.
//!
//! **Validation is not reimplemented.** An experiment is parsed with
//! [`csid::config::ExperimentConfig`] and checked with the same
//! [`csid::caps::validate_radio`] the daemon runs, so the console rejects
//! exactly what `csid validate` rejects. A second implementation would drift,
//! and the drift would surface four hours into an unattended run.
//!
//! **No authentication is not the same as no input validation.** The console is
//! deliberately unauthenticated — it is a lab instrument on a lab network — but
//! every name that reaches the filesystem or `systemctl` is checked against a
//! strict pattern first, and unit control is restricted to units this project
//! owns. Nothing here interpolates into a shell.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use csid::config::{ExperimentConfig, GlobalConfig};

/// Units `csiscope` is willing to act on. Anything else is refused, so an
/// unauthenticated console can never be talked into restarting sshd.
const CONTROLLABLE: &[&str] = &["csid-sync", "csid-prune", "csid-driver-guard"];

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
    if CONTROLLABLE.contains(&base) {
        return Ok(if unit.contains('.') {
            unit.to_string()
        } else {
            format!("{base}.service")
        });
    }
    bail!("{unit} is not a unit csiscope controls")
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

/// Write an experiment, refusing anything the daemon would reject.
///
/// The write is atomic (temp file + rename) so a half-written file can never be
/// what a `systemctl start` picks up.
pub fn write_experiment(dir: &Path, name: &str, text: &str) -> Result<Check> {
    safe_name(name)?;
    let check = check_experiment(text);
    if !check.valid {
        bail!(
            "refusing to write an invalid configuration: {}",
            check.error.as_deref().unwrap_or("unknown error")
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    atomic_write(&dir.join(format!("{name}.toml")), text)?;
    Ok(check)
}

/// Delete an experiment file.
pub fn delete_experiment(dir: &Path, name: &str) -> Result<()> {
    safe_name(name)?;
    let path = dir.join(format!("{name}.toml"));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// Parse-check a candidate node-global configuration.
pub fn check_global(text: &str) -> Check {
    match toml::from_str::<GlobalConfig>(text) {
        Ok(cfg) => Check {
            valid: true,
            config: serde_json::to_value(&cfg).ok(),
            ..Default::default()
        },
        Err(e) => Check {
            valid: false,
            error: Some(format!("TOML: {e}")),
            ..Default::default()
        },
    }
}

/// Write the node-global configuration, atomically, after a parse check.
pub fn write_global(path: &Path, text: &str) -> Result<Check> {
    let check = check_global(text);
    if !check.valid {
        bail!(
            "refusing to write an invalid configuration: {}",
            check.error.as_deref().unwrap_or("unknown error")
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    atomic_write(path, text)?;
    Ok(check)
}

/// Write via a sibling temp file and rename, so readers never see a partial
/// file and a failed write leaves the previous configuration intact.
fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("csiscope-tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("installing {}", path.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
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

/// `systemctl start|stop|restart <unit>`, with the unit checked first.
pub fn unit_action(unit: &str, action: &str) -> Result<Run> {
    let unit = resolve_unit(unit)?;
    if !matches!(action, "start" | "stop" | "restart") {
        bail!("action must be start, stop or restart");
    }
    Ok(run("systemctl", &[action, &unit]))
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

// -- sessions -----------------------------------------------------------------

/// A capture session in the spool, described by its sidecar.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub path: String,
    pub raw_bytes: u64,
    pub csiq_bytes: u64,
    pub modified_ns: u64,
    /// The sidecar verbatim. Design rule from IP-120: it alone must suffice to
    /// interpret the capture, so the console shows all of it rather than a
    /// curated subset.
    pub sidecar: Option<serde_json::Value>,
}

/// List spool sessions, newest first.
pub fn list_sessions(spool: &Path, limit: usize) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(spool) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let path = e.path();
            let id = path.file_name()?.to_string_lossy().to_string();
            safe_name(&id).ok()?;
            Some(describe_session(&path, id))
        })
        .collect();
    out.sort_by(|a, b| b.modified_ns.cmp(&a.modified_ns));
    out.truncate(limit);
    out
}

fn describe_session(path: &Path, id: String) -> Session {
    let size = |name: &str| {
        std::fs::metadata(path.join(name))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let modified_ns = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    Session {
        sidecar: std::fs::read_to_string(path.join("metadata.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok()),
        raw_bytes: size("capture.raw"),
        csiq_bytes: size("capture.csiq"),
        modified_ns,
        path: path.display().to_string(),
        id,
    }
}

/// Resolve a session id to a directory inside the spool.
pub fn session_dir(spool: &Path, id: &str) -> Result<PathBuf> {
    safe_name(id)?;
    let dir = spool.join(id);
    if !dir.is_dir() {
        bail!("no session {id} under {}", spool.display());
    }
    Ok(dir)
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
    fn only_project_units_are_controllable() {
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

    #[test]
    fn writes_are_atomic_and_gated_on_validity() {
        let dir = std::env::temp_dir().join(format!("csiscope-console-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        write_experiment(&dir, "unit-test", GOOD).unwrap();
        let back = read_experiment(&dir, "unit-test").unwrap();
        assert!(back.check.valid);
        assert_eq!(back.toml, GOOD);

        // A rejected write must leave the good file in place.
        let err = write_experiment(&dir, "unit-test", "this is not toml").unwrap_err();
        assert!(err.to_string().contains("refusing"));
        assert_eq!(read_experiment(&dir, "unit-test").unwrap().toml, GOOD);
        assert!(
            !dir.join("unit-test.csiscope-tmp").exists(),
            "no temp file may survive"
        );

        assert!(write_experiment(&dir, "../escape", GOOD).is_err());

        let listed = list_experiments(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "unit-test");

        delete_experiment(&dir, "unit-test").unwrap();
        assert!(list_experiments(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn global_config_round_trips() {
        let text = r#"
[node]
spool = "/var/lib/csid"

[driver]
vendor_oui = 0x001735
"#;
        let c = check_global(text);
        assert!(c.valid, "{:?}", c.error);
        assert_eq!(c.config.unwrap()["node"]["spool"], "/var/lib/csid");
        assert!(!check_global("[node]\nspool = 12").valid);
    }

    #[test]
    fn sessions_are_described_from_the_sidecar() {
        let dir = std::env::temp_dir().join(format!("csiscope-spool-{}", std::process::id()));
        let s = dir.join("monad05_smoke_20260722-093107");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("capture.raw"), vec![0u8; 4096]).unwrap();
        std::fs::write(
            s.join("metadata.json"),
            r#"{"session_id":"monad05_smoke_20260722-093107","status":"complete"}"#,
        )
        .unwrap();

        let list = list_sessions(&dir, 10);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].raw_bytes, 4096);
        assert_eq!(list[0].csiq_bytes, 0);
        assert_eq!(list[0].sidecar.as_ref().unwrap()["status"], "complete");

        assert!(session_dir(&dir, "monad05_smoke_20260722-093107").is_ok());
        assert!(session_dir(&dir, "nope").is_err());
        assert!(session_dir(&dir, "../etc").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
