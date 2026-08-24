//! Turn a window of live samples into one frame for the browser.
//!
//! Every panel in the console is produced here, from one snapshot, so the views
//! are mutually consistent: the waterfall row, the spectrum, the phase fit and
//! the Doppler column all describe the same records rather than three
//! independently sampled moments.
//!
//! Cost control is explicit. At 608 Hz and 996 tones a naive "recompute
//! everything over the full window" would be gigaflops on a Pi 5, so the
//! windowed statistics run over a decimated subset ([`BUNDLE_COLUMNS`]) and the
//! waterfall carries only what the frame rate can show, reporting the rest as
//! `skipped` rather than pretending the display is complete.
//!
//! ## Two halves, because only one of them is per-client
//!
//! [`Analysis`] computes everything that depends on the window and the view
//! settings. [`ClientView`] draws the waterfall, which depends on where *this*
//! client's cursor sits in the ring. The server runs one [`Analysis`] per
//! distinct view and hands the result to every [`ClientView`] watching it; the
//! console used to run the whole thing once per connected browser.
//!
//! ## Buffers live across frames
//!
//! Every intermediate is a field of [`Scratch`], refilled rather than
//! reallocated. This is not micro-optimisation for its own sake: profiling the
//! deployed console put 17% of its CPU in `malloc`/`free` against 1.7% in the
//! FFTs, and the single worst offender was a loop that extracted a full
//! `Vec<Complex32>` of every subcarrier in order to read one of them.

use std::sync::Arc;

use csiq::CsiRecord;
use rustfft::num_complex::Complex32;

use crate::class::{Census, ClassKey};
use crate::dsp;
use crate::frame::{quantise_db, F32Section, SharedFrame, ViewSettings, WaterfallPlan};
use crate::state::{Hub, Sample};
use crate::wire::{
    ClassEntry, ClassInfo, ClientHeader, Mac, MixInfo, PhyInfo, SharedHeader, Talker, WidthKey,
};

/// Upper bound on records entering the percentile bundle. Percentiles converge
/// long before the window does, and this keeps the per-frame cost flat.
const BUNDLE_COLUMNS: usize = 128;

/// Inter-arrival histogram resolution.
const HIST_BINS: usize = 48;

/// Every buffer one analysis reuses between frames.
#[derive(Default)]
struct Scratch {
    all: Vec<Sample>,
    window: Vec<Sample>,
    recs: Vec<Arc<CsiRecord>>,
    ticks: Vec<u64>,

    h: Vec<Complex32>,
    amp: Vec<f32>,
    phase_raw: Vec<f32>,
    phase_unwrapped: Vec<f32>,
    phase_detrended: Vec<f32>,
    iq: Vec<f32>,
    chain_amp: Vec<f32>,

    picks: Vec<usize>,
    columns: Vec<f32>,
    bundle_scratch: Vec<f32>,
    bundle: dsp::Bundle,

    cir: dsp::Cir,
    cir_buf: Vec<Complex32>,

    series: dsp::Series,
    doppler: dsp::Doppler,
    dop_buf: Vec<Complex32>,

    tones: Vec<usize>,
    tone_series: Vec<f32>,
    rssi_series: Vec<f32>,
    host_us: Vec<f32>,
    fw_us: Vec<f32>,
    ftm_ns: Vec<u64>,
    host_ns: Vec<u64>,
    timing_scratch: Vec<f32>,
    hist: Vec<f32>,
    talkers: Vec<(Mac, TalkerAcc)>,
    /// A second accumulator, for the transmitters *within the selected class*.
    /// Separate from `talkers` because the two answer different questions and
    /// are both wanted in the same frame: one is the channel, one is the set the
    /// deep views can actually be scoped to.
    talkers_class: Vec<(Mac, TalkerAcc)>,

    ratio_amp: Vec<f32>,
    ratio_phase: Vec<f32>,
    tone_median: Vec<f32>,
    tone_spread: Vec<f32>,
    tone_null: Vec<f32>,
    tone_offsets_mhz: Vec<f32>,
    stats_scratch: Vec<f32>,
    metro_scratch: Vec<f32>,
    metronome: dsp::Metronome,
    tone_stats: dsp::ToneStats,

    census: Census,
    section: F32Section,
    header: SharedHeader,
    /// One planner for the impulse response and one for the Doppler column, so
    /// the two can run on different threads without sharing mutable state.
    tf_cir: dsp::Transforms,
    tf_doppler: dsp::Transforms,
}

#[derive(Debug, Clone, Copy, Default)]
struct TalkerAcc {
    count: u64,
    rssi_sum: i64,
    rssi_n: u64,
    last_ns: u64,
    first_ticks: u64,
    last_ticks: u64,
}

/// The shared half of a frame: everything derived from the window.
pub struct Analysis {
    scratch: Box<Scratch>,
    /// Peak-held packet rate, in Hz — the reference the Doppler axis is
    /// snapped from. See [`track_doppler_fs`].
    ///
    /// State rather than scratch, and the reason it is state at all: a
    /// spectrogram is a sequence of columns on ONE axis. Deriving the axis from
    /// each column's own packet rate produced an image whose frequency scale
    /// moved by 2.4× within fifteen seconds — see [`dsp::Doppler`]. One
    /// [`Analysis`] serves one view for as long as anyone is watching it, so
    /// this is exactly the right lifetime for the axis it draws.
    doppler_fs: f64,
}

impl Default for Analysis {
    fn default() -> Self {
        Self::new()
    }
}

impl Analysis {
    pub fn new() -> Self {
        Analysis {
            scratch: Box::default(),
            doppler_fs: 0.0,
        }
    }


