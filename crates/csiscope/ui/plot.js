// Canvas drawing primitives for the console.
//
// Deliberately small and dependency-free — the whole UI is compiled into the
// csiscope binary, so a build step or a CDN would defeat the point.
//
// Encoding rules follow the project's figure contract:
//   * sequential magnitude  -> viridis / magma (perceptually uniform, CVD-safe)
//   * categorical series    -> Okabe-Ito, in order
//   * never a rainbow ramp, never a dual y-axis, never 3D
// The surface is dark rather than print-white because this is an instrument
// read in a lab and on a projector, but the ramps and the categorical order are
// the same ones the committed figures use.

// -- colour -------------------------------------------------------------------

const RAMPS = {
  // Ten-stop samples of the matplotlib maps, linearly interpolated to 256.
  viridis: ['#440154', '#482878', '#3e4a89', '#31688e', '#26828e',
            '#1f9e89', '#35b779', '#6dcd59', '#b4de2c', '#fde725'],
  magma:   ['#000004', '#180f3d', '#440f76', '#721f81', '#9e2f7f',
            '#cd4071', '#f1605d', '#fd9668', '#fec98d', '#fcfdbf'],
  grey:    ['#000000', '#ffffff'],
};

// Okabe-Ito, reordered so the first entries read well on a dark surface.
// Used for chains and any small set of co-equal series.
export const CATEGORICAL = [
  '#56B4E9', '#E69F00', '#009E73', '#D55E00',
  '#CC79A7', '#F0E442', '#0072B2', '#BBBBBB',
];

function hex(c) {
  return [parseInt(c.slice(1, 3), 16), parseInt(c.slice(3, 5), 16), parseInt(c.slice(5, 7), 16)];
}

/** Build a 256-entry RGB lookup table for a named ramp. */
export function ramp(name) {
  const stops = (RAMPS[name] || RAMPS.viridis).map(hex);
  const out = new Uint8Array(256 * 3);
  for (let i = 0; i < 256; i++) {
    const t = (i / 255) * (stops.length - 1);
    const a = Math.min(Math.floor(t), stops.length - 2);
    const f = t - a;
    for (let k = 0; k < 3; k++) {
      out[i * 3 + k] = Math.round(stops[a][k] + (stops[a + 1][k] - stops[a][k]) * f);
    }
  }
  return out;
}

/** CSS colour for one position along a ramp, for legends and swatches. */
export function rampColor(lut, t) {
  const i = Math.max(0, Math.min(255, Math.round(t * 255))) * 3;
  return `rgb(${lut[i]},${lut[i + 1]},${lut[i + 2]})`;
}

// -- theme --------------------------------------------------------------------

const INK = '#e6e9ee';
const MUTED = '#7d8794';
const GRID = '#20262e';
const AXIS = '#39414c';

export const THEME = { INK, MUTED, GRID, AXIS };

// -- 2D panel -----------------------------------------------------------------

/**
 * A single plot: device-pixel-ratio scaling, frame, gridlines and ticks, plus
 * drawing in data coordinates.
 *
 * Everything defaults to grey and one series carries the accent — the same
 * "build it all grey, then promote one thing" rule the figure contract uses.
 */
export class Panel {
  constructor(canvas) {
    this.c = canvas;
    this.ctx = canvas.getContext('2d');
    this.pad = { l: 48, r: 12, t: 10, b: 20 };
    this.clipped = false;
  }

