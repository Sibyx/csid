//! The transport: run one command on N nodes at once and collect the answers.
//!
//! ## Why ssh and not an agent
//!
//! The fleet already has exactly one universal, authenticated, always-on
//! control channel — ssh over the Headscale tailnet, which is what Ansible
//! rides on. Adding a csid-side network service to a capture node would mean a
//! second listening socket on every Pi, a second thing to secure, a second
//! thing to keep running, and a second thing that can be down when the cockpit
//! needs it. ssh is already there, already works, and already fails in ways the
//! operator recognises.
//!
//! ## The rules this module enforces
//!
//! - **`BatchMode=yes`.** A cockpit command must never block on a password
//!   prompt with ten threads behind it. No credential is prompted for, ever.
//! - **A hard deadline per node.** A node that hangs is reported as
//!   [`Unreachable::TimedOut`] and the other nine are still rendered. The
//!   failure this defends against is one dead node taking the whole readout
//!   with it.
//! - **Connection multiplexing.** `ControlMaster` + `ControlPersist` means the
//!   clock probe's five round trips cost five RTTs, not five TCP+TLS
//!   handshakes — which is the difference between a millisecond-scale clock
//!   instrument and a second-scale one. The socket lives in a temp directory:
//!   `~/.ssh` is the operator's, and this tool does not write there.
//! - **Failure is data.** Nothing here returns `Result` for a node-level
//!   problem; it returns a [`RemoteOutput`] that says what went wrong, so the
//!   caller can render `??` with a reason instead of aborting the sweep.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::health::Unreachable;

/// One node in the cockpit's inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// The fleet identity — `monad04`. This is what the table shows and what
    /// the capture's `host` column carries.
    pub name: String,
    /// How to reach it right now. Defaults to `name`, because the fleet's whole
    /// naming convention (`docs/FLEET-WIFI.md`) is that the MagicDNS name and
    /// the node identity are the same string.
    pub addr: String,
    pub user: Option<String>,
}

impl NodeSpec {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        NodeSpec {
            addr: name.clone(),
            name,
            user: None,
        }
    }

    /// `monad@monad04` or `monad04`.
    pub fn target(&self, default_user: Option<&str>) -> String {
        match self.user.as_deref().or(default_user) {
            Some(u) => format!("{u}@{}", self.addr),
            None => self.addr.clone(),
        }
    }
}

/// What a remote command produced.
#[derive(Debug, Clone)]
pub struct RemoteOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: Option<i32>,
    /// Wall time the whole round trip took, for the clock probe.
    pub elapsed: Duration,
    /// `Some` when the node did not produce a usable answer.
    pub unreachable: Option<Unreachable>,
}

impl RemoteOutput {
    pub fn ok(&self) -> bool {
        self.unreachable.is_none() && self.status == Some(0)
    }

    /// Stdout if the command succeeded, otherwise the reason it did not.
    pub fn require(&self) -> Result<&str, Unreachable> {
        if let Some(u) = &self.unreachable {
            return Err(u.clone());
        }
        if self.status == Some(0) {
            Ok(&self.stdout)
        } else {
            Err(Unreachable::RemoteFailed {
                code: self.status,
                stderr: if self.stderr.trim().is_empty() {
                    self.stdout.clone()
                } else {
                    self.stderr.clone()
                },
            })
        }
    }
}

/// How the cockpit talks to the fleet.
#[derive(Debug, Clone)]
pub struct SshRunner {
    pub user: Option<String>,
    pub connect_timeout: Duration,
    /// Deadline for the whole remote command.
    pub command_timeout: Duration,
    /// Extra `ssh` arguments from configuration, passed through verbatim.
    pub extra_args: Vec<String>,
    /// Multiplexing socket directory. `None` disables multiplexing.
    pub control_dir: Option<PathBuf>,
}

