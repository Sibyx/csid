# csiscope — the live CSI console

A second binary that subscribes to `csid`'s live stream and serves a browser
console: every representation the Wi-Fi sensing literature actually uses, plus
the node's configuration and unit control, on one unauthenticated page.

```console
$ csiscope                                  # http://127.0.0.1:8088
$ csiscope --bind 0.0.0.0:8088              # reachable from a laptop
$ csiscope --udp-bind 0.0.0.0:5599          # off-node, via [stream] transport = "udp"
$ csiscope --read-only                      # views only: no config, no unit control
```

It is a **strict consumer**. It never touches the radio, never writes to the
capture path, and holds nothing the daemon needs. If it dies mid-experiment the
capture is unaffected — that is what `csid`'s best-effort live path is for.

## Why a console at all

`csid validate` tells you a configuration is legal. `csid caps` tells you what
the hardware can do. Neither tells you whether the thing you are capturing
*right now* is what you think it is — and on a shared channel it usually is not.
The console exists for the two questions that only a live view answers:

- **Debugging.** Is this really CSI? Is the driver ABI still what the parser
  assumes? Why is the rate 8 Hz instead of 600? Which transmitter is actually
  sounding the channel?
- **Showing the work.** A waterfall and a Doppler spectrogram of a real channel
  are the most direct explanation of what this project measures.

## Capture yield — read this before the record class

`records / frames_seen`, first tile on the strip and first panel on the page.

It cannot be computed from the live stream, which carries records and nothing
else. `csid` publishes it to `/run/csid/status.json` about once a second, and
the console polls that file.

The reason it leads is that its most important reading is the one where the
stream is silent. Measured on 2026-08-17, a `smoke-bench-ch11` session received
**3915 frames and produced 0 CSI records**. Without the denominator that is four
indistinguishable states — a quiet room, a mistuned radio, a dead driver, a
stopped unit — and it is none of them. On 2.4 GHz the usual cause is DSSS/CCK,
which has no OFDM preamble and therefore no channel estimate to report. The
channel was busy. Every frame was seen. None could become CSI.

The verdict is banded per band, because the bands genuinely differ. Over that
day's channel survey, 5 GHz yielded a median **99.4%** across 57 sessions while
2.4 GHz yielded **3.5%** across 18:

| band | ≥ 80 % | 20–80 % | < 20 % | no frames |
|---|---|---|---|---|
| 5 / 6 GHz | `ok` | `low` | `bad` | `no frames` |
| 2.4 GHz | `ok` | `ok` | `low` — expected | `no frames` |

The panel also carries the **run id**, which is what makes a multi-node capture
one addressable object, and says plainly when `csid` generated one — a generated
id groups nothing but its own session.

When no status file can be read — no capture running, or an off-node console on
a UDP stream — the panel says so rather than showing a zero.

## The record class — read this first

`csid caps` states it plainly: **CSI type follows the received frame.** An
ambient capture on a busy channel is therefore not one signal but several
interleaved ones — legacy 52-tone beacons, HT 56-tone data frames, HE 242-tone
bursts — arriving in the same second from different transmitters.

Measured on channel 11 at the reference site: 74% legacy 52-tone, 26% HT
56-tone, mixed packet by packet.

A console that renders "the newest record" therefore flickers between
incompatible geometries: the PHY label blinks several times a second, the
spectrum changes width, the waterfall has no stable column count, and a time
series compares samples that are not measurements of the same thing.

So the console has one organising idea:

> Every analytical view is scoped to exactly **one record class** —
> `(tone count, modulation)`. The operator picks it in the rail; the default is
> whichever class dominates the analysis window.

## …and one transmitter

The class axis is right on an ambient channel and does nothing on an illuminated
one. Both coexistence sessions of 2026-08-17 were ~100% a single class, so the
class selector had nothing to select.

