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
| identity | `session_id`, `experiment`, `tag`, `schema` |
| radio | `interface`, `monitor`, `band`, `channel`, `control_freq_mhz`, `center_freq_mhz`, `width`, `interval_us`, `mac_filter` |
| environment | `hostname`, `kernel`, `driver_module`, `firmware`, `regdomain`, `cpu_governor`, `csid_version` |
| lifecycle | `started_at`, `ended_at`, `status` |
| summary | `records`, `capture_bytes`, `mean_rate_hz`, `tone_counts`, `live_dropped` |

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
| `0x0C` | `RSSI` | `i16[]` — one per RX chain, **dBm** (negative); `0` = no measurement | |
| `0x0D` | `SEQ` | `u8` — 802.11 sequence byte | |
| `0x10` | `CSI_MATRIX` | `i16[]` — interleaved I/Q | |
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

Interleaved little-endian `i16` pairs — `re, im, re, im, …` — of length
`2 × ntone × nrx × ntx`, ordered tone-major:

```text
index(tone t, chain c) = 2 * (t * (nrx*ntx) + c)
```

Reshaped, this is `[ntone, nrx*ntx]` complex.

**Amplitude is AGC-normalised.** `|H|` carries the channel's *shape* only; the
correlation between `|H|²` and RSSI is ≈ 0.01 on the reference hardware. Any
absolute scale must come from the `RSSI` field.

**RSSI is dBm, and the sign is applied at parse time.** The driver header
carries RSSI as a *positive magnitude* — Intel's convention for the `__le32`
RSSI fields in `fw/api/stats.h`. Measured on the reference node across 20 000
consecutive records: range 47…89, no negative and no zero value, monotone with
distance. Both reference readers negate it, so a `CsiRecord` always carries
ordinary negative dBm and no consumer has to know the driver's convention. A
`0` is passed through unchanged and means *this chain reported nothing* — not
0 dBm.

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
| 52 | `ntone` | `u16` |
| 60 | `rssi_a` | `i32` — positive magnitude; readers negate into dBm |
| 64 | `rssi_b` | `i32` — positive magnitude; readers negate into dBm |
| 68 | `src_mac` | 6 B |
| 76 | `seq` | `u8` |
| 88 | `us` | `u32` |
| 92 | `rnf` | `u32` |
| 208 | `unix_ts_ns` | `u64` |
| 216 | `channel` | `u32` |

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