/// A Unix-domain socket path is bounded by `sun_path`: 104 bytes on macOS, 108
/// on Linux. OpenSSH refuses a longer `ControlPath` outright — every connection
/// fails with `ControlPath too long`.
///
/// This is not hypothetical. macOS puts `$TMPDIR` under
/// `/var/folders/<2>/<26>/T/`, so `std::env::temp_dir()` alone is ~49
/// characters; add a directory name and `%C`'s 64-character hash and the
/// template is ~130. The bench laptop is a Mac, so the obvious choice would
/// have broken multiplexing on exactly the machine the cockpit runs on — and
/// silently, because ssh still works without a mux socket, just one full
/// handshake per round trip. The clock probe would have degraded from a
/// millisecond instrument to a second-scale one with no error message.
const CONTROL_PATH_LIMIT: usize = 100;

/// A short, per-user multiplexing directory that fits [`CONTROL_PATH_LIMIT`].
fn default_control_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        Some(PathBuf::from(format!("/tmp/csid-mux-{uid}")))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

impl Default for SshRunner {
    fn default() -> Self {
        SshRunner {
            user: None,
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(30),
            extra_args: Vec::new(),
            control_dir: default_control_dir(),
        }
    }
}

/// The `ControlPath` template for a directory, or `None` when it would exceed
/// what a Unix socket can hold. `%C` expands to a 64-character hash.
pub fn control_path(dir: &std::path::Path) -> Option<String> {
    let template = format!("{}/%C", dir.display());
    // `%C` is two characters in the template and 64 when expanded.
    let expanded = template.len() - 2 + 64;
    (expanded <= CONTROL_PATH_LIMIT).then_some(template)
}

impl SshRunner {
    fn base_args(&self, node: &NodeSpec) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs().max(1)),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            // The operator's ~/.ssh/config may set up port forwards for the
            // observability tunnel; a cockpit sweep must not trip over an
            // already-bound port (the same fix the Ansible inventory carries
            // for ccx33).
            "-o".into(),
            "ClearAllForwardings=yes".into(),
        ];
        if let Some(dir) = &self.control_dir {
            match control_path(dir) {
                Some(path) => {
                    let _ = std::fs::create_dir_all(dir);
                    a.extend([
                        "-o".into(),
                        "ControlMaster=auto".into(),
                        "-o".into(),
                        format!("ControlPath={path}"),
                        "-o".into(),
                        "ControlPersist=90s".into(),
                    ]);
                }
                // Degrade loudly rather than fail every connection: without a
                // mux socket the sweep still works, it just pays a full
                // handshake per round trip — and the clock probe's uncertainty
                // widens accordingly, which is exactly the sort of silent
                // precision loss the operator has to be told about.
                None => tracing::warn!(
                    dir = %dir.display(),
                    "ControlPath would exceed the Unix-socket length limit; \
                     running without ssh multiplexing (slower, and clock offsets \
                     will carry a much larger uncertainty)"
                ),
            }
        }
        a.extend(self.extra_args.iter().cloned());
        a.push(node.target(self.user.as_deref()));
        a
    }

    /// Run one command on one node, with a hard deadline.
    ///
    /// The deadline is enforced by killing the child, which also tears down the
    /// ssh session. A hung node therefore costs `command_timeout`, once, and
    /// never blocks the sweep.
    pub fn run(&self, node: &NodeSpec, command: &str) -> RemoteOutput {
        let started = Instant::now();
        let mut args = self.base_args(node);
        args.push(command.to_string());

        let child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                return RemoteOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    status: None,
                    elapsed: started.elapsed(),
                    unreachable: Some(Unreachable::SshFailed(format!("could not spawn ssh: {e}"))),
                }
            }
        };

        // Drain both pipes on their own threads: a command that fills the 64 KB
        // stderr pipe while we wait on stdout would deadlock, and `csid fleet
        // probe --json` on a chatty node is well within reach of that.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(p) = &mut out_pipe {
                let _ = p.read_to_string(&mut s);
            }
            let _ = out_tx.send(s);
        });
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(p) = &mut err_pipe {
                let _ = p.read_to_string(&mut s);
            }
            let _ = err_tx.send(s);
        });

        let deadline = Instant::now() + self.command_timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    break None;
                }
            }
        };

        let stdout = out_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();
        let stderr = err_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();
        let elapsed = started.elapsed();

        match status {
            None => RemoteOutput {
                stdout,
                stderr,
                status: None,
                elapsed,
                unreachable: Some(Unreachable::TimedOut),
            },
            Some(s) => {
                let code = s.code();
                // ssh exits 255 for its own transport failures, which is a
                // different problem from the remote command failing.
                let unreachable = (code == Some(255)).then(|| {
                    Unreachable::SshFailed(if stderr.trim().is_empty() {
                        "connection failed".to_string()
                    } else {
                        stderr.trim().to_string()
                    })
                });
                RemoteOutput {
                    stdout,
                    stderr,
                    status: code,
                    elapsed,
                    unreachable,
                }
            }
        }
    }

    /// Run the same command on every node, concurrently.
    ///
    /// One thread per node. Ten threads that spend their lives in `read` is not
    /// a scheduling problem, and a thread pool here would only add a way for a
    /// slow node to delay a fast one.
    pub fn run_all(&self, nodes: &[NodeSpec], command: &str) -> Vec<(NodeSpec, RemoteOutput)> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = nodes
                .iter()
                .map(|n| {
                    let runner = self;
                    scope.spawn(move || (n.clone(), runner.run(n, command)))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        // A panicked worker must not panic the sweep.
                        (
                            NodeSpec::new("?"),
                            RemoteOutput {
                                stdout: String::new(),
                                stderr: String::new(),
                                status: None,
                                elapsed: Duration::ZERO,
                                unreachable: Some(Unreachable::SshFailed(
                                    "the cockpit's worker thread panicked".into(),
                                )),
                            },
                        )
                    })
                })
                .collect()
        })
    }

    /// Run a *different* command per node — the session-start path, where each
    /// node may get its own experiment or tag.
    pub fn run_each(&self, jobs: &[(NodeSpec, String)]) -> Vec<(NodeSpec, RemoteOutput)> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .iter()
                .map(|(n, cmd)| {
                    let runner = self;
                    scope.spawn(move || (n.clone(), runner.run(n, cmd)))
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        })
    }
}