  /** Resize to the element's CSS box at the display's pixel ratio. */
  fit() {
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(40, this.c.clientWidth);
    const h = Math.max(30, this.c.clientHeight);
    if (this.c.width !== Math.round(w * dpr) || this.c.height !== Math.round(h * dpr)) {
      this.c.width = Math.round(w * dpr);
      this.c.height = Math.round(h * dpr);
    }
    this.w = w;
    this.h = h;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  /**
   * Start a frame.
   * @param {{x0:number,x1:number,y0:number,y1:number,xticks?:number,
   *          yticks?:number,xfmt?:Function,yfmt?:Function}} o
   */
  begin(o) {
    this.fit();
    const { ctx } = this;
    this.o = Object.assign({ xticks: 5, yticks: 4, xfmt: fmtNum, yfmt: fmtNum }, o);
    this.x0 = o.x0; this.x1 = o.x1; this.y0 = o.y0; this.y1 = o.y1;
    if (!(this.x1 > this.x0)) this.x1 = this.x0 + 1;
    if (!(this.y1 > this.y0)) this.y1 = this.y0 + 1;

    ctx.clearRect(0, 0, this.w, this.h);
    ctx.save();
    ctx.font = '10px ui-monospace, SFMono-Regular, Menlo, monospace';
    ctx.textBaseline = 'middle';

    const P = this.pad;
    const iw = this.w - P.l - P.r;
    const ih = this.h - P.t - P.b;

    // Horizontal gridlines only, behind everything.
    ctx.strokeStyle = GRID;
    ctx.lineWidth = 1;
    ctx.fillStyle = MUTED;
    ctx.textAlign = 'right';
    for (let i = 0; i <= this.o.yticks; i++) {
      const v = this.y0 + (this.y1 - this.y0) * (i / this.o.yticks);
      const y = Math.round(this.py(v)) + 0.5;
      ctx.beginPath(); ctx.moveTo(P.l, y); ctx.lineTo(P.l + iw, y); ctx.stroke();
      ctx.fillText(this.o.yfmt(v), P.l - 5, y);
    }

    ctx.textAlign = 'center';
    for (let i = 0; i <= this.o.xticks; i++) {
      const v = this.x0 + (this.x1 - this.x0) * (i / this.o.xticks);
      const x = Math.round(this.px(v)) + 0.5;
      ctx.fillText(this.o.xfmt(v), x, this.h - P.b / 2);
    }

    // Left and bottom spines only.
    ctx.strokeStyle = AXIS;
    ctx.beginPath();
    ctx.moveTo(P.l + 0.5, P.t); ctx.lineTo(P.l + 0.5, P.t + ih);
    ctx.lineTo(P.l + iw, P.t + ih);
    ctx.stroke();

    ctx.save();
    ctx.beginPath();
    ctx.rect(P.l, P.t - 3, iw, ih + 6);
    ctx.clip();
    this.clipped = true;
  }

  /** Finish a frame (releases the clip and the saved state). */
  end() {
    if (this.clipped) { this.ctx.restore(); this.clipped = false; }
    this.ctx.restore();
  }

  px(v) {
    const P = this.pad;
    return P.l + ((v - this.x0) / (this.x1 - this.x0)) * (this.w - P.l - P.r);
  }

  py(v) {
    const P = this.pad;
    return P.t + (1 - (v - this.y0) / (this.y1 - this.y0)) * (this.h - P.t - P.b);
  }

  /**
   * Polyline over an indexed array. `xs` is optional; without it the index
   * maps linearly onto the x range. Non-finite values break the line rather
   * than being interpolated across — a gap in the data must look like a gap.
   */
  seriesY(ys, color, width = 1.25, xs = null) {
    const { ctx } = this;
    ctx.strokeStyle = color;
    ctx.lineWidth = width;
    ctx.beginPath();
    let pen = false;
    for (let i = 0; i < ys.length; i++) {
      const v = ys[i];
      if (!Number.isFinite(v)) { pen = false; continue; }
      const x = this.px(xs ? xs[i] : i);
      const y = this.py(v);
      if (pen) ctx.lineTo(x, y); else { ctx.moveTo(x, y); pen = true; }
    }
    ctx.stroke();
  }

  /** Filled band between two indexed arrays (percentile envelopes). */
  band(lo, hi, fill) {
    const { ctx } = this;
    ctx.fillStyle = fill;
    ctx.beginPath();
    for (let i = 0; i < hi.length; i++) ctx.lineTo(this.px(i), this.py(hi[i]));
    for (let i = lo.length - 1; i >= 0; i--) ctx.lineTo(this.px(i), this.py(lo[i]));
    ctx.closePath();
    ctx.fill();
  }

  /** Vertical bars from a baseline, one per array element. */
  bars(values, color, baseline = null) {
    const { ctx } = this;
    ctx.fillStyle = color;
    const base = this.py(baseline === null ? this.y0 : baseline);
    const w = Math.max(1, (this.w - this.pad.l - this.pad.r) / values.length - 1);
    for (let i = 0; i < values.length; i++) {
      if (!Number.isFinite(values[i])) continue;
      const y = this.py(values[i]);
      ctx.fillRect(this.px(i) - w / 2, Math.min(y, base), w, Math.abs(base - y));
    }
  }

  /** Scatter of paired arrays; `colorAt(i)` gives each point's colour. */
  scatter(xs, ys, colorAt, r = 1.4) {
    const { ctx } = this;
    for (let i = 0; i < xs.length; i++) {
      if (!Number.isFinite(xs[i]) || !Number.isFinite(ys[i])) continue;
      ctx.fillStyle = colorAt(i);
      ctx.beginPath();
      ctx.arc(this.px(xs[i]), this.py(ys[i]), r, 0, 6.2832);
      ctx.fill();
    }
  }

  /** A labelled reference line — every reference line must say what it means. */
  rule(axis, v, color, label) {
    const { ctx } = this;
    ctx.save();
    ctx.strokeStyle = color;
    ctx.setLineDash([3, 3]);
    ctx.lineWidth = 1;
    ctx.beginPath();
    if (axis === 'x') {
      const x = Math.round(this.px(v)) + 0.5;
      ctx.moveTo(x, this.pad.t); ctx.lineTo(x, this.h - this.pad.b);
    } else {
      const y = Math.round(this.py(v)) + 0.5;
      ctx.moveTo(this.pad.l, y); ctx.lineTo(this.w - this.pad.r, y);
    }
    ctx.stroke();
    ctx.restore();
    if (label) {
      ctx.fillStyle = color;
      ctx.textAlign = axis === 'x' ? 'left' : 'right';
      ctx.fillText(label,
        axis === 'x' ? this.px(v) + 3 : this.w - this.pad.r - 3,
        axis === 'x' ? this.pad.t + 6 : this.py(v) - 6);
    }
  }

  /** Corner annotation in pixel space (legends, units, live readouts). */
  note(text, corner = 'tr', color = MUTED) {
    const { ctx } = this;
    ctx.fillStyle = color;
    const top = corner[0] === 't';
    const right = corner[1] === 'r';
    ctx.textAlign = right ? 'right' : 'left';
    ctx.fillText(text,
      right ? this.w - this.pad.r - 2 : this.pad.l + 4,
      top ? this.pad.t + 6 : this.h - this.pad.b - 8);
  }
}

/** Compact numeric formatting for axis ticks. */
export function fmtNum(v) {
  const a = Math.abs(v);
  if (a >= 1e6) return (v / 1e6).toFixed(1) + 'M';
  if (a >= 1e4) return (v / 1e3).toFixed(0) + 'k';
  if (a >= 100) return v.toFixed(0);
  if (a >= 1) return v.toFixed(1);
  if (a === 0) return '0';
  return v.toFixed(2);
}

// -- scrolling heatmap --------------------------------------------------------

/**
 * A scrolling false-colour heatmap: the waterfall.
 *
 * Lines are written into a circular offscreen buffer and blitted in two pieces,
 * so adding k lines costs O(k) pixel writes and two draws rather than a
 * full-canvas scroll every frame. That matters: the stream delivers up to 600
 * lines a second and the browser gets 20 frames to show them.
 *
 * @param {'row'|'column'} axis  'row' scrolls downwards with the data axis
 *   horizontal (the CSI waterfall, as in Nexmon CSI); 'column' scrolls
 *   leftwards with the data axis vertical (the Doppler spectrogram, as in the
 *   radar-style plots the sensing literature uses).
 */
export class Heatmap {
  constructor(canvas, axis = 'row', depth = 600) {
    this.c = canvas;
    this.ctx = canvas.getContext('2d');
    this.axis = axis;
    this.depth = depth;
    this.bins = 0;
    this.cursor = 0;
    this.filled = 0;
    this.lut = ramp('viridis');
    this.rampName = 'viridis';
  }

