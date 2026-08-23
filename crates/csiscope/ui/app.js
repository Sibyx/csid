// csiscope console.
//
// Three responsibilities, kept apart:
//   1. the live socket — decode binary frames, keep the settings in sync
//   2. the views       — one render function per panel, all reading one frame
//   3. the node tab    — a read-only description of the host, over REST
//
// Every view states its units and its assumptions on the panel itself. A CSI
// plot with an unlabelled axis is decoration; this is an instrument.
//
// One rule governs every readout below, and most of the bugs this file has
// carried were a breach of it: **a panel whose input does not support its
// statistic prints the reason, not the statistic**. A deficit against a rate
// nobody commanded, a percentile resting on a null, a velocity axis derived
// from a packet rate that moved — each of those was a number rendered where a
// sentence belonged.

import { Panel, Heatmap, drawRampLegend, ramp, rampColor, CATEGORICAL, THEME } from '/plot.js';

const $ = (id) => document.getElementById(id);
const el = (sel, root = document) => root.querySelector(sel);

/** Set a readout's short text and keep the long version in its tooltip. */
function readout(id, html, tip) {
  const n = $(id);
  n.innerHTML = html;
  n.title = tip || n.textContent;
}

/**
 * Blank an analytical panel when the window held no measurement at all.
 *
 * Returns true when the caller must stop. Every record in the window carried an
 * all-zero I/Q matrix, so the arrays behind these views are the quantisation
 * floor — and a flat line at the floor reads as "the channel is dead" rather
 * than "the driver reported nothing". The band-plan panel has always handled
 * an inapplicable input this way; this is the same move, applied to the rest.
 */
function blanked(panel, readoutId, h, what) {
  if (!h.nulls || !h.nulls.no_measurement) return false;
  panel.begin({ x0: 0, x1: 1, y0: 0, y1: 1, xticks: 1, yticks: 1 });
  panel.note('no channel estimate in this window', 'tl');
  panel.end();
  readout(readoutId, '<span class="bad">no measurement</span>',
    `Every one of the ${h.nulls.considered} records in this window arrived with ` +
    'an all-zero I/Q matrix: the radio delivered them, the headers are intact ' +
    `and none of them measured the channel. There is nothing for ${what} to ` +
    'describe. Check the driver — this is not a quiet room.');
  return true;
}

// ---------------------------------------------------------------- utilities

function toast(text, bad = false) {
  const d = document.createElement('div');
  d.textContent = text;
  if (bad) d.className = 'bad';
  $('toast').appendChild(d);
  setTimeout(() => d.remove(), bad ? 8000 : 3500);
}

function fmt(v, digits = 1) {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—';
  return v.toFixed(digits);
}

function bytes(n) {
  if (!n) return '0';
  const u = ['B', 'kB', 'MB', 'GB', 'TB'];
  const i = Math.min(u.length - 1, Math.floor(Math.log10(n) / 3));
  return (n / 1000 ** i).toFixed(i ? 1 : 0) + ' ' + u[i];
}

function table(node, columns, rows, opts = {}) {
  const head = `<thead><tr>${columns
    .map((c) => `<th class="${c.num ? 'num' : ''}">${c.label}</th>`)
    .join('')}</tr></thead>`;
  const body = rows
    .map((r, i) => {
      const cls = [opts.click ? 'click' : '', opts.selected === i ? 'sel' : ''].join(' ').trim();
      const cells = columns
        .map((c) => {
          const v = c.get(r, i);
          const klass = [c.num ? 'num' : '', c.mono ? 'mono' : '', c.cls ? c.cls(r) : '']
            .join(' ').trim();
          return `<td class="${klass}">${v === undefined || v === null ? '—' : v}</td>`;
        })
        .join('');
      return `<tr data-i="${i}" class="${cls}">${cells}</tr>`;
    })
    .join('');
  node.innerHTML = head + `<tbody>${body}</tbody>`;
  if (opts.click) {
    node.querySelectorAll('tbody tr').forEach((tr) => {
      tr.addEventListener('click', (ev) => {
        if (ev.target.tagName === 'BUTTON') return;
        opts.click(rows[+tr.dataset.i], +tr.dataset.i);
      });
    });
  }
  return node;
}

/** Two-column key/value table — the console's default for any small record. */
function kv(node, pairs) {
  node.innerHTML = pairs
    .map(([k, v, cls]) =>
      `<tr><td class="dim">${k}</td>` +
      `<td class="mono clip ${cls || ''}" title="${String(v).replace(/<[^>]*>/g, '')}">${v}</td></tr>`)
    .join('');
}

async function api(path, opts) {
  const r = await fetch(path, opts);
  const body = await r.json().catch(() => ({ error: `${r.status} ${r.statusText}` }));
  if (!r.ok) throw new Error(body.error || `${r.status}`);
  return body;
}

// ------------------------------------------------------------ frame decoding

/**
 * Decode one binary frame into `{h, a, b}`: the JSON header, the named f32
 * arrays and the named u8 arrays. Arrays are zero-copy views over the received
 * buffer — see `frame.rs` for the layout.
 */
function decodeFrame(buf) {
  const dv = new DataView(buf);
  const hlen = dv.getUint32(0, true);
  const h = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 4, hlen)));
  const base = 4 + hlen;
  const f32 = new Float32Array(buf, base, h.n_f32);
  const u8 = new Uint8Array(buf, base + h.n_f32 * 4, h.n_u8);
  const a = {}, b = {};
  for (const [k, [o, l]] of Object.entries(h.f32)) a[k] = f32.subarray(o, o + l);
  for (const [k, [o, l]] of Object.entries(h.u8)) b[k] = u8.subarray(o, o + l);
  return { h, a, b };
}

// ------------------------------------------------------------------- state

const S = {
  settings: null,          // authoritative copy, echoed by the server
  frame: null,             // last decoded frame
  overview: null,
  ramp: 'viridis',
  // Doppler frequency axis the spectrogram image was drawn on. When the server
  // moves the pinned rate the image is started again, because two rates in one
  // picture is the fault the pinning exists to remove.
  dopplerAxis: null,
  phaseMode: 'detrended',
  ws: null,
  lastFrameAt: 0,
  wfWidth: null,          // tone count the waterfall buffer is sized for
  scaledFor: null,        // geometry the waterfall colour window was fitted to
  manualScale: false,     // the operator moved a dB slider; stop refitting
  renderErrorShown: false,
};

// ------------------------------------------------------------------- panels

const panels = {};
const heat = {};

function setupPanels() {
  for (const id of ['spectrum', 'phase', 'cir', 'constellation', 'chains',
                    'tones', 'rssi', 'jitter', 'clocks',
                    'metronome', 'bandplan', 'ratio', 'tonestats']) {
    panels[id] = new Panel(el('canvas', $(`p-${id}`)));
  }
  // Depth is history length, in lines. The waterfall gets one line per record
  // carried, so 700 is a couple of seconds; the spectrogram gets one column
  // per frame, so 400 at 20 fps is twenty seconds — long enough to watch a
  // person cross a room, short enough to fill quickly after a reconnect.
  heat.waterfall = new Heatmap(el('canvas', $('p-waterfall')), 'row', 700);
  heat.doppler = new Heatmap(el('canvas', $('p-doppler')), 'column', 400);
}

/** Percentile of a typed array, without mutating it. */
function pct(arr, q) {
  const a = Array.from(arr).filter(Number.isFinite).sort((x, y) => x - y);
  if (!a.length) return 0;
  return a[Math.min(a.length - 1, Math.round((a.length - 1) * q))];
}

/**
 * Percentile over the subcarriers that actually carry a measurement.
 *
 * 802.11 nulls the DC and guard subcarriers and the driver delivers them as
 * exact zeros, which reach the console as 0 dB — the quantisation floor. On a
 * real 56-tone capture that is roughly one subcarrier in eight, so a plain p05
 * lands on the floor and every automatic axis stretches to include a value
 * that is not a signal.
 */
const FLOOR_DB = 1.0;
function pctSignal(arr, q) {
  const a = Array.from(arr).filter((v) => Number.isFinite(v) && v > FLOOR_DB)
    .sort((x, y) => x - y);
  if (!a.length) return pct(arr, q);
  return a[Math.min(a.length - 1, Math.round((a.length - 1) * q))];
}

// -- waterfall ----------------------------------------------------------------

function renderWaterfall(f) {
  const { h, b } = f;
  const n = h.waterfall.bins;
  const all = h.waterfall.scope === 'all';
  // Bin count changes when the scope or the class does; the buffer holds rows
  // of a fixed width, so it has to start over rather than tear.
  if (S.wfWidth !== n) { S.wfWidth = n; heat.waterfall.clear(); }
  heat.waterfall.setRamp(S.ramp);
  heat.waterfall.push(b.waterfall, h.wf_rows, n);
  heat.waterfall.render();

  drawRampLegend(el('canvas.legend', $('p-waterfall')), S.ramp,
    S.settings.db_min, S.settings.db_max, 'dB');
  el('.sub', $('p-waterfall')).textContent = all
    ? '|H| in dB · frequency across the channel (x) × time (y, newest at top) · all classes'
    : '|H| in dB · subcarrier (x) × time (y, newest at top)';

  // The waterfall cannot show 600 lines a second at 20 frames a second; saying
  // how much it skipped is the difference between a display and a claim.
  //
  // Nothing arriving is its own state, and it is NOT full coverage. The
  // fallback used to be 100%, so a stalled stream read "0 rows/frame · 100%
  // drawn" — the healthiest possible sentence at the moment there was nothing
  // to draw.
  const shown = h.wf_rows, skipped = h.skipped, empty = h.empty || 0;
  const offered = shown + skipped;
  const cov = offered > 0 ? (100 * shown) / offered : null;
  const coverage = cov === null
    ? (empty > 0 ? `<span class="warn">${empty} empty</span>` : '<span class="dim">no rows</span>')
    : `${shown} rows/frame · ${cov.toFixed(0)}% drawn`;

  readout('r-waterfall',
    (all ? `all classes · ${fmt(h.waterfall.span_mhz, 1)} MHz`
         : `${n} tones · ${h.class.label}`) + ` · ${coverage}`,
    (all
      ? `Every record class, resampled onto one ${fmt(h.waterfall.span_mhz, 1)} MHz ` +
        `axis of ${n} bins, so a narrow legacy frame occupies its true share of ` +
        `the channel next to a wider HE one. `
      : `${n} subcarriers of the ${h.class.label} class at its native grid. `) +
    (offered > 0
      ? `${shown} rows carried this frame, ${skipped} skipped ` +
        `(${cov.toFixed(0)}% of the stream reaches the display). Raise "waterfall ` +
        'rows/frame" or lower the frame rate to draw more. '
      : 'No rows of this class and transmitter arrived in this frame, so nothing ' +
        'was drawn — which is a different state from drawing everything. ') +
    (empty > 0
      ? `${empty} record${empty === 1 ? '' : 's'} arrived with an all-zero I/Q ` +
        'matrix and had nothing to draw. Those are not skipped rows: the display ' +
        'kept up, the records were empty. '
      : '') +
    'The capture is unaffected — csiscope never touches it.');
}

