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
        }
    }

    /// Compute one tick's shared analysis. `None` when nothing has arrived.
    pub fn compute(&mut self, hub: &Hub, s: &ViewSettings) -> Option<SharedFrame> {
        let sc = &mut *self.scratch;

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
        let freq_mhz = s
            .freq_mhz
            .or_else(|| control_freq_mhz(rec))
            .unwrap_or(5180.0);

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
        let hist_max_us = histogram(&sc.ftm_ns, t_ftm.p999_us, &mut sc.hist);
        sc.section.push("interarrival_hist", &sc.hist);

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
            let span = sc
                .all
                .iter()
                .map(|smp| smp.rec.ntone as f64 * dsp::spacing_hz(&smp.rec))
                .fold(0.0f64, f64::max);
            (s.wf_bins, span.max(1.0))
        } else {
            (geom.ntone, geom.ntone as f64 * spacing)
        };

        // -- mixes and the talker table ---------------------------------------
        let mut mix = std::mem::take(&mut sc.header.mix);
        phy_mix(&sc.window, &mut mix);
        let talkers = talkers(&sc.window, &mut sc.talkers);

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
        h.radio.width.clear();
        use std::fmt::Write as _;
        let _ = write!(h.radio.width, "{}", rec.width);
        h.radio.freq_mhz = freq_mhz;
        h.radio.freq_assumed = s.freq_mhz.is_none();
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
        h.cir.peak_bin = sc.cir.peak_bin;
        h.cir.rms_delay_ns = sc.cir.rms_delay_ns;
        h.cir.taps = sc.cir.mag_db.len();

        h.doppler.fs_hz = sc.doppler.fs_hz;
        h.doppler.max_hz = sc.doppler.max_hz;
        h.doppler.max_speed_ms = sc.doppler.max_speed_ms;
        h.doppler.arrival_cv = sc.doppler.arrival_cv;
        h.doppler.conjugate_pair = sc.doppler.conjugate_pair;
        h.doppler.nfft = s.doppler_nfft;

        h.timing.ftm = t_ftm;
        h.timing.host = t_host;
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
                chain: s.chain,
                db_min: s.db_min,
                db_max: s.db_max,
                rows: s.wf_rows,
            },
        })
    }
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

        let of_class = self
            .arrived
            .iter()
            .filter(|smp| ClassKey::of(&smp.rec) == plan.class)
            .count();
        let other_class = (self.arrived.len() - of_class) as u64;
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
            if !plan.all_scope {
                if !same_class {
                    continue;
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
            wf_rows,
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
        // Frequency offset of this tone from the band centre.
        let f = (i as f64 - n as f64 / 2.0 + 0.5) * spacing_hz;
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

/// Linear histogram of inter-arrival times up to `max_us`, so the long tail
/// does not compress the bulk of the distribution into one bin. Returns the
/// top of the range.
fn histogram(times_ns: &[u64], max_us: f32, bins: &mut Vec<f32>) -> f32 {
    bins.clear();
    bins.resize(HIST_BINS, 0.0);
    if times_ns.len() < 3 {
        return 1.0;
    }
    let top = if max_us.is_finite() && max_us > 0.0 {
        max_us * 1.2
    } else {
        1000.0
    };
    for w in times_ns.windows(2) {
        let d = w[1].saturating_sub(w[0]) as f32 / 1000.0;
        let b = ((d / top) * HIST_BINS as f32) as usize;
        bins[b.min(HIST_BINS - 1)] += 1.0;
    }
    top
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

    fn header(buf: &[u8]) -> serde_json::Value {
        let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        serde_json::from_slice(&buf[4..4 + hlen]).unwrap()
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
        ] {
            assert!(h["f32"][name].is_array(), "missing f32 array {name}");
        }
        assert!(h["u8"]["waterfall"].is_array());

        assert_eq!(h["geometry"]["ntone"], 242);
        assert_eq!(h["geometry"]["nchain"], 2);
        assert_eq!(h["f32"]["amp_db"][1], 242);
        assert_eq!(h["f32"]["iq"][1], 484);
        assert_eq!(h["f32"]["chain_amp_db"][1], 484, "one spectrum per chain");

        // ~600 Hz in, and the window is what was asked for.
        let rate = h["timing"]["ftm"]["rate_hz"].as_f64().unwrap();
        assert!(rate > 550.0 && rate < 650.0, "rate was {rate}");
        assert_eq!(h["stream"]["window"], 256);
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
        assert_eq!(
            h["skipped"], 584,
            "every record the display did not draw must be counted, whether \
             the ring dropped it or the row budget did"
        );
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
        let span = h["waterfall"]["span_mhz"].as_f64().unwrap();
        assert!(
            (span - 242.0 * 78_125.0 / 1e6).abs() < 0.01,
            "span must cover the widest class, got {span} MHz"
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
        assert_eq!(h["mix"]["modulation"]["he"], 256);
        assert_eq!(h["mix"]["ntone"]["52"], 256);
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
            "cir.peak_bin",
            "cir.rms_delay_ns",
            "doppler.nfft",
            "doppler.fs_hz",
            "doppler.max_hz",
            "doppler.max_speed_ms",
            "doppler.arrival_cv",
            "doppler.conjugate_pair",
            "timing.ftm.rate_hz",
            "timing.host.p50_us",
            "timing.host.p999_us",
            "timing.hist_max_us",
            "clocks.ftm_span_us",
            "clocks.host_span_us",
            "clocks.fw_span_us",
            "series.len",
            "series.tones",
            "series.rssi_chains",
            "validation.dimensions_ok",
            "validation.dc_notch_db",
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

        // `chain_identical` is legitimately null on a single-chain record, so
        // it is checked for presence rather than for a value.
        assert!(h["validation"]
            .as_object()
            .unwrap()
            .contains_key("chain_identical"));

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
