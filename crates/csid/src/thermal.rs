//! SoC temperature and the firmware's throttle state.
//!
//! ## Why this is in `csid` and not left to the metrics stack
//!
//! The fleet nodes pin `cpufreq.default_governor=performance` so the capture
//! path has a clock that does not move underneath it. Thermal throttling
//! defeats that silently: at 80 °C the Pi 5 firmware reduces the ARM clock
//! regardless of the governor, and nothing in `cpufreq` reports it — on
//! monad02 on 2026-07-28, `scaling_cur_freq` read a confident 2 400 000 while
//! the firmware's own flag said the soft limit had already engaged.
//!
//! So a capture can be timing-degraded in exactly the way the governor exists
//! to prevent, and every OS-level signal says everything is fine. A benchmark
//! that reports a rate without reporting the temperature it was achieved at is
//! not reproducible: run it on a cold node and a hot node and you measure the
//! enclosure, not the radio.
//!
//! ## Cost
//!
//! Two small `read()`s on sysfs per sample, from a sampler thread that never
//! touches the RX path, the ring, or either sink. At the default cadence this
//! is a few microseconds a second. It is deliberately NOT wired into
//! `engine::run_session`'s hot loop.
//!
//! ## Platform
//!
//! Both files are Raspberry Pi specifics. Everything here degrades to `None`
//! rather than failing, so the harness runs (without a thermal verdict) on a
//! development machine.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Die temperature, in millidegrees Celsius.
const THERMAL_ZONE: &str = "/sys/class/thermal/thermal_zone0/temp";

/// The firmware's throttle bitmask. Same value as `vcgencmd get_throttled`.
const GET_THROTTLED: &str = "/sys/devices/platform/soc/soc:firmware/get_throttled";

/// The clock is reduced at and above this die temperature.
///
/// Raspberry Pi 5. Below it the ARM clock is whatever the governor asks for;
/// at it the firmware starts taking the clock away.
pub const SOFT_LIMIT_C: f32 = 80.0;

/// Heavier throttling, and the point a capture's timing is definitely not what
/// the experiment configuration says it is.
pub const HARD_LIMIT_C: f32 = 85.0;

/// Peak temperature a node must stay under to be trusted for a long unattended
/// run.
///
/// Five degrees below the soft limit. The margin is not arbitrary: it is the
/// measured cost of a full-rate capture on a node that was otherwise idle
/// (54.0 → 59.5 °C at 608 Hz, `pi5-csi-iax`, 2026-07-21). A node that cannot
/// absorb that much without reaching the soft limit cannot run the drift
/// experiment, because the experiment IS a full-rate capture.
pub const SAFE_PEAK_C: f32 = SOFT_LIMIT_C - 5.0;

/// The firmware's throttle word, split into "right now" and "since boot".
///
/// The sticky half is why this is worth reading at all: a node can look
/// perfectly healthy at the moment you check it and still have spent an hour
/// throttled overnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Throttle {
    pub raw: u32,
}

impl Throttle {
    pub const UNDERVOLT_NOW: u32 = 1 << 0;
    pub const FREQ_CAPPED_NOW: u32 = 1 << 1;
    pub const THROTTLED_NOW: u32 = 1 << 2;
    pub const SOFT_LIMIT_NOW: u32 = 1 << 3;
    pub const UNDERVOLT_SEEN: u32 = 1 << 16;
    pub const FREQ_CAPPED_SEEN: u32 = 1 << 17;
    pub const THROTTLED_SEEN: u32 = 1 << 18;
    pub const SOFT_LIMIT_SEEN: u32 = 1 << 19;

    pub fn bit(&self, mask: u32) -> bool {
        self.raw & mask != 0
    }

    /// Anything currently degrading the clock or the supply.
    pub fn degraded_now(&self) -> bool {
        self.raw
            & (Self::UNDERVOLT_NOW
                | Self::FREQ_CAPPED_NOW
                | Self::THROTTLED_NOW
                | Self::SOFT_LIMIT_NOW)
            != 0
    }

    /// Anything that has degraded the clock or the supply since boot.
    pub fn degraded_since_boot(&self) -> bool {
        self.raw
            & (Self::UNDERVOLT_SEEN
                | Self::FREQ_CAPPED_SEEN
                | Self::THROTTLED_SEEN
                | Self::SOFT_LIMIT_SEEN)
            != 0
    }

    /// Human-readable flag list, for a report line.
    pub fn describe(&self) -> String {
        if self.raw == 0 {
            return "none".into();
        }
        let mut out = Vec::new();
        for (mask, name) in [
            (Self::UNDERVOLT_NOW, "undervolt"),
            (Self::FREQ_CAPPED_NOW, "freq-capped"),
            (Self::THROTTLED_NOW, "throttled"),
            (Self::SOFT_LIMIT_NOW, "soft-limit"),
            (Self::UNDERVOLT_SEEN, "undervolt-since-boot"),
            (Self::FREQ_CAPPED_SEEN, "freq-capped-since-boot"),
            (Self::THROTTLED_SEEN, "throttled-since-boot"),
            (Self::SOFT_LIMIT_SEEN, "soft-limit-since-boot"),
        ] {
            if self.bit(mask) {
                out.push(name);
            }
        }
        out.join(",")
    }
}