// -- spectrum + bundle --------------------------------------------------------

function renderSpectrum(f) {
  const { h, a } = f;
  const p = panels.spectrum;
  if (blanked(p, 'r-spectrum', f.h, 'the spectrum')) return;
  const amp = a.amp_db;
  if (!amp || !amp.length) return;

  // Range from the data. The waterfall's dB window is a colour mapping, not
  // an axis, and tying this plot to it flattens the trace whenever the two
  // disagree.
  const src = a.bundle_p05 ? a.bundle_p05 : amp;
  const lo = Math.min(pctSignal(src, 0.02), pctSignal(amp, 0.02)) - 3;
  const hi = Math.max(pctSignal(a.bundle_p95 || amp, 0.98), pctSignal(amp, 0.98)) + 3;

  p.begin({
    x0: 0, x1: amp.length - 1, y0: lo, y1: hi,
    xfmt: (v) => v.toFixed(0),
    yfmt: (v) => v.toFixed(0),
  });
  if (a.bundle_p05) {
    // The envelope is the "CSI bundle": its width is the crowd-sensing feature.
    p.band(a.bundle_p05, a.bundle_p95, 'rgba(86,180,233,0.16)');
    p.seriesY(a.bundle_p50, THEME.MUTED, 1);
  }
  p.seriesY(amp, CATEGORICAL[0], 1.5);
  p.note('subcarrier index →', 'bl');
  // AGC-RELATIVE, not AGC-normalised. The label used to claim a correction that
  // is not applied anywhere in the pipeline: |H| carries the receiver's own gain
  // setting, which moves several dB between packets on an unchanged channel
  // (Xie et al. 2015 measured 7 dB between two traces of one band). An operator
  // who reads "normalised" believes amplitude is comparable across packets.
  p.note('|H| dB (AGC-relative — read shape, not level)', 'tl');
  p.end();

  const n = h.nulls || {};
  const dropped = n.dropped
    ? ` ${n.dropped} of ${n.considered} records carried no channel estimate and are not in it.`
    : '';
  readout('r-spectrum', `bundle ${fmt(h.bundle.width_db)} dB`,
    `The shaded envelope is the p05–p95 spread of |H(f)| over ${h.bundle.n} ` +
    `records. Its mean width, ${fmt(h.bundle.width_db)} dB, is the "CSI bundle ` +
    `width" used as an occupancy feature: a static channel gives a tight ` +
    `bundle, motion widens it.${dropped} ` +
    'The level is AGC-relative: the shape across subcarriers is the measurement, ' +
    'the height above the axis is not.');
}

// -- phase --------------------------------------------------------------------

function renderPhase(f) {
  const { h, a } = f;
  const p = panels.phase;
  if (blanked(p, 'r-phase', f.h, 'the phase')) return;
  const key = { raw: 'phase_raw', unwrapped: 'phase_unwrapped', detrended: 'phase_detrended' }[S.phaseMode];
  const y = a[key];
  if (!y || !y.length) return;

  let lo, hi;
  if (S.phaseMode === 'raw') { lo = -Math.PI; hi = Math.PI; }
  else {
    const m = Math.max(Math.abs(pct(y, 0.01)), Math.abs(pct(y, 0.99)), 0.2);
    lo = -m * 1.15; hi = m * 1.15;
  }

  p.begin({ x0: 0, x1: y.length - 1, y0: lo, y1: hi, yfmt: (v) => v.toFixed(1) });
  if (S.phaseMode !== 'raw') p.rule('y', 0, THEME.AXIS, '');
  p.seriesY(y, CATEGORICAL[0], 1.4);
  p.note('subcarrier index →', 'bl');
  p.note('phase, radians', 'tl');
  p.end();

  const explain = {
    raw: 'Raw phase is wrapped into (−π, π] and is dominated by carrier and ' +
         'sampling frequency offsets, not by the channel. It is shown to make ' +
         'that concrete, not to be read.',
    unwrapped: 'Unwrapped along the subcarrier axis. The slope is the residual ' +
               'delay — time of flight plus the receiver sampling-time offset.',
    detrended: 'A least-squares line across the band has been subtracted, which ' +
               'removes the constant (CFO) and linear-in-subcarrier (SFO/STO) ' +
               'terms. Any genuinely linear part of the channel goes with it.',
  }[S.phaseMode];
  readout('r-phase', `τ ${fmt(h.phase_fit.tau_ns, 1)} ns · ${S.phaseMode}`, explain);
}

// -- impulse response ---------------------------------------------------------

function renderCir(f) {
  const { h, a } = f;
  const p = panels.cir;
  const y = a.cir_db;
  if (!y || !y.length) return;
  if (blanked(p, 'r-cir', h, 'the impulse response')) return;

  // The axis is centred on the strongest tap and runs negative to positive.
  // Absolute delay is not recoverable here: the radio starts its transform at a
  // packet boundary it detected, and that detection moves packet to packet, so
  // the profile slid across this panel while the room did nothing. Measured
  // 2026-08-23, the peak swept the whole 0–127 tap window on two nodes.
  const binNs = h.cir.bin_ns;
  const startNs = h.cir.axis_start_ns;
  const endNs = startNs + (y.length - 1) * binNs;
  const resNs = h.cir.resolution_ns;

  p.begin({
    x0: startNs, x1: endNs, y0: -45, y1: 2,
    xfmt: (v) => v.toFixed(0),
    yfmt: (v) => v.toFixed(0),
  });

  // The resolution cell, drawn rather than only stated. A profile plotted at
  // `bin_ns` looks like it resolves forty times more structure than the
  // bandwidth can distinguish; this is the interval two paths must exceed to be
  // two paths at all (Xie et al. 2015).
  if (resNs > 0 && resNs < (endNs - startNs)) {
    const ctx = p.ctx;
    ctx.save();
    ctx.fillStyle = 'rgba(125,135,148,0.16)';
    const x0 = p.px(-resNs / 2), x1 = p.px(resNs / 2);
    ctx.fillRect(x0, p.pad.t, x1 - x0, p.h - p.pad.t - p.pad.b);
    ctx.restore();
  }

  const xs = Array.from({ length: y.length }, (_, i) => startNs + i * binNs);
  p.seriesY(y, CATEGORICAL[0], 1.3, xs);
  p.rule('y', -20, THEME.AXIS, '−20 dB');
  p.note('delay from the strongest tap, ns →', 'bl');
  p.note('power, dB below the strongest tap', 'tl');
  // Top-right, not bottom-right: the x-axis label already owns the bottom of
  // this panel and the two collided.
  if (resNs > 0) p.note(`shaded = 1/B = ${fmt(resNs)} ns, one resolution cell`, 'tr');
  p.end();

  // The spread is only a measurement while it exceeds what the bandwidth can
  // distinguish. Below that it describes the Hann window, and printing it as a
  // channel property is how a still room reports 35 ns one second and 57 ns the
  // next.
  const spread = h.cir.spread_resolvable
    ? `rms spread ${fmt(h.cir.rms_delay_ns)} ns`
    : `<span class="dim">spread below 1/B</span>`;
  readout('r-cir', `${spread} · 1/B ${fmt(resNs)} ns`,
    (h.cir.spread_resolvable
      ? `RMS delay spread ${fmt(h.cir.rms_delay_ns)} ns = ` +
        `${fmt(h.cir.rms_delay_ns * 0.2998)} m of excess path, over taps within ` +
        '20 dB of the peak. '
      : `The profile is one resolution cell wide (spread ${fmt(h.cir.rms_delay_ns)} ns ` +
        `against a resolution of ${fmt(resNs)} ns), so its width is the window ` +
        'function rather than the channel and no number is quoted. ') +
    `Resolution is 1/B = ${fmt(resNs)} ns ≈ ${fmt(resNs * 0.2998)} m of path: two ` +
    'reflections closer together than that are one tap however smooth the curve ' +
    `looks. Bin spacing ${fmt(binNs, 2)} ns is zero-padding interpolation between ` +
    'those taps, not extra detail. ' +
    `The peak sat at transform bin ${h.cir.peak_bin} — the packet-detection ` +
    'delay, which is why the axis is relative rather than absolute.');
}

// -- complex plane ------------------------------------------------------------

function renderConstellation(f) {
  const { a } = f;
  const p = panels.constellation;
  if (blanked(p, 'r-constellation', f.h, 'the complex plane')) return;
  const iq = a.iq;
  if (!iq || iq.length < 4) return;

  const n = iq.length / 2;
  const xs = new Float32Array(n), ys = new Float32Array(n);
  let m = 1;
  for (let i = 0; i < n; i++) {
    xs[i] = iq[2 * i];
    ys[i] = iq[2 * i + 1];
    m = Math.max(m, Math.abs(xs[i]), Math.abs(ys[i]));
  }
  m *= 1.1;

  const lut = ramp(S.ramp);
  p.begin({ x0: -m, x1: m, y0: -m, y1: m, xticks: 4, yticks: 4 });
  p.rule('x', 0, THEME.AXIS, '');
  p.rule('y', 0, THEME.AXIS, '');
  p.scatter(xs, ys, (i) => rampColor(lut, i / Math.max(1, n - 1)), 1.5);
  p.note('I →', 'bl');
  p.note('Q ↑ · colour = subcarrier', 'tl');
  p.end();

  readout('r-constellation', `${n} subcarriers`,
    'Each point is one subcarrier\'s complex channel coefficient. A clean ' +
    'single-path channel traces a circle; multipath folds it, and the radius ' +
    'is |H| — shape only, since the AGC has normalised the absolute scale.');
}

// -- per-chain small multiples ------------------------------------------------

function renderChains(f) {
  const { h, a } = f;
  const p = panels.chains;
  if (blanked(p, 'r-chains', f.h, 'the per-chain view')) return;
  const n = h.geometry.ntone;
  const nc = h.geometry.nchain;
  const flat = a.chain_amp_db;
  if (!flat || !n) return;

  const lo = pctSignal(flat, 0.02) - 2, hi = pctSignal(flat, 0.99) + 2;
  p.begin({ x0: 0, x1: n - 1, y0: lo, y1: hi, yfmt: (v) => v.toFixed(0) });
  for (let c = 0; c < nc; c++) {
    p.seriesY(flat.subarray(c * n, (c + 1) * n), CATEGORICAL[c % CATEGORICAL.length], 1.1);
  }
  p.note('subcarrier index →', 'bl');
  p.end();

  $('r-chains').innerHTML = (h.geometry.chain_labels || [])
    .map((l, i) => `<span style="color:${CATEGORICAL[i % CATEGORICAL.length]}">${l}</span>`)
    .join(' ');
}

// -- subcarrier time series ---------------------------------------------------