  setRamp(name) {
    if (name === this.rampName) return;
    this.rampName = name;
    this.lut = ramp(name);
  }

  /** (Re)allocate the offscreen buffer when the data width changes. */
  resize(bins) {
    if (bins === this.bins && this.buf) return;
    this.bins = bins;
    this.cursor = 0;
    this.filled = 0;
    this.buf = document.createElement('canvas');
    if (this.axis === 'row') { this.buf.width = bins; this.buf.height = this.depth; }
    else { this.buf.width = this.depth; this.buf.height = bins; }
    this.bctx = this.buf.getContext('2d');
    this.bctx.fillStyle = '#05070a';
    this.bctx.fillRect(0, 0, this.buf.width, this.buf.height);
    this.line = this.bctx.createImageData(
      this.axis === 'row' ? bins : 1,
      this.axis === 'row' ? 1 : bins);
  }

  /** Append `rows` lines of `bins` bytes each from a flat Uint8Array. */
  push(bytes, rows, bins) {
    if (!bins || !rows) return;
    this.resize(bins);
    const d = this.line.data;
    for (let r = 0; r < rows; r++) {
      const off = r * bins;
      for (let i = 0; i < bins; i++) {
        const v = bytes[off + i] * 3;
        const p = i * 4;
        d[p] = this.lut[v]; d[p + 1] = this.lut[v + 1]; d[p + 2] = this.lut[v + 2]; d[p + 3] = 255;
      }
      if (this.axis === 'row') this.bctx.putImageData(this.line, 0, this.cursor);
      else this.bctx.putImageData(this.line, this.cursor, 0);
      this.cursor = (this.cursor + 1) % this.depth;
      this.filled = Math.min(this.filled + 1, this.depth);
    }
  }

