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

use std::collections::HashMap;
use std::sync::Arc;

use csiq::CsiRecord;
use serde_json::json;

use crate::dsp;
use crate::frame::{quantise_db, Encoder, ViewSettings};
use crate::state::{Hub, Sample};

/// Upper bound on records entering the percentile bundle. Percentiles converge
/// long before the window does, and this keeps the per-frame cost flat.
const BUNDLE_COLUMNS: usize = 128;

/// Inter-arrival histogram resolution.
const HIST_BINS: usize = 48;

/// The identity of a **record class**: tone count and modulation family.
///
/// This is the load-bearing abstraction for real captures. `csid caps` puts it
/// plainly — *CSI type follows the received frame* — so an ambient stream on a
/// busy channel is not one signal but several interleaved ones: legacy 52-tone
/// beacons, HT 56-tone data, HE 242-tone bursts, all arriving in the same
/// second from different transmitters.
///
/// A console that renders "the newest record" therefore flickers between
/// incompatible geometries: the PHY label blinks, the spectrum changes width,
/// and the waterfall has no stable number of columns. Worse, a time series
/// built across the mix is meaningless, because consecutive samples describe
/// different measurements.
///
/// So every view is scoped to exactly one class. The operator picks it; the
/// default is whichever class dominates the window. The full mix stays visible
/// in its own panel, because *what else is on the channel* is real information.
fn class_of(rec: &CsiRecord) -> String {
    let modulation = match rec.phy.map(|p| p.modulation) {
        Some(m) => format!("{m:?}").to_lowercase(),
        None => "unlabelled".to_string(),
    };
    format!("{}:{}", rec.ntone, modulation)
}

/// Human label for a class key, e.g. `56-tone ht`.
fn class_label(key: &str) -> String {
    match key.split_once(':') {
        Some((tones, modulation)) => format!("{tones}-tone {modulation}"),
        None => key.to_string(),
    }
}

/// Per-connection analysis state: just where in the stream this client is.
///
/// `Copy` on purpose — the WebSocket loop hands it to a blocking task and takes
/// it back each frame, and a one-word cursor is not worth an `Option` dance.
#[derive(Debug, Clone, Copy)]
pub struct Analyzer {
    cursor: u64,
}

impl Analyzer {
    /// Start a client at the live edge, so it does not open on stale history.
    pub fn at_live_edge(hub: &Hub) -> Self {
        Analyzer {
            cursor: hub.total(),
        }
    }

    /// Build one frame. Returns `None` when nothing has been received yet.
    pub fn frame(&mut self, hub: &Hub, s: &ViewSettings) -> Option<Vec<u8>> {
        let all = hub.tail(s.window);
        if all.is_empty() {
            return None;
        }

        // Which record classes are on the channel, and which one are we
        // looking at? The requested class wins if it is still present;
        // otherwise fall back to the most common, so the console recovers on
        // its own when a transmitter goes quiet.
        let mut census: HashMap<String, u64> = HashMap::new();
        for smp in &all {
            *census.entry(class_of(&smp.rec)).or_default() += 1;
        }
        // Ties break on the key so the default class cannot oscillate between
        // two equally common PHY types frame to frame.
        let dominant = census
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(k, _)| k.clone())?;
        let class = match &s.class {
            Some(want) if census.contains_key(want) => want.clone(),
            _ => dominant.clone(),
        };

        // Every view below sees only this class. Mixing geometries into one
        // series would produce arrays of changing width and a time series
        // whose consecutive samples are not comparable.
        let window: Vec<Sample> = all
            .iter()
            .filter(|smp| class_of(&smp.rec) == class)
            .cloned()
            .collect();
        let latest = window.last()?.clone();
        let rec = &latest.rec;
        let geom = dsp::Geometry::of(rec);
        let nchain = geom.nchain();
        let chain_idx = s.chain.min(nchain.saturating_sub(1));