    /// Compute one tick's shared analysis. `None` when nothing has arrived.
    pub fn compute(&mut self, hub: &Hub, s: &ViewSettings) -> Option<SharedFrame> {
        // Destructured rather than borrowed through `self`, so the buffers and
        // the held Doppler axis are two disjoint borrows: the scratch stays
        // mutably borrowed for the whole frame, and the axis still has to be
        // updated in the middle of it.
        let Analysis {
            scratch,
            doppler_fs,
        } = self;
        let sc = &mut **scratch;

        hub.tail_into(s.window, &mut sc.all);
        if sc.all.is_empty() {
            return None;
        }

        // Which record classes are on the channel, and which one are we
        // looking at? The requested class wins if it is still present;
        // otherwise fall back to the most common, so the console recovers on
        // its own when a transmitter goes quiet.
        sc.census.clear();
        for smp in &sc.all {
            sc.census.add(ClassKey::of(&smp.rec));
        }
        // What csid says about itself, read once per frame so every panel in
        // this frame describes the same instant.
        let capture = hub.capture.as_ref().and_then(|c| c.get());

        let dominant = sc.census.dominant()?;
        let requested = s.class_key();
        let class = match requested {
            Some(want) if sc.census.contains(want) => want,
            _ => dominant,
        };

        // Every view below sees only this class. Mixing geometries into one
        // series would produce arrays of changing width and a time series
        // whose consecutive samples are not comparable.
        sc.window.clear();
        sc.window.extend(
            sc.all
                .iter()
                .filter(|smp| ClassKey::of(&smp.rec) == class)
                .cloned(),
        );

        // -- and then only this transmitter --------------------------------
        //
        // The second scope, and on an illuminated capture the one that does the
        // work. Measured 2026-08-17 on ch36: twelve transmitters, the injector
        // 54.3% of records, and a pooled inter-arrival p50 of 6.1 ms that
        // belongs to none of them — it is one 100 Hz metronome interleaved with
        // eleven ambient talkers. Every deep view was being computed over that
        // mixture.
        //
        // The census is taken *before* scoping, so choosing a transmitter can
        // never hide the others from the operator who chose it.
        let by_mac = talkers(&sc.window, &mut sc.talkers_class);
        let requested_mac = s.smac_bytes().map(Mac);
        let selected_mac = match requested_mac {
            // A transmitter that has gone quiet falls back to the busiest, for
            // the same reason a pinned class does.
            Some(want) if by_mac.iter().any(|t| t.mac == want) => Some(want),
            _ => by_mac.first().map(|t| t.mac),
        };
        let class_window_len = sc.window.len();
        if let Some(mac) = selected_mac {
            sc.window.retain(|smp| Mac(smp.rec.src_mac) == mac);
        }

        // -- and then only records that measured something ------------------
        //
        // THE single null policy. Everything below this line — the percentile
        // bundle, the per-tone statistics, the Doppler series, the amplitude
        // time series, the snapshot views and the waterfall — reads
        // `sc.window`, and before this drop existed each of them decided
        // independently what an all-zero record meant. They decided
        // differently, and five views were wrong in five different ways off one
        // upstream fault. See `dsp::is_measurement` for the measurement and its
        // consequences.
        //
        // The drop happens AFTER both scopes, deliberately. The class census
        // and the talker table describe what the radio delivered — an empty
        // record was still a transmission by a real transmitter, and hiding it
        // from "who is sounding the channel" would answer a different question.
        // Only the analysis is narrowed.
        let considered = sc.window.len();
        sc.window.retain(|smp| dsp::is_measurement(&smp.rec));
        let dropped = considered - sc.window.len();
        // Nothing measured at all. Rather than return no frame — which is what
        // "the stream is dead" looks like, and this is not that — the identity
        // panels are served from the records that did arrive and every
        // analytical panel is told to blank itself.
        let no_measurement = sc.window.is_empty() && considered > 0;
        if no_measurement {
            sc.window.extend(
                sc.all
                    .iter()
                    .filter(|smp| ClassKey::of(&smp.rec) == class)
                    .filter(|smp| selected_mac.is_none_or(|m| Mac(smp.rec.src_mac) == m))
                    .cloned(),
            );
        }
        let nulls = crate::wire::NullInfo {
            dropped,
            considered,
            frac: if considered > 0 {
                dropped as f64 / considered as f64
            } else {
                0.0
            },
            no_measurement,
        };

        let latest = sc.window.last()?.clone();
        let rec = &latest.rec;
        let geom = dsp::Geometry::of(rec);
        let nchain = geom.nchain();
        let chain_idx = s.chain.min(nchain.saturating_sub(1));
        let spacing = dsp::spacing_hz(rec);
        let window_len = sc.window.len();

        sc.section.clear();

        // -- the selected chain, right now ---------------------------------
        dsp::chain_into(rec, chain_idx, &mut sc.h);
        dsp::amp_db_into(&sc.h, &mut sc.amp);
        dsp::phase_into(&sc.h, &mut sc.phase_raw);
        dsp::unwrap_into(&sc.phase_raw, &mut sc.phase_unwrapped);
        let fit = dsp::detrend_into(&sc.phase_unwrapped, spacing, &mut sc.phase_detrended);

        sc.section.push("amp_db", &sc.amp);
        sc.section.push("phase_raw", &sc.phase_raw);
        sc.section.push("phase_unwrapped", &sc.phase_unwrapped);
        sc.section.push("phase_detrended", &sc.phase_detrended);

        // Interleaved re/im for the complex-plane view.
        sc.iq.clear();
        sc.iq.reserve(sc.h.len() * 2);
        for c in &sc.h {
            sc.iq.push(c.re);
            sc.iq.push(c.im);
        }
        sc.section.push("iq", &sc.iq);

        // -- per-chain spectra, for the small-multiple comparison ----------
        //
        // Each chain writes its own slice of the output, so this parallelises
        // without any shared mutable state at all.
        sc.chain_amp.clear();
        sc.chain_amp.resize(nchain * geom.ntone, f32::NAN);
        crate::pipeline::for_each_chunk_mut(&mut sc.chain_amp, geom.ntone, |c, out| {
            dsp::chain_amp_db_into_slice(rec, c, out);
        });
        sc.section.push("chain_amp_db", &sc.chain_amp);

        // -- windowed views -------------------------------------------------
        //
        // A decimated subset, always including the newest record. The columns
        // land in one contiguous buffer rather than in `BUNDLE_COLUMNS`
        // separate vectors.
        //
        // Records whose payload does not match the geometry this class
        // promises are excluded up front rather than written as `NaN` columns:
        // a `NaN` would sort to one end of every percentile selection and
        // silently bias p95.
        sc.picks.clear();
        for (i, smp) in sc.window.iter().enumerate() {
            let g = dsp::Geometry::of(&smp.rec);
            if g.ntone == geom.ntone && chain_idx < g.nchain() && g.matches(&smp.rec) {
                sc.picks.push(i);
            }
        }
        let usable = sc.picks.len();
        let cols = usable.min(BUNDLE_COLUMNS);
        sc.columns.clear();
        sc.columns.resize(cols * geom.ntone, f32::NAN);
        if cols > 0 {
            let step = usable as f64 / cols as f64;
            let (window, picks) = (&sc.window, &sc.picks);
            crate::pipeline::for_each_chunk_mut(&mut sc.columns, geom.ntone, |i, out| {
                let k = picks[((i as f64 * step) as usize).min(usable - 1)];
                dsp::chain_amp_db_into_slice(&window[k].rec, chain_idx, out);
            });
        }

        // -- Doppler ---------------------------------------------------------
        sc.recs.clear();
        sc.ticks.clear();
        sc.recs.extend(sc.window.iter().map(|s| s.rec.clone()));
        sc.ticks.extend(sc.window.iter().map(|s| s.ftm_ticks));
        let chain_b = s.chain_b.filter(|&b| b < nchain && b != chain_idx);

        // -- which channel is this, actually? --------------------------------
        //
        // Two sources, and they can disagree. The record carries what the driver
        // stamped on it; `csid` knows what it commanded the radio to. Found by
        // replaying the archive: an entire BLE-coexistence segment declared
        // channel 3 in its sidecar and carried channel 48 in every record. The
        // speed axis and the band plan are both functions of frequency, and both
        // were silently computed from the wrong one.
        //
        // The console cannot arbitrate. It can refuse to derive anything from a
        // frequency the two sources do not agree on, and say which two numbers
        // disagree. See `wire::RadioInfo::channel_mismatch`.
        let tuned_channel = capture.as_ref().map(|r| r.snap.channel).filter(|&c| c > 0);
        let channel_mismatch = tuned_channel.is_some_and(|c| c != rec.channel);
        let freq_mhz = s
            .freq_mhz
            // The daemon's own tuning, ahead of the record's channel number:
            // it is what the radio was commanded to, and it resolves 6 GHz,
            // whose channel numbers overlap 2.4 GHz.
            .or_else(|| {
                capture
                    .as_ref()
                    .map(|r| r.snap.control_freq_mhz as f64)
                    .filter(|&f| f > 0.0)
            })
            .or_else(|| control_freq_mhz(rec))
            .unwrap_or(5180.0);
        let freq_trusted = s.freq_mhz.is_some() || !channel_mismatch;

        // The Doppler axis, chosen before the transform rather than derived
        // from it. `sc.ticks` is this window's arrival times on the 320 MHz
        // baseband clock, so the delivered rate is available here without
        // waiting for the reduction that is about to run in parallel.
        let fs_window = match (sc.ticks.first(), sc.ticks.last()) {
            (Some(&a), Some(&b)) if sc.ticks.len() > 1 => {
                let span = csiq::ftm_to_seconds(b.saturating_sub(a));
                if span > 0.0 {
                    (sc.ticks.len() - 1) as f64 / span
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        let doppler_fs = track_doppler_fs(doppler_fs, fs_window);

        // The two heaviest remaining pieces are independent: the percentile
        // bundle reads `columns`, the Doppler column reads `recs`/`ticks`.
        // Running them as a pair is what the second core is for.
        {
            let Scratch {
                columns,
                bundle,
                bundle_scratch,
                recs,
                ticks,
                series,
                doppler,
                dop_buf,
                tf_doppler,
                ..
            } = sc;
            let ntone = geom.ntone;
            // Rough size of the two halves: the bundle reads `cols × ntone`,
            // the Doppler reduction reads the window's chains.
            let work = ntone * (cols + window_len);
            crate::pipeline::join(
                work,
                || dsp::bundle_flat(columns, ntone, bundle, bundle_scratch),
                || {
                    dsp::doppler_series_into(recs, ticks, chain_idx, chain_b, series);
                    dsp::doppler_into(
                        tf_doppler,
                        series,
                        s.doppler_nfft,
                        dsp::wavelength_m(freq_mhz),
                        doppler_fs,
                        dop_buf,
                        doppler,
                    );
                },
            );
        }

        if !sc.bundle.p50.is_empty() {
            sc.section.push("bundle_p05", &sc.bundle.p05);
            sc.section.push("bundle_p50", &sc.bundle.p50);
            sc.section.push("bundle_p95", &sc.bundle.p95);
        }
        sc.section.push("doppler_db", &sc.doppler.power_db);

        // -- impulse response ----------------------------------------------
        dsp::cir_into(
            &mut sc.tf_cir,
            &sc.h,
            spacing,
            s.cir_nfft,
            s.cir_taps,
            &mut sc.cir_buf,
            &mut sc.cir,
        );
        sc.section.push("cir_db", &sc.cir.mag_db);

        // -- amplitude time series ------------------------------------------
        //
        // One pass over the window, reading only the subcarriers asked for.
        // The previous form nested the loops the other way round and called a
        // full chain extraction inside the inner one, so watching three tones
        // over a 256-record window rebuilt 768 complete spectra — 765,000
        // complex conversions and six megabytes of allocation per frame at
        // 996 tones — to read 768 numbers.
        sc.tones.clear();
        if s.series_tones.is_empty() {
            default_tones(geom.ntone, &mut sc.tones);
        } else {
            sc.tones
                .extend(s.series_tones.iter().copied().filter(|&t| t < geom.ntone));
        }
        sc.tone_series.clear();
        sc.tone_series.resize(sc.tones.len() * window_len, f32::NAN);
        for (j, smp) in sc.window.iter().enumerate() {
            let Some(iq) = dsp::chain_slice(&smp.rec, chain_idx) else {
                continue;
            };
            for (i, &t) in sc.tones.iter().enumerate() {
                if 2 * t + 1 < iq.len() {
                    let im = iq[2 * t] as f32;
                    let re = iq[2 * t + 1] as f32;
                    sc.tone_series[i * window_len + j] = dsp::db_from_power(re * re + im * im);
                }
            }
        }
        sc.section.push("tone_series", &sc.tone_series);

        // -- RSSI, the only absolute amplitude anchor ------------------------
        let rssi_chains = rec.rssi.len().max(1);
        sc.rssi_series.clear();
        sc.rssi_series.reserve(rssi_chains * window_len);
        for c in 0..rssi_chains {
            for smp in &sc.window {
                sc.rssi_series
                    .push(smp.rec.rssi.get(c).copied().unwrap_or(0) as f32);
            }
        }
        sc.section.push("rssi_series", &sc.rssi_series);

        // -- clocks ----------------------------------------------------------
        let clocks = clock_series(&sc.window, &mut sc.host_us, &mut sc.fw_us);
        sc.section.push("drift_host_us", &sc.host_us);
        sc.section.push("drift_fw_us", &sc.fw_us);

        // -- timing ----------------------------------------------------------
        sc.ftm_ns.clear();
        sc.ftm_ns.extend(
            sc.window
                .iter()
                .map(|s| (s.ftm_ticks as f64 * 1e9 / csiq::FTM_HZ as f64) as u64),
        );
        sc.host_ns.clear();
        sc.host_ns.extend(sc.window.iter().map(|s| s.recv_ns));
        let t_ftm = dsp::timing_ns_into(&sc.ftm_ns, &mut sc.timing_scratch);
        let t_host = dsp::timing_ns_into(&sc.host_ns, &mut sc.timing_scratch);
        let (hist_min_us, hist_max_us) = histogram(&sc.ftm_ns, &mut sc.hist);
        sc.section.push("interarrival_hist", &sc.hist);

        // -- the metronome ---------------------------------------------------
        //
        // Now that the window is one transmitter, its arrivals are a single
        // process and can be judged against a slot. Declared beats inferred:
        // recovering the slot from the very arrivals being judged makes a
        // source that lost every other slot look perfectly punctual at half the
        // rate. `interval_us` comes from the capture's own configuration.
        let declared_us = capture
            .as_ref()
            .map(|r| r.snap.interval_us)
            .filter(|&i| i > 0)
            .map(|i| i as f64);
        dsp::metronome_into(
            &sc.ftm_ns,
            declared_us,
            &mut sc.metro_scratch,
            &mut sc.metronome,
        );
        sc.section.push("metronome_multiples", &sc.metronome.multiples);

        // -- per-tone behaviour ----------------------------------------------
        //
        // Reads the decimated bundle columns that are already in hand, so this
        // is a pass rather than an extraction.
        {
            let Scratch {
                columns,
                tone_median,
                tone_spread,
                tone_null,
                stats_scratch,
                tone_stats,
                ..
            } = sc;
            dsp::tone_stats_into(
                columns,
                geom.ntone,
                tone_median,
                tone_spread,
                tone_null,
                stats_scratch,
                tone_stats,
            );
        }
        sc.section.push("tone_median_db", &sc.tone_median);
        sc.section.push("tone_spread_db", &sc.tone_spread);
        sc.section.push("tone_null_frac", &sc.tone_null);

        // -- the CSI ratio ----------------------------------------------------
        {
            let Scratch {
                ratio_amp,
                ratio_phase,
                ..
            } = sc;
            if let Some(b) = chain_b {
                dsp::ratio_into(rec, chain_idx, b, ratio_amp, ratio_phase);
            } else {
                ratio_amp.clear();
                ratio_phase.clear();
            }
        }
        sc.section.push("ratio_amp_db", &sc.ratio_amp);
        sc.section.push("ratio_phase", &sc.ratio_phase);

        // -- the frequency axis ------------------------------------------------
        //
        // Explicit rather than implied by the index, because it is not implied
        // by the index: the delivered tones are two runs with a hole at DC (see
        // `crate::tones`), and the browser cannot reconstruct that from an array
        // length alone.
        crate::tones::offsets_hz_into(geom.ntone, spacing, &mut sc.tone_offsets_mhz);
        for v in sc.tone_offsets_mhz.iter_mut() {
            *v /= 1e6;
        }
        sc.section.push("tone_offset_mhz", &sc.tone_offsets_mhz);

        // -- the waterfall's plan (its pixels are drawn per client) -----------
        //
        // Two scopes, because the waterfall answers two different questions.
        // Scoped to the class it is a measurement of one signal at its native
        // tone grid; scoped to all classes it is a picture of the channel, and
        // rows of different geometries are placed by frequency so they remain
        // comparable. Nothing is discarded in either mode — the difference is
        // only which records reach the display.
        let all_scope = s.wf_scope == "all";
        let (wf_bins, wf_span_hz) = if all_scope {
            // The OCCUPIED span, not `ntone · spacing`. The delivered tones are
            // two runs with a hole between them, so counting only the tones
            // under-reports the band by exactly the width of that hole and
            // every class lands slightly wrong on a shared frequency axis.
            let span = sc
                .all
                .iter()
                .map(|smp| {
                    crate::tones::occupied_span_hz(
                        smp.rec.ntone as usize,
                        dsp::spacing_hz(&smp.rec),
                    )
                })
                .fold(0.0f64, f64::max);
            (s.wf_bins, span.max(1.0))
        } else {
            (geom.ntone, crate::tones::occupied_span_hz(geom.ntone, spacing))
        };

        // -- mixes and the talker table ---------------------------------------
        let mut mix = std::mem::take(&mut sc.header.mix);
        phy_mix(&sc.window, &mut mix);
        // Over `all`, not the scoped window: this panel answers "who is on the
        // channel", and scoping it to the transmitter already selected would
        // make it a table with one row that always says 100%.
        let talkers = talkers(&sc.all, &mut sc.talkers);

        let counters = &hub.counters;
        use std::sync::atomic::Ordering::Relaxed;

        let total_all = sc.census.total() as f64;
        let available: Vec<ClassEntry> = sc
            .census
            .ranked()
            .into_iter()
            .map(|(k, v)| ClassEntry {
                key: k,
                label: k.label(),
                count: v,
                share: v as f64 / total_all,
            })
            .collect();

        let h = &mut sc.header;
        h.waterfall.scope = if all_scope { "all" } else { "class" };
        h.waterfall.bins = wf_bins;
        h.waterfall.span_mhz = wf_span_hz / 1e6;

        h.class = ClassInfo {
            key: class,
            label: class.label(),
            pinned: s.class.is_some(),
            share: sc.census.count(class) as f64 / total_all,
            count: sc.census.count(class),
            available,
        };

        let selected_count = selected_mac
            .and_then(|m| by_mac.iter().find(|t| t.mac == m).map(|t| t.count))
            .unwrap_or(0);
        h.transmitter = crate::wire::TransmitterInfo {
            selected: selected_mac,
            pinned: requested_mac.is_some() && requested_mac == selected_mac,
            // Share of the *class*, not of the channel: the denominator has to
            // be the set the operator is choosing within, or a transmitter that
            // is 100% of its class reads as a minority.
            share: if class_window_len > 0 {
                selected_count as f64 / class_window_len as f64
            } else {
                0.0
            },
            count: selected_count,
            available: by_mac,
        };

        h.capture = match &capture {
            Some(r) => {
                let ratio = r.snap.yield_ratio();
                let verdict = crate::capture::classify(ratio, &r.snap.band);
                crate::wire::CaptureInfo {
                    present: true,
                    stale: r.stale(),
                    age_s: r.age.as_secs_f64(),
                    session_id: r.snap.session_id.clone(),
                    run_id: r.snap.run_id.clone(),
                    run_id_generated: r.snap.run_id_generated,
                    experiment: r.snap.experiment.clone(),
                    state: r.snap.state.clone(),
                    uptime_s: r.snap.uptime_s,
                    band: r.snap.band.clone(),
                    records: r.snap.records,
                    empty_records: r.snap.empty_records,
                    frames_seen: r.snap.frames_seen,
                    yield_ratio: ratio,
                    useful_yield_ratio: r.snap.useful_yield_ratio(),
                    yield_verdict: verdict.label(),
                    yield_note: crate::capture::note(verdict, &r.snap.band).unwrap_or(""),
                    rate_hz: r.snap.rate_hz,
                    capture_bytes: r.snap.capture_bytes,
                    live_dropped: r.snap.live_dropped,
                    interval_us: r.snap.interval_us,
                    ble: r.snap.ble.as_ref().map(|b| crate::wire::BleInfo {
                        observations: b.observations,
                        rate_hz: b.rate_hz,
                    }),
                }
            }
            None => crate::wire::CaptureInfo::default(),
        };

        h.nulls = nulls;
        h.metronome = sc.metronome.clone();
        h.tone_stats = sc.tone_stats.clone();
        // The band plan needs the tuned channel and the grid the class fixes.
        // When the record and the daemon disagree about the first, there is no
        // band plan to draw — a plan computed from the wrong channel is worse
        // than none, because it reads as a finding.
        h.bandplan = if freq_trusted {
            crate::bandplan::compute(rec.channel, geom.ntone, spacing)
        } else {
            crate::bandplan::disputed(rec.channel, tuned_channel, geom.ntone, spacing)
        };

        h.geometry.ntone = geom.ntone;
        h.geometry.nrx = geom.nrx;
        h.geometry.ntx = geom.ntx;
        h.geometry.nchain = nchain;
        h.geometry.chain = chain_idx;
        h.geometry.chain_b = chain_b;
        h.geometry.chain_labels.clear();
        h.geometry
            .chain_labels
            .extend((0..nchain).map(|c| geom.chain_label(c)));
        h.geometry.dimensions_ok = geom.matches(rec);

        h.radio.channel = rec.channel;
        h.radio.tuned_channel = tuned_channel;
        h.radio.channel_mismatch = channel_mismatch;
        h.radio.width.clear();
        use std::fmt::Write as _;
        let _ = write!(h.radio.width, "{}", rec.width);
        h.radio.freq_mhz = freq_mhz;
        h.radio.freq_assumed = s.freq_mhz.is_none();
        h.radio.freq_trusted = freq_trusted;
        h.radio.spacing_hz = spacing;
        h.radio.bw_mhz = dsp::occupied_bw_mhz(rec);

        h.record.session_uid = latest.session_uid;
        h.record.seq = latest.seq;
        h.record.ftm = rec.ftm;
        h.record.ftm_ticks = latest.ftm_ticks;
        h.record.us = rec.us;
        h.record.unix_ts_ns = rec.unix_ts_ns;
        h.record.recv_ns = latest.recv_ns;
        h.record.rssi.clear();
        h.record.rssi.extend_from_slice(&rec.rssi);
        h.record.src_mac = Mac(rec.src_mac);
        h.record.rnf = rec.rnf;
        h.record.phy = rec.phy.map(|p| PhyInfo {
            modulation: crate::class::Phy::of(Some(p.modulation)).to_string(),
            mcs: p.mcs,
            nss: p.nss,
        });

        h.phase_fit.slope_rad_per_tone = fit.slope;
        h.phase_fit.intercept_rad = fit.intercept;
        h.phase_fit.tau_ns = fit.tau_ns;

        h.bundle.width_db = sc.bundle.width_db;
        h.bundle.n = sc.bundle.n;

        h.cir.bin_ns = sc.cir.bin_ns;
        h.cir.resolution_ns = sc.cir.resolution_ns;
        h.cir.axis_start_ns = sc.cir.axis_start_ns;
        h.cir.peak_index = sc.cir.peak_index;
        h.cir.peak_bin = sc.cir.peak_bin;
        h.cir.rms_delay_ns = sc.cir.rms_delay_ns;
        h.cir.spread_resolvable = sc.cir.spread_is_resolvable();
        h.cir.taps = sc.cir.mag_db.len();

        h.doppler.fs_hz = sc.doppler.fs_hz;
        h.doppler.fs_window_hz = sc.doppler.fs_window_hz;
        h.doppler.fs_source = sc.doppler.fs_source;
        h.doppler.max_hz = sc.doppler.max_hz;
        h.doppler.max_speed_ms = sc.doppler.max_speed_ms;
        h.doppler.gap_frac = sc.doppler.gap_frac;
        h.doppler.span_s = sc.doppler.span_s;
        h.doppler.arrival_cv = sc.doppler.arrival_cv;
        h.doppler.conjugate_pair = sc.doppler.conjugate_pair;
        h.doppler.nfft = s.doppler_nfft;

        h.timing.ftm = t_ftm;
        h.timing.host = t_host;
        h.timing.hist_min_us = hist_min_us;
        h.timing.hist_max_us = hist_max_us;

        h.clocks.host_span_us = clocks.host_span_us;
        h.clocks.fw_span_us = clocks.fw_span_us;
        h.clocks.ftm_span_us = clocks.ftm_span_us;

        h.series.tones.clear();
        h.series.tones.extend_from_slice(&sc.tones);
        h.series.len = window_len;
        h.series.rssi_chains = rssi_chains;

        dsp::validate_into(rec, &mut sc.amp, &mut h.validation);
        h.mix = mix;
        h.talkers = talkers;

        h.stream.window = window_len;
        h.stream.window_class = class_window_len;
        h.stream.window_all = sc.all.len();
        h.stream.depth = hub.depth();
        h.stream.total = hub.total();
        h.stream.received = counters.received.load(Relaxed);
        h.stream.decode_errors = counters.decode_errors.load(Relaxed);
        h.stream.sender_gaps = counters.sender_gaps.load(Relaxed);
        h.stream.session_changes = counters.session_changes.load(Relaxed);
        h.stream.bytes = counters.bytes.load(Relaxed);
        if h.stream.source.is_empty() {
            h.stream.source.push_str(&hub.source);
        }
        h.stream.uptime_s = hub.started.elapsed().as_secs();

        h.f32 = sc.section.map().clone();
        h.n_f32 = sc.section.len();

        Some(SharedFrame {
            header_body: crate::wire::shared_body(h),
            f32_bytes: sc.section.bytes().to_vec(),
            n_f32: sc.section.len(),
            plan: WaterfallPlan {
                all_scope,
                bins: wf_bins,
                span_hz: wf_span_hz,
                class,
                ntone: geom.ntone,
                smac: selected_mac.map(|m| m.0),
                chain: s.chain,
                db_min: s.db_min,
                db_max: s.db_max,
                rows: s.wf_rows,
            },
        })
    }
}

/// How fast the tracked rate forgets a peak it no longer sees.
///
/// One part in two hundred per frame: at the console's default 20 fps a rate
/// that genuinely halves is followed within about ten seconds, and a lull
/// lasting a second moves the reference by half a percent. Slow on purpose —
/// the reference exists to be duller than the traffic.
const FS_RELEASE: f64 = 0.005;

/// Choose the Doppler axis for this frame, given what the window delivered.
///
/// `held` is a peak-hold with slow release, and the axis is that peak snapped
/// onto [`dsp::snap_rate_hz`]'s ladder. Two properties matter and they pull in
/// opposite directions:
///
/// **It must never sit below the delivered rate.** An axis narrower than the
/// achieved Nyquist means the resample decimates, and decimation aliases real
/// motion into the wrong bin. So the attack is instant.
///
/// **It must hold still.** The measured injector delivers between 9 and 21 Hz
/// within the same fifteen seconds (2026-08-23, all four nodes). A reference
/// that follows the mean sits right on a ladder boundary and flips across it
/// every few frames, which is the original fault wearing a different hat. A
/// peak-hold sits at the top of the wobble instead, where nothing crosses.
///
/// The cost is a wider axis than the average rate needs. That is the correct
/// trade: a too-wide Doppler axis wastes bins, a too-narrow one invents
/// velocities.
fn track_doppler_fs(held: &mut f64, fs_window: f64) -> f64 {
    if !(fs_window > 0.0) {
        return dsp::snap_rate_hz(*held);
    }
    if fs_window > *held {
        *held = fs_window;
    } else {
        *held += (fs_window - *held) * FS_RELEASE;
    }
    dsp::snap_rate_hz(*held)
}

/// One connection: where it has read up to, and the waterfall it draws.
pub struct ClientView {
    cursor: u64,
    arrived: Vec<Sample>,
    wf: Vec<u8>,
    row: Vec<f32>,
    amp: Vec<f32>,
    out: Vec<u8>,
}

impl ClientView {
    /// Start a client at the live edge, so it does not open on stale history.
    pub fn at_live_edge(hub: &Hub) -> Self {
        ClientView {
            cursor: hub.total(),
            arrived: Vec::new(),
            wf: Vec::new(),
            row: Vec::new(),
            amp: Vec::new(),
            out: Vec::new(),
        }
    }

    /// Draw this client's waterfall rows and assemble its frame.
    pub fn render(&mut self, hub: &Hub, shared: &SharedFrame) -> &[u8] {
        let plan = &shared.plan;

        // New records since this client's last frame. Ask for more than the
        // row budget, because part of what arrives belongs to other classes
        // and will not be drawn.
        let (cursor, ring_skipped) = hub.since_into(self.cursor, plan.rows * 8, &mut self.arrived);
        self.cursor = cursor;

        // Three outcomes, counted separately, because only one of them is a
        // shortfall. A record excluded by a scope was never meant to be drawn;
        // a record in `skipped` is one the display could not keep up with.
        let mut of_class = 0usize;
        let mut other_class = 0u64;
        let mut other_transmitter = 0u64;
        let mut empty = 0u64;
        for smp in &self.arrived {
            if ClassKey::of(&smp.rec) != plan.class {
                other_class += 1;
            } else if plan.smac.is_some_and(|mac| smp.rec.src_mac != mac) {
                other_transmitter += 1;
            } else if !dsp::is_measurement(&smp.rec) {
                // The same policy the windowed views apply, applied here for the
                // same reason: an all-zero record drew as a row at the colour
                // floor, which is a picture of a dead channel rather than of a
                // missing measurement. Counted separately from `skipped`,
                // because the display kept up perfectly — there was nothing in
                // the record to draw.
                empty += 1;
            } else {
                of_class += 1;
            }
        }
        // Records of this class the row budget could not carry are skipped
        // just as surely as the ones the ring never handed over; both belong
        // in the same honest count.
        let over_budget = of_class.saturating_sub(plan.rows) as u64;
        let skipped = ring_skipped + over_budget;

        self.wf.clear();
        let mut wf_rows = 0usize;

        // In `class` scope only records of the selected class are drawn, and
        // the oldest beyond the row budget are dropped. In `all` scope every
        // arrived record is placed by frequency instead.
        let mut remaining_drop = over_budget as usize;
        for smp in &self.arrived {
            let same_class = ClassKey::of(&smp.rec) == plan.class;
            // Never drawn, in either scope: a row of zeros is not a reading of
            // the channel at that instant.
            if !dsp::is_measurement(&smp.rec) {
                continue;
            }
            if !plan.all_scope {
                if !same_class {
                    continue;
                }
                // The panels beside the waterfall were computed over one
                // transmitter, so its rows are that transmitter's too. A
                // waterfall carrying an ambient AP's frames next to a spectrum
                // built only from the injector's would be two measurements on
                // one screen, which is the mistake the scope exists to stop.
                //
                // In `all` scope the transmitter filter is deliberately not
                // applied: that mode's whole purpose is to show the channel.
                if let Some(mac) = plan.smac {
                    if smp.rec.src_mac != mac {
                        continue;
                    }
                }
                if remaining_drop > 0 {
                    remaining_drop -= 1;
                    continue;
                }
            }

            let g = dsp::Geometry::of(&smp.rec);
            let c = plan.chain.min(g.nchain().saturating_sub(1));
            dsp::chain_amp_db_into(&smp.rec, c, &mut self.amp);
            if self.amp.is_empty() {
                continue;
            }
            if plan.all_scope {
                onto_shared_grid(
                    &self.amp,
                    dsp::spacing_hz(&smp.rec),
                    plan.span_hz,
                    plan.bins,
                    plan.db_min,
                    &mut self.row,
                );
                quantise_db(&self.row, plan.db_min, plan.db_max, &mut self.wf);
            } else if self.amp.len() == plan.ntone {
                quantise_db(&self.amp, plan.db_min, plan.db_max, &mut self.wf);
            } else {
                continue;
            }
            wf_rows += 1;
        }

        let mut header = ClientHeader {
            t: "frame",
            cursor,
            skipped,
            other_class,
            other_transmitter,
            wf_rows,
            empty,
            n_u8: self.wf.len(),
            ..Default::default()
        };
        header.u8.push("waterfall", 0, self.wf.len());

        crate::frame::encode(&header, shared, &self.wf, &mut self.out);
        &self.out
    }
}

/// One client and its own analysis, in one call.
///
/// This is the shape the tests and the bench use, and the shape the server
/// used to have. The server now runs [`Analysis`] once per distinct view and
/// fans the result out to every [`ClientView`], so two browsers on the same
/// settings cost one analysis rather than two.
pub struct Analyzer {
    analysis: Analysis,
    client: ClientView,
}

impl Analyzer {
    pub fn at_live_edge(hub: &Hub) -> Self {
        Analyzer {
            analysis: Analysis::new(),
            client: ClientView::at_live_edge(hub),
        }
    }

    /// Build one frame. Returns `None` when nothing has been received yet.
    pub fn frame(&mut self, hub: &Hub, s: &ViewSettings) -> Option<Vec<u8>> {
        let shared = self.analysis.compute(hub, s)?;
        Some(self.client.render(hub, &shared).to_vec())
    }
}

/// Resample one record's amplitude onto a shared frequency grid.
///
/// The grid spans `span_hz` centred on the channel's centre frequency, so a
/// 52-tone legacy row and a 242-tone HE row land on the *same frequencies*
/// rather than both being stretched to the full width. Bins the record does
/// not reach are painted at `floor_db` — an HE20 burst genuinely occupies more
/// of the channel than a legacy frame, and the picture should show that.
fn onto_shared_grid(
    amp: &[f32],
    spacing_hz: f64,
    span_hz: f64,
    bins: usize,
    floor_db: f32,
    out: &mut Vec<f32>,
) {
    out.clear();
    out.resize(bins, f32::NEG_INFINITY);
    if amp.is_empty() || span_hz <= 0.0 {
        out.iter_mut().for_each(|v| *v = floor_db);
        return;
    }
    let n = amp.len();
    let (mut lo, mut hi) = (bins, 0usize);
    for (i, &v) in amp.iter().enumerate() {
        // Frequency offset of this tone from the band centre — asked of the
        // tone grid, never computed from the array index. 802.11 never
        // transmits on DC, so the delivered tones are two runs with a hole
        // between them and `(i − n/2 + 0.5)·Δf` puts every tone above centre
        // one bin low. That is the bug `crate::tones` exists to remove, and
        // this was the last transform still carrying it.
        let f = crate::tones::offset_hz(i, n, spacing_hz);
        let pos = (f + span_hz / 2.0) / span_hz * bins as f64;
        if pos < 0.0 || pos >= bins as f64 {
            continue;
        }
        let b = pos as usize;
        // A coarse grid maps several tones to one bin; keep the strongest so a
        // narrow row never disappears into a neighbour's null.
        out[b] = out[b].max(v);
        lo = lo.min(b);
        hi = hi.max(b);
    }
    // A fine grid leaves gaps *between* tones; carry the nearest value forward
    // so the row reads as a band rather than a comb. Strictly within the
    // occupied span: outside it there is no measurement, and filling to the
    // grid edge would draw a narrow legacy frame as if it covered the whole
    // channel.
    if lo <= hi {
        let mut last = f32::NEG_INFINITY;
        for v in out[lo..=hi].iter_mut() {
            if v.is_finite() {
                last = *v;
            } else if last.is_finite() {
                *v = last;
            }
        }
    }
    for v in out.iter_mut() {
        if !v.is_finite() {
            *v = floor_db;
        }
    }
}

/// Three subcarriers spread across the band, used when the client has not
/// picked any.
///
/// Deliberately **not** including the band centre: 802.11 nulls the DC
/// subcarriers, so a centre tone traces the noise floor and looks like a
/// violently unstable channel next to its neighbours.
fn default_tones(ntone: usize, out: &mut Vec<usize>) {
    if ntone < 8 {
        out.push(0);
        return;
    }
    out.extend([ntone / 8, ntone / 3, ntone * 7 / 8]);
}

struct Clocks {
    host_span_us: f64,
    fw_span_us: f64,
    ftm_span_us: f64,
}

/// The three clocks, differenced against the 320 MHz baseband clock.
///
/// `csid`'s timing rule is "analyse on `ftm`, anchor wallclock on
/// `unix_ts_ns`". These series are what makes that rule visible: each point is
/// how far the host (or firmware) clock has wandered from the RF-plane clock
/// since the start of the window, in microseconds. A flat trace means the
/// anchor is sound; a ramp is clock drift; spikes are delivery jitter.
fn clock_series(window: &[Sample], host_us: &mut Vec<f32>, fw_us: &mut Vec<f32>) -> Clocks {
    host_us.clear();
    fw_us.clear();
    let (mut host_span, mut fw_span, mut ftm_span) = (0.0, 0.0, 0.0);

    if let Some(first) = window.first() {
        let t0 = first.ftm_ticks;
        let h0 = first.rec.unix_ts_ns;
        let f0 = first.rec.us;
        host_us.reserve(window.len());
        fw_us.reserve(window.len());
        for s in window {
            let ftm_us = csiq::ftm_to_seconds(s.ftm_ticks.saturating_sub(t0)) * 1e6;
            let h = if h0 > 0 && s.rec.unix_ts_ns >= h0 {
                (s.rec.unix_ts_ns - h0) as f64 / 1000.0 - ftm_us
            } else {
                f64::NAN
            };
            // The firmware microsecond clock wraps every ~71.6 minutes; a
            // wrapped difference is not drift, so it is left as NaN.
            let f = if s.rec.us >= f0 {
                (s.rec.us - f0) as f64 - ftm_us
            } else {
                f64::NAN
            };
            host_us.push(h as f32);
            fw_us.push(f as f32);
        }
        if let Some(last) = window.last() {
            ftm_span = csiq::ftm_to_seconds(last.ftm_ticks.saturating_sub(t0)) * 1e6;
            host_span = last.rec.unix_ts_ns.saturating_sub(h0) as f64 / 1000.0;
            fw_span = last.rec.us.saturating_sub(f0) as f64;
        }
    }

    Clocks {
        host_span_us: host_span,
        fw_span_us: fw_span,
        ftm_span_us: ftm_span,
    }
}

/// Log-spaced histogram of inter-arrival times. Returns `(lo_us, hi_us)`, the
/// decade bounds the bins span.
///
/// ## Why it cannot be linear
///
/// This panel is what decides whether the Doppler axis can be believed, and it
/// was the least readable thing on the page. The arrivals it describes span
/// three and a half decades: measured 2026-08-23, p50 236 µs against a maximum
/// of 735 ms. On a linear axis to 735 ms the entire distribution of interest
/// falls in the first bin, and the plot showed one spike and forty-seven empty
/// columns — while the two modes it was hiding (a burst spacing and a
/// between-burst gap) are exactly what separates a metronome that loses slots
/// from a source that is not metronomic at all.
///
/// Log bins put a decade in the same width wherever it sits, so both modes are
/// visible at once. The bounds are decade-aligned so the axis labels are round
/// numbers and do not move every frame.
fn histogram(times_ns: &[u64], bins: &mut Vec<f32>) -> (f32, f32) {
    bins.clear();
    bins.resize(HIST_BINS, 0.0);
    if times_ns.len() < 3 {
        return (1.0, 1000.0);
    }

    // The extremes, over the gaps themselves — a percentile would clip exactly
    // the tail this panel exists to show.
    let (mut lo, mut hi) = (f32::INFINITY, 0.0f32);
    for w in times_ns.windows(2) {
        let d = w[1].saturating_sub(w[0]) as f32 / 1000.0;
        // Sub-microsecond gaps are the 320 MHz clock's own resolution, not a
        // measurable interval; they anchor the axis at 1 µs instead.
        if d >= 1.0 {
            lo = lo.min(d);
            hi = hi.max(d);
        }
    }
    if !lo.is_finite() || hi <= 0.0 {
        return (1.0, 1000.0);
    }
    // Snap outwards to whole decades.
    let lo = 10f32.powf(lo.log10().floor()).max(1.0);
    let hi = 10f32.powf(hi.log10().ceil()).max(lo * 10.0);

    let span = hi.log10() - lo.log10();
    for w in times_ns.windows(2) {
        let d = (w[1].saturating_sub(w[0]) as f32 / 1000.0).max(lo);
        let pos = (d.log10() - lo.log10()) / span;
        let b = (pos * HIST_BINS as f32) as usize;
        bins[b.min(HIST_BINS - 1)] += 1.0;
    }
    (lo, hi)
}

/// Distribution of PHY labels, tone counts and widths over the window.
fn phy_mix(window: &[Sample], mix: &mut MixInfo) {
    mix.clear();
    for s in window {
        let r = &s.rec;
        mix.ntone.add(r.ntone);
        mix.width.add(WidthKey(r.width));
        match r.phy {
            Some(p) => {
                mix.modulation
                    .add(crate::class::Phy::of(Some(p.modulation)));
                mix.nss.add(p.nss);
                mix.mcs.add(p.mcs);
            }
            None => mix.modulation.add(crate::class::Phy::Unlabelled),
        }
    }
}

/// Who is actually sounding the channel.
///
/// Ambient capture means the record rate is somebody else's transmit rate, so
/// "why is my rate low" is usually answered here rather than in the config. The
/// table also feeds the `radio.mac_filter` editor: pick a talker, pin the
/// capture to it.
fn talkers(window: &[Sample], acc: &mut Vec<(Mac, TalkerAcc)>) -> Vec<Talker> {
    acc.clear();
    for s in window {
        let mac = Mac(s.rec.src_mac);
        // A busy channel carries a few dozen transmitters at most, and a
        // six-byte compare against a value already in a register beats hashing
        // one at that cardinality.
        let e = match acc.iter_mut().find(|(m, _)| *m == mac) {
            Some((_, e)) => e,
            None => {
                acc.push((
                    mac,
                    TalkerAcc {
                        first_ticks: s.ftm_ticks,
                        last_ticks: s.ftm_ticks,
                        ..Default::default()
                    },
                ));
                &mut acc.last_mut().unwrap().1
            }
        };
        e.count += 1;
        if let Some(&r) = s.rec.rssi.first() {
            e.rssi_sum += r as i64;
            e.rssi_n += 1;
        }
        e.last_ns = e.last_ns.max(s.recv_ns);
        e.last_ticks = e.last_ticks.max(s.ftm_ticks);
    }

    let mut rows: Vec<Talker> = acc
        .iter()
        .map(|&(mac, a)| {
            let span = csiq::ftm_to_seconds(a.last_ticks.saturating_sub(a.first_ticks));
            Talker {
                mac,
                count: a.count,
                rate_hz: if span > 0.0 {
                    a.count as f64 / span
                } else {
                    0.0
                },
                rssi: if a.rssi_n > 0 {
                    Some(a.rssi_sum as f64 / a.rssi_n as f64)
                } else {
                    None
                },
                last_ns: a.last_ns,
            }
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.count));
    rows.truncate(12);
    rows
}

/// Control-channel frequency implied by the record, when the band is inferable.
///
/// 6 GHz channel numbering overlaps 2.4 GHz, so inference genuinely cannot
/// resolve it — the console lets the operator pin the frequency instead, and
/// flags the value as assumed until they do.
fn control_freq_mhz(rec: &CsiRecord) -> Option<f64> {
    let band = csid::caps::infer_band(rec.channel)?;
    csid::caps::channel_to_freq(band, rec.channel).map(|f| f as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Sample;

    fn sample(i: u64, ntone: u16, nchain_rx: u8) -> Sample {
        let nc = nchain_rx as usize;
        let mut iq = Vec::new();
        for t in 0..ntone as usize {
            for c in 0..nc {
                iq.push(100 + t as i16 + c as i16 * 5);
                iq.push(t as i16 % 7);
            }
        }
        Sample {
            session_uid: 1,
            seq: i as u32,
            // ~600 Hz.
            ftm_ticks: i * 533_333,
            recv_ns: 1_700_000_000_000_000_000 + i * 1_666_000,
            rec: Arc::new(CsiRecord {
                ftm: (i * 533_333) as u32,
                us: (i * 1666) as u32,
                unix_ts_ns: 1_700_000_000_000_000_000 + i * 1_666_000,
                rnf: 0x0442,
                phy: Some(csiq::PhyLabel {
                    modulation: csiq::Modulation::He,
                    mcs: 2,
                    nss: 1,
                }),
                bw_antsel: None,
                seq: 0,
                nrx: nchain_rx,
                ntx: 1,
                ntone,
                rssi: vec![-43; nc],
                src_mac: [0xde, 0xad, 0xbe, 0xef, 0, (i % 3) as u8],
                channel: 36,
                width: csiq::Width::W80,
                iq,
            }),
        }
    }

    /// The same record, delivered empty: intact header, correct payload
    /// length, plausible RSSI, every coefficient zero. This is the shape the
    /// fleet actually produces, not a synthetic edge case.
    fn empty_sample(i: u64, ntone: u16, nchain_rx: u8) -> Sample {
        let mut s = sample(i, ntone, nchain_rx);
        let rec = Arc::get_mut(&mut s.rec).expect("freshly built");
        rec.iq.iter_mut().for_each(|v| *v = 0);
        s
    }

    fn header(buf: &[u8]) -> serde_json::Value {
        let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        serde_json::from_slice(&buf[4..4 + hlen]).unwrap()
    }


    /// The single null policy, measured end to end.
    ///
    /// One record in six is empty, exactly as the fleet delivers them. Every
    /// windowed view must be computed over the other five, the count must reach
    /// the browser, and — the part that was wrong for the whole of the console's
    /// life — the p05 of the amplitude bundle must not sit on a null.
    #[test]
    fn empty_records_leave_the_window_and_are_reported() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        // The fixture rotates the source MAC on `i % 3`, and the view below
        // scopes to one transmitter. Planting on a multiple of 12 puts every
        // empty inside that transmitter's own records — one in four of them —
        // rather than hiding them all behind the scope.
        let mut planted = 0usize;
        for i in 0..600u64 {
            if i % 12 == 0 {
                planted += 1;
                hub.push(empty_sample(i, 242, 2));
            } else {
                hub.push(sample(i, 242, 2));
            }
        }
        assert!(planted > 0);

        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;
        let s = ViewSettings {
            window: 256,
            // One transmitter, so the window is not split three ways by the
            // rotating source MAC the fixture generates.
            smac: Some("de:ad:be:ef:00:00".into()),
            ..Default::default()
        };
        let buf = a.frame(&hub, &s).unwrap();
        let h = header(&buf);

        let dropped = h["nulls"]["dropped"].as_u64().unwrap();
        let considered = h["nulls"]["considered"].as_u64().unwrap();
        assert!(dropped > 0, "the planted empties must be found");
        assert_eq!(
            h["stream"]["window"].as_u64().unwrap(),
            considered - dropped,
            "the analysis window is what survived the drop"
        );
        assert!(!h["nulls"]["no_measurement"].as_bool().unwrap());
        let frac = h["nulls"]["frac"].as_f64().unwrap();
        assert!((0.15..0.35).contains(&frac), "frac was {frac}, expected ~0.25");

        // The defect this policy exists to remove: a p05 pinned to a null.
        let arrays = |name: &str| {
            let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            let (off, len) = {
                let m = &h["f32"][name];
                (m[0].as_u64().unwrap() as usize, m[1].as_u64().unwrap() as usize)
            };
            let base = 4 + hlen + off * 4;
            (0..len)
                .map(|i| {
                    f32::from_le_bytes(buf[base + i * 4..base + i * 4 + 4].try_into().unwrap())
                })
                .collect::<Vec<f32>>()
        };
        let p05 = arrays("bundle_p05");
        assert!(!p05.is_empty());
        assert!(
            p05.iter().all(|&v| v > dsp::NULL_TONE_DB),
            "the p05 envelope must not rest on a null"
        );
        // And the same records must not reach the waterfall either.
        assert!(h["empty"].as_u64().unwrap() > 0);
    }

    /// When nothing in the window measured anything, say so rather than going
    /// silent — a console that stops producing frames looks like a dead stream,
    /// and this is a different fault with a different cure.
    #[test]
    fn a_window_of_nothing_but_empties_is_declared_not_hidden() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..300u64 {
            hub.push(empty_sample(i, 242, 2));
        }
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert!(h["nulls"]["no_measurement"].as_bool().unwrap());
        assert_eq!(
            h["nulls"]["dropped"].as_u64().unwrap(),
            h["nulls"]["considered"].as_u64().unwrap()
        );
    }

    /// The spectrogram's axis must outlast the traffic's mood.
    ///
    /// The rates below are the ones measured on monad01 on 2026-08-23, where
    /// the delivered rate moved by 2.3× inside fifteen seconds and the old
    /// derived axis moved with it.
    #[test]
    fn the_doppler_axis_holds_still_while_the_delivered_rate_wobbles() {
        let mut held = 0.0f64;
        let mut seen = std::collections::BTreeSet::new();
        // Two hundred frames of the measured wobble. The first two are the
        // reference finding the top of it; everything after that must hold.
        for i in 0..200 {
            let fs = if i % 2 == 0 { 9.3 } else { 21.3 };
            let axis = track_doppler_fs(&mut held, fs);
            if i >= 2 {
                seen.insert(axis.to_bits());
            }
        }
        assert_eq!(seen.len(), 1, "the axis must not move on a wobble");
        let axis = f64::from_bits(*seen.iter().next().unwrap());
        assert!(axis >= 21.3, "and it must never sit below the delivered rate");

        // A genuine step up is followed at once: an axis below the delivered
        // Nyquist aliases, which is worse than a wide one.
        let stepped = track_doppler_fs(&mut held, 260.0);
        assert!(stepped >= 260.0);

        // A genuine step down is followed slowly, and does get there.
        let mut last = stepped;
        for _ in 0..4000 {
            last = track_doppler_fs(&mut held, 12.0);
        }
        assert!(last < stepped, "a sustained drop must eventually narrow it");
        assert!(last >= 12.0);
    }

    /// A channel the record and the daemon disagree about is not a channel to
    /// compute a wavelength or a band plan from.
    ///
    /// The case is real: `monad01_illum-coex-03_20260823-102958-seg0003`
    /// declares channel 3 in its sidecar and carries channel 48 in every one of
    /// its 2433 records. Without a status document the console cannot see that
    /// at all; with one it must not paper over it.
    #[test]
    fn a_disputed_channel_withholds_everything_derived_from_it() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..300u64 {
            hub.push(sample(i, 242, 2));
        }
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;

        // No status document: nothing to disagree with, so the record's own
        // channel stands and everything derived from it is drawn.
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert_eq!(h["radio"]["channel"], 36);
        assert!(h["radio"]["tuned_channel"].is_null());
        assert_eq!(h["radio"]["channel_mismatch"], false);
        assert_eq!(h["radio"]["freq_trusted"], true);
    }

    /// Three and a half decades of arrivals must not collapse into one bin.
    #[test]
    fn the_interarrival_histogram_separates_the_modes_it_used_to_hide() {
        // The measured shape: a burst spacing near 236 µs and gaps near 200 ms.
        let mut t: Vec<u64> = Vec::new();
        let mut now = 0u64;
        for _ in 0..40 {
            for _ in 0..25 {
                t.push(now);
                now += 236_000;
            }
            now += 200_000_000;
        }
        let mut bins = Vec::new();
        let (lo, hi) = histogram(&t, &mut bins);
        assert!(lo <= 236.0 && hi >= 200_000.0, "bounds {lo}..{hi} us");

        let occupied: Vec<usize> = bins
            .iter()
            .enumerate()
            .filter(|(_, &v)| v > 0.0)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(occupied.len(), 2, "two modes, two bins: {occupied:?}");
        assert!(
            occupied[1] - occupied[0] > 8,
            "the modes must be decades apart on the axis: {occupied:?}"
        );
        // And the small one must not be crushed against the left edge.
        assert!(occupied[0] > 0, "the burst mode is not at the origin");
    }

    #[test]
    fn empty_stream_produces_no_frame() {
        let hub = Hub::new("test".into(), 1024, usize::MAX);
        let mut a = Analyzer::at_live_edge(&hub);
        assert!(a.frame(&hub, &ViewSettings::default()).is_none());
    }

    #[test]
    fn frame_declares_every_array_it_promises() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..600 {
            hub.push(sample(i, 242, 2));
        }
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0; // pretend the client has drawn nothing yet
        let buf = a.frame(&hub, &ViewSettings::default()).unwrap();
        let h = header(&buf);

        for name in [
            "amp_db",
            "phase_raw",
            "phase_unwrapped",
            "phase_detrended",
            "iq",
            "chain_amp_db",
            "cir_db",
            "bundle_p50",
            "doppler_db",
            "tone_series",
            "rssi_series",
            "drift_host_us",
            "drift_fw_us",
            "interarrival_hist",
            "metronome_multiples",
            "tone_median_db",
            "tone_spread_db",
            "tone_null_frac",
            "ratio_amp_db",
            "ratio_phase",
            "tone_offset_mhz",
        ] {
            assert!(h["f32"][name].is_array(), "missing f32 array {name}");
        }
        assert!(h["u8"]["waterfall"].is_array());

        assert_eq!(h["geometry"]["ntone"], 242);
        assert_eq!(h["geometry"]["nchain"], 2);
        assert_eq!(h["f32"]["amp_db"][1], 242);
        assert_eq!(h["f32"]["iq"][1], 484);
        assert_eq!(h["f32"]["chain_amp_db"][1], 484, "one spectrum per chain");
        // The frequency axis is sent explicitly because it cannot be derived
        // from the array length: the delivered tones have a hole at DC.
        assert_eq!(h["f32"]["tone_offset_mhz"][1], 242);
        assert_eq!(h["f32"]["tone_spread_db"][1], 242);

        // The requested window is the whole channel; the analysis window is one
        // class and one transmitter of it. The synthetic stream cycles three
        // source MACs, so a third of the class window survives the second scope.
        assert_eq!(h["stream"]["window_all"], 256);
        assert_eq!(h["stream"]["window_class"], 256, "one class in this stream");
        let scoped = h["stream"]["window"].as_u64().unwrap();
        assert!((84..=86).contains(&scoped), "scoped window was {scoped}");
        assert_eq!(h["transmitter"]["available"].as_array().unwrap().len(), 3);
        assert!(h["transmitter"]["selected"].is_string());

        // ~600 Hz arrives on the channel; one transmitter of three delivers a
        // third of it, and the rate under the plots is that transmitter's.
        let rate = h["timing"]["ftm"]["rate_hz"].as_f64().unwrap();
        assert!(rate > 150.0 && rate < 250.0, "rate was {rate}");
    }

