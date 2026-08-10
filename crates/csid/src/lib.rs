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
//! | [`inject`] | Linux (AF_PACKET) | Paced monitor-mode frame injection (`capture.mode = "inject"`) |
//! | [`ble`] | any | BLE co-capture: pseudonymisation, the durable log, `ble_rssi.parquet` |
//! | [`hci`] | Linux (AF_BLUETOOTH) | The passive LE scan that feeds [`ble`] |
//! | [`timesync`] | any (rx: Linux AF_PACKET) | Time transfer over the illumination stream: payload stamps, inter-node skew, the phone affine fit, `time_transfer.parquet` |
//! | [`engine`], [`sinks`], [`notify`] | Linux | Session orchestration, the two sinks, `sd_notify` |
//! | [`thermal`] | Linux (Pi) | Die temperature and the firmware throttle word — what qualifies a node for a long run |
//! | [`marker`] | any | Block markers stamped in native `unix_ts_ns` — the boundary reference |
//! | [`fleet`] | any | The bench cockpit: fan-out status, the pre-registered gates, session lifecycle, clock coherence |
//!
//! The last two rows are the operator's half of the system rather than the
//! daemon's. `csid` on a capture node writes captures; the same binary on the
//! bench laptop drives ten of them ([`fleet`]) over the fleet's existing ssh
//! control channel. One binary, because a second tool would need its own copy
//! of the gate arithmetic and the two would drift — the same argument that put
//! [`caps::validate_radio`] in this library for `csiscope`.
//!
//! [`csiscope`]: https://github.com/Sibyx/csid/tree/master/crates/csiscope

pub mod ble;
pub mod caps;
pub mod commands;
pub mod config;
pub mod debugfs;
pub mod engine;
pub mod export;
pub mod fleet;
pub mod hci;
pub mod inject;
pub mod marker;
pub mod notify;
pub mod radio;
pub mod sidecar;
pub mod sinks;
pub mod source;
pub mod thermal;
pub mod timesync;
pub mod util;

/// The `csid` version string, so consumers report the same one the daemon does.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