/// Quote a string for `sh -c` on the far side.
///
/// Single-quote wrapping with `'\''` escaping is the only form that is safe for
/// arbitrary content, and marker text is arbitrary content: a block id or an
/// operator note is typed at a bench at 11pm and will eventually contain a
/// quote, a space, a semicolon or a `$`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_defaults_to_being_addressed_by_its_fleet_name() {
        let n = NodeSpec::new("monad04");
        assert_eq!(n.addr, "monad04");
        assert_eq!(n.target(Some("monad")), "monad@monad04");
        assert_eq!(n.target(None), "monad04");

        let mut n = NodeSpec::new("monad04");
        n.user = Some("root".into());
        assert_eq!(
            n.target(Some("monad")),
            "root@monad04",
            "a per-node user overrides the fleet default"
        );
    }

    #[test]
    fn the_ssh_invocation_never_prompts_and_never_forwards_ports() {
        let r = SshRunner::default();
        let args = r.base_args(&NodeSpec::new("monad04"));
        let joined = args.join(" ");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert!(joined.contains("ClearAllForwardings=yes"), "{joined}");
        assert!(joined.contains("ConnectTimeout=5"), "{joined}");
        // Multiplexing is a Unix-socket feature: `default_control_dir` returns
        // None off Unix, and the invocation correctly omits ControlMaster
        // there. Asserting it unconditionally only tests the host OS.
        #[cfg(unix)]
        {
            assert!(joined.contains("ControlMaster=auto"), "{joined}");
            assert!(joined.contains("ControlPersist"), "{joined}");
        }
        assert_eq!(args.last().unwrap(), "monad04");
        // The mux socket must not live in the operator's ~/.ssh.
        assert!(
            !joined.contains(".ssh"),
            "the cockpit does not write into ~/.ssh: {joined}"
        );
    }

    #[test]
    fn multiplexing_can_be_turned_off() {
        let r = SshRunner {
            control_dir: None,
            ..SshRunner::default()
        };
        let joined = r.base_args(&NodeSpec::new("monad04")).join(" ");
        assert!(!joined.contains("ControlMaster"), "{joined}");
    }

    /// Regression: the first version of this used `std::env::temp_dir()`, which
    /// on macOS is `/var/folders/<2>/<26>/T/`. With `%C`'s 64-character hash
    /// that template is ~130 bytes and OpenSSH rejects every connection with
    /// `ControlPath too long` — on the exact machine the cockpit runs on.
    ///
    /// Unix-only by construction: the thing under test is the length budget of
    /// a Unix domain socket path, which does not exist on Windows.
    #[test]
    #[cfg(unix)]
    fn the_default_control_path_fits_a_unix_socket_on_this_machine() {
        let dir = default_control_dir().expect("unix has a control dir");
        let path = control_path(&dir)
            .unwrap_or_else(|| panic!("the default ControlPath must fit: {}", dir.display()));
        assert!(
            path.len() - 2 + 64 <= CONTROL_PATH_LIMIT,
            "{path} expands past the limit"
        );

        // And the real invocation carries it.
        let joined = SshRunner::default()
            .base_args(&NodeSpec::new("monad04"))
            .join(" ");
        assert!(joined.contains("ControlPath=/tmp/csid-mux-"), "{joined}");
    }

    /// A long directory must disable multiplexing, not produce an invocation
    /// that fails on every node.
    #[test]
    fn an_over_long_control_dir_degrades_to_no_multiplexing() {
        let long = std::path::PathBuf::from(
            "/var/folders/pn/gvsh7sg50897b143wlyzqzd80000gn/T/csid-fleet-mux",
        );
        assert_eq!(control_path(&long), None);

        let r = SshRunner {
            control_dir: Some(long),
            ..SshRunner::default()
        };
        let joined = r.base_args(&NodeSpec::new("monad04")).join(" ");
        assert!(!joined.contains("ControlPath"), "{joined}");
        assert!(!joined.contains("ControlMaster"), "{joined}");
        // The rest of the invocation is unaffected.
        assert!(joined.contains("BatchMode=yes"), "{joined}");
    }

    #[test]
    fn shell_quoting_survives_what_an_operator_actually_types() {
        assert_eq!(shell_quote("ZONE-A"), "'ZONE-A'");
        assert_eq!(shell_quote("S1 ZA CYC 03"), "'S1 ZA CYC 03'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a; b"), "'a; b'");
        // Nothing escapes the quotes.
        for s in ["'", "''", "'; touch /tmp/x; '"] {
            let q = shell_quote(s);
            assert!(q.starts_with('\'') && q.ends_with('\''), "{q}");
        }
    }

    /// A transport failure must come back as data, not as an error that aborts
    /// the sweep — the whole table has to render even when a node is dark.
    #[test]
    fn an_unreachable_host_returns_a_reason_rather_than_failing_the_sweep() {
        let r = SshRunner {
            connect_timeout: Duration::from_secs(1),
            command_timeout: Duration::from_secs(8),
            // Force a fast, deterministic failure: an address that cannot
            // resolve, and no multiplexing socket to reuse.
            control_dir: None,
            ..SshRunner::default()
        };
        let node = NodeSpec::new("csid-fleet-test-host.invalid");
        let out = r.run(&node, "true");
        assert!(!out.ok());
        let err = out.require().unwrap_err();
        // Either ssh refused to connect, or ssh itself is absent on this
        // machine — both are "we did not measure this node", which is the
        // property under test.
        assert!(
            matches!(err, Unreachable::SshFailed(_) | Unreachable::TimedOut),
            "{err:?}"
        );
        assert!(!err.summary().is_empty());
    }

    #[test]
    fn a_failing_remote_command_is_distinguished_from_an_unreachable_node() {
        let out = RemoteOutput {
            stdout: String::new(),
            stderr: "csid: command not found".into(),
            status: Some(127),
            elapsed: Duration::from_millis(40),
            unreachable: None,
        };
        assert!(!out.ok());
        match out.require().unwrap_err() {
            Unreachable::RemoteFailed { code, stderr } => {
                assert_eq!(code, Some(127));
                assert!(stderr.contains("not found"));
            }
            other => panic!("{other:?}"),
        }
    }
}