The axis that carried the structure was the transmitter. The 5 GHz capture held
**twelve** of them and the injector was 54.3% of records, so its pooled
inter-arrival p50 of 6.1 ms described no transmitter at all — it was one 100 Hz
metronome interleaved with eleven ambient talkers, and every deep view was being
computed over the mixture.

> Every analytical view is *also* scoped to exactly **one transmitter**. The
> default is the busiest of the selected class, which on a lit channel is the
> injector.

The same safeguard as the class axis applies. The selector's list is built
before the scope is applied, and the talker table beside it counts the whole
channel, so choosing one transmitter can never hide the rest. The status bar
states all three window sizes — records in the window, of the class, of the
transmitter — because two scopes shrink the analysis window and a Doppler
spectrogram over 139 records when 256 were requested is a different
measurement.

Rows the waterfall does not draw are counted in three separate buckets: not kept
up with, wrong class, wrong transmitter. Only the first is a shortfall of the
display.

**Nothing is dropped from the capture.** The scope is a *display* decision.
`capture.raw` receives every record regardless, and the console shows the full
census in two places:

- **Everything on the channel** — every class in the window, with its share and
  its own record rate. Click a row to scope the deep views to it.
- **The waterfall's `all` scope** — draws *every* record, resampled onto a
  shared frequency axis so a narrow legacy frame occupies its true slice of the
  channel next to a wider HE one.

If you want to *stop capturing* a class, that is a `csid` decision, not a
console one: the driver's `csi_frame_types` and `csi_rate_n_flags_mask` debugfs
knobs filter at the source. `csid` does not expose them yet, and doing so would
mean those frames are never recorded — the opposite of what the console does.

## The views, and where they come from

| Panel | What it shows | Grounding |
|---|---|---|
| **Capture** | yield, run id, tuned channel, commanded interval, BLE | `csid`'s own status document — the half the live stream cannot report |
| **Metronome** | delivered against commanded, inter-arrival as slot multiples | measured: 61.3 Hz against a commanded 100 Hz on 2.4 GHz, 99.4 Hz on 5 GHz |
| **Band plan** | where BLE's channels fall in this tone grid, over the median `\|H(f)\|` | the EXP-010 spectral derivation; ABBA-probe artefact regions |
| **CSI ratio** | `H_a/H_b` — phase without a fit | FarSense (Zeng et al. 2019) |
| **Subcarrier statistics** | per-tone median, temporal spread, null fraction | ABBA probe: 55% of spurious events in the outer three tones |
| **Waterfall** | `\|H\|` in dB, subcarrier × time | Gringoli et al. 2019 (Nexmon CSI) — the canonical live CSI view |
| **Spectrum & bundle** | current `\|H(f)\|` against the p05–p95 envelope over the window | Choi et al. 2021/2022 — bundle *width* is the occupancy feature |
| **Doppler spectrogram** | STFT of the conjugate product between two chains | Li et al. 2022 (STFT); Zheng et al. 2019 (conjugate multiplication) |
| **Phase** | raw / unwrapped / sanitised | Ma et al. 2020 §Phase Offsets Removal (SpotFi, SignFi linear regression) |
| **Impulse response** | power–delay profile from the Hann-windowed IFFT of `H(f)` | Bocus et al. 2022 |
| **Complex plane** | `H` per subcarrier, coloured by index | Chen et al. 2023 Fig. 2 |
| **Chains** | `\|H(f)\|` per RX·TX chain | Gringoli et al. 2019 — different chains prove a real measurement |
| **Extraction checks** | DC null, band-edge roll-off, distinct chains, zero fraction | Gringoli et al. 2019 §"Crime Scene Investigation" |
| **Inter-arrival / clocks** | jitter distribution and clock divergence on the 320 MHz baseband clock | this repository's own timing rule |

### Things the views will not let you misread

**Amplitude is AGC-normalised.** `|H|` carries channel *shape*, never absolute
scale. Every amplitude axis is relative, and the absolute anchor is the RSSI
panel.