function renderTones(f) {
  const { h, a } = f;
  const p = panels.tones;
  if (blanked(p, 'r-tones', f.h, 'the time series')) return;
  const len = h.series.len;
  const tones = h.series.tones || [];
  const flat = a.tone_series;
  if (!flat || !len || !tones.length) return;

  const lo = pctSignal(flat, 0.01) - 2, hi = pctSignal(flat, 0.99) + 2;
  p.begin({ x0: 0, x1: len - 1, y0: lo, y1: hi, yfmt: (v) => v.toFixed(0) });
  tones.forEach((_, i) => {
    p.seriesY(flat.subarray(i * len, (i + 1) * len), CATEGORICAL[i % CATEGORICAL.length], 1.2);
  });
  p.note('← older    records    newer →', 'bl');
  p.note('|H| dB', 'tl');
  p.end();

  $('r-tones').innerHTML = tones
    .map((t, i) => `<span style="color:${CATEGORICAL[i % CATEGORICAL.length]}">#${t}</span>`)
    .join(' ');
}

// -- RSSI ---------------------------------------------------------------------

function renderRssi(f) {
  const { h, a } = f;
  const p = panels.rssi;
  const len = h.series.len;
  const nc = h.series.rssi_chains;
  const flat = a.rssi_series;
  if (!flat || !len) return;

  // Records carry dBm (csiq negates the driver's magnitude at parse time), so
  // the axis is a real signal-strength scale. Ranged from the data but held to
  // a sane minimum span, or a steady link would be drawn as dramatic noise
  // across a 2 dB window.
  const lo = Math.min(pct(flat, 0.02) - 3, -40);
  const hi = Math.max(pct(flat, 0.98) + 3, -30);
  p.begin({ x0: 0, x1: len - 1, y0: lo, y1: hi, yfmt: (v) => v.toFixed(0) });
  for (let c = 0; c < nc; c++) {
    p.seriesY(flat.subarray(c * len, (c + 1) * len), CATEGORICAL[c % CATEGORICAL.length], 1.2);
  }
  p.note('← older    records    newer →', 'bl');
  p.note('dBm', 'tl');
  p.end();

  readout('r-rssi', (h.record.rssi || []).map((v) => `${v} dBm`).join(' / ') || '—',
    'Per-chain RSSI in dBm. The driver reports a positive magnitude and csiq ' +
    'negates it at parse time, so records carry ordinary signal strength. This ' +
    'is the only absolute amplitude reference available: |H| is AGC-relative ' +
    'and carries channel shape alone. A zero means the chain reported nothing.');
}

// -- Doppler ------------------------------------------------------------------

const DOPPLER_FLOOR = -45;

function renderDoppler(f) {
  const { h, a } = f;
  const d = h.doppler;
  const y = a.doppler_db;
  if (!y || !y.length) return;
  // The spectrogram is a heatmap, not a Panel, so it blanks by clearing its own
  // image rather than by drawing an empty axis.
  if (h.nulls && h.nulls.no_measurement) {
    heat.doppler.clear();
    S.dopplerAxis = null;
    readout('r-doppler', '<span class="bad">no measurement</span>',
      `Every one of the ${h.nulls.considered} records in this window arrived with ` +
      'an all-zero I/Q matrix, so there is no series to transform. Check the ' +
      'driver — this is not a still room.');
    return;
  }

  // The spectrogram is a SEQUENCE of columns on ONE axis, so a column whose
  // axis differs from the image's must not be appended to it. The server pins
  // the rate for exactly this reason; the guard here catches the moment it
  // moves — a genuine change of regime — and starts a fresh image rather than
  // splicing two scales into one picture.
  //
  // Before this, each column was an FFT over its own window's mean packet rate.
  // Measured 2026-08-23, that rate moved 2.3x within fifteen seconds, so bins
  // 0.07 Hz wide sat beside bins 1.0 Hz wide under a single axis label.
  if (S.dopplerAxis !== d.fs_hz) {
    heat.doppler.clear();
    S.dopplerAxis = d.fs_hz;
  }

  // One column per frame, quantised over a fixed dynamic range so the colour
  // means the same thing from frame to frame.
  const col = new Uint8Array(y.length);
  for (let i = 0; i < y.length; i++) {
    // Flip so positive Doppler is at the top of the panel.
    const v = y[y.length - 1 - i];
    col[i] = Math.max(0, Math.min(255, Math.round(((v - DOPPLER_FLOOR) / -DOPPLER_FLOOR) * 255)));
  }
  heat.doppler.setRamp(S.ramp);
  heat.doppler.push(col, 1, y.length);
  heat.doppler.render();

  drawRampLegend(el('canvas.legend', $('p-doppler')), S.ramp,
    DOPPLER_FLOOR, 0, 'dB rel. peak');

  // Thresholds are set by what nearest-neighbour resampling survives: a
  // perfectly regular stream has CV 0, bursty ambient traffic sits near 0.6,
  // and pure Poisson arrivals reach 1.0, where the frequency axis stops being
  // quantitative.
  const m = h.metronome;
  // A metronomic source that loses whole slots has a high CV and a perfectly
  // usable axis: every surviving arrival is on grid, so nearest-neighbour
  // resampling only has to fill the missing slots. Measured 2026-08-17, the
  // 2.4 GHz injector read CV 0.71 while being exactly on a 10 ms grid. So the
  // metronome verdict overrides the CV whenever there is one.
  const mv = metronomeVerdict(m);
  const quantised = mv === 'on grid';
  const gappy = d.gap_frac > 0.5;
  const trust = gappy ? 'bad'
    : quantised ? 'ok'
    : mv === 'deferred' ? 'warn'
    : d.arrival_cv > 1.2 ? 'bad' : d.arrival_cv > 0.5 ? 'warn' : 'ok';

  // The speed axis is a headline only while it is a measurement. On an axis the
  // sampling cannot support it is a number people quote, so it moves into the
  // tooltip and the panel leads with the reason instead.
  // A speed is a frequency times a shift. When nobody agrees on the frequency
  // there is no speed to quote, whatever the sampling did.
  const quantitative = trust === 'ok' && h.radio.freq_trusted;
  const speed = quantitative
    ? `±${fmt(d.max_speed_ms, 2)} m/s`
    : '<span class="dim">speed not quantitative</span>';

  const flags = [];
  if (!d.conjugate_pair) flags.push('<span class="warn">CFO</span>');
  if (h.radio.freq_assumed) flags.push('<span class="warn">f assumed</span>');
  if (!h.radio.freq_trusted) flags.push('<span class="bad">channel disputed</span>');
  if (d.gap_frac > 0.02) {
    flags.push(`<span class="${gappy ? 'bad' : 'warn'}">${(100 * d.gap_frac).toFixed(0)}% fill</span>`);
  }

  readout('r-doppler',
    `±${fmt(d.max_hz, 0)} Hz · ${speed} · ` +
    `<span class="${trust}">CV ${fmt(d.arrival_cv, 2)}</span>` +
    (flags.length ? ' · ' + flags.join(' ') : ''),
    `Unambiguous range ±${fmt(d.max_hz, 0)} Hz, i.e. radial speeds up to ` +
    `±${fmt(d.max_speed_ms, 2)} m/s at ${fmt(h.radio.freq_mhz, 0)} MHz ` +
    `(v = λ·f_D/2). ` +
    `The axis is pinned at ${fmt(d.fs_hz, 0)} Hz and held across columns, so the ` +
    'left of the image can be compared with the right; this window actually ' +
    `delivered ${fmt(d.fs_window_hz, 0)} Hz. Each column is a ` +
    `${d.nfft}-point transform over ${fmt(d.span_s, 1)} s. ` +
    (d.gap_frac > 0.02
      ? `${(100 * d.gap_frac).toFixed(0)}% of the resample grid had no packet near ` +
        'it and was filled with the window mean, which contributes nothing — so ' +
        'that share of the column is fill rather than evidence. '
      : '') +
    `Packet arrivals have CV ${fmt(d.arrival_cv, 2)}. ` +
    (quantised
      ? `The metronome panel puts this transmitter on a ${fmt(m.slot_us / 1000, 2)} ms ` +
        'grid, so despite that CV every surviving arrival is on grid and the ' +
        'resampling only has to fill missing slots. The axis is quantitative.'
      : mv === 'deferred'
      ? `The metronome panel finds a ${fmt(m.slot_us / 1000, 2)} ms slot with ` +
        `${(100 * m.off_slot).toFixed(0)}% of arrivals deferred off it. The ` +
        'dropped slots resample cleanly; the deferred ones do not, so read the ' +
        'axis as approximate rather than either exact or worthless.'
      : 'The axis assumes even sampling, so above ~1.2 read this as a picture ' +
        'rather than a measurement — throttle with radio.interval_us for a ' +
        'regular rate.') +
    (d.conjugate_pair ? '' :
      ' No second chain, so the series still carries CFO and the line smears.') +
    (h.radio.freq_assumed ?
      ' Centre frequency inferred from the channel number; pin it in the rail.' : '') +
    (h.radio.freq_trusted ? '' :
      ` The records carry channel ${h.radio.channel} and csid says the radio is ` +
      `tuned to channel ${h.radio.tuned_channel}. The speed axis is a function of ` +
      'the centre frequency, so no speed is quoted until those agree — pin the ' +
      'centre frequency in the rail if you know which is right.'));
}

// -- inter-arrival ------------------------------------------------------------

/**
 * Format a duration given in microseconds, choosing its own unit.
 *
 * The axis used to be labelled `µs` while its ticks read `147.0m`, which is
 * both a wrong unit and an ambiguous one. Every tick now carries the unit it is
 * actually in.
 */
function dur(us) {
  if (!Number.isFinite(us)) return '—';
  if (us >= 1e6) return (us / 1e6).toFixed(us >= 1e7 ? 0 : 1) + ' s';
  if (us >= 1e3) return (us / 1e3).toFixed(us >= 1e4 ? 0 : 1) + ' ms';
  return us.toFixed(us >= 100 ? 0 : 1) + ' µs';
}

