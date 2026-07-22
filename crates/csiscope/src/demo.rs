//! A synthetic CSI source, for demonstrations and UI work without hardware.
//!
//! This exists for two honest reasons: a conference talk should not depend on a
//! Raspberry Pi surviving the venue's Wi-Fi, and a console panel that is only
//! exercised by real captures is a panel nobody tests. It is **not** a
//! simulator of anything scientific — it produces a channel with the *shape*
//! real CSI has, so that every view has something structured to draw.
//!
//! What it models, and why each part is there:
//!
//! | Feature | Why the console needs it |
//! |---|---|
//! | Several static multipath taps | the impulse response has taps to resolve |
//! | One tap with time-varying delay | the Doppler spectrogram has a line to show |
//! | Per-packet CFO and a sampling-time ramp | raw phase is wrapped nonsense and sanitisation visibly fixes it |
//! | Band-edge taper and a DC notch | the extraction checks have something to pass |
//! | Per-chain gain and phase offsets | the chain panel shows genuinely different chains |
//! | Exponentially distributed arrivals | the inter-arrival histogram has a real tail |
//!
//! Everything is stamped with a locally-administered source MAC and the hub is
//! labelled `demo:` so no screenshot can be mistaken for a measurement.

use std::f64::consts::PI;
use std::sync::Arc;
use std::time::{Duration, Instant};

use csiq::{CsiRecord, Modulation, PhyLabel, Width};

use crate::state::{now_ns, Hub, Sample};

/// Tone count: HE20's 242-tone RU, the geometry the reference node actually
/// produces most of the time.
const NTONE: u16 = 242;
const NRX: u8 = 2;
const NTX: u8 = 2;
/// Mean packet rate. Deliberately below the measured 608 Hz ceiling so the
/// demo does not look like a benchmark.
const RATE_HZ: f64 = 300.0;
/// I/Q full scale after AGC normalisation.
const SCALE: f64 = 1800.0;

/// Static multipath: (relative amplitude, delay in ns).
///
/// The direct path stays stronger than everything else combined. A channel
/// where it does not is perfectly realistic — and produces deep fades where
/// the phase swings through a full turn — but it would make the phase panel a
/// demonstration of fading rather than of sanitisation.
const TAPS: &[(f64, f64)] = &[(1.0, 0.0), (0.35, 28.0), (0.18, 64.0), (0.10, 121.0)];

/// The moving reflector: amplitude, mean delay (ns), delay swing (ns), and how
/// fast it sweeps (Hz).
///
/// The swing and rate set where the Doppler energy lands:
/// `f_D,max = f_c · 2π · rate · swing`. At 5.18 GHz these values give about
/// ±23 Hz — a radial speed near 0.7 m/s, a person walking, and comfortably
/// inside the ±fs/2 the packet rate can represent. Larger values alias, which
/// is exactly the failure a real under-sampled capture shows.
///
/// Because the delay is sinusoidal the instantaneous shift sweeps continuously
/// between those limits, so the spectrogram shows a rocking band rather than a
/// single line — which is what a person actually produces.
const MOVER: (f64, f64, f64, f64) = (0.30, 45.0, 2.0, 0.35);

/// Carrier frequency the demo pretends to be on (channel 36).
const CARRIER_HZ: f64 = 5.18e9;

/// Per-chain excess path delay, in nanoseconds, for the **static** paths and
/// for the **moving** one.
///
/// This is the array geometry, and it is the single most important thing the
/// generator has to get right. Antennas half a wavelength apart see the same
/// reflector at slightly different path lengths — about 0.1 ns at 5 GHz — and
/// crucially the offset differs *per path*, because paths arrive from
/// different angles.
///
/// If every chain saw an identical channel scaled by a constant, the
/// conjugate product `H_a·conj(H_b)` that the Doppler view depends on would
/// collapse to `const · |H|²` and cancel the very dynamics it exists to
/// recover. That is a real property of the method, not an artifact — and a
/// generator that ignored it would silently produce a console panel that shows
/// noise and looks broken.
const CHAIN_TAU_NS: [f64; 4] = [0.0, 0.10, 0.05, 0.15];
const MOVER_TAU_NS: [f64; 4] = [0.0, -0.08, 0.12, 0.03];