**RSSI is shown as the driver reports it.** On the iax/AX210 path the value is a
*positive magnitude* — measured on hardware, strong talkers read ≈50 and weak
ones ≈85, monotone with distance. The console does not silently negate it into
dBm; it plots what the driver delivered and says so. (Whether `csiq` should
normalise the sign at parse time is an open format question, not a console one.)

**Raw phase is not the channel.** It is dominated by carrier and sampling
frequency offsets. The sanitised view subtracts a least-squares line across the
band, which removes the constant (CFO) and linear-in-subcarrier (SFO/STO) terms
— and takes any genuinely linear part of the channel with it.

**The Doppler axis assumes uniform sampling, and ambient traffic is not.**
Packets arrive when somebody transmits, so the series is nearest-neighbour
resampled onto a uniform grid. **For Doppler work that has to hold up, throttle
the capture to a regular rate with `radio.interval_us`.**

**But the coefficient of variation is the wrong test for a throttled source,**
and the console no longer uses it as one. Measured on 2026-08-17, one injector
at a commanded 10 ms slot:

| | 2.4 GHz ch6 | 5 GHz ch36 |
|---|---|---|
| delivered | 61.3 Hz | 99.4 Hz |
| p50 / p95 / p99.9 | 10.00 / 40.00 / 80.00 ms | 10.00 / 10.03 / 20.02 ms |
| CV | 0.714 | 0.083 |