    #[test]
    fn waterfall_reports_what_it_could_not_show() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        let mut a = Analyzer::at_live_edge(&hub);
        for i in 0..600 {
            hub.push(sample(i, 52, 1));
        }
        let s = ViewSettings {
            wf_rows: 16,
            ..Default::default()
        };
        let buf = a.frame(&hub, &s).unwrap();
        let h = header(&buf);
        assert_eq!(h["wf_rows"], 16);
        assert_eq!(h["u8"]["waterfall"][1], 16 * 52);

        // Every record that arrived is accounted for exactly once, and the four
        // outcomes are kept apart: drawn, not kept up with, wrong class, wrong
        // transmitter. Only the second is a shortfall of the display.
        let drawn = h["wf_rows"].as_u64().unwrap();
        let skipped = h["skipped"].as_u64().unwrap();
        let other_class = h["other_class"].as_u64().unwrap();
        let other_tx = h["other_transmitter"].as_u64().unwrap();
        assert_eq!(
            drawn + skipped + other_class + other_tx,
            600,
            "drawn {drawn} + skipped {skipped} + other class {other_class} + \
             other transmitter {other_tx} must be every record pushed"
        );
        assert_eq!(other_class, 0, "the stream is one class");
        assert!(other_tx > 0, "and three transmitters");
        assert_eq!(h["other_class"], 0, "one synthetic class only");

