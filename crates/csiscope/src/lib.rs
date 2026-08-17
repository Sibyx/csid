//! `csiscope` — a live oscilloscope and operator console for `csid`.
//!
//! The crate is a library with a thin binary on top, so the analysis path can
//! be benchmarked and tested from outside the process that serves it. That
//! matters here more than it usually would: the console runs on the same Pi 5
//! that is capturing, niced below the RX thread, and "what does a frame cost"
//! is a number the crate has to be able to answer on demand rather than infer
//! from a flame graph after the fact.
//!
//! ## The shape of the analysis path
//!
//! ```text
//!   ingest  ->  Hub (bounded ring)
//!                 |
//!                 |  tail_into / since_into   (no steady-state allocation)
//!                 v
//!            Analysis  ---- shared per (view, tick), computed once ----.
//!                 |                                                    |
//!                 |  Scratch arena, cached FFT plans, 2-thread pool    |
//!                 v                                                    v
//!            SharedFrame  ------ fan out ------>  one client's waterfall
//!                                                        |
//!                                                        v
//!                                                  binary WS frame
//! ```
//!
//! Everything derived from the window — spectrum, phase, bundle, impulse
//! response, Doppler, timing, mixes — depends only on the view settings and
//! the ring, so it is computed once per tick per distinct view and shared by
//! every client watching it. Only the waterfall is genuinely per-client,
//! because each client holds its own cursor into the ring.

pub mod analyze;
pub mod bandplan;
pub mod capture;
pub mod class;
pub mod console;
pub mod dsp;
pub mod frame;
pub mod ingest;
pub mod pipeline;
pub mod server;
pub mod state;
pub mod tones;
pub mod wire;

/// Records held for the windowed views. At the measured 608 Hz ceiling this is
/// about 13 seconds of history — comfortably more than the longest window the
/// UI offers.
pub const DEFAULT_HISTORY: usize = 8192;

/// Total I/Q coefficient budget for the ring, in complex samples. 4 M is ≈16 MiB
/// of `i16` pairs and bounds the console's footprint independently of tone
/// count: a 996-tone 2×2 record is 76× larger than a legacy 52-tone 1×1 one.
pub const DEFAULT_COEFF_BUDGET: usize = 4_000_000;