function renderJitter(f) {
  const { h, a } = f;
  const p = panels.jitter;
  const bins = a.interarrival_hist;
  if (!bins || !bins.length) return;

  // LOG axis, because these gaps span decades and a linear one hides the
  // structure this panel exists to show. Measured 2026-08-23: p50 236 µs
  // against a maximum of 735 ms — three and a half decades, of which a linear
  // plot rendered one spike in the first bin and forty-seven empty columns.
  // The two modes it was hiding, a burst spacing and a between-burst gap, are
  // exactly what separates a metronome losing slots from a source that is not
  // a metronome. The server bins logarithmically to match.
  const lo = Math.max(1, h.timing.hist_min_us);
  const hi = Math.max(lo * 10, h.timing.hist_max_us);
  const L = Math.log10(lo), Hi = Math.log10(hi);
  const top = Math.max(1, ...bins);

  p.begin({
    x0: L, x1: Hi, y0: 0, y1: top * 1.1,
    xticks: Math.max(1, Math.round(Hi - L)),
    xfmt: (v) => dur(10 ** v),
    yfmt: (v) => v.toFixed(0),
  });

  // Bars are indexed over log-spaced bins; place each at its own bin centre in
  // log space so the widths are uniform on screen.
  const step = (Hi - L) / bins.length;
  p.ctx.fillStyle = CATEGORICAL[0];
  const base = p.py(0);
  const bw = Math.max(1, (p.w - p.pad.l - p.pad.r) / bins.length - 1);
  bins.forEach((v, i) => {
    if (!(v > 0)) return;
    const yy = p.py(v);
    p.ctx.fillRect(p.px(L + (i + 0.5) * step) - bw / 2, yy, bw, base - yy);
  });

  const t = h.timing.ftm;
  const at = (us) => Math.log10(Math.min(hi, Math.max(lo, us)));
  p.rule('x', at(t.p50_us), THEME.MUTED, `p50 ${dur(t.p50_us)}`);
  p.rule('x', at(t.p99_us), CATEGORICAL[3], `p99 ${dur(t.p99_us)}`);
  p.note('inter-arrival, log scale →', 'bl');
  p.end();

  // Two modes decades apart is the signature of a bursty source, and it is why
  // an inferred slot cannot be trusted. Naming it here saves the operator from
  // deriving it from the metronome panel's silence.
  const occupied = bins.reduce((n, v, i) => (v > top * 0.05 ? n.concat(i) : n), []);
  const spread = occupied.length >= 2
    ? (occupied[occupied.length - 1] - occupied[0]) * step
    : 0;
  const bursty = spread >= 1.5;

  readout('r-jitter',
    `p50 ${dur(t.p50_us)} · p99.9 ${dur(t.p999_us)}` +
    (bursty ? ' · <span class="warn">bursty</span>' : ''),
    `Inter-arrival on the 320 MHz baseband clock: p50 ${dur(t.p50_us)}, ` +
    `p95 ${dur(t.p95_us)}, p99 ${dur(t.p99_us)}, p99.9 ${dur(t.p999_us)}, ` +
    `max ${dur(t.max_us)}. CV ${fmt(t.cv, 2)}. Host-side delivery for the same ` +
    `window: p50 ${dur(h.timing.host.p50_us)}, p99.9 ${dur(h.timing.host.p999_us)}. ` +
    (bursty
      ? `The occupied bins span about ${spread.toFixed(1)} decades, so this source ` +
        'is bursty rather than periodic: a short spacing inside a burst and a much ' +
        'longer gap between bursts. A slot inferred from these arrivals lands on ' +
        'the burst spacing and describes no rate at all, which is why the ' +
        'metronome panel withholds a deficit.'
      : 'The gaps occupy a narrow range, so a single rate describes them.'));
}

// -- clocks -------------------------------------------------------------------

function renderClocks(f) {
  const { h, a } = f;
  const p = panels.clocks;
  const host = a.drift_host_us, fw = a.drift_fw_us;
  if (!host || !host.length) return;

  const all = [pct(host, 0.01), pct(host, 0.99), pct(fw, 0.01), pct(fw, 0.99)]
    .filter(Number.isFinite);
  const m = Math.max(2, ...all.map(Math.abs)) * 1.2;

  p.begin({ x0: 0, x1: host.length - 1, y0: -m, y1: m, yfmt: (v) => v.toFixed(0) });
  p.rule('y', 0, THEME.AXIS, '');
  p.seriesY(fw, CATEGORICAL[2], 1.1);
  p.seriesY(host, CATEGORICAL[1], 1.2);
  p.note('← older    records    newer →', 'bl');
  p.note('µs from the 320 MHz baseband clock', 'tl');
  p.end();

  readout('r-clocks',
    `<span style="color:${CATEGORICAL[1]}">host</span> ` +
    `<span style="color:${CATEGORICAL[2]}">firmware</span> · ` +
    `${fmt(h.clocks.ftm_span_us / 1000)} ms`,
    `Each point is how far a clock has drifted from the 320 MHz baseband clock ` +
    `since the start of the window. Flat means the wallclock anchor is sound; ` +
    `a ramp is drift; spikes are delivery jitter. Window spans ` +
    `${fmt(h.clocks.ftm_span_us / 1000)} ms on ftm, ` +
    `${fmt(h.clocks.host_span_us / 1000)} ms on the host clock.`);
}

// -- capture (csid's own view) ------------------------------------------------

/**
 * The half of the truth the live stream cannot carry.
 *
 * `frames_seen` exists nowhere in the record path, and without it an empty
 * waterfall has four indistinguishable explanations. Measured on 2026-08-17:
 * a ch11 session received 3915 frames and produced 0 CSI records, because
 * DSSS/CCK has no OFDM preamble and therefore no channel estimate to report.
 * The channel was busy. Every frame was seen. None could become CSI.
 */
function renderCapture(f) {
  const c = f.h.capture;
  const note = $('capture-note');

  if (!c || !c.present) {
    kv($('tbl-capture'), [['status', '<span class="dim">no status file</span>']]);
    note.className = 'verdict';
    note.textContent =
      'csid is not publishing /run/csid/status.json here — either no capture ' +
      'is running, or this console is watching a UDP stream from another host. ' +
      'Without it there is no frames_seen, so the yield cannot be computed and ' +
      'an empty waterfall stays ambiguous.';
    $('t-yield').textContent = '—';
    $('tile-yield').className = 'tile';
    return;
  }

  // TWO yields, and the tile shows the second.
  //
  // `yield_ratio` is records over frames the radio delivered — did we get a
  // record. `useful_yield_ratio` is the same over records that carried a
  // channel estimate — did we measure the channel. On this fleet they differ by
  // fifteen points, and the tile used to show only the first: 98.4% in green
  // over a stream in which one record in six was empty.
  const asPct = (v) => (v === null || v === undefined ? '—' : (100 * v).toFixed(1) + '%');
  const pct = asPct(c.yield_ratio);
  const usefulPct = asPct(c.useful_yield_ratio);
  const cls = { ok: 'ok', low: 'warn', bad: 'bad' }[c.yield_verdict] || 'warn';
  const emptyFrac = c.records > 0 ? c.empty_records / c.records : 0;

  $('t-yield').textContent = c.stale ? '—' : usefulPct;
  $('tile-yield').className = 'tile' + (c.stale ? '' :
    c.yield_verdict === 'ok' ? '' : c.yield_verdict === 'bad' ? ' alert' : ' warn');

  // A generated run id groups nothing but its own session, so it is never
  // presented as a fleet key.
  const runId = c.run_id_generated
    ? `<span class="dim">${c.run_id} (generated — groups nothing)</span>`
    : `<span class="ok">${c.run_id}</span>`;

  const rows = [
    ['capture yield', `<span class="${cls}">${pct} · ${c.yield_verdict}</span>`],
    ['useful yield', `<span class="${emptyFrac > 0.05 ? 'warn' : 'ok'}">${usefulPct}</span>` +
      ' <span class="dim">records that measured something</span>'],
    ['records / frames', `${c.records.toLocaleString()} / ${c.frames_seen.toLocaleString()}`],
    ['empty records', c.empty_records
      ? `<span class="${emptyFrac > 0.05 ? 'warn' : ''}">${c.empty_records.toLocaleString()}` +
        ` · ${(100 * emptyFrac).toFixed(1)}% of records</span>`
      : '<span class="ok">none</span>'],
    ['state', c.stale
      ? `<span class="bad">${c.state} — ${fmt(c.age_s, 0)} s stale</span>`
      : `<span class="ok">${c.state}</span>`],
    ['run id', runId],
    ['experiment', c.experiment || '—'],
    ['session', c.session_id || '—'],
    ['band', c.band ? c.band + ' GHz' : '—'],
    ['commanded interval', c.interval_us
      ? `${(c.interval_us / 1000).toFixed(2)} ms (${fmt(1e6 / c.interval_us, 1)} Hz)`
      : '<span class="dim">unthrottled</span>'],
    ['csid rate', fmt(c.rate_hz) + ' Hz'],
    ['on disk', bytes(c.capture_bytes)],
    ['live drops', `<span class="${c.live_dropped ? 'warn' : 'ok'}">${c.live_dropped}</span>`],
    ['uptime', `${c.uptime_s} s`],
  ];
  if (c.ble) {
    // A scanner that is on and silent is a void arm, not a missing feature.
    rows.push(['ble', `<span class="${c.ble.rate_hz > 0 ? 'ok' : 'bad'}">` +
      `${c.ble.observations.toLocaleString()} obs · ${fmt(c.ble.rate_hz)} Hz</span>`]);
  }
  kv($('tbl-capture'), rows);

  note.className = 'verdict' + (c.yield_verdict === 'bad' ? ' bad'
    : c.yield_verdict === 'ok' ? ' ok' : ' warn');
  note.textContent = c.stale
    ? `The status file is ${fmt(c.age_s, 0)} s old. csid writes it every second, ` +
      'so these numbers describe a capture that may already have stopped.'
    : (c.yield_note || 'Yield is what this band gives.');

  readout('r-capture', `${c.yield_verdict} · ${usefulPct}`,
    `Capture yield is records / frames_seen: ${c.records} of ${c.frames_seen} ` +
    `frames produced a record, which is ${pct}. The denominator comes from ` +
    'csid\'s status file and exists nowhere in the live stream, which is why an ' +
    'empty waterfall used to be unreadable. ' +
    (c.empty_records
      ? `Of those records ${c.empty_records} — ${(100 * emptyFrac).toFixed(1)}% — ` +
        'arrived with an all-zero I/Q matrix: delivered, correctly sized, and ' +
        `carrying no channel estimate. The useful yield is therefore ${usefulPct}, ` +
        'and that is the number the tile shows and the one to quote for a corpus ' +
        'size. '
      : 'Every record carried a channel estimate. ') +
    (c.yield_note || ''));
}

// -- the metronome ------------------------------------------------------------

/**
 * Delivery against the slot, for one transmitter.
 *
 * The panel that replaced a coefficient of variation. Measured 2026-08-17, one
 * injector at a commanded 10 ms slot: 2.4 GHz delivered 61.3 Hz with
 * percentiles at 10/40/80 ms, 5 GHz delivered 99.4 Hz with 10/10/20 ms. The
 * source never jittered — it lost whole slots, 38.7% of them against 0.6%. The
 * old readout called the first one "CV 0.71, Doppler qualitative", which is the
 * wrong verdict for a process whose surviving arrivals are all on grid.
 */