        // New records since this client's last frame, for the waterfall. Ask
        // for more than the row budget, because part of what arrives belongs
        // to other classes and will not be drawn.
        let (cursor, arrived, ring_skipped) = hub.since(self.cursor, s.wf_rows * 8);
        self.cursor = cursor;
        let mut fresh: Vec<Sample> = arrived
            .iter()
            .filter(|smp| class_of(&smp.rec) == class)
            .cloned()
            .collect();
        let other_class = (arrived.len() - fresh.len()) as u64;
        // Records of this class the row budget could not carry are skipped
        // just as surely as the ones the ring never handed over; both belong
        // in the same honest count.
        let over_budget = fresh.len().saturating_sub(s.wf_rows) as u64;
        if over_budget > 0 {
            fresh.drain(..over_budget as usize);
        }
        let skipped = ring_skipped + over_budget;

        let mut enc = Encoder::new();

        // -- the selected chain, right now ---------------------------------
        let h = dsp::chain(rec, chain_idx);
        let spacing = dsp::spacing_hz(rec);
        let amp = dsp::amp_db(&h);
        let phase_raw = dsp::phase(&h);
        let phase_unwrapped = dsp::unwrap(&phase_raw);
        let (phase_detrended, fit) = dsp::detrend(&phase_unwrapped, spacing);

        enc.f32s("amp_db", &amp);
        enc.f32s("phase_raw", &phase_raw);
        enc.f32s("phase_unwrapped", &phase_unwrapped);
        enc.f32s("phase_detrended", &phase_detrended);

        // Interleaved re/im for the complex-plane view.
        let mut iq = Vec::with_capacity(h.len() * 2);
        for c in &h {
            iq.push(c.re);
            iq.push(c.im);
        }
        enc.f32s("iq", &iq);

        // -- per-chain spectra, for the small-multiple comparison ----------
        let mut chain_amp = Vec::with_capacity(nchain * geom.ntone);
        for c in 0..nchain {
            let a = dsp::amp_db(&dsp::chain(rec, c));
            if a.len() == geom.ntone {
                chain_amp.extend_from_slice(&a);
            } else {
                chain_amp.extend(std::iter::repeat_n(f32::NAN, geom.ntone));
            }
        }
        enc.f32s("chain_amp_db", &chain_amp);

        // -- impulse response ----------------------------------------------
        let cir = dsp::cir(&h, spacing, s.cir_nfft, s.cir_taps);
        enc.f32s("cir_db", &cir.mag_db);

        // -- windowed views -------------------------------------------------
        let columns = decimate(&window, BUNDLE_COLUMNS)
            .iter()
            .map(|s| dsp::amp_db(&dsp::chain(&s.rec, chain_idx)))
            .filter(|a| a.len() == geom.ntone)
            .collect::<Vec<_>>();
        let bundle = dsp::bundle(&columns);
        if !bundle.p50.is_empty() {
            enc.f32s("bundle_p05", &bundle.p05);
            enc.f32s("bundle_p50", &bundle.p50);
            enc.f32s("bundle_p95", &bundle.p95);
        }

        // -- Doppler ---------------------------------------------------------
        let recs: Vec<Arc<CsiRecord>> = window.iter().map(|s| s.rec.clone()).collect();
        let ticks: Vec<u64> = window.iter().map(|s| s.ftm_ticks).collect();
        let chain_b = s.chain_b.filter(|&b| b < nchain && b != chain_idx);
        let series = dsp::doppler_series(&recs, &ticks, chain_idx, chain_b);
        let freq_mhz = s
            .freq_mhz
            .or_else(|| control_freq_mhz(rec))
            .unwrap_or(5180.0);
        let dop = dsp::doppler(&series, s.doppler_nfft, dsp::wavelength_m(freq_mhz));
        enc.f32s("doppler_db", &dop.power_db);