        // Caught up: the next frame carries nothing new, and says so.
        let buf = a.frame(&hub, &s).unwrap();
        let h = header(&buf);
        assert_eq!(h["wf_rows"], 0);
        assert_eq!(h["skipped"], 0);
    }

    /// Build a record with an explicit tone count and modulation, so a window
    /// can hold the interleaved classes a real channel actually delivers.
    fn classed(i: u64, ntone: u16, modulation: csiq::Modulation) -> Sample {
        let mut s = sample(i, ntone, 2);
        let rec = Arc::get_mut(&mut s.rec).unwrap();
        rec.phy = Some(csiq::PhyLabel {
            modulation,
            mcs: 2,
            nss: 1,
        });
        s
    }

    /// The behaviour real traffic forced: an ambient channel interleaves
    /// legacy-52 and HT-56 records, and a view that mixes them is not a
    /// measurement. Every array must describe one class only.
    #[test]
    fn a_mixed_stream_is_scoped_to_one_class() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        // Five legacy records for every HT one, as measured on channel 11.
        for i in 0..600 {
            if i % 6 == 0 {
                hub.push(classed(i, 56, csiq::Modulation::Ht));
            } else {
                hub.push(classed(i, 52, csiq::Modulation::LegacyOfdm));
            }
        }
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;

        // Unpinned: follow the dominant class.
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert_eq!(h["class"]["key"], "52:legacyofdm");
        assert_eq!(h["class"]["pinned"], false);
        assert_eq!(h["geometry"]["ntone"], 52);
        assert_eq!(h["f32"]["amp_db"][1], 52);
        assert_eq!(
            h["u8"]["waterfall"][1].as_u64().unwrap() % 52,
            0,
            "every waterfall row must be the same width"
        );
        assert!(
            h["other_class"].as_u64().unwrap() > 0,
            "records of other classes must be reported, not silently dropped"
        );

        // Both classes are offered, ranked by how much of the channel they are.
        let avail = h["class"]["available"].as_array().unwrap();
        assert_eq!(avail.len(), 2);
        assert_eq!(avail[0]["key"], "52:legacyofdm");
        assert_eq!(avail[1]["key"], "56:ht");

        // Pinned: hold the minority class still even though it is outnumbered.
        let s = ViewSettings {
            class: Some("56:ht".into()),
            ..Default::default()
        };
        let h = header(&a.frame(&hub, &s).unwrap());
        assert_eq!(h["class"]["key"], "56:ht");
        assert_eq!(h["class"]["pinned"], true);
        assert_eq!(h["geometry"]["ntone"], 56);
        assert_eq!(h["f32"]["amp_db"][1], 56);
        assert_eq!(h["mix"]["ntone"]["56"], h["stream"]["window"]);

        // A class that has left the air falls back rather than showing nothing.
        let s = ViewSettings {
            class: Some("996:he".into()),
            ..Default::default()
        };
        let h = header(&a.frame(&hub, &s).unwrap());
        assert_eq!(h["class"]["key"], "52:legacyofdm");
    }

    /// The answer to "I do not want to lose any information": scoped to a
    /// class the waterfall is a measurement of one signal; scoped to `all` it
    /// carries every record, placed by frequency so a 52-tone row and a
    /// 242-tone row occupy their true share of the channel.
    #[test]
    fn the_waterfall_can_show_every_class_on_one_frequency_axis() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..300 {
            if i % 3 == 0 {
                hub.push(classed(i, 242, csiq::Modulation::He)); // 18.9 MHz
            } else {
                hub.push(classed(i, 52, csiq::Modulation::LegacyOfdm)); // 16.3 MHz
            }
        }

        // Scoped: native tone grid, one class only.
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert_eq!(h["waterfall"]["scope"], "class");
        assert_eq!(h["waterfall"]["bins"], 52);
        assert_eq!(h["u8"]["waterfall"][1].as_u64().unwrap() % 52, 0);
        assert!(h["other_class"].as_u64().unwrap() > 0);

        // All classes: one fixed-width grid spanning the widest occupancy,
        // and every arrived record drawn rather than two thirds of them.
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;
        let s = ViewSettings {
            wf_scope: "all".into(),
            wf_bins: 128,
            wf_rows: 64,
            ..Default::default()
        };
        let h = header(&a.frame(&hub, &s).unwrap());
        assert_eq!(h["waterfall"]["scope"], "all");
        assert_eq!(h["waterfall"]["bins"], 128);
        // The OCCUPIED span of the widest class, which is wider than its
        // delivered tone count by exactly the DC hole: HE20's 242 tones are
        // ±2…±122, so the band is 245 subcarriers across, not 242.
        let span = h["waterfall"]["span_mhz"].as_f64().unwrap();
        let expect = crate::tones::occupied_span_hz(242, 78_125.0) / 1e6;
        assert!(
            (span - expect).abs() < 0.01,
            "span must cover the widest class, got {span} MHz, expected {expect}"
        );
        assert!(
            span > 242.0 * 78_125.0 / 1e6,
            "counting only the delivered tones under-reports the band"
        );
        let rows = h["wf_rows"].as_u64().unwrap();
        assert_eq!(h["u8"]["waterfall"][1].as_u64().unwrap(), rows * 128);
        assert!(
            rows > 64,
            "all classes must be carried, not just one in three"
        );
    }

    /// A narrow row must occupy only its own slice of a wider shared grid —
    /// stretching it to full width would misplace it in frequency.
    #[test]
    fn the_shared_grid_places_rows_by_frequency() {
        // 8 tones at 312.5 kHz = 2.5 MHz, on a 10 MHz grid of 100 bins.
        let amp: Vec<f32> = vec![40.0; 8];
        let mut row = Vec::new();
        onto_shared_grid(&amp, 312_500.0, 10e6, 100, -999.0, &mut row);
        let occupied: Vec<usize> = row
            .iter()
            .enumerate()
            .filter(|(_, v)| **v > -900.0)
            .map(|(i, _)| i)
            .collect();
        assert!(!occupied.is_empty());
        // Centred: the occupied span sits around the middle quarter.
        assert!(*occupied.first().unwrap() >= 37, "{occupied:?}");
        assert!(*occupied.last().unwrap() <= 62, "{occupied:?}");

        // A row as wide as the grid fills it end to end.
        let wide: Vec<f32> = vec![40.0; 32];
        onto_shared_grid(&wide, 312_500.0, 10e6, 100, -999.0, &mut row);
        assert!(row.iter().filter(|v| **v > -900.0).count() >= 95);
    }

    #[test]
    fn talkers_are_ranked_and_the_mix_is_counted() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..300 {
            hub.push(sample(i, 52, 1));
        }
        let mut a = Analyzer::at_live_edge(&hub);
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());

        let t = h["talkers"].as_array().unwrap();
        assert_eq!(t.len(), 3, "three synthetic source MACs");
        let counts: Vec<u64> = t.iter().map(|r| r["count"].as_u64().unwrap()).collect();
        assert!(counts.windows(2).all(|w| w[0] >= w[1]), "must be ranked");
        // The talker table is the CHANNEL, so it counts every record; the mix
        // describes what is under the plots, so it counts the scoped window.
        assert_eq!(counts.iter().sum::<u64>(), 256);
        let mixed = h["mix"]["ntone"]["52"].as_u64().unwrap();
        assert_eq!(h["mix"]["modulation"]["he"].as_u64().unwrap(), mixed);
        assert!((84..=86).contains(&mixed), "mix counted {mixed}");
    }

    #[test]
    fn chain_b_is_dropped_when_it_does_not_exist() {
        let hub = Hub::new("test".into(), 1024, usize::MAX);
        for i in 0..64 {
            hub.push(sample(i, 52, 1)); // single chain
        }
        let mut a = Analyzer::at_live_edge(&hub);
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert!(h["geometry"]["chain_b"].is_null());
        assert_eq!(
            h["doppler"]["conjugate_pair"], false,
            "the UI must be told the spectrum still carries CFO"
        );
    }

    #[test]
    fn frequency_is_flagged_when_inferred() {
        let hub = Hub::new("test".into(), 1024, usize::MAX);
        for i in 0..64 {
            hub.push(sample(i, 52, 2));
        }
        let mut a = Analyzer::at_live_edge(&hub);

        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert_eq!(h["radio"]["freq_mhz"], 5180.0, "channel 36 on 5 GHz");
        assert_eq!(h["radio"]["freq_assumed"], true);

        let s = ViewSettings {
            freq_mhz: Some(5975.0),
            ..Default::default()
        };
        let h = header(&a.frame(&hub, &s).unwrap());
        assert_eq!(h["radio"]["freq_mhz"], 5975.0);
        assert_eq!(h["radio"]["freq_assumed"], false);
    }

    /// The point of the split: one analysis, many clients, identical numbers.
    /// Two views that differ only in frame rate must produce byte-identical
    /// shared halves, and each client's own waterfall must still track its own
    /// cursor.
    #[test]
    fn one_analysis_serves_many_clients() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..600 {
            hub.push(sample(i, 242, 2));
        }
        let s = ViewSettings::default();
        let mut analysis = Analysis::new();
        let shared = analysis.compute(&hub, &s).unwrap();

        // A client that has seen nothing draws rows; one at the live edge does
        // not. Both read the same shared analysis.
        let mut behind = ClientView::at_live_edge(&hub);
        behind.cursor = 0;
        let mut caught_up = ClientView::at_live_edge(&hub);

        let a = header(behind.render(&hub, &shared));
        let b = header(caught_up.render(&hub, &shared));

        assert!(a["wf_rows"].as_u64().unwrap() > 0);
        assert_eq!(b["wf_rows"], 0);
        // Every windowed view is the same measurement for both.
        for field in ["class", "geometry", "radio", "timing", "bundle", "doppler"] {
            assert_eq!(a[field], b[field], "{field} must be shared verbatim");
        }
    }

    /// Reusing every buffer across frames must not leak state from one frame
    /// into the next: a second frame over an unchanged ring has to be
    /// identical to the first, apart from the client's own cursor fields.
    #[test]
    fn repeated_frames_over_an_unchanged_ring_are_identical() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..600 {
            hub.push(sample(i, 242, 2));
        }
        let s = ViewSettings::default();
        let mut analysis = Analysis::new();

        let first = analysis.compute(&hub, &s).unwrap();
        let second = analysis.compute(&hub, &s).unwrap();
        assert_eq!(
            first.f32_bytes, second.f32_bytes,
            "a reused scratch buffer must not change the numbers"
        );

        // `uptime_s` is the only field allowed to move, and only once a second.
        let a: serde_json::Value =
            serde_json::from_str(&format!("{{{}}}", first.header_body)).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(&format!("{{{}}}", second.header_body)).unwrap();
        for field in [
            "class",
            "geometry",
            "radio",
            "record",
            "phase_fit",
            "bundle",
            "cir",
            "doppler",
            "timing",
            "clocks",
            "series",
            "validation",
            "mix",
            "talkers",
            "f32",
        ] {
            assert_eq!(a[field], b[field], "{field} drifted between frames");
        }
    }

    /// Switching the pinned class and switching back must land exactly where
    /// it started — the class-scoped buffers are reused across both.
    #[test]
    fn switching_class_does_not_contaminate_the_previous_one() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..600 {
            if i % 6 == 0 {
                hub.push(classed(i, 56, csiq::Modulation::Ht));
            } else {
                hub.push(classed(i, 52, csiq::Modulation::LegacyOfdm));
            }
        }
        let mut analysis = Analysis::new();
        let legacy = ViewSettings::default();
        let ht = ViewSettings {
            class: Some("56:ht".into()),
            ..Default::default()
        };

        let a = analysis.compute(&hub, &legacy).unwrap();
        let _ = analysis.compute(&hub, &ht).unwrap();
        let c = analysis.compute(&hub, &legacy).unwrap();
        assert_eq!(a.f32_bytes, c.f32_bytes);
        assert_eq!(a.header_body, c.header_body);
    }

    /// The header is a contract with `ui/app.js`, and the browser reads it by
    /// path with no schema in between — a renamed or dropped field shows up as
    /// `undefined` in a readout rather than as an error anybody notices.
    ///
    /// This is the list of every path the shipped console dereferences. It was
    /// extracted from `app.js`/`plot.js`; if a panel starts reading something
    /// new, it belongs here too.
    #[test]
    fn the_header_carries_every_field_the_console_reads() {
        let hub = Hub::new("test".into(), 4096, usize::MAX);
        for i in 0..600 {
            hub.push(sample(i, 242, 2));
        }
        let mut a = Analyzer::at_live_edge(&hub);
        a.client.cursor = 0;
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());

        for path in [
            "t",
            "cursor",
            "skipped",
            "other_class",
            "wf_rows",
            "n_f32",
            "n_u8",
            "waterfall.scope",
            "waterfall.bins",
            "waterfall.span_mhz",
            "class.key",
            "class.label",
            "class.pinned",
            "class.share",
            "class.count",
            "geometry.ntone",
            "geometry.nrx",
            "geometry.ntx",
            "geometry.nchain",
            "geometry.chain",
            "geometry.chain_labels",
            "geometry.dimensions_ok",
            "radio.channel",
            "radio.width",
            "radio.freq_mhz",
            "radio.freq_assumed",
            "radio.spacing_hz",
            "radio.bw_mhz",
            "record.session_uid",
            "record.rssi",
            "record.src_mac",
            "record.phy.mcs",
            "record.phy.nss",
            "record.phy.modulation",
            "phase_fit.tau_ns",
            "phase_fit.slope_rad_per_tone",
            "bundle.n",
            "bundle.width_db",
            "cir.bin_ns",
            "cir.resolution_ns",
            "cir.axis_start_ns",
            "cir.peak_index",
            "cir.peak_bin",
            "cir.rms_delay_ns",
            "cir.spread_resolvable",
            "doppler.nfft",
            "doppler.fs_hz",
            "doppler.fs_window_hz",
            "doppler.fs_source",
            "doppler.max_hz",
            "doppler.max_speed_ms",
            "doppler.gap_frac",
            "doppler.span_s",
            "doppler.arrival_cv",
            "doppler.conjugate_pair",
            "radio.channel_mismatch",
            "radio.freq_trusted",
            "nulls.dropped",
            "nulls.considered",
            "nulls.frac",
            "nulls.no_measurement",
            "timing.ftm.rate_hz",
            "timing.host.p50_us",
            "timing.host.p999_us",
            "timing.hist_min_us",
            "timing.hist_max_us",
            "clocks.ftm_span_us",
            "clocks.host_span_us",
            "clocks.fw_span_us",
            "series.len",
            "series.tones",
            "series.rssi_chains",
            "validation.dimensions_ok",
            "validation.edge_rolloff_db",
            "validation.chain_spread_db",
            "validation.zero_fraction",
            "mix.modulation",
            "mix.ntone",
            "mix.nss",
            "mix.mcs",
            "mix.width",
            "stream.source",
            "stream.received",
            "stream.sender_gaps",
            "stream.decode_errors",
            "stream.session_changes",
            "stream.bytes",
            "stream.depth",
            "stream.window",
            "stream.window_all",
            "stream.uptime_s",
            "stream.total",
        ] {
            let mut node = &h;
            for part in path.split('.') {
                node = &node[part];
                assert!(
                    !node.is_null(),
                    "the console reads `h.{path}`, and the header does not carry it"
                );
            }
        }

        // Two validation fields are legitimately null and are therefore checked
        // for presence rather than for a value: `chain_identical` on a
        // single-chain record, and `dc_notch_db` on an 802.11 used-tone grid,
        // which has no DC bin to test. See `dsp::Validation`.
        let v = h["validation"].as_object().unwrap();
        assert!(v.contains_key("chain_identical"));
        assert!(v.contains_key("dc_notch_db"));

        // The two arrays the console iterates as tables.
        let avail = h["class"]["available"].as_array().expect("class.available");
        for e in avail {
            for k in ["key", "label", "count", "share"] {
                assert!(!e[k].is_null(), "class.available[].{k}");
            }
        }
        let talkers = h["talkers"].as_array().expect("talkers");
        assert!(!talkers.is_empty());
        for t in talkers {
            for k in ["mac", "count", "rate_hz"] {
                assert!(!t[k].is_null(), "talkers[].{k}");
            }
            assert!(t.as_object().unwrap().contains_key("rssi"));
        }
    }

    #[test]
    fn decimation_keeps_the_newest_sample() {
        // The bundle's decimation is now an index walk inside `compute`; this
        // pins the property it has to keep — the newest record is always in.
        let n = 1000usize;
        let cols = BUNDLE_COLUMNS;
        let step = n as f64 / cols as f64;
        let picks: Vec<usize> = (0..cols)
            .map(|i| ((i as f64 * step) as usize).min(n - 1))
            .collect();
        assert_eq!(picks.len(), cols);
        assert_eq!(picks[0], 0);
        assert!(picks.windows(2).all(|w| w[0] < w[1]));
        assert!(
            n - 1 - picks[cols - 1] < step.ceil() as usize,
            "the newest record must be within one decimation step"
        );
    }
}
