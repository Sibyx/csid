//! What one console frame costs, at the tone counts real captures deliver.
//!
//! The console runs on the same Pi 5 that is capturing, niced below the RX
//! thread, so "how many milliseconds does a frame cost" is the number that
//! decides whether the operator can watch a 996-tone 2x2 capture at 20 fps
//! without the analysis becoming the reason records are dropped.
//!
//! The cases are the three geometries the AX210 actually produces — legacy 52,
//! HE20 242, HE80 996 — over the default 256-record window.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use csiq::{CsiRecord, Modulation, PhyLabel, Width};
use csiscope::analyze::Analyzer;
use csiscope::dsp;
use csiscope::frame::ViewSettings;
use csiscope::state::{Hub, Sample};

use rustfft::num_complex::Complex32;

/// A record with a plausible channel shape: a roll-off towards the band edges,
/// a DC notch, and per-chain offsets — so the validation and bundle paths see
/// the branches they would see on real data rather than a flat constant.
fn record(i: u64, ntone: u16, nchain: u8) -> CsiRecord {
    let n = ntone as usize;
    let nc = nchain as usize;
    let mut iq = Vec::with_capacity(2 * n * nc);
    for c in 0..nc {
        for t in 0..n {
            let x = t as f32 / n as f32;
            // Edge roll-off, a notch at DC, a slow drift over records.
            let mut a = 800.0 * (1.0 - 0.8 * (2.0 * x - 1.0).abs().powi(2));
            if (t as i32 - n as i32 / 2).abs() < 2 {
                a *= 0.02;
            }
            a *= 1.0 + 0.1 * ((i as f32 * 0.13 + c as f32).sin());
            let ph = 0.7 * t as f32 + i as f32 * 0.05 + c as f32;
            iq.push((a * ph.sin()) as i16); // imaginary first
            iq.push((a * ph.cos()) as i16);
        }
    }
    CsiRecord {
        ftm: (i * 533_333) as u32,
        us: (i * 1666) as u32,
        unix_ts_ns: 1_700_000_000_000_000_000 + i * 1_666_000,
        rnf: 0x0442,
        phy: Some(PhyLabel {
            modulation: Modulation::He,
            mcs: 2,
            nss: 1,
        }),
        seq: 0,
        nrx: nchain,
        ntx: 1,
        ntone,
        rssi: vec![-43; nc],
        // A handful of talkers, so the talker table does real work.
        src_mac: [0xde, 0xad, 0xbe, 0xef, 0, (i % 5) as u8],
        channel: 36,
        width: Width::W80,
        iq,
    }
}

fn sample(i: u64, ntone: u16, nchain: u8) -> Sample {
    Sample {
        session_uid: 1,
        seq: i as u32,
        // ~608 Hz, the measured live rate.
        ftm_ticks: i * 526_315,
        recv_ns: 1_700_000_000_000_000_000 + i * 1_644_000,
        rec: Arc::new(record(i, ntone, nchain)),
    }
}

fn hub_with(ntone: u16, nchain: u8, records: usize) -> Arc<Hub> {
    let hub = Hub::new("bench".into(), 8192, usize::MAX);
    for i in 0..records as u64 {
        hub.push(sample(i, ntone, nchain));
    }
    hub
}

/// The whole frame: every panel the console draws, from one window.
fn bench_frame(c: &mut Criterion) {
    let mut g = c.benchmark_group("frame");
    // A frame is one analysis of `window` records; report throughput in
    // records so the tone counts stay comparable.
    for &ntone in &[52u16, 242, 996] {
        let hub = hub_with(ntone, 2, 2048);
        let mut settings = ViewSettings::default();
        settings.sanitise();
        g.throughput(Throughput::Elements(settings.window as u64));
        g.bench_with_input(BenchmarkId::from_parameter(ntone), &ntone, |b, _| {
            let mut a = Analyzer::at_live_edge(&hub);
            b.iter(|| std::hint::black_box(a.frame(&hub, &settings)));
        });
    }
    g.finish();
}

/// The percentile bundle: 128 columns of `ntone` values, three quantiles each.
fn bench_bundle(c: &mut Criterion) {
    let mut g = c.benchmark_group("bundle");
    for &ntone in &[52usize, 242, 996] {
        let columns: Vec<Vec<f32>> = (0..128)
            .map(|i| {
                (0..ntone)
                    .map(|t| 30.0 + 8.0 * ((t as f32 * 0.1 + i as f32 * 0.7).sin()))
                    .collect()
            })
            .collect();
        g.throughput(Throughput::Elements((ntone * 128) as u64));
        g.bench_with_input(BenchmarkId::from_parameter(ntone), &ntone, |b, _| {
            b.iter(|| std::hint::black_box(dsp::bundle(&columns)));
        });
    }
    g.finish();
}

/// Amplitude in dB — the single most-called kernel in the crate.
fn bench_amp_db(c: &mut Criterion) {
    let mut g = c.benchmark_group("amp_db");
    for &ntone in &[52usize, 242, 996] {
        let h: Vec<Complex32> = (0..ntone)
            .map(|t| {
                let ph = 0.7 * t as f32;
                Complex32::new(700.0 * ph.cos(), 700.0 * ph.sin())
            })
            .collect();
        g.throughput(Throughput::Elements(ntone as u64));
        g.bench_with_input(BenchmarkId::from_parameter(ntone), &ntone, |b, _| {
            b.iter(|| std::hint::black_box(dsp::amp_db(&h)));
        });
    }
    g.finish();
}

/// The impulse response: a zero-padded inverse FFT per frame.
fn bench_cir(c: &mut Criterion) {
    let h: Vec<Complex32> = (0..242)
        .map(|t| {
            let ph = 0.7 * t as f32;
            Complex32::new(700.0 * ph.cos(), 700.0 * ph.sin())
        })
        .collect();
    c.bench_function("cir/2048", |b| {
        b.iter(|| std::hint::black_box(dsp::cir(&h, 78_125.0, 2048, 128)));
    });
}

/// One Doppler column: resample, window, forward FFT.
fn bench_doppler(c: &mut Criterion) {
    let recs: Vec<Arc<CsiRecord>> = (0..256).map(|i| Arc::new(record(i, 242, 2))).collect();
    let ticks: Vec<u64> = (0..256u64).map(|i| i * 526_315).collect();
    c.bench_function("doppler/256", |b| {
        b.iter(|| {
            let s = dsp::doppler_series(&recs, &ticks, 0, Some(1));
            // A pinned axis, as the console always supplies one.
            std::hint::black_box(dsp::doppler(&s, 256, dsp::wavelength_m(5180.0), 600.0))
        });
    });
}

criterion_group!(
    benches,
    bench_frame,
    bench_bundle,
    bench_amp_db,
    bench_cir,
    bench_doppler
);
criterion_main!(benches);