/// Carrier frequency offset between transmitter and receiver, in Hz.
///
/// Modelled as a genuine frequency offset — a phase that *accumulates* — not a
/// fresh random number per packet. That distinction matters: real CFO is
/// strongly correlated between consecutive packets, which is why the conjugate
/// product between two chains cancels it and why a single-chain series cannot.
const CFO_HZ: f64 = 2_500.0;

/// Start the generator on its own thread. Returns immediately.
pub fn spawn(hub: Arc<Hub>) {
    std::thread::Builder::new()
        .name("csiscope-demo".into())
        .spawn(move || generate(hub))
        .ok();
}

fn generate(hub: Arc<Hub>) {
    tracing::warn!("DEMO MODE: the stream is synthetic. Nothing here is a measurement.");

    let mut rng = Xorshift::new(0x5EED_1234_ABCD_0001);
    let start = Instant::now();
    let mut seq: u32 = 0;
    let session_uid = now_ns();
    let spacing = 78_125.0f64; // HE grid

    // Three source MACs in the locally-administered range, so the talker table
    // is populated and obviously synthetic.
    let macs: [[u8; 6]; 3] = [
        [0x02, 0xde, 0x30, 0x01, 0x00, 0x01],
        [0x02, 0xde, 0x30, 0x01, 0x00, 0x02],
        [0x02, 0xde, 0x30, 0x01, 0x00, 0x03],
    ];

    loop {
        // Erlang-3 inter-arrivals (CV ≈ 0.58): ambient Wi-Fi is neither a
        // metronome nor pure Poisson — frames come in aggregated bursts on top
        // of periodic beacons. A metronome would make the jitter histogram a
        // lie; pure Poisson would make the Doppler axis untrustworthy for
        // reasons the hardware does not actually have.
        let gap =
            -(rng.unit().max(1e-6).ln() + rng.unit().max(1e-6).ln() + rng.unit().max(1e-6).ln())
                / (3.0 * RATE_HZ);
        let gap = gap.clamp(0.0002, 0.05);
        std::thread::sleep(Duration::from_secs_f64(gap));

        let t = start.elapsed().as_secs_f64();
        let ftm_ticks = (t * csiq::FTM_HZ as f64) as u64;

        // Per-packet hardware offsets. These are what makes raw phase useless
        // and the sanitised phase informative. CFO accumulates with time
        // (wrapped into one turn); the sampling-time offset re-randomises per
        // packet, as packet-detection jitter does.
        let cfo = (2.0 * PI * CFO_HZ * t) % (2.0 * PI);
        let sto_ns = 15.0 + rng.unit() * 40.0;

        // Where the mover is right now, and the same for the carrier phase it
        // imposes — the carrier term is what actually carries the Doppler,
        // since a 2 ns delay swing is ~10 carrier wavelengths.
        let (ma, mtau, mswing, mrate) = MOVER;
        let mover_tau_ns = mtau + mswing * (2.0 * PI * mrate * t).sin();

        let nchain = (NRX * NTX) as usize;
        let mut iq: Vec<i16> = Vec::with_capacity(2 * NTONE as usize * nchain);

        for tone in 0..NTONE as usize {
            // Frequency offset from band centre, and the absolute frequency the
            // carrier phase is accumulated at.
            let f = (tone as f64 - NTONE as f64 / 2.0) * spacing;
            let f_abs = CARRIER_HZ + f;

            // Analogue front end: taper at the band edges, notch at DC.
            let edge = (tone as f64 / NTONE as f64 - 0.5).abs() * 2.0; // 0 centre, 1 edge
            let mut gain = 1.0 - 0.55 * edge.powi(4);
            let from_centre = (tone as i32 - NTONE as i32 / 2).abs();
            if from_centre < 2 {
                gain *= 0.02;
            }

            // Common phase error: CFO (constant across the band) plus STO
            // (linear in frequency).
            let err = cfo - 2.0 * PI * f * sto_ns * 1e-9;

            for c in 0..nchain {
                // Each chain sees its own geometry: the static paths and the
                // mover arrive with different excess delays.
                let dstat = CHAIN_TAU_NS[c.min(3)];
                let dmove = MOVER_TAU_NS[c.min(3)];

                let mut re = 0.0;
                let mut im = 0.0;
                for &(a, tau_ns) in TAPS {
                    let ph = -2.0 * PI * f_abs * (tau_ns + dstat) * 1e-9;
                    let (s, co) = ph.sin_cos();
                    re += a * co;
                    im += a * s;
                }
                let ph = -2.0 * PI * f_abs * (mover_tau_ns + dmove) * 1e-9;
                let (s, co) = ph.sin_cos();
                re += ma * co;
                im += ma * s;

                // Chains differ in gain, as real ones do.
                let cg = gain * [1.0, 0.72, 0.55, 0.41][c.min(3)];
                let (es, ec) = err.sin_cos();
                let rr = (re * ec - im * es) * cg;
                let ii = (re * es + im * ec) * cg;

                // AGC keeps the level roughly constant; noise sits under it.
                let n = 0.02;
                iq.push(((rr * SCALE / 2.0) + rng.centred() * n * SCALE) as i16);
                iq.push(((ii * SCALE / 2.0) + rng.centred() * n * SCALE) as i16);
            }
        }

        let mac = macs[(rng.next() % 3) as usize];
        let rssi_base = -48 - (rng.next() % 7) as i16;

        let rec = CsiRecord {
            ftm: ftm_ticks as u32,
            us: (t * 1e6) as u32,
            // Host stamp with realistic delivery jitter: a tight body and a
            // rare multi-millisecond scheduler stall, matching the measured
            // p50 19 µs / p99.9 5.4 ms profile.
            unix_ts_ns: now_ns()
                + if rng.next() % 2000 == 0 {
                    (rng.unit() * 5e6) as u64
                } else {
                    (rng.unit() * 60e3) as u64
                },
            rnf: 0x0442,
            phy: Some(PhyLabel {
                modulation: Modulation::He,
                mcs: (rng.next() % 8) as u8,
                nss: 2,
            }),
            seq: (seq & 0xff) as u8,
            nrx: NRX,
            ntx: NTX,
            ntone: NTONE,
            rssi: vec![rssi_base, rssi_base - 3],
            src_mac: mac,
            channel: 36,
            width: Width::W80,
            iq,
        };

        hub.push(Sample {
            session_uid,
            seq,
            ftm_ticks,
            recv_ns: now_ns(),
            rec: Arc::new(rec),
        });
        hub.counters
            .received
            .fetch_add(0, std::sync::atomic::Ordering::Relaxed);
        seq = seq.wrapping_add(1);
    }
}