  /** Blit the buffer so the newest line sits at the leading edge. */
  render() {
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(40, this.c.clientWidth);
    const h = Math.max(30, this.c.clientHeight);
    if (this.c.width !== Math.round(w * dpr) || this.c.height !== Math.round(h * dpr)) {
      this.c.width = Math.round(w * dpr);
      this.c.height = Math.round(h * dpr);
    }
    const ctx = this.ctx;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.imageSmoothingEnabled = false;
    ctx.fillStyle = '#05070a';
    ctx.fillRect(0, 0, this.c.width, this.c.height);
    if (!this.buf) return;

    // The split point is rounded to a whole device pixel and the second draw
    // starts exactly there, or the two halves leave a hairline seam that reads
    // as a real discontinuity in the data.
    const W = this.c.width, H = this.c.height;
    const tail = this.depth - this.cursor;
    if (this.axis === 'row') {
      // Newest at the top: draw [cursor..depth) then [0..cursor).
      const th = Math.round((tail / this.depth) * H);
      ctx.drawImage(this.buf, 0, this.cursor, this.bins, tail, 0, 0, W, th);
      if (this.cursor > 0) {
        ctx.drawImage(this.buf, 0, 0, this.bins, this.cursor, 0, th, W, H - th);
      }
    } else {
      // Newest at the right.
      const tw = Math.round((tail / this.depth) * W);
      ctx.drawImage(this.buf, this.cursor, 0, tail, this.bins, 0, 0, tw, H);
      if (this.cursor > 0) {
        ctx.drawImage(this.buf, 0, 0, this.cursor, this.bins, tw, 0, W - tw, H);
      }
    }
  }

  clear() {
    if (!this.bctx) return;
    this.bctx.fillStyle = '#05070a';
    this.bctx.fillRect(0, 0, this.buf.width, this.buf.height);
    this.cursor = 0;
    this.filled = 0;
  }
}

/** Draw a horizontal colour-ramp legend with its end labels. */
export function drawRampLegend(canvas, lutName, lo, hi, unit) {
  const lut = ramp(lutName);
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(40, canvas.clientWidth), h = Math.max(12, canvas.clientHeight);
  canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const barH = Math.max(4, h - 12);
  for (let x = 0; x < w; x++) {
    const i = Math.round((x / Math.max(1, w - 1)) * 255) * 3;
    ctx.fillStyle = `rgb(${lut[i]},${lut[i + 1]},${lut[i + 2]})`;
    ctx.fillRect(x, 0, 1, barH);
  }
  ctx.font = '9px ui-monospace, monospace';
  ctx.fillStyle = MUTED;
  ctx.textBaseline = 'bottom';
  ctx.textAlign = 'left';
  ctx.fillText(`${lo} ${unit}`, 0, h);
  ctx.textAlign = 'right';
  ctx.fillText(`${hi} ${unit}`, w, h);
}