        // -- amplitude time series ------------------------------------------
        let tones = if s.series_tones.is_empty() {
            default_tones(geom.ntone)
        } else {
            s.series_tones
                .iter()
                .copied()
                .filter(|&t| t < geom.ntone)
                .collect()
        };
        let mut series_flat = Vec::with_capacity(tones.len() * window.len());
        for &t in &tones {
            for smp in &window {
                let h = dsp::chain(&smp.rec, chain_idx);
                series_flat.push(match h.get(t) {
                    Some(c) => 20.0 * c.norm().max(1e-6).log10(),
                    None => f32::NAN,
                });
            }
        }
        enc.f32s("tone_series", &series_flat);

        // -- RSSI, the only absolute amplitude anchor ------------------------
        let rssi_chains = rec.rssi.len().max(1);
        let mut rssi_flat = Vec::with_capacity(rssi_chains * window.len());
        for c in 0..rssi_chains {
            for smp in &window {
                rssi_flat.push(smp.rec.rssi.get(c).copied().unwrap_or(0) as f32);
            }
        }
        enc.f32s("rssi_series", &rssi_flat);

        // -- clocks ----------------------------------------------------------
        let clocks = clock_series(&window);
        enc.f32s("drift_host_us", &clocks.host_us);
        enc.f32s("drift_fw_us", &clocks.fw_us);

        // -- timing ----------------------------------------------------------
        let ftm_ns: Vec<u64> = window
            .iter()
            .map(|s| (s.ftm_ticks as f64 * 1e9 / csiq::FTM_HZ as f64) as u64)
            .collect();
        let host_ns: Vec<u64> = window.iter().map(|s| s.recv_ns).collect();
        let t_ftm = dsp::timing_ns(&ftm_ns);
        let t_host = dsp::timing_ns(&host_ns);
        let (hist, hist_max_us) = histogram(&ftm_ns, t_ftm.p999_us);
        enc.f32s("interarrival_hist", &hist);

        // -- waterfall --------------------------------------------------------
        //
        // Two scopes, because the waterfall answers two different questions.
        // Scoped to the class it is a measurement of one signal at its native
        // tone grid; scoped to all classes it is a picture of the channel, and
        // rows of different geometries are placed by frequency so they remain
        // comparable. Nothing is discarded in either mode — the difference is
        // only which records reach the display.
        let all_scope = s.wf_scope == "all";
        let (wf_bins, wf_span_hz) = if all_scope {
            let span = all
                .iter()
                .map(|smp| smp.rec.ntone as f64 * dsp::spacing_hz(&smp.rec))
                .fold(0.0f64, f64::max);
            (s.wf_bins, span.max(1.0))
        } else {
            (geom.ntone, geom.ntone as f64 * spacing)
        };

        let wf_source: &[Sample] = if all_scope { &arrived } else { &fresh };
        let mut wf = Vec::with_capacity(wf_source.len() * wf_bins);
        let mut wf_rows = 0usize;
        for smp in wf_source {
            let g = dsp::Geometry::of(&smp.rec);
            let c = s.chain.min(g.nchain().saturating_sub(1));
            let a = dsp::amp_db(&dsp::chain(&smp.rec, c));
            if a.is_empty() {
                continue;
            }
            if all_scope {
                let row = onto_shared_grid(&a, dsp::spacing_hz(&smp.rec), wf_span_hz, wf_bins);
                let floored: Vec<f32> = row
                    .iter()
                    .map(|v| if v.is_finite() { *v } else { s.db_min })
                    .collect();
                quantise_db(&floored, s.db_min, s.db_max, &mut wf);
            } else if a.len() == geom.ntone {
                quantise_db(&a, s.db_min, s.db_max, &mut wf);
            } else {
                continue;
            }
            wf_rows += 1;
        }
        enc.u8s("waterfall", &wf);

        // -- mixes and the talker table ---------------------------------------
        let mix = phy_mix(&window);
        let talkers = talkers(&window);

        let counters = &hub.counters;
        use std::sync::atomic::Ordering::Relaxed;

