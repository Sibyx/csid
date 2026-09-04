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
//! | [`census`] | any (rx: Linux AF_PACKET) | Frame census: who is on the air from the raw 802.11 header, per minute, plus the beamforming-feedback frames that make BFI sensing possible |
//! | [`survey`] | any (scan: Linux, execs `iw`) | Channel survey on the management radio at open and close — where the access points are |
//! | [`rawsock`] | Linux | The one `AF_PACKET` receive socket the two rows above share |
//! | [`engine`], [`sinks`], [`notify`] | Linux | Session orchestration, the two sinks, `sd_notify` |
//! | [`segment`] | any | Rotating a long capture into session-shaped segments that sync and prune while it runs |
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
pub mod census;
pub mod commands;
pub mod config;
pub mod debugfs;
pub mod engine;
pub mod export;
pub mod fleet;
pub mod hci;
pub mod inject;
pub mod marker;
pub mod nodestate;
pub mod notify;
pub mod radio;
pub mod rawsock;
pub mod segment;
pub mod sidecar;
pub mod sinks;
pub mod source;
pub mod status;
pub mod survey;
pub mod thermal;
pub mod timesync;
pub mod util;

/// The `csid` version string, so consumers report the same one the daemon does.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-time provenance, baked in by `build.rs` (IP-139 Phase 3).
///
/// The semantic [`VERSION`] answers "which release is this". These answer
/// "which build", which is the question the archive could not answer at all:
/// every capture ever taken carries `csid_version = "0.1.0"`, because that
/// literal was never bumped while the daemon gained injection, time transfer,
/// segmentation, the BLE scanner and the empty-record counter.
///
/// [`REVISION`] is empty when the build could not name itself, and
/// [`REVISION_SOURCE`] says why. That happens by design on the fleet: the csid
/// Ansible role rsyncs the source to each node with `--exclude=.git` and
/// compiles there, so `git describe` has nothing to read. The control host does
/// hold the checkout, so it can pass an identity in through the
/// `CSID_BUILD_REVISION` environment variable at build time — the role already
/// computes a deterministic source content hash that is exactly the right value.
///
/// A build that cannot name its revision says so. It never guesses one.
pub mod build_info {
    /// `git describe` output, an operator-supplied identity, or empty.
    pub const REVISION: &str = env!("CSID_BUILD_REVISION");
    /// `git` · `supplied` · `none`.
    pub const REVISION_SOURCE: &str = env!("CSID_BUILD_REVISION_SOURCE");
    /// Compile time, seconds since the Unix epoch.
    pub const EPOCH: &str = env!("CSID_BUILD_EPOCH");
    /// The compiler that built this binary.
    pub const RUSTC: &str = env!("CSID_BUILD_RUSTC");
    /// `release` or `debug`.
    pub const PROFILE: &str = env!("CSID_BUILD_PROFILE");

    /// Compile time as RFC 3339 UTC, or an empty string if it did not parse.
    pub fn built_at() -> String {
        match EPOCH.parse::<u64>() {
            Ok(0) | Err(_) => String::new(),
            Ok(secs) => crate::util::rfc3339_utc(secs),
        }
    }
}