function renderMetronome(f) {
  const { h, a } = f;
  const p = panels.metronome;
  const m = h.metronome;
  const bins = a.metronome_multiples;
  const verdict = metronomeVerdict(m);

  // Nothing periodic here, so nothing in this struct describes the source —
  // which is what `dsp::Metronome::verdict` has always said about `irregular`,
  // and what this panel used to ignore. It printed the inferred slot, a
  // commanded rate derived from it and a delivery deficit, in red, against a
  // rate nobody had commanded. Measured 2026-08-23: a slot of 0.16 ms, a
  // "commanded" 6305 Hz and a 99.4% deficit, permanently, on all four nodes,
  // for a source configured at 25 Hz with no throttle at all.
  //
  // The server now withholds the deficit in exactly this case, so the absence
  // is a fact on the wire rather than a decision taken twice.
  if (!m || !(m.slot_us > 0) || verdict === 'irregular' || m.deficit === null) {
    p.begin({ x0: 0, x1: 1, y0: 0, y1: 1, xticks: 1, yticks: 1 });
    p.note(m && m.n_gaps < 8
      ? 'not enough arrivals yet'
      : 'not a metronome — no slot to measure against', 'tl');
    p.end();

    const why = !m || !(m.slot_us > 0)
      ? 'No nominal slot could be established for this transmitter, so there is ' +
        'nothing to measure a deficit against. That is the expected reading for ' +
        'ambient traffic, which arrives when somebody transmits.'
      : `A slot of ${fmt(m.slot_us / 1000, 2)} ms was inferred from the arrivals, ` +
        `but only ${(100 * m.exact_slot).toFixed(0)}% of gaps land on a multiple ` +
        `of it and ${(100 * m.off_slot).toFixed(0)}% are off the grid entirely. ` +
        'This source is not periodic, so the inferred slot describes a burst ' +
        'spacing rather than a rate, and no deficit is reported against it. ' +
        'Look at the inter-arrival panel: two modes decades apart is what this ' +
        'looks like. Set radio.interval_us to have a rate declared, and the ' +
        'deficit becomes a real measurement whatever the arrivals do.';
    readout('r-metronome',
      `<span class="dim">${verdict === 'irregular' ? 'irregular' : 'no slot'}</span>`,
      why);
    $('t-deficit').textContent = '—';
    $('tile-deficit').className = 'tile';
    return;
  }

  const top = Math.max(0.05, ...bins) * 1.15;
  p.begin({
    x0: 0.5, x1: bins.length + 0.5, y0: 0, y1: top,
    xticks: 8,
    xfmt: (v) => (Math.abs(v - Math.round(v)) < 0.01 ? Math.round(v) + '×' : ''),
    yfmt: (v) => (100 * v).toFixed(0) + '%',
  });
  // Bar 1 is "arrived on the next slot"; everything right of it is loss.
  p.ctx.fillStyle = CATEGORICAL[0];
  const bw = Math.max(2, (p.w - p.pad.l - p.pad.r) / bins.length - 3);
  const base = p.py(0);
  for (let i = 0; i < bins.length; i++) {
    if (!(bins[i] > 0)) continue;
    p.ctx.fillStyle = i === 0 ? CATEGORICAL[2] : CATEGORICAL[1];
    const y = p.py(bins[i]);
    p.ctx.fillRect(p.px(i + 1) - bw / 2, y, bw, base - y);
  }
  p.note('slot multiples — 1× arrived, k× lost k−1 slots', 'bl');
  p.note(`${fmt(m.delivered_hz)} / ${fmt(m.commanded_hz)} Hz`, 'tl');
  // The last bin is an overflow, not a multiple of sixteen. Saying so stops it
  // being read as a population of gaps that are exactly 16 slots long.
  if (bins[bins.length - 1] > 0.01) p.note(`${bins.length}× is ≥, not =`, 'br');
  p.end();

  const deficit = (100 * m.deficit).toFixed(1) + '%';
  $('t-deficit').textContent = deficit;
  $('tile-deficit').className = 'tile' +
    (m.deficit > 0.2 ? ' alert' : m.deficit > 0.05 ? ' warn' : '');

  // Two verdicts reach here; `irregular` returned above.
  const verdictCls = { 'on grid': 'ok', deferred: 'warn' }[verdict] || 'warn';
  const explain = {
    'on grid':
      'Every surviving arrival sits on a multiple of the slot, so the missing ' +
      'slots are the whole story. Nearest-neighbour resampling onto a uniform ' +
      'grid is near-exact and the Doppler axis stays quantitative, whatever the ' +
      'coefficient of variation says.',
    deferred:
      'There is a real slot underneath — a clear mode on the grid — but a ' +
      'substantial population has been pushed off it. On 2.4 GHz that is ' +
      'CSMA/CA: the radio waits for a clear channel, so a frame is not merely ' +
      'dropped, it is delayed by a random backoff. Two mechanisms, not one, and ' +
      'only the dropped half resamples cleanly.',
  }[verdict] || '';

  readout('r-metronome',
    `slot ${fmt(m.slot_us / 1000, 2)} ms · deficit ${deficit} · ` +
    `<span class="${verdictCls}">${verdict}</span>`,
    `Nominal slot ${fmt(m.slot_us / 1000, 3)} ms (${m.slot_source}), so the ` +
    `commanded rate is ${fmt(m.commanded_hz)} Hz. This transmitter delivered ` +
    `${fmt(m.delivered_hz)} Hz over ${m.n_gaps} gaps — a deficit of ${deficit}. ` +
    `${(100 * m.exact_slot).toFixed(1)}% of gaps sit dead on a multiple and ` +
    `${(100 * m.off_slot).toFixed(1)}% are off the grid entirely; the longest ` +
    `run of consecutive missed slots was ${m.longest_run} ` +
    `(${fmt((m.longest_run * m.slot_us) / 1000, 1)} ms). ` + explain +
    (m.slot_source === 'inferred'
      ? ' The slot was recovered from the arrivals themselves. A source that ' +
        'lost every other slot would infer twice the interval and report no ' +
        'deficit at all — set radio.interval_us to have it declared.'
      : ''));
}

/** The three-way verdict, mirroring `dsp::Metronome::verdict`. */
function metronomeVerdict(m) {
  if (!m || !(m.slot_us > 0)) return 'no slot';
  if (m.quantised) return 'on grid';
  return m.exact_slot >= 0.40 ? 'deferred' : 'irregular';
}

// -- band plan ----------------------------------------------------------------

/**
 * Where BLE can and cannot appear inside this capture.
 *
 * A design-time check on a live console, because the cost of skipping it is a
 * booked room: EXP-010's inclusion arm was written as ch13/advertising 39,
 * which lands at array index 50.6 of 51 — inside the band-edge region that
 * produced 55% of the ABBA probe's spurious events.
 */
function renderBandplan(f) {
  const { h, a } = f;
  const p = panels.bandplan;
  const bp = h.bandplan;
  const note = $('bandplan-note');
  const med = a.bundle_p50 && a.bundle_p50.length ? a.bundle_p50 : a.amp_db;
  const xs = a.tone_offset_mhz;

  if (!bp || !xs || !xs.length) return;

  if (!bp.applicable) {
    p.begin({ x0: -1, x1: 1, y0: 0, y1: 1, xticks: 1, yticks: 1 });
    p.note('no BLE channel can exist in this passband', 'tl', THEME.MUTED);
    p.end();
    note.className = 'verdict ok';
    note.textContent = bp.verdict;
    readout('r-bandplan', '<span class="ok">not 2.4 GHz</span>', bp.verdict);
    return;
  }

  const lo = med && med.length ? pctSignal(med, 0.02) - 4 : 0;
  const hi = med && med.length ? pctSignal(med, 0.99) + 4 : 60;
  const span = bp.half_span_mhz * 1.02;
  p.begin({
    x0: -span, x1: span, y0: lo, y1: hi,
    xfmt: (v) => v.toFixed(1), yfmt: (v) => v.toFixed(0),
  });

  // Artefact regions, shaded first so everything else sits on top of them.
  const shade = (a0, a1) => {
    if (a1 <= a0) return;
    const x0 = p.px(xs[Math.max(0, Math.min(xs.length - 1, a0))]);
    const x1 = p.px(xs[Math.max(0, Math.min(xs.length - 1, a1 - 1))]);
    p.ctx.fillStyle = 'rgba(213,94,0,0.14)';
    p.ctx.fillRect(Math.min(x0, x1), p.pad.t, Math.abs(x1 - x0) + 3, p.h - p.pad.t - p.pad.b);
  };
  shade(bp.dc_zone[0], bp.dc_zone[1]);
  shade(bp.low_edge_zone[0], bp.low_edge_zone[1]);
  shade(bp.high_edge_zone[0], bp.high_edge_zone[1]);

  if (med && med.length === xs.length) p.seriesY(med, THEME.INK, 1.3, xs);

  // Every BLE channel in band; the advertising ones are promoted.
  for (const l of bp.inside) {
    const isAdv = l.advertising !== null && l.advertising !== undefined;
    const colour = isAdv
      ? (l.artefact_distance <= 3 ? CATEGORICAL[3] : CATEGORICAL[2])
      : 'rgba(125,135,148,0.55)';
    p.ctx.save();
    p.ctx.strokeStyle = colour;
    p.ctx.lineWidth = isAdv ? 2 : 1;
    const x = Math.round(p.px(l.freq_mhz - bp.centre_mhz)) + 0.5;
    p.ctx.beginPath();
    p.ctx.moveTo(x, p.pad.t);
    p.ctx.lineTo(x, p.h - p.pad.b);
    p.ctx.stroke();
    if (isAdv) {
      p.ctx.fillStyle = colour;
      p.ctx.textAlign = 'center';
      p.ctx.fillText('adv ' + l.advertising, x, p.pad.t + 6);
    }
    p.ctx.restore();
  }

  p.note('MHz from band centre →', 'bl');
  p.note('median |H| dB · shaded = measured artefact regions', 'tl');
  p.end();

  const adv = bp.inside.find((l) => l.advertising !== null && l.advertising !== undefined);
  const bad = adv && adv.artefact_distance <= 3;
  note.className = 'verdict ' + (adv ? (bad ? 'bad' : 'ok') : '');
  note.textContent = bp.verdict;
  readout('r-bandplan',
    adv
      ? `<span class="${bad ? 'bad' : 'ok'}">adv ${adv.advertising} @ index ` +
        `${fmt(adv.array_index, 1)}</span> · ${bp.inside.length} BLE ch in band`
      : `<span class="dim">no advertising channel</span> · ${bp.inside.length} BLE ch in band`,
    `ch${bp.wifi_channel} is centred ${fmt(bp.centre_mhz, 0)} MHz and its ` +
    `${bp.ntone} tones occupy ±${fmt(bp.half_span_mhz, 2)} MHz at ` +
    `${fmt(bp.spacing_khz, 1)} kHz spacing. ${bp.verdict} ` +
    (adv
      ? `From DC ${fmt(adv.tones_from_dc, 1)} tones, from the band edge ` +
        `${fmt(adv.tones_from_edge, 1)}. `
      : '') +
    'Landing in band is necessary, not sufficient: an LE 1M burst occupies ' +
    'about 3.4 subcarriers, which buys ~11.9 dB against a wideband comparison, ' +
    'and a 3 dB excursion still needs the burst within ~19.6 dB of the Wi-Fi frame.');
}

// -- CSI ratio ----------------------------------------------------------------