        let mut classes: Vec<_> = census
            .iter()
            .map(|(k, v)| {
                json!({
                    "key": k,
                    "label": class_label(k),
                    "count": v,
                    "share": *v as f64 / all.len() as f64,
                })
            })
            .collect();
        classes.sort_by(|a, b| {
            b["count"]
                .as_u64()
                .cmp(&a["count"].as_u64())
                .then_with(|| a["key"].as_str().cmp(&b["key"].as_str()))
        });

        let meta = json!({
            "t": "frame",
            "cursor": cursor,
            "skipped": skipped,
            "other_class": other_class,
            "wf_rows": wf_rows,
            "waterfall": {
                "scope": if all_scope { "all" } else { "class" },
                "bins": wf_bins,
                "span_mhz": wf_span_hz / 1e6,
            },
            "class": {
                "key": class,
                "label": class_label(&class),
                "pinned": s.class.is_some(),
                "share": census.get(&class).copied().unwrap_or(0) as f64 / all.len() as f64,
                "count": census.get(&class).copied().unwrap_or(0),
                "available": classes,
            },
            "geometry": {
                "ntone": geom.ntone,
                "nrx": geom.nrx,
                "ntx": geom.ntx,
                "nchain": nchain,
                "chain": chain_idx,
                "chain_b": chain_b,
                "chain_labels": (0..nchain).map(|c| geom.chain_label(c)).collect::<Vec<_>>(),
                "dimensions_ok": geom.matches(rec),
            },
            "radio": {
                "channel": rec.channel,
                "width": rec.width.to_string(),
                "freq_mhz": freq_mhz,
                "freq_assumed": s.freq_mhz.is_none(),
                "spacing_hz": spacing,
                "bw_mhz": dsp::occupied_bw_mhz(rec),
            },
            "record": {
                "session_uid": latest.session_uid,
                "seq": latest.seq,
                "ftm": rec.ftm,
                "ftm_ticks": latest.ftm_ticks,
                "us": rec.us,
                "unix_ts_ns": rec.unix_ts_ns,
                "recv_ns": latest.recv_ns,
                "rssi": rec.rssi,
                "src_mac": mac(&rec.src_mac),
                "rnf": rec.rnf,
                "phy": rec.phy.map(|p| json!({
                    "modulation": format!("{:?}", p.modulation).to_lowercase(),
                    "mcs": p.mcs,
                    "nss": p.nss,
                })),
            },
            "phase_fit": {
                "slope_rad_per_tone": fit.slope,
                "intercept_rad": fit.intercept,
                "tau_ns": fit.tau_ns,
            },
            "bundle": { "width_db": bundle.width_db, "n": bundle.n },
            "cir": {
                "bin_ns": cir.bin_ns,
                "peak_bin": cir.peak_bin,
                "rms_delay_ns": cir.rms_delay_ns,
                "taps": cir.mag_db.len(),
            },
            "doppler": {
                "fs_hz": dop.fs_hz,
                "max_hz": dop.max_hz,
                "max_speed_ms": dop.max_speed_ms,
                "arrival_cv": dop.arrival_cv,
                "conjugate_pair": dop.conjugate_pair,
                "nfft": s.doppler_nfft,
            },
            "timing": { "ftm": t_ftm, "host": t_host, "hist_max_us": hist_max_us },
            "clocks": {
                "host_span_us": clocks.host_span_us,
                "fw_span_us": clocks.fw_span_us,
                "ftm_span_us": clocks.ftm_span_us,
            },
            "series": { "tones": tones, "len": window.len(), "rssi_chains": rssi_chains },
            "validation": dsp::validate(rec),
            "mix": mix,
            "talkers": talkers,
            "stream": {
                "window": window.len(),
                "window_all": all.len(),
                "depth": hub.depth(),
                "total": cursor,
                "received": counters.received.load(Relaxed),
                "decode_errors": counters.decode_errors.load(Relaxed),
                "sender_gaps": counters.sender_gaps.load(Relaxed),
                "session_changes": counters.session_changes.load(Relaxed),
                "bytes": counters.bytes.load(Relaxed),
                "source": hub.source,
                "uptime_s": hub.started.elapsed().as_secs(),
            },
        });