/// A tiny deterministic PRNG. The demo must be reproducible frame to frame for
/// screenshots, and pulling in a random-number crate for a demo generator would
/// be a dependency the daemon's users pay for.
struct Xorshift(u64);

impl Xorshift {
    fn new(seed: u64) -> Self {
        Xorshift(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Roughly standard-normal, by summing three uniforms (Bates).
    fn centred(&mut self) -> f64 {
        (self.unit() + self.unit() + self.unit() - 1.5) * 1.15
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp;

    /// The demo has to exercise the views, which means its output has to have
    /// the properties the views look for. This is the check that it does.
    #[test]
    fn synthetic_records_pass_the_extraction_checks() {
        let hub = Hub::new("demo:test".into(), 64, usize::MAX);
        spawn(hub.clone());

        // Wait for a handful of records without a hard sleep on a fixed rate.
        let deadline = Instant::now() + Duration::from_secs(5);
        while hub.depth() < 8 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let tail = hub.tail(8);
        assert!(tail.len() >= 8, "demo produced only {} records", tail.len());

        let rec = &tail.last().unwrap().rec;
        let g = dsp::Geometry::of(rec);
        assert_eq!(g.ntone, NTONE as usize);
        assert_eq!(g.nchain(), 4);

        let v = dsp::validate(rec);
        assert!(v.dimensions_ok);
        assert!(v.dc_notch_db < -10.0, "dc notch {}", v.dc_notch_db);
        assert!(v.edge_rolloff_db < -1.0, "roll-off {}", v.edge_rolloff_db);
        assert!(
            v.chain_spread_db > 1.0,
            "chain spread {}",
            v.chain_spread_db
        );

        // A resolvable impulse response: the planted taps must show up.
        let h = dsp::chain(rec, 0);
        let cir = dsp::cir(&h, 78_125.0, 512, 128);
        assert!(cir.rms_delay_ns > 5.0, "rms delay {}", cir.rms_delay_ns);

        // Raw phase must be wrapped, and detrending must flatten it.
        let raw = dsp::phase(&h);
        let (residual, fit) = dsp::detrend(&dsp::unwrap(&raw), 78_125.0);
        assert!(fit.tau_ns.abs() > 1.0, "no delay slope to remove");
        let spread = residual.iter().cloned().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(spread < 4.0, "residual phase spread {spread}");
    }
}
