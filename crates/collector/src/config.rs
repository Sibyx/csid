//! TOML configuration, in the same idiom as `csid`: a single node-global file, strict field
//! checking, and a `validate` subcommand so a candidate file can be checked before it is installed
//! — the `nginx -t` discipline applied to a daemon.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PATH: &str = "/etc/collector/config.toml";

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub listen: ListenConfig,
    #[serde(default)]
    pub session: SessionConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Node name recorded into every sidecar. Defaults to the hostname.
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    /// Bind address. Defaults to all interfaces: the phone arrives over the experiment AP, whose
    /// address on this node is not knowable ahead of time.
    pub bind: String,
    /// Answer clock exchanges. Disabling turns the collector into a pure sink.
    pub answer_time_requests: bool,
    /// Receive buffer size; a 1500-byte MTU bounds a single datagram.
    pub buffer_bytes: usize,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:9999".to_string(),
            answer_time_requests: true,
            buffer_bytes: 2048,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Where sessions are written. Deliberately NOT csid's spool: sharing it would expose these
    /// artefacts to `csid-prune`, which deletes by csid's filenames and would leave the
    /// collector's growing without bound.
    pub spool: PathBuf,
    /// Silence after which a session is considered finished and becomes shippable. A phone never
    /// announces the end of a session — it runs out of battery or walks out of range — so quiet is
    /// the only end-of-session signal available.
    pub idle_timeout_seconds: u64,
    /// How often the sidecar of a live session is refreshed, so a session in progress is
    /// inspectable from the node without waiting for it to close.
    pub heartbeat_seconds: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            spool: PathBuf::from("/var/lib/collector/sessions"),
            idle_timeout_seconds: 120,
            heartbeat_seconds: 15,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.listen.bind.parse::<std::net::SocketAddr>().is_err() {
            bail!(
                "listen.bind must be an ip:port literal, got '{}'",
                self.listen.bind
            );
        }
        if self.listen.buffer_bytes < 64 {
            bail!("listen.buffer_bytes must be at least 64");
        }
        if self.session.idle_timeout_seconds == 0 {
            bail!("session.idle_timeout_seconds must be non-zero, or no session would ever close");
        }
        Ok(())
    }

    pub fn node_name(&self) -> String {
        if !self.node.name.is_empty() {
            return self.node.name.clone();
        }
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "unknown-node".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn rejects_a_hostname_bind() {
        let mut c = Config::default();
        c.listen.bind = "collector.local:9999".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_a_zero_idle_timeout() {
        let mut c = Config::default();
        c.session.idle_timeout_seconds = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let toml = "[listen]\nbind = \"0.0.0.0:1\"\nanswer_time_requests = true\n\
                    buffer_bytes = 2048\nnonsense = 1\n";
        assert!(toml::from_str::<Config>(toml).is_err());
    }
}
