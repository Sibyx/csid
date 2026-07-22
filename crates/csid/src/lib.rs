//! `csid` as a library — the daemon's internals, exposed so sibling tools reuse
//! them instead of reimplementing them.
//!
//! The binary (`src/main.rs`) is a thin CLI over this crate. The reason the
//! library exists is [`csiscope`]: an operator console that edits experiment
//! configuration and must reject an impossible capture **exactly** the way
//! `csid validate` does. Duplicating [`caps::validate_radio`] in a second crate
//! would guarantee the two drift apart, and the drift would only show up four
//! hours into an unattended run.
//!
//! Module map:
//!
//! | Module | Platform | Role |
//! |---|---|---|
//! | [`caps`] | any | Band/channel/width legality, ETSI centre-frequency tables, the measured envelope |
//! | [`config`] | any | The node-global and per-experiment TOML schema |
//! | [`sidecar`] | any | `metadata.json` — the provenance document |
//! | [`export`] | any | raw → `.csiq` |
//! | [`radio`] | Linux (execs `iw`) | Monitor setup, tuning, regdomain |
//! | [`debugfs`] | Linux | The `iwlmvm` CSI knobs |
//! | [`source`] | Linux | nl80211 vendor-event netlink consumption |
//! | [`engine`], [`sinks`], [`notify`] | Linux | Session orchestration, the two sinks, `sd_notify` |
//!
//! [`csiscope`]: https://github.com/Sibyx/csid/tree/master/crates/csiscope

pub mod caps;
pub mod commands;
pub mod config;
pub mod debugfs;
pub mod engine;
pub mod export;
pub mod notify;
pub mod radio;
pub mod sidecar;
pub mod sinks;
pub mod source;
pub mod util;

/// The `csid` version string, so consumers report the same one the daemon does.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