        Some(enc.finish(meta))
    }
}

/// Resample one record's amplitude onto a shared frequency grid.
///
/// The grid spans `span_hz` centred on the channel's centre frequency, so a
/// 52-tone legacy row and a 242-tone HE row land on the *same frequencies*
/// rather than both being stretched to the full width. Bins the record does
/// not reach keep `None`, and the caller paints them at the floor — an HE20
/// burst genuinely occupies more of the channel than a legacy frame, and the
/// picture should show that.
fn onto_shared_grid(amp: &[f32], spacing_hz: f64, span_hz: f64, bins: usize) -> Vec<f32> {
    let mut out = vec![f32::NEG_INFINITY; bins];
    if amp.is_empty() || span_hz <= 0.0 {
        return out;
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
    out
}

/// Evenly spaced subset of `items`, always including the newest.
fn decimate(items: &[Sample], max: usize) -> Vec<Sample> {
    if items.len() <= max {
        return items.to_vec();
    }
    let step = items.len() as f64 / max as f64;
    (0..max)
        .map(|i| items[((i as f64 * step) as usize).min(items.len() - 1)].clone())
        .collect()
}

/// Three subcarriers spread across the band, used when the client has not
/// picked any.
///
/// Deliberately **not** including the band centre: 802.11 nulls the DC
/// subcarriers, so a centre tone traces the noise floor and looks like a
/// violently unstable channel next to its neighbours.
fn default_tones(ntone: usize) -> Vec<usize> {
    if ntone < 8 {
        return vec![0];
    }
    vec![ntone / 8, ntone / 3, ntone * 7 / 8]
}

struct Clocks {
    host_us: Vec<f32>,
    fw_us: Vec<f32>,
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
fn clock_series(window: &[Sample]) -> Clocks {
    let mut host_us = Vec::with_capacity(window.len());
    let mut fw_us = Vec::with_capacity(window.len());
    let (mut host_span, mut fw_span, mut ftm_span) = (0.0, 0.0, 0.0);

    if let Some(first) = window.first() {
        let t0 = first.ftm_ticks;
        let h0 = first.rec.unix_ts_ns;
        let f0 = first.rec.us;
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
        host_us,
        fw_us,
        host_span_us: host_span,
        fw_span_us: fw_span,
        ftm_span_us: ftm_span,
    }
}

/// Linear histogram of inter-arrival times up to `max_us`, so the long tail
/// does not compress the bulk of the distribution into one bin.
fn histogram(times_ns: &[u64], max_us: f32) -> (Vec<f32>, f32) {
    let mut bins = vec![0.0f32; HIST_BINS];
    if times_ns.len() < 3 {
        return (bins, 1.0);
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
    (bins, top)
}

/// Distribution of PHY labels, tone counts and widths over the window — the
/// live version of the "CSI mix" column `csid bench` reports per run.
fn phy_mix(window: &[Sample]) -> serde_json::Value {
    let mut modulation: HashMap<String, u64> = HashMap::new();
    let mut ntone: HashMap<String, u64> = HashMap::new();
    let mut nss: HashMap<String, u64> = HashMap::new();
    let mut mcs: HashMap<String, u64> = HashMap::new();
    let mut width: HashMap<String, u64> = HashMap::new();

    for s in window {
        let r = &s.rec;
        *ntone.entry(r.ntone.to_string()).or_default() += 1;
        *width.entry(r.width.to_string()).or_default() += 1;
        match r.phy {
            Some(p) => {
                *modulation
                    .entry(format!("{:?}", p.modulation).to_lowercase())
                    .or_default() += 1;
                *nss.entry(p.nss.to_string()).or_default() += 1;
                *mcs.entry(p.mcs.to_string()).or_default() += 1;
            }
            None => *modulation.entry("unlabelled".into()).or_default() += 1,
        }
    }
    json!({ "modulation": modulation, "ntone": ntone, "nss": nss, "mcs": mcs, "width": width })
}

/// Who is actually sounding the channel.
///
/// Ambient capture means the record rate is somebody else's transmit rate, so
/// "why is my rate low" is usually answered here rather than in the config. The
/// table also feeds the `radio.mac_filter` editor: pick a talker, pin the
/// capture to it.
fn talkers(window: &[Sample]) -> serde_json::Value {
    struct Acc {
        count: u64,
        rssi_sum: i64,
        rssi_n: u64,
        last_ns: u64,
        first_ticks: u64,
        last_ticks: u64,
    }
    let mut by_mac: HashMap<[u8; 6], Acc> = HashMap::new();
    for s in window {
        let e = by_mac.entry(s.rec.src_mac).or_insert(Acc {
            count: 0,
            rssi_sum: 0,
            rssi_n: 0,
            last_ns: 0,
            first_ticks: s.ftm_ticks,
            last_ticks: s.ftm_ticks,
        });
        e.count += 1;
        if let Some(&r) = s.rec.rssi.first() {
            e.rssi_sum += r as i64;
            e.rssi_n += 1;
        }
        e.last_ns = e.last_ns.max(s.recv_ns);
        e.last_ticks = e.last_ticks.max(s.ftm_ticks);
    }

    let mut rows: Vec<_> = by_mac
        .into_iter()
        .map(|(m, a)| {
            let span = csiq::ftm_to_seconds(a.last_ticks.saturating_sub(a.first_ticks));
            json!({
                "mac": mac(&m),
                "count": a.count,
                "rate_hz": if span > 0.0 { a.count as f64 / span } else { 0.0 },
                "rssi": if a.rssi_n > 0 { Some(a.rssi_sum as f64 / a.rssi_n as f64) } else { None },
                "last_ns": a.last_ns,
            })
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r["count"].as_u64().unwrap_or(0)));
    rows.truncate(12);
    json!(rows)
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

fn mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
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
                iq.push((100 + t as i16 + c as i16 * 5) as i16);
                iq.push((t as i16 % 7) as i16);
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
        a.cursor = 0; // pretend the client has drawn nothing yet
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
        a.cursor = 0;

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
        a.cursor = 0;
        let h = header(&a.frame(&hub, &ViewSettings::default()).unwrap());
        assert_eq!(h["waterfall"]["scope"], "class");
        assert_eq!(h["waterfall"]["bins"], 52);
        assert_eq!(h["u8"]["waterfall"][1].as_u64().unwrap() % 52, 0);
        assert!(h["other_class"].as_u64().unwrap() > 0);

        // All classes: one fixed-width grid spanning the widest occupancy,
        // and every arrived record drawn rather than two thirds of them.
        let mut a = Analyzer::at_live_edge(&hub);
        a.cursor = 0;
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
        let row = onto_shared_grid(&amp, 312_500.0, 10e6, 100);
        let occupied: Vec<usize> = row
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_finite())
            .map(|(i, _)| i)
            .collect();
        assert!(!occupied.is_empty());
        // Centred: the occupied span sits around the middle quarter.
        assert!(*occupied.first().unwrap() >= 37, "{occupied:?}");
        assert!(*occupied.last().unwrap() <= 62, "{occupied:?}");

        // A row as wide as the grid fills it end to end.
        let wide: Vec<f32> = vec![40.0; 32];
        let row = onto_shared_grid(&wide, 312_500.0, 10e6, 100);
        assert!(row.iter().filter(|v| v.is_finite()).count() >= 95);
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

    #[test]
    fn decimation_keeps_the_newest_sample() {
        let items: Vec<Sample> = (0..1000).map(|i| sample(i, 52, 1)).collect();
        let d = decimate(&items, 10);
        assert_eq!(d.len(), 10);
        assert_eq!(d[0].seq, 0);
        assert!(d.windows(2).all(|w| w[0].seq < w[1].seq));
    }
}