function renderRatio(f) {
  const { h, a } = f;
  const p = panels.ratio;
  const amp = a.ratio_amp_db;
  const ph = a.ratio_phase;

  if (!amp || !amp.length) {
    p.begin({ x0: 0, x1: 1, y0: -1, y1: 1, xticks: 1, yticks: 2 });
    p.note('needs a second chain — pick one in the rail', 'tl');
    p.end();
    readout('r-ratio', '<span class="dim">no pair</span>',
      'The ratio needs two chains. Without one the console can only offer the ' +
      'sanitised phase, which subtracts a least-squares line and takes any ' +
      'genuinely linear part of the channel with it.');
    return;
  }

  // Phase on the axis, amplitude as a muted companion: the phase is the point.
  p.begin({ x0: 0, x1: amp.length - 1, y0: -Math.PI, y1: Math.PI,
            yfmt: (v) => v.toFixed(1) });
  p.rule('y', 0, THEME.AXIS, '');
  if (ph && ph.length) p.seriesY(ph, CATEGORICAL[0], 1.4);
  p.note('subcarrier index →', 'bl');
  p.note('∠(H_a/H_b), radians', 'tl');
  p.end();

  const g = h.geometry;
  readout('r-ratio',
    `chain ${g.chain} / ${g.chain_b} · |ratio| ${fmt(amp[Math.floor(amp.length / 2)])} dB`,
    'H_a/H_b across two chains of one radio. Carrier-frequency offset, ' +
    'sampling-frequency offset and packet-detection delay are common to both ' +
    'chains, so the division cancels them exactly (FarSense, Zeng et al. 2019). ' +
    'That makes this the one representation whose phase is stable from packet ' +
    'to packet without fitting anything — unlike the sanitised phase, which ' +
    'removes a linear term that is partly the channel.');
}

// -- per-subcarrier statistics ------------------------------------------------

function renderToneStats(f) {
  const { h, a } = f;
  const p = panels.tonestats;
  if (blanked(p, 'r-tonestats', f.h, 'the per-tone statistics')) return;
  const spread = a.tone_spread_db;
  const nulls = a.tone_null_frac;
  if (!spread || !spread.length) return;

  const hi = Math.max(0.5, pct(spread, 0.99) * 1.25);
  p.begin({ x0: 0, x1: spread.length - 1, y0: 0, y1: hi,
            yfmt: (v) => v.toFixed(1) });

  // Nulled tones first, as background: they are an absence of measurement, not
  // a very steady one, and drawing them as zero spread would say the opposite.
  // Shaded in proportion, not past a threshold. The old form drew a tone only
  // when it was null in MORE THAN HALF the window, so a band-wide null rate of
  // 11–23% — which is what the fleet actually delivers — drew nothing at all
  // and the readout said "0 nulled". A threshold answers "is this tone always
  // dead"; the question the operator has is "how much of this tone is missing".
  if (nulls && nulls.length === spread.length) {
    for (let i = 0; i < nulls.length; i++) {
      if (!(nulls[i] > 0.01)) continue;
      p.ctx.fillStyle = `rgba(125,135,148,${(0.12 + 0.6 * nulls[i]).toFixed(3)})`;
      p.ctx.fillRect(p.px(i) - 1.5, p.pad.t, 3, p.h - p.pad.t - p.pad.b);
    }
  }
  p.seriesY(spread, CATEGORICAL[0], 1.3);

  const st = h.tone_stats;
  if (st && st.max_spread_db > 0) {
    p.rule('x', st.max_spread_tone, CATEGORICAL[3], `#${st.max_spread_tone}`);
  }
  p.note('subcarrier index →', 'bl');
  p.note('temporal spread, dB (1σ over the window)', 'tl');
  p.end();

  // The band-wide mean, as a number. A shading intensity is a hint; this is the
  // fact, and it is the one that was invisible.
  const meanNull = nulls && nulls.length
    ? nulls.reduce((x, y) => x + y, 0) / nulls.length
    : 0;
  const nullTxt = meanNull > 0.005
    ? ` · <span class="${meanNull > 0.1 ? 'warn' : ''}">${(100 * meanNull).toFixed(0)}% null</span>`
    : '';
  readout('r-tonestats',
    `max ${fmt(st.max_spread_db)} dB @ #${st.max_spread_tone}${nullTxt} · ${st.n} cols`,
    `Per-tone standard deviation of |H| in dB over ${st.n} decimated columns. ` +
    'Reported in dB rather than as a coefficient of variation, because a CV of ' +
    'a logarithmic quantity is not a CV of the amplitude underneath it — for ' +
    `small excursions CV ≈ spread/8.686, so ${fmt(st.max_spread_db)} dB is about ` +
    `${fmt(st.max_spread_db / 8.686, 3)}. ` +
    `Grey shading is each tone's null share, ${(100 * meanNull).toFixed(1)}% across ` +
    `the band and total on ${st.null_tones} tone${st.null_tones === 1 ? '' : 's'}. ` +
    'A null is an absence of measurement, not a very steady one, and every ' +
    'statistic here excludes it. Watch the outer three tones — the ABBA probe ' +
    'put 55% of its spurious events there.');
}

// -- tables -------------------------------------------------------------------

function renderTables(f) {
  const { h } = f;

  // The complete census. The deep views are scoped to one class; this panel is
  // the guarantee that scoping never hides what else is on the air.
  const sel = h.class.key;
  table($('tbl-classes'),
    [
      // The selected class is marked in place rather than with a trailing
      // badge column, which the narrow panel clipped.
      { label: 'class', mono: true,
        get: (c) => (c.key === sel ? `▸ ${c.label}` : `\u00a0\u00a0${c.label}`),
        cls: (c) => (c.key === sel ? 'ok' : '') },
      { label: 'records', num: true, get: (c) => c.count },
      { label: 'share', num: true, get: (c) => (100 * c.share).toFixed(1) + '%' },
      {
        label: 'rate', num: true,
        get: (c) => fmt(c.share * (h.timing.ftm.rate_hz / Math.max(h.class.share, 1e-6))) + ' Hz',
      },
    ],
    h.class.available,
    { click: (c) => { $('c-class').value = c.key; $('c-class').onchange({ target: { value: c.key } }); } });
  readout('r-classes',
    `${h.class.available.length} classes · ${h.stream.window_all} records in window`,
    'Every PHY geometry seen in the analysis window, with its share of the ' +
    'channel. csiscope reads the live stream only — the capture records all ' +
    'of them regardless of what is selected here.');

  table($('tbl-talkers'),
    [
      { label: 'source MAC', get: (r) => r.mac, mono: true },
      { label: 'records', num: true, get: (r) => r.count },
      { label: 'rate', num: true, get: (r) => fmt(r.rate_hz) + ' Hz' },
      { label: 'rssi', num: true, get: (r) => (r.rssi === null ? '—' : fmt(r.rssi) + ' dBm') },
      { label: '', get: (r) => `<button data-scope="${r.mac}">scope</button>` +
                               `<button data-mac="${r.mac}">capture</button>` },
    ],
    h.talkers || [],
    { selected: (h.talkers || []).findIndex((r) => r.mac === h.transmitter.selected) });
  // Two different verbs, deliberately far apart in consequence: `scope` is a
  // display decision this console makes on its own, `capture` rewrites the
  // radio's MAC filter and changes what is recorded.
  $('tbl-talkers').querySelectorAll('button[data-scope]').forEach((b) => {
    b.onclick = () => {
      heat.waterfall.clear();
      heat.doppler.clear();
      S.scaledFor = null;
      push({ smac: b.dataset.scope });
    };
  });
  $('tbl-talkers').querySelectorAll('button[data-mac]').forEach((b) => {
    b.onclick = () => pinMacFilter(b.dataset.mac);
  });

  const mixRows = (obj, label) => {
    const entries = Object.entries(obj || {}).sort((a, b) => b[1] - a[1]);
    const total = entries.reduce((s, [, v]) => s + v, 0) || 1;
    return `<div class="mixrow"><table>${entries
      .map(([k, v]) =>
        `<tr><td class="dim">${label}</td><td class="mono">${k}</td>` +
        `<td class="num">${v}</td><td class="num dim">${((100 * v) / total).toFixed(0)}%</td></tr>`)
      .join('')}</table></div>`;
  };
  $('mix-body').innerHTML =
    mixRows(h.mix.modulation, 'phy') +
    mixRows(h.mix.ntone, 'tones') +
    mixRows(h.mix.nss, 'nss') +
    mixRows(h.mix.width, 'width');

  const v = h.validation;
  const nulls = h.nulls || { dropped: 0, considered: 0, frac: 0 };
  const verdict = (ok, text) => `<span class="${ok ? 'ok' : 'warn'}">${text}</span>`;
  const na = (why) => `<span class="dim">n/a — ${why}</span>`;
  kv($('tbl-validation'), [
    ['payload matches header', verdict(v.dimensions_ok, v.dimensions_ok ? 'yes' : 'NO — ABI drift?')],
    // Null means the delivered tone set has no DC bin, which is every 802.11
    // used-tone set. The check used to run anyway, on the two data tones either
    // side of centre, and therefore failed on every node of this fleet forever:
    // measured 2026-08-23 it read between −1.3 and +7.9 dB against a threshold
    // of −3. A check that cannot pass is not a check.
    ['DC suppression', v.dc_notch_db === null || v.dc_notch_db === undefined
      ? na('this tone grid has no DC bin')
      : verdict(v.dc_notch_db < -3, fmt(v.dc_notch_db) + ' dB')],
    ['band-edge roll-off', verdict(v.edge_rolloff_db < -1, fmt(v.edge_rolloff_db) + ' dB')],
    ['chain amplitude spread', `<span class="dim">${fmt(v.chain_spread_db)} dB</span>`],
    ['chains are distinct', v.chain_identical === null || v.chain_identical === undefined
      ? '<span class="dim">single chain</span>'
      : verdict(v.chain_identical < 0.5,
          v.chain_identical > 0.99
            ? 'NO — chains are copies'
            : `yes (${(100 * v.chain_identical).toFixed(1)}% identical)`)],
    // TWO scopes, because they answer different questions and the panel used to
    // answer only the narrow one. `zero_fraction` is this record; the window
    // share is the stream. Reading 0.0% in green while one record in six was
    // entirely zero is what a single-record check looks like when it is
    // presented as a property of the capture.
    ['zero coefficients, this record', verdict(v.zero_fraction < 0.2,
      (100 * v.zero_fraction).toFixed(1) + '%')],
    ['empty records, this window', nulls.considered
      ? verdict(nulls.frac < 0.02,
          `${nulls.dropped} of ${nulls.considered} · ${(100 * nulls.frac).toFixed(1)}%`)
      : '<span class="dim">—</span>'],
  ]);

  const s = h.stream;
  kv($('tbl-stream'), [
    ['source', s.source],
    ['records received', s.received.toLocaleString()],
    ['sender-side drops', `<span class="${s.sender_gaps ? 'warn' : 'ok'}">${s.sender_gaps}</span>`],
    ['undecodable datagrams', `<span class="${s.decode_errors ? 'bad' : 'ok'}">${s.decode_errors}</span>`],
    ['capture restarts seen', s.session_changes],
    ['bytes', bytes(s.bytes)],
    ['ring depth', `${s.depth} records`],
    ['window', `${s.window} records`],
    ['uptime', `${s.uptime_s} s`],
  ]);
}

// -- header strip -------------------------------------------------------------