/// Read the die temperature in degrees Celsius.
pub fn read_temp_c() -> Option<f32> {
    read_temp_c_at(Path::new(THERMAL_ZONE))
}

fn read_temp_c_at(path: &Path) -> Option<f32> {
    parse_temp_c(&fs::read_to_string(path).ok()?)
}

/// Millidegrees, one integer, trailing newline.
fn parse_temp_c(raw: &str) -> Option<f32> {
    let milli: i64 = raw.trim().parse().ok()?;
    Some(milli as f32 / 1000.0)
}

/// Read the firmware throttle word.
pub fn read_throttle() -> Option<Throttle> {
    read_throttle_at(Path::new(GET_THROTTLED))
}

fn read_throttle_at(path: &Path) -> Option<Throttle> {
    parse_throttle(&fs::read_to_string(path).ok()?)
}

/// The sysfs file is bare hex without an `0x`; `vcgencmd` prints
/// `throttled=0x...`. Accept every spelling so an operator can paste either.
fn parse_throttle(raw: &str) -> Option<Throttle> {
    let t = raw.trim();
    let t = t.strip_prefix("throttled=").unwrap_or(t);
    let t = t.strip_prefix("0x").unwrap_or(t);
    u32::from_str_radix(t, 16).ok().map(|raw| Throttle { raw })
}

/// What a node's thermals did over the span of a measurement.
#[derive(Debug, Clone, Default)]
pub struct ThermalTrace {
    pub samples: usize,
    pub min_c: f32,
    pub mean_c: f32,
    pub max_c: f32,
    /// Throttle word at the start, before the workload.
    pub throttle_before: Option<Throttle>,
    /// Throttle word at the end.
    pub throttle_after: Option<Throttle>,
    /// True if any sample DURING the run had a live degradation bit set. This
    /// is the one that indicts the run itself, as opposed to its history.
    pub degraded_during: bool,
}

impl ThermalTrace {
    /// Whether a temperature reading was available at all.
    pub fn is_measured(&self) -> bool {
        self.samples > 0
    }

    /// Degrees of margin between the hottest sample and the soft limit.
    /// Negative means the firmware was taking the clock away.
    pub fn headroom_c(&self) -> f32 {
        SOFT_LIMIT_C - self.max_c
    }

    /// The verdict a stress run exists to produce.
    pub fn verdict(&self) -> Verdict {
        if !self.is_measured() {
            return Verdict::Unmeasured;
        }
        let undervolt = self
            .throttle_after
            .map(|t| t.bit(Throttle::UNDERVOLT_NOW) || t.bit(Throttle::UNDERVOLT_SEEN))
            .unwrap_or(false);
        // Under-voltage is never "marginal". It corrupts captures and SD cards
        // and it means the power supply, not the cooling, is wrong.
        if undervolt {
            return Verdict::Fail;
        }
        if self.max_c >= SOFT_LIMIT_C || self.degraded_during {
            return Verdict::Fail;
        }
        if self.max_c >= SAFE_PEAK_C {
            return Verdict::Marginal;
        }
        Verdict::Pass
    }
}

/// Whether a node may be trusted with a long unattended capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Peak stayed below [`SAFE_PEAK_C`] with no degradation: run the experiment.
    Pass,
    /// Peak entered the last five degrees before the soft limit. The capture is
    /// valid, but a warmer room or a longer run will cross the line.
    Marginal,
    /// The soft limit was reached, the firmware degraded the clock, or the
    /// supply sagged. Timing-sensitive results from this node are not
    /// trustworthy.
    Fail,
    /// No thermal zone — not a Pi, or sysfs is not where it should be.
    Unmeasured,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Marginal => "MARGINAL",
            Verdict::Fail => "FAIL",
            Verdict::Unmeasured => "UNMEASURED",
        }
    }
}

