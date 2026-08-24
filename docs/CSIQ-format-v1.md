# CSIQ Interchange Format — Version 1

**Status**: stable · **Version**: 1 · **Magic**: `CSIQ` · **Extension**: `.csiq`

CSIQ is a small, self-describing container for Wi-Fi Channel State Information.
It exists because the useful CSI captures in circulation are stored in
driver-native formats that cannot be read without the driver's source: offsets
into a fixed header, no version, no provenance, no way to tell what a number
means. Such a capture is scientifically worthless a year later.

CSIQ's design goal is narrow and concrete: **a capture should be interpretable
by someone who has only the file and this document.**

## Design principles

1. **Self-describing.** Every field is tagged. A reader that does not recognise
   a field skips it; it never has to guess a layout from a version number.
2. **Provenance travels with the data.** The capture session's full metadata —
   radio configuration, firmware, kernel, regulatory domain — is embedded in the
   file header, not left in a sibling file that gets separated in transit.
3. **The raw stream is never replaced.** CSIQ is a *derived* format. The
   lossless, driver-native capture remains the source of truth (see
   [Appendix A](#appendix-a-the-raw-driver-stream)); CSIQ is what you publish,
   share, and archive alongside it.
4. **One codec, three uses.** The same record encoding serves the file
   container, the live stream, and any future transport. A field is defined once.
5. **Forward compatible by construction.** New fields get new type codes;
   readers ignore what they do not know. Ranges are reserved for 802.11be (EHT)
   and 802.11bf (sensing) so those do not require a format break.

All multi-byte integers are **little-endian** except the length prefixes of the
raw driver stream (Appendix A), which are big-endian for historical reasons.

## File layout

```text
┌─ FileHeader ───────────────────────────────────────────────┐
│ magic        4 B    "CSIQ"                                 │
│ version      u16    = 1                                    │
│ flags        u16    bit0 = session block present           │
│ session_len  u32    length of the session block            │
│ session      var    UTF-8 JSON (session_len bytes)         │
├─ Record  (repeated until EOF) ─────────────────────────────┤
│ tag          u8     = 0xA1                                 │
│ len          u32    payload length in bytes                │
│ payload      var    TLV sequence (see below)               │
└────────────────────────────────────────────────────────────┘
```

The `0xA1` record tag is a framing check: a reader that encounters any other
byte where a tag is expected knows the stream has desynchronised and must stop
rather than emit garbage.

### The session block

Opaque UTF-8 JSON. CSIQ does not constrain its schema — it is whatever the
capture tool recorded. `csid` embeds its session sidecar (`csid-session/1`),
which carries:

| Group | Fields |
|---|---|
| identity | `session_id`, `run_id`, `experiment`, `tag`, `schema` |
| radio | `interface`, `monitor`, `band`, `channel`, `control_freq_mhz`, `center_freq_mhz`, `width`, `interval_us`, `mac_filter` |
| filter | `frame_types`, `rate_n_flags_val`, `rate_n_flags_mask`, `count`, `timeout_us`, `fingerprint` |
| environment | `hostname`, `kernel`, `driver_module`, `firmware`, `regdomain`, `cpu_governor`, `csid_version`, `build` |
| lifecycle | `started_at`, `ended_at`, `status` |
| summary | `records`, `empty_records`, `capture_bytes`, `mean_rate_hz`, `tone_counts`, `live_dropped` |

Adding a group to the sidecar is **not** a CSIQ version bump. The block is
opaque to the container, and a reader that does not know a group ignores it.

#### `status` is true at close (since csid 0.2.0)

A published file describes a *finished* capture. Before csid 0.2.0 the export
re-read the sidecar from disk, and a segmented capture deliberately leaves that
file at `status: capturing` until the export lands — so `csid-sync` skips a
directory whose export was interrupted. The embedded block therefore said
`capturing` forever, on effectively every segmented file in the archive.

The export now takes the session block **by value**, so the on-disk file and the
embedded copy can be written in opposite orders. A file written by csid 0.2.0 or
later states its real outcome, and both copies carry one `ended_at`.

A file that says `capturing` was written by an earlier build. It is not evidence
that the capture was truncated.

#### `filter` — what the radio was allowed to report

A filter is a claim about the data. A capture that selected only data frames and
one that selected nothing are not the same measurement, so the selection in
force is recorded whether or not any of it is set.

`fingerprint` is a stable digest over `(frame_types, rate_n_flags_val,
rate_n_flags_mask)`, so two differently-filtered captures cannot land in one
poolable group by accident. `count` and `timeout_us` are excluded from it: they
bound how much the radio reports, not which frames it selects, so two captures
that differ only in duration stay poolable.

Two reserved values, and they are different facts:

| `fingerprint` | Meaning |
|---|---|
| `no-filter` | the radio filtered nothing |
| `""` (empty) | written before the group existed — not recorded |

The two selection knobs `csid` does drive are not repeated here. `csi_interval`
is `radio.interval_us` and `csi_addresses` is `radio.mac_filter`.

#### `environment.build` — which build wrote this file

`csid_version` is the semantic version and keeps its meaning. `build` names the
binary beside it:

| Field | Meaning |
|---|---|
| `revision` | `git describe` output, an operator-supplied identity, or empty |
| `revision_source` | `git` · `supplied` · `none` |
| `built_at` | compile time, RFC 3339 UTC |
| `rustc`, `profile` | compiler and build profile |
| `csiq_format_version` | the container version this build writes |

Read `revision_source` first. A build that cannot name its revision says `none`
and leaves `revision` empty — it never guesses one. That is the expected state
when the source was deployed without its `.git` directory, which is how the
capture fleet is built.

An all-empty `build` group means the file predates build provenance. Every such
file reports `csid_version = "0.1.0"`, because that literal was never bumped
while the daemon gained injection, time transfer, segmentation, the BLE scanner
and the empty-record counter. Those files cannot be distinguished by build, and
no later pass can recover it.

## Record payload — TLV

Each record payload is a flat sequence of fields:

```text
┌──────────┬──────────┬─────────────┐
│ type u8  │ len  u32 │ value (len) │
└──────────┴──────────┴─────────────┘
```

`len` is a `u32` (not `u16`) so a single field can carry a full EHT CSI matrix:
4096 tones × 8 chains × 2 × 2 bytes exceeds 64 KiB.

### Type codes

| Code | Name | Value encoding | Required |
|---:|---|---|:---:|
| `0x00` | *(reserved / padding)* | — | |
| `0x01` | `FTM` | `u32` — 320 MHz baseband timestamp | ● |
| `0x02` | `US` | `u32` — firmware microsecond clock | |
| `0x03` | `UNIX_TS_NS` | `u64` — host wallclock at delivery | |
| `0x04` | `RNF` | `u32` — raw `rate_n_flags` v2 word | |
| `0x05` | `PHY` | `u8` modulation, `u8` mcs, `u8` nss | |
| `0x06` | `NRX` | `u8` — receive chains | ● |
| `0x07` | `NTX` | `u8` — transmit spatial streams | ● |
| `0x08` | `NTONE` | `u16` — subcarriers | ● |
| `0x09` | `SRC_MAC` | 6 B | |
| `0x0A` | `CHANNEL` | `u32` — 802.11 channel number | |
| `0x0B` | `WIDTH` | `u16` — width code (below) | |
| `0x0C` | `RSSI` | `i16[]` — one per RX chain, **dBm** (negative); `-127` = no measurement | |
| `0x0D` | `SEQ` | `u8` — 802.11 sequence byte | |
| `0x10` | `CSI_MATRIX` | `i16[]` — interleaved I/Q | |
| `0x11` | `BW_ANTSEL` | `u8` bandwidth code, `u8` antenna mask (below) | |
| `0x12`–`0x1F` | *reserved* | further recoveries from the 272-byte driver header | |
| `0x20`–`0x2F` | *reserved* | 802.11be / EHT (RU allocation, per-RU tone maps) | |
| `0x30`–`0x3F` | *reserved* | 802.11bf sensing metadata | |

A reader **must** reject a record missing any field marked required, and
**must** silently skip unknown type codes.

### Width codes (`0x0B`)

| Code | Width |
|---:|---|
| 0 | NOHT |
| 1 | HT20 |
| 2 | HT40− |
| 3 | HT40+ |
| 4 | 80 MHz |
| 5 | 160 MHz |
| 6 | 320 MHz *(reserved; no 802.11be hardware yet)* |

Width is a property of the **monitor interface**, not of the record. It bounds
what is decodable; the actual CSI type follows the received frame. A 20 MHz HE
frame captured on a 160 MHz monitor still yields 242 tones.

#### Corrigendum: `WIDTH` is a session constant (2026-08-24)

`WIDTH` was easy to read as the frame's bandwidth. It is not, and never was.

An ambient channel interleaves PHY types frame by frame, so on those captures
`WIDTH` is the *configured monitor width* for every record and describes none of
them. Consumers compensated by inferring tone spacing from the modulation label
and falling back on tone count, which cannot separate HE20 from VHT80 — both
carry 242 tones, four times apart in spacing.

The field is **retained unchanged**, because what it records is real and worth
recording: it bounds what the receiver could decode. A wrong description is
fixed by corrigendum, never by redefining a field that files already carry.

The frame's own bandwidth is `BW_ANTSEL` (`0x11`). Prefer it for anything that
describes a record. Prefer `WIDTH` for anything that describes the receiver.

### Bandwidth and antenna codes (`0x11`)

`u8 bandwidth_code`, then `u8 antenna_sel`. Both are decoded from the
`rate_n_flags` word that `RNF` (`0x04`) already carries verbatim, so **every
CSIQ file ever written contains these bits** — a reader that wants them from an
older file can decode `0x04` itself, with no re-capture.

| Code | Bandwidth |
|---:|---|
| 0 | 20 MHz |
| 1 | 40 MHz |
| 2 | 80 MHz |
| 3 | 160 MHz |
| 4 | 320 MHz *(reserved; no 802.11be hardware yet)* |

The codes are the driver's own `RATE_MCS_CHAN_WIDTH_*` values, so no table sits
between the firmware and the file. An unrecognised code is carried verbatim and
**must not** be coerced to 20 MHz.

`antenna_sel` is a two-bit mask, not an index: bit 0 = antenna A, bit 1 =
antenna B. It is the driver's `RATE_MCS_ANT_A_MSK` / `_B_MSK` shifted down from
bits 14–15. A value of `0` means the word named no antenna, which is the normal
state of a receive record — the field is populated on transmit descriptors.

**An absent `0x11` is not 20 MHz.** It means the writer did not record the
field, or `rate_n_flags` was unavailable for that record.

**Reader rule.** When `0x11` is absent but `RNF` (`0x04`) is present, a reader
**should** decode these fields from `RNF` and report them as if the writer had
emitted them. `BW_ANTSEL` *is* that decode performed at write time, so the two
paths return the same bits, and this is what makes every file written before
csid 0.2.0 carry per-frame bandwidth with no re-capture. When neither is
present, the field is genuinely absent and **must** be reported as such. Both
reference readers implement this rule.

#### Deriving the tone grid

With per-record bandwidth the tone grid is derivable rather than assumed. The
occupied span cannot exceed the channel, so:

```
spacing = 312.5 kHz   if  ntone × 312.5 kHz ≤ bandwidth
          78.125 kHz  otherwise
```

242 tones in 20 MHz is HE20 (75.6 MHz would not fit); 242 tones in 80 MHz is
VHT80. The `PHY` label (`0x05`) remains authoritative when present — this rule
is what resolves the case where it is absent.

### Modulation codes (`0x05`)

| Code | Modulation |
|---:|---|
| 0 | CCK |
| 1 | Legacy OFDM |
| 2 | HT (802.11n) |
| 3 | VHT (802.11ac) |
| 4 | HE (802.11ax) |
| 5 | EHT (802.11be) *(reserved)* |

### The CSI matrix (`0x10`)

An array of little-endian `i16` pairs, `2 x ntone x nrx x ntx` values, stored
exactly as the driver produced them. **Two properties of that storage are not
what a naive read assumes, and getting either wrong silently corrupts every
phase-derived result:**

**1. The layout is chain-major.** The payload is `nrx*ntx` *contiguous blocks*
of `ntone` coefficients — not tone-interleaved:

```text
[chain 0: tone 0 .. tone N-1][chain 1: tone 0 .. tone N-1] ...

index(chain c, tone t) = 2 * (c * ntone + t)
```

This is the vendor reader's own loop order (`for rx { for tx { for tone { ... } } }`,
`iaxcsi.m`), and it is confirmed empirically: reading chain-major yields a more
compact channel impulse response than the tone-major reading in **99.4 % of
5 186 records** (mean gain in top-4-tap energy +0.115, 95 % CI [+0.113, +0.117]),
across both legacy 52-tone and HT 56-tone frames.

**2. Each coefficient is imaginary first, then real.**

```text
value(c, t) = iq[index+1] + i * iq[index]
```

The vendor readers do the same (`imag = le16i(buf(pos:pos+1)); real = le16i(buf(pos+2:pos+3))`).
Reading it the other way round yields `i * conj(H)` — which leaves `|H|`
untouched, so amplitude work looks perfectly healthy while every phase is
mirrored. The tell is causality: on real captures the correct order concentrates
**21.5x** more impulse-response energy at early delays than at late ones, while
the swapped order inverts that ratio to 0.48 — an anti-causal "channel", which
is physically impossible.

Both reference readers expose a tone-major *view* (`[ntone, nrx*ntx]`) built
from this storage, so consumers keep a stable shape without having to know the
on-disk order. Use `chain()` when you want one chain's contiguous response.

**Amplitude is AGC-normalised.** `|H|` carries the channel's *shape* only; the
correlation between `|H|²` and RSSI is ≈ 0.01 on the reference hardware. Any
absolute scale must come from the `RSSI` field.

**RSSI is dBm, and the sign is applied at parse time.** The driver header
carries RSSI as a **`u8` positive magnitude** — the vendor reader's own
`iaxcsi.h` declares `uint8_t opp_rssi1; uint8_t v61[3];`, i.e. one byte followed
by three reserved bytes, and both its C++ and MATLAB readers print
`-opp_rssi1`. Both reference readers here negate it, so a `CsiRecord` always
carries ordinary negative dBm and no consumer has to know the driver's
convention. Observed valid range on the reference hardware: **-18 ... -89 dBm**,
a smooth unimodal continuum over 659 083 records.

**`-127` dBm means "this chain reported no measurement".** The firmware writes
the magnitude `0x7F`; this is Intel's documented not-available marker, the same
value as `IWL_NOISE_MEAS_NOT_AVAILABLE` in the driver's `dvm/dev.h`, chosen
there because it "is below the range of measurable". Treating it as a very weak
signal is wrong in two independent ways: -127 dBm sits roughly 26 dB *below* the
thermal noise floor of a 20 MHz channel (kTB is about -101 dBm), so it cannot be
a measurement; and empirically the sentinel is a discrete spike separated from
the real distribution by a 60 dB gap, not a tail.

> **Consumer rule.** When a chain reports `-127`, that chain's slice of
> `CSI_MATRIX` **must be discarded** — it is a byte-identical stale copy of an
> earlier frame, not a weak measurement. Verified as an exact biconditional over
> 44 577 records: a chain reads `-127` if and only if its CSI block duplicates
> the previous one. A record whose other chain is valid remains usable
> single-chain; records with *both* chains at `-127` have never been observed.
> Both reference readers expose `chains_measured()` / `fully_measured()` for
> exactly this.

`0` is **not** a sentinel and has never been observed (703 660 records).

**Phase is usable, with alignment.** Raw phase is dominated by carrier frequency
offset. The inter-chain conjugate product is concentrated (circular σ ≈ 2.2°)
around two discrete per-packet states ≈ 15° apart; aligning per record leaves
≈ 13° residual — enough for AoA and Doppler work.

## The three clocks

Every record can carry three timestamps. They are not redundant; they answer
different questions.

| Field | Source | Resolution | Wraps | Use for |
|---|---|---|---|---|
| `FTM` | radio baseband, 320 MHz | 3.125 ns | ≈ 13.42 s | **all timing analysis** |
| `US` | firmware | 1 µs | ≈ 71.6 min | coarse cross-checks |
| `UNIX_TS_NS` | host kernel at delivery | ns (µs-jittered) | — | **wallclock anchoring** |

**The rule: analyse on `FTM`, anchor wallclock on `UNIX_TS_NS`.** `FTM` is
stamped in the RF plane before any host software runs, so it is immune to
scheduling jitter (measured host delivery jitter: p50 19 µs, p95 57 µs,
p99.9 5.4 ms). `UNIX_TS_NS` is NTP-disciplined and therefore comparable across
nodes; `FTM` is not, because each radio's clock is free-running.

Because `FTM` wraps every ~13.4 s, readers must unwrap it. Records arrive in
order, so a value lower than its predecessor implies exactly one wrap. Both
reference implementations provide an `FtmUnwrapper`.

> **Cross-node alignment.** Two nodes tuned to the same channel stamp the *same*
> ambient frames on their own 320 MHz clocks. Pairwise offset and drift can
> therefore be estimated from the captures themselves at sub-microsecond
> precision, with no clock-distribution protocol. NTP on `UNIX_TS_NS` is enough
> to bootstrap the pairing.

### A caution on `SEQ`

`SEQ` is the low byte of the 802.11 sequence number. It is **not** a reliable
completeness counter: it is 8 bits of a per-TID 12-bit field, observed over a
subsampled frame population. Do not compute loss from it. True drop accounting
requires a known injected cadence.

## Live datagrams

One record per datagram, self-contained so a subscriber can join mid-stream.

```text
┌──────────────┬──────┬───────────────┬──────────┬──────────────┐
│ magic "CL" 2B│ ver 1│ session_uid u64│ seq  u32 │ payload TLV  │
└──────────────┴──────┴───────────────┴──────────┴──────────────┘
```

`session_uid` is stable for a capture session, so a subscriber can detect a new
session. `seq` increments per datagram; **gaps are meaningful** — they are
sender-side drops from the bounded best-effort queue, and counting them is how a
consumer knows it is falling behind. The payload is byte-identical to a file
record's payload.

## Versioning policy

- **Adding a type code** is not a version bump. Readers skip what they do not
  know; writers may emit new fields freely.
- **Changing the meaning or encoding of an existing type code** requires a
  version bump.
- **Structural changes** (framing, header layout) require a version bump.
- A reader encountering a `version` it does not implement **must** refuse the
  file rather than guess.

### Corrigendum: RSSI sign (2026-07-22, pre-release)

The `RSSI` type code originally said only "dB" and did not state a sign
convention, so writers emitted the driver's positive magnitude verbatim. It now
specifies **dBm**, and both reference readers negate at parse time.

This is a *specification defect being closed*, not a change of meaning, so it
does not bump the version — the field never had a defined sign to change. The
decision rests on v1 being pre-release: the only affected files are on the
reference node.

**Any `.csiq` written before this change carries positive magnitudes.** They
are not silently wrong-but-plausible — a `+53` RSSI is physically impossible —
so they are easy to spot. Re-derive them from the lossless source:

```console
$ csid export /var/lib/csid/<session>      # rewrites capture.csiq from capture.raw
```

This is exactly the property the two-layer data model exists for: `capture.raw`
is never rewritten, so a parser defect can be corrected after the fact without
the archive having lost anything.

## Reference implementations

| Language | Location | Notes |
|---|---|---|
| Rust | [`crates/csiq`](../crates/csiq) | Writer + reader + raw parser; no OS dependencies |
| Python | [`python/csiq`](../python/csiq) | Reader only; pure stdlib, NumPy optional for `matrix()` |

If the two disagree, **this document is authoritative** and both are bugs.

## Appendix A: the raw driver stream

`csid` also writes `capture.raw` — the driver's bytes, verbatim, in the framing
the upstream `iaxcsi` reader used, so existing tooling keeps working. It is the
lossless source of truth; CSIQ is derived from it.

```text
[be32 msg_len][be32 hdr_len][hdr (hdr_len B)][be32 csi_len][csi (csi_len B)]
```

Note the **big-endian** length prefixes. The header is 272 bytes on the
reference hardware, with little-endian fields at these offsets:

| Offset | Field | Type |
|---:|---|---|
| 8 | `ftm` | `u32` |
| 46 | `nrx` | `u8` |
| 47 | `ntx` | `u8` |
| 52 | `ntone` | `u16` (low half of the vendor struct's `u32`) |
| 60 | `rssi_a` | `u8` — positive magnitude; readers negate into dBm (61-63 reserved) |
| 64 | `rssi_b` | `u8` — positive magnitude; readers negate into dBm (65-67 reserved) |
| 68 | `src_mac` | 6 B |
| 76 | `seq` | `u8` |
| 88 | `us` | `u32` |
| 92 | `rnf` | `u32` |
| 208 | `unix_ts_ns` | `u64` |
| 216 | `channel` | `u8` (217-219 reserved) |

This layout is **driver-coupled** — it is the one part of the pipeline that a
firmware or driver revision can invalidate. That is precisely why CSIQ exists:
convert once, at capture time, and downstream consumers never depend on it.

`rate_n_flags` v2 (`rnf`) decodes as: MCS in bits 0–3, `NSS−1` in bits 4–5,
modulation type in bits 8–10.

## Appendix B: what CSIQ deliberately is not

- **Not a signal-processing format.** No filtering, calibration, or phase
  sanitisation is applied or described. Those are analysis choices and belong in
  analysis code, where they can be varied and reproduced.
- **Not a columnar analytics format.** Convert to Parquet/Zarr for analysis at
  scale; CSIQ is the archival and interchange layer that such derivatives are
  generated *from*.
- **Not a capture-time compression format.** Records are stored as the hardware
  produced them.