function renderStrip(f) {
  const { h } = f;
  const t = h.timing.ftm;
  // Rate is for the selected class, which is the rate of the thing on screen —
  // not the channel's total, which is in the CSI-mix panel.
  $('t-rate').textContent = fmt(t.rate_hz, 0) + ' Hz';

  // The window's own empty share, beside the yield. The yield is what csid saw
  // over the whole session; this is what the analysis on screen right now had
  // to throw away.
  const nulls = h.nulls || { dropped: 0, considered: 0, frac: 0 };
  $('t-empty').textContent = nulls.considered
    ? `${(100 * nulls.frac).toFixed(0)}%`
    : '—';
  $('tile-empty').className = 'tile' +
    (nulls.no_measurement ? ' alert' : nulls.frac > 0.05 ? ' warn' : '');

  $('t-class').textContent =
    `${h.class.label} ${(100 * h.class.share).toFixed(0)}%`;
  $('t-mimo').textContent = `${h.geometry.nrx}×${h.geometry.ntx}`;
  // A per-record label would blink between PHY types five times a second on a
  // real channel; the class is fixed, so only MCS and streams can move.
  $('t-phy').textContent = h.record.phy
    ? `m${h.record.phy.mcs} ${h.record.phy.nss}ss`
    : 'unlabelled';
  $('t-rssi').textContent = (h.record.rssi || []).join(' / ') || '—';
  // Weak-signal warning is meaningful again now that the sign is real.
  $('t-rssi').parentElement.classList.toggle('warn',
    (h.record.rssi || []).some((r) => r !== 0 && r < -80));
  $('t-bundle').textContent = fmt(h.bundle.width_db) + ' dB';
  $('t-records').textContent = h.stream.received.toLocaleString();

  $('t-class').parentElement.classList.toggle('alert',
    !h.geometry.dimensions_ok || h.radio.channel_mismatch);
  $('t-class').parentElement.classList.toggle('warn', h.class.pinned);

  // The channel, and the disagreement about it when there is one. This is the
  // one line always on screen, so it is where a capture on a channel nobody
  // agrees on has to be visible.
  const ch = h.radio.channel_mismatch
    ? `ch ${h.radio.channel} (csid says ${h.radio.tuned_channel}) `
    : `ch ${h.radio.channel} `;
  $('s-session').textContent =
    `session ${h.record.session_uid} · ${ch}` +
    `${h.radio.width} · ${h.geometry.ntone} tones over ${fmt(h.radio.bw_mhz, 1)} MHz ` +
    `· ${(h.radio.spacing_hz / 1000).toFixed(3)} kHz spacing`;
  $('s-session').className = h.radio.channel_mismatch ? 'bad' : '';
  // Two scopes sit between the requested window and the one under the plots,
  // and both shrink it. A Doppler spectrogram computed over 139 records when
  // 256 were asked for is a different measurement, so the narrowing is stated
  // rather than left to be discovered.
  // What the analysis ran over, in one sentence that HOLDS STILL.
  //
  // This line used to mix two kinds of number and it read as noise. The window
  // counts are properties of the window and change slowly; `skipped`,
  // `other_class` and `other_transmitter` are per-FRAME counts of what arrived
  // in the last 50 ms, so at 20 fps they flickered between "1 other
  // transmitter" and "2 other transmitters" and nothing several times a second,
  // in a footer nobody can read at that rate. Worse, the two were separated
  // only by a dot, so a per-frame count looked like a property of the capture.
  //
  // Now: the footer states the scope, which is stable, and says how many
  // transmitters and classes are ON THE CHANNEL — both from the census, which
  // is a window quantity. The per-frame arrival counts belong to the waterfall,
  // which is where they are already reported and where they refer to something
  // the reader can see.
  const st = h.stream;
  const parts = [`${st.window_all} in window`];
  if (st.window_all !== st.window_class) {
    parts.push(`${st.window_class} of ${h.class.label}`);
  }
  if (st.window_class !== st.window) {
    parts.push(`${st.window} from ${h.transmitter.selected || 'one transmitter'}`);
  }
  const others = Math.max(0, (h.transmitter.available || []).length - 1);
  if (others > 0) {
    parts.push(`${others} other transmitter${others === 1 ? '' : 's'} on air`);
  }
  const classes = Math.max(0, (h.class.available || []).length - 1);
  if (classes > 0) {
    parts.push(`${classes} other class${classes === 1 ? '' : 'es'}`);
  }
  $('s-skip').textContent = parts.join(' → ');
}

// -- the render loop ----------------------------------------------------------

let dirty = false;

function scheduleRender() {
  if (dirty) return;
  dirty = true;
  requestAnimationFrame(() => {
    dirty = false;
    const f = S.frame;
    if (!f) return;
    try {
      renderStrip(f);
      renderCapture(f);
      renderMetronome(f);
      renderWaterfall(f);
      renderSpectrum(f);
      renderPhase(f);
      renderCir(f);
      renderDoppler(f);
      renderConstellation(f);
      renderChains(f);
      renderTones(f);
      renderRssi(f);
      renderJitter(f);
      renderClocks(f);
      renderBandplan(f);
      renderRatio(f);
      renderToneStats(f);
      renderTables(f);
    } catch (e) {
      // A render bug otherwise shows up as a frozen panel and nothing else —
      // the console log is invisible unless devtools happen to be open. Say it
      // once, out loud, then keep quiet so a per-frame fault cannot bury the
      // screen in toasts.
      console.error('render', e);
      if (!S.renderErrorShown) {
        S.renderErrorShown = true;
        toast(`render failed: ${e.message}`, true);
      }
    }
  });
}

// -------------------------------------------------------------- live socket

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${proto}://${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  S.ws = ws;

  ws.onopen = () => setConn('live');
  ws.onclose = () => {
    setConn('offline');
    setTimeout(connect, 1500);
  };
  ws.onerror = () => setConn('offline');

  ws.onmessage = (ev) => {
    if (typeof ev.data === 'string') {
      const m = JSON.parse(ev.data);
      if (m.t === 'hello') {
        S.settings = m.settings;
        $('s-source').textContent = m.source;
        $('s-version').textContent = `csiscope ${m.csiscope_version} · csid ${m.csid_version}`;
        // Stated once, and always true: the console serves no route that
        // mutates anything. See `server.rs`.
        $('s-mode').textContent = 'READ-ONLY';
        syncControls();
      } else if (m.t === 'settings') {
        S.settings = m.settings;
        syncControls();
      } else if (m.t === 'error') {
        toast(m.error, true);
      }
      return;
    }
    S.frame = decodeFrame(ev.data);
    S.lastFrameAt = performance.now();
    setConn('live');
    fillClassSelect(S.frame.h.class);
    fillTransmitterSelect(S.frame.h.transmitter);
    fillChainSelects(S.frame.h.geometry);
    autoScaleOnce(S.frame);
    scheduleRender();
  };
}

/**
 * Scale the waterfall to the signal the first time a given geometry/chain is
 * seen.
 *
 * |H| is AGC-relative, so its absolute level depends on the radio's gain
 * state, not on a number anyone can predict. A fixed default colour window
 * therefore saturates or blacks out on first contact, which looks like a bug.
 * The operator's own choice always wins: touching a dB slider disables this.
 */
function autoScaleOnce(f) {
  if (S.manualScale) return;
  const key = `${f.h.class.key}:${f.h.transmitter.selected}:${f.h.geometry.chain}`;
  if (key === S.scaledFor) return;
  const amp = f.a.amp_db;
  if (!amp || amp.length < 8) return;
  S.scaledFor = key;
  const lo = Math.floor(pctSignal(amp, 0.05)) - 2;
  const hi = Math.ceil(pctSignal(amp, 0.995)) + 2;
  heat.waterfall.clear();
  push({ db_min: lo, db_max: hi });
}

function setConn(state) {
  const c = $('conn');
  c.className = 'conn ' + (state === 'live' ? 'live' : state === 'stale' ? 'stale' : '');
  c.textContent = state;
}

// A live socket with no records looks exactly like a healthy one until you
// notice nothing moves — so say so explicitly.
setInterval(() => {
  if (!S.ws || S.ws.readyState !== 1) return;
  if (S.settings && S.settings.paused) return;
  const age = performance.now() - S.lastFrameAt;
  if (age > 3000) setConn('stale');
}, 1000);

function push(patch) {
  if (!S.settings || !S.ws || S.ws.readyState !== 1) return;
  S.settings = Object.assign({}, S.settings, patch);
  S.ws.send(JSON.stringify(S.settings));
}

// -------------------------------------------------------------- rail controls

function syncControls() {
  const s = S.settings;
  if (!s) return;
  const set = (id, v) => { const n = $(id); if (n && document.activeElement !== n) n.value = v; };
  set('c-window', s.window); $('o-window').textContent = s.window + ' rec';
  set('c-fps', s.fps); $('o-fps').textContent = s.fps + ' fps';
  set('c-wfrows', s.wf_rows); $('o-wfrows').textContent = s.wf_rows;
  set('c-dbmin', s.db_min); $('o-dbmin').textContent = s.db_min + ' dB';
  set('c-dbmax', s.db_max); $('o-dbmax').textContent = s.db_max + ' dB';
  set('c-taps', s.cir_taps); $('o-taps').textContent = s.cir_taps;
  set('c-dnfft', s.doppler_nfft);
  set('c-wfscope', s.wf_scope);
  set('c-freq', s.freq_mhz === null ? '' : s.freq_mhz);
  $('btn-pause').classList.toggle('on', s.paused);
  $('btn-pause').textContent = s.paused ? 'resume' : 'pause';
}

/**
 * Rebuild the class selector when what is on the channel changes.
 *
 * Rebuilt only on a real change: the options carry live shares, and replacing
 * the list every frame would make it impossible to open.
 */
let classSig = '';
function fillClassSelect(cls) {
  const sig = cls.available.map((c) => c.key).join(',');
  const sel = $('c-class');
  if (sig !== classSig) {
    classSig = sig;
    sel.innerHTML = '<option value="">follow the busiest</option>' +
      cls.available.map((c) => `<option value="${c.key}"></option>`).join('');
  }
  // Shares move continuously, but rebuilding the list every frame would make
  // the dropdown impossible to open. Update the labels in place instead.
  cls.available.forEach((c, i) => {
    const o = sel.options[i + 1];
    const text = `${c.label} · ${(100 * c.share).toFixed(0)}%`;
    if (o && o.text !== text) o.text = text;
  });
  const want = S.settings?.class ?? '';
  if (document.activeElement !== sel && sel.value !== want) sel.value = want;
}

/**
 * Rebuild the transmitter selector when the set on the channel changes.
 *
 * Same discipline as the class selector: the option list is rebuilt only when
 * the membership changes, because the labels carry live shares and replacing
 * the list every frame makes the dropdown impossible to open.
 */