The percentiles are exact integer multiples of the slot. The 2.4 GHz source was
not jittering — it was losing whole slots, and the old readout's `CV 0.71 →
qualitative` was the wrong verdict for a process whose surviving arrivals are
all on grid. The metronome panel decides this instead, on two tolerances, and
reports one of three mechanisms:

- **`on grid`** — ≥90% of gaps within a quarter-slot of a multiple. Resampling
  is near-exact and the axis is quantitative, whatever the CV says.
- **`deferred`** — a clear mode on the grid with a population pushed off it. On
  2.4 GHz that is CSMA/CA: the radio waits for a clear channel, so a frame is
  delayed by a random backoff rather than merely dropped. Measured on that arm,
  65.8% of gaps sit within 2% of a multiple while the off-grid remainder lands
  at a uniform phase (fractional part p25/p50/p75 = 0.40/0.52/0.63). Only the
  dropped half resamples cleanly, so the axis is approximate.
- **`irregular`** — no mode at all. Ambient traffic; the axis is qualitative.

The Doppler readout defers to that verdict rather than to the CV.

**Zero subcarriers are not measurements.** 802.11 nulls DC and the guard bands
and the driver delivers exact zeros; the console floors them at one LSB of the
`i16` grid (0 dB) and excludes them from automatic axis ranges.

**The tone axis is a grid, not a ruler.** Because 802.11 never transmits on DC,
the delivered tones are two runs with a hole between them, and the console used
to map array position to frequency as `(i − n/2 + 0.5) · spacing` — which put
the outermost 52-tone legacy tone at +7.97 MHz where it is physically
+8.125 MHz. `tones.rs` now carries the used-tone set for each delivered tone
count and every frequency axis reads from it. Half a subcarrier sounds like
nothing until it decides an experiment: the same arithmetic places BLE
advertising channel 39, on Wi-Fi ch13, at array index **50.6 of 51** — inside
the band-edge region — which is what moved EXP-010's inclusion arm to ch3.

Two transforms deliberately still treat the tones as contiguous, matching the
Python service exactly rather than diverging from it: `cir` does not zero-pad
the DC hole onto its true FFT bin, and `detrend` fits over array index rather
than subcarrier `k`, so a 52-tone `tau_ns` is about 2% off. Both are bounded,
neither changes a shape, and neither should be quoted as an absolute delay.

**The waterfall says how much it is not showing.** The stream can exceed the
frame rate thirtyfold; the readout reports the percentage of records that
actually reached the display, rather than implying it drew everything.

**The impulse response is Hann-windowed.** A rectangular band edge produces sinc
sidelobes that are indistinguishable from multipath by eye. The window costs
about a factor of two in main-lobe width and buys ~30 dB of sidelobe
suppression. Bin spacing is interpolation; the true resolution is `1/BW`, which
the readout states.

## Configuration and control

The **Config** tab reads `/etc/csid/config.toml` and `/etc/csid/experiments/`,
edits them, and drives `systemctl`.

Validation is not reimplemented: an experiment is parsed with `csid`'s own
`ExperimentConfig` and checked with the same `caps::validate_radio` the daemon
runs, so the console rejects exactly what `csid validate` rejects — including
"160 MHz is not valid on 2.4 GHz" and "channel 132 belongs to no 160 MHz group".
An invalid configuration is never written. Writes are atomic (temp file +
rename), so a half-written file can never be what `systemctl start` picks up.

**Sessions** browses the spool and shows each sidecar verbatim; **Node** shows
the capability envelope and runs `csid doctor`.

## Security

The console has **no authentication**. It is a lab instrument, and on anything
but loopback it hands node configuration and unit control to whoever can reach
the port. That is a deployment constraint, not an oversight:

- `--bind` defaults to `127.0.0.1:8088`, so exposing it is an explicit act.
- `--read-only` serves every view and refuses every write.
- Unit control is restricted to an allowlist of this project's own units, and
  every name reaching the filesystem or `systemctl` is checked against a strict
  pattern first. Nothing is interpolated into a shell.

Reach it over the tailnet or an SSH tunnel rather than binding `0.0.0.0`:

```console
$ ssh -N -L 8088:127.0.0.1:8088 monad05
```

## One subscriber per socket

A Unix **datagram** socket has exactly one owner: the process that binds the
path. While `csiscope` is bound to `/run/csid/live.sock`, `csid stream` cannot
also attach — they are alternative subscribers, not concurrent ones.

To watch from a second machine, or alongside the CLI, set the experiment's
`[stream] transport = "udp"` with a target and run the console with
`--udp-bind`.

## Keyboard

| Key | Action |
|---|---|
| `space` | pause / resume the analysis (the capture is untouched either way) |
| `P` | presentation mode — hide the chrome, enlarge the panels |
| `A` | auto-scale the waterfall to the current signal |
| `Esc` | leave presentation mode |

## Deployment

The Ansible `csid` role builds and installs `csiscope` alongside the daemon and
manages `csiscope.service`. The relevant knobs:

```yaml
csid_csiscope_enabled: true
csid_csiscope_bind: "127.0.0.1:8088"   # widen only with read_only, or use the tailnet
csid_csiscope_read_only: false
csid_csiscope_autostart: true
csid_csiscope_history: 8192            # records retained for the windowed views
csid_csiscope_coeff_budget: 4000000    # I/Q coefficient ceiling (~16 MiB)
csid_status_path: /run/csid/status.json  # what the yield panel reads; "" disables
```

Arguments are rendered into `/etc/csid/csiscope.env`; the unit reads them from
there, so changing the bind address is a config change rather than a unit edit.

Unlike `csid@` sessions — which are never restarted automatically, because that
would destroy in-flight data — the console is bounced on every update. It is a
pure consumer, so restarting it cannot disturb a capture.

## Cost

Analysis runs per connected client, on a blocking thread, at the client's frame
rate. Two bounds keep it flat on a Pi 5 that is also capturing:

- the ring is bounded by record count **and** a total I/Q coefficient budget, so
  the footprint does not scale with tone count;
- windowed statistics run over a decimated subset of the window, and the
  waterfall carries only what the frame rate can show;
- the transmitter scope makes the analysis window smaller, never larger, so the
  five panels added in IP-132 cost nothing measurable — the frame benchmark
  moved by at most +1.9% at 996 tones, and improved by 10.6% on one case.

The unit runs at `Nice=10` with the default scheduling policy, so it can never
compete with `csid`'s `SCHED_RR` RX thread for a core.