/// Samples temperature on its own thread for the duration of a workload.
///
/// Deliberately a separate thread rather than a hook in the session loop: the
/// RX thread runs `SCHED_RR` priority 50 and the entire design of the capture
/// path is that nothing optional shares it.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<f32>>>,
    degraded: Arc<AtomicBool>,
    before: Option<Throttle>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Sampler {
    /// Start sampling every `interval`.
    pub fn start(interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let degraded = Arc::new(AtomicBool::new(false));
        let before = read_throttle();

        let handle = {
            let stop = stop.clone();
            let samples = samples.clone();
            let degraded = degraded.clone();
            thread::Builder::new()
                .name("csid-thermal".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(c) = read_temp_c() {
                            if let Ok(mut s) = samples.lock() {
                                s.push(c);
                            }
                        }
                        if let Some(t) = read_throttle() {
                            if t.degraded_now() {
                                degraded.store(true, Ordering::Relaxed);
                            }
                        }
                        thread::sleep(interval);
                    }
                })
                .ok()
        };

        Sampler {
            stop,
            samples,
            degraded,
            before,
            handle,
        }
    }

    /// Stop sampling and summarise.
    pub fn finish(mut self) -> ThermalTrace {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let samples = self
            .samples
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone());

        let mut trace = ThermalTrace {
            samples: samples.len(),
            throttle_before: self.before,
            throttle_after: read_throttle(),
            degraded_during: self.degraded.load(Ordering::Relaxed),
            ..Default::default()
        };
        if !samples.is_empty() {
            trace.min_c = samples.iter().copied().fold(f32::INFINITY, f32::min);
            trace.max_c = samples.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            trace.mean_c = samples.iter().sum::<f32>() / samples.len() as f32;
        }
        trace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact string read off monad02's thermal zone on 2026-07-28.
    #[test]
    fn millidegrees_become_degrees() {
        assert_eq!(parse_temp_c("81500\n"), Some(81.5));
        assert_eq!(parse_temp_c("70500\n"), Some(70.5));
    }

    #[test]
    fn a_missing_thermal_zone_is_absence_not_failure() {
        assert_eq!(read_temp_c_at(Path::new("/nonexistent/zone/temp")), None);
        assert_eq!(parse_temp_c("not a number"), None);
        assert_eq!(parse_throttle("garbage"), None);
    }

    /// The three spellings the value appears in: bare sysfs hex, the `0x` form,
    /// and `vcgencmd`'s `throttled=0x...`.
    #[test]
    fn the_throttle_word_parses_in_every_spelling() {
        for s in ["80000\n", "0x80000\n", "throttled=0x80000\n"] {
            assert_eq!(parse_throttle(s), Some(Throttle { raw: 0x8_0000 }), "{s:?}");
        }
        assert_eq!(parse_throttle("0\n"), Some(Throttle { raw: 0 }));
    }

    /// The exact word read off monad02 on 2026-07-28: the soft temperature
    /// limit HAS engaged since boot, but nothing is degraded at this instant.
    /// A check that only looked at the live bits would have called it healthy.
    #[test]
    fn monad02s_word_reads_as_history_not_present_tense() {
        let t = Throttle { raw: 0x8_0000 };
        assert!(!t.degraded_now());
        assert!(t.degraded_since_boot());
        assert!(t.bit(Throttle::SOFT_LIMIT_SEEN));
        assert!(!t.bit(Throttle::UNDERVOLT_SEEN));
        assert_eq!(t.describe(), "soft-limit-since-boot");
    }

    #[test]
    fn a_clean_word_describes_itself_as_none() {
        assert_eq!(Throttle { raw: 0 }.describe(), "none");
        assert!(!Throttle { raw: 0 }.degraded_since_boot());
    }

    fn trace(max_c: f32, degraded: bool, after: u32) -> ThermalTrace {
        ThermalTrace {
            samples: 10,
            min_c: max_c - 5.0,
            mean_c: max_c - 2.0,
            max_c,
            throttle_before: Some(Throttle { raw: 0 }),
            throttle_after: Some(Throttle { raw: after }),
            degraded_during: degraded,
        }
    }

    #[test]
    fn the_verdict_brackets_the_soft_limit() {
        assert_eq!(trace(60.0, false, 0).verdict(), Verdict::Pass);
        assert_eq!(trace(74.9, false, 0).verdict(), Verdict::Pass);
        // The last five degrees are a warning, not a pass.
        assert_eq!(trace(75.0, false, 0).verdict(), Verdict::Marginal);
        assert_eq!(trace(79.9, false, 0).verdict(), Verdict::Marginal);
        assert_eq!(trace(80.0, false, 0).verdict(), Verdict::Fail);
    }

    /// A run can fail on the firmware's word even if the temperature samples
    /// never happened to land on a hot one — sampling is periodic, throttling
    /// is not.
    #[test]
    fn a_degradation_during_the_run_fails_regardless_of_peak() {
        assert_eq!(trace(60.0, true, 0).verdict(), Verdict::Fail);
    }

    /// Under-voltage is a power-supply fault. It is never marginal, and it is
    /// not excused by a cool die.
    #[test]
    fn undervoltage_fails_even_when_cold() {
        assert_eq!(
            trace(50.0, false, Throttle::UNDERVOLT_SEEN).verdict(),
            Verdict::Fail
        );
    }

    /// Pre-existing thermal HISTORY must not condemn a run that was itself
    /// clean — otherwise every node that ever got hot fails forever.
    #[test]
    fn soft_limit_history_alone_does_not_fail_a_cool_run() {
        assert_eq!(
            trace(60.0, false, Throttle::SOFT_LIMIT_SEEN).verdict(),
            Verdict::Pass
        );
    }

    #[test]
    fn an_unmeasured_node_says_so_rather_than_passing() {
        assert_eq!(ThermalTrace::default().verdict(), Verdict::Unmeasured);
        assert!(!ThermalTrace::default().is_measured());
    }

    #[test]
    fn headroom_is_signed_against_the_soft_limit() {
        assert_eq!(trace(70.0, false, 0).headroom_c(), 10.0);
        assert_eq!(trace(82.0, false, 0).headroom_c(), -2.0);
    }
}