let txSig = '';
function fillTransmitterSelect(tx) {
  const list = (tx && tx.available) || [];
  const sig = list.map((t) => t.mac).join(',');
  const sel = $('c-smac');
  if (sig !== txSig) {
    txSig = sig;
    sel.innerHTML = '<option value="">follow the busiest</option>' +
      list.map((t) => `<option value="${t.mac}"></option>`).join('');
  }
  const total = list.reduce((a, t) => a + t.count, 0) || 1;
  list.forEach((t, i) => {
    const o = sel.options[i + 1];
    const text = `${t.mac} · ${((100 * t.count) / total).toFixed(0)}%`;
    if (o && o.text !== text) o.text = text;
  });
  const want = S.settings?.smac ?? '';
  if (document.activeElement !== sel && sel.value !== want) sel.value = want;
}

/** Rebuild the chain selectors when the geometry changes. */
let chainSig = '';
function fillChainSelects(g) {
  const sig = (g.chain_labels || []).join(',');
  if (sig === chainSig) return;
  chainSig = sig;
  const a = $('c-chain'), b = $('c-chainb');
  a.innerHTML = (g.chain_labels || [])
    .map((l, i) => `<option value="${i}">${i} · ${l}</option>`).join('');
  b.innerHTML = '<option value="">none</option>' + (g.chain_labels || [])
    .map((l, i) => `<option value="${i}">${i} · ${l}</option>`).join('');
  a.value = String(S.settings?.chain ?? 0);
  b.value = S.settings?.chain_b === null || S.settings?.chain_b === undefined
    ? '' : String(S.settings.chain_b);
  $('hint-pair').classList.toggle('warn', (g.nchain || 1) < 2);
}

function wireRail() {
  $('c-class').onchange = (e) => {
    // Classes differ in width, gain and frequency grid; nothing already drawn
    // belongs on the same axes.
    heat.waterfall.clear();
    heat.doppler.clear();
    S.scaledFor = null;
    push({ class: e.target.value === '' ? null : e.target.value });
  };
  $('c-smac').onchange = (e) => {
    // A different transmitter is a different link: different gain, different
    // delay, different cadence. Nothing already drawn belongs on the new axes.
    heat.waterfall.clear();
    heat.doppler.clear();
    S.scaledFor = null;
    push({ smac: e.target.value === '' ? null : e.target.value });
  };
  $('c-wfscope').onchange = (e) => {
    heat.waterfall.clear();
    push({ wf_scope: e.target.value });
  };
  $('c-chain').onchange = (e) => {
    heat.waterfall.clear();
    S.scaledFor = null;              // chains differ in gain; refit the colours
    push({ chain: +e.target.value });
  };
  $('c-chainb').onchange = (e) =>
    push({ chain_b: e.target.value === '' ? null : +e.target.value });
  $('c-window').oninput = (e) => push({ window: +e.target.value });
  $('c-fps').oninput = (e) => push({ fps: +e.target.value });
  $('c-wfrows').oninput = (e) => push({ wf_rows: +e.target.value });
  $('c-dbmin').oninput = (e) => {
    S.manualScale = true; heat.waterfall.clear(); push({ db_min: +e.target.value });
  };
  $('c-dbmax').oninput = (e) => {
    S.manualScale = true; heat.waterfall.clear(); push({ db_max: +e.target.value });
  };
  $('c-taps').oninput = (e) => push({ cir_taps: +e.target.value });
  $('c-dnfft').onchange = (e) => { heat.doppler.clear(); push({ doppler_nfft: +e.target.value }); };
  $('c-freq').onchange = (e) =>
    push({ freq_mhz: e.target.value === '' ? null : +e.target.value });

  $('c-ramp').onchange = (e) => { S.ramp = e.target.value; scheduleRender(); };
  $('c-phase').onchange = (e) => { S.phaseMode = e.target.value; scheduleRender(); };

  // Auto-scale sets the waterfall's dB window from the signal actually present,
  // which is the only way a 52-tone legacy record and a 996-tone HE record can
  // share one colour scale sensibly.
  $('btn-autoscale').onclick = () => {
    const a = S.frame?.a?.amp_db;
    if (!a || !a.length) return;
    const lo = Math.floor(pctSignal(a, 0.02)) - 2;
    const hi = Math.ceil(pctSignal(a, 0.995)) + 2;
    heat.waterfall.clear();
    S.manualScale = false;
    S.scaledFor = `${S.frame.h.class.key}:${S.frame.h.geometry.chain}`;
    push({ db_min: lo, db_max: hi });
    toast(`waterfall scaled to ${lo} … ${hi} dB`);
  };

  $('btn-pause').onclick = () => push({ paused: !S.settings.paused });
  $('btn-present').onclick = togglePresent;
}

function togglePresent() {
  document.body.classList.toggle('present');
  $('btn-present').classList.toggle('on', document.body.classList.contains('present'));
  setTimeout(scheduleRender, 60);
}

document.addEventListener('keydown', (e) => {
  if (['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName)) return;
  if (e.key === ' ') { e.preventDefault(); push({ paused: !S.settings.paused }); }
  else if (e.key === 'p' || e.key === 'P') togglePresent();
  else if (e.key === 'a' || e.key === 'A') $('btn-autoscale').click();
  else if (e.key === 'Escape') document.body.classList.remove('present');
});

// -------------------------------------------------------------------- tabs

function wireTabs() {
  $('tabs').querySelectorAll('button').forEach((b) => {
    b.onclick = () => {
      $('tabs').querySelectorAll('button').forEach((x) => x.classList.toggle('on', x === b));
      document.querySelectorAll('.tab').forEach((t) =>
        t.classList.toggle('on', t.id === `tab-${b.dataset.tab}`));
      document.body.classList.toggle('no-rail', b.dataset.tab !== 'scope');
      if (b.dataset.tab === 'node') loadNode();
      if (b.dataset.tab === 'scope') scheduleRender();
    };
  });
}

// --------------------------------------------------------------------- node
//
// Read-only, and there is nothing here that could be otherwise: the console
// serves no route that mutates anything. What this tab does is describe the
// host — versions, kernel, capability envelope, the experiment profiles
// deployed to it and the state of their units — plus two diagnostics that read
// and change nothing, the journal and `csid doctor`.

const UNIT_STATE = {
  active: 'ok',
  activating: 'warn',
  deactivating: 'warn',
  failed: 'bad',
  inactive: 'dim',
};

function unitCell(u) {
  if (!u) return '<span class="dim">—</span>';
  if (!u.available) return '<span class="dim">no systemd</span>';
  const cls = UNIT_STATE[u.active_state] || 'dim';
  const sub = u.sub_state && u.sub_state !== u.active_state ? ` (${u.sub_state})` : '';
  return `<span class="${cls}">${u.active_state}${sub}</span>`;
}

async function loadNode() {
  try {
    const [o, caps] = await Promise.all([api('/api/overview'), api('/api/caps')]);
    S.overview = o;

    kv($('tbl-node'), [
      ['hostname', o.hostname || '—'],
      ['kernel', o.kernel || '—'],
      ['csid', o.csid_version],
      ['csiscope', o.csiscope_version],
      ['live source', o.source],
      ['config', o.config_path],
      ['experiments', o.experiment_dir],
      ['default interface', o.interface],
      ['console', '<span class="ok">read-only</span>'],
      ['csid on PATH', o.helpers.csid ? '<span class="ok">yes</span>' : '<span class="bad">no</span>'],
      ['systemd', o.helpers.systemctl ? '<span class="ok">yes</span>' : '<span class="warn">absent</span>'],
      ['journal', o.helpers.journalctl ? '<span class="ok">yes</span>' : '<span class="warn">absent</span>'],
    ]);

    kv($('tbl-caps'), [
      ['tone counts', caps.tone_counts.join(', ')],
      ['max MIMO', caps.max_mimo],
      ['sustained rate', `${caps.sustained_rate_hz} Hz`],
      ['sustained throughput', `${(caps.sustained_bytes_per_sec / 1024).toFixed(0)} kB/s`],
      ['ftm clock', `${(caps.ftm_clock_hz / 1e6).toFixed(0)} MHz · ${caps.ftm_resolution_ns} ns`],
      ['ftm wrap', `${caps.ftm_wrap_seconds.toFixed(2)} s`],
    ]);
    $('caps-notes').innerHTML = caps.notes.map((n) => `<li>${n}</li>`).join('');

    // Experiments and their units, joined by name. The overview returns them in
    // the same order, but joining on the unit name rather than the index means a
    // future divergence shows as a blank cell instead of a wrong one.
    const units = new Map((o.units || []).map((u) => [u.unit, u]));
    table($('tbl-experiments'), [
      { label: 'experiment', get: (e) => e.name, mono: true },
      { label: 'tag', get: (e) => e.config?.tag || '<span class="dim">—</span>' },
      {
        label: 'tuning',
        get: (e) => (e.tuning
          ? `ch ${e.config?.radio?.channel ?? '?'} · ${e.tuning.width} · ${e.tuning.band} GHz`
          : `<span class="bad">${e.error || 'invalid'}</span>`),
        mono: true,
      },
      {
        label: 'rate',
        num: true,
        mono: true,
        get: (e) => (e.tuning?.interval_rate_hz
          ? `${e.tuning.interval_rate_hz.toFixed(0)} Hz`
          : '<span class="dim">unthrottled</span>'),
      },
      {
        label: 'duration',
        num: true,
        mono: true,
        get: (e) => (e.tuning?.duration_s ? `${e.tuning.duration_s} s` : '<span class="dim">open</span>'),
      },
      { label: 'unit', get: (e) => unitCell(units.get(`csid@${e.name}.service`)) },
    ], o.experiments || []);

    const sel = $('c-journal-unit');
    const names = (o.experiments || []).map((e) => `csid@${e.name}.service`);
    for (const extra of ['csid-sync.timer', 'csid-prune.timer']) names.push(extra);
    const keep = sel.value;
    sel.innerHTML = names.map((n) => `<option value="${n}">${n}</option>`).join('');
    if (names.includes(keep)) sel.value = keep;

    if (!$('c-doctor-iface').value) $('c-doctor-iface').value = o.interface;
  } catch (e) { toast(e.message, true); }
}

function wireNode() {
  $('btn-journal').onclick = async () => {
    const unit = $('c-journal-unit').value;
    if (!unit) return;
    $('journal-out').textContent = 'reading…';
    try {
      const r = await api(`/api/journal?unit=${encodeURIComponent(unit)}&lines=300`);
      $('journal-out').textContent = r.text || '(nothing in the journal for this unit)';
    } catch (e) { $('journal-out').textContent = e.message; }
  };

  $('btn-doctor').onclick = async () => {
    $('doctor-out').textContent = 'running…';
    try {
      const r = await api(`/api/doctor?interface=${encodeURIComponent($('c-doctor-iface').value)}`);
      $('doctor-out').textContent = (r.stdout || '') + (r.stderr ? '\n' + r.stderr : '') ||
        '(no output — is csid on PATH?)';
    } catch (e) { $('doctor-out').textContent = e.message; }
  };
}

// -------------------------------------------------------------------- boot

setupPanels();
wireRail();
wireTabs();
wireNode();
connect();
new ResizeObserver(() => scheduleRender()).observe(document.body);
