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
| `0x12` | `MONO_US` | `u64` — `CLOCK_MONOTONIC` microseconds (below) | |
| `0x13` | *reserved, unused* | see the `SEQ` corrigendum | |
| `0x14` | `VENDOR_HDR` | the 272-byte driver header, verbatim | |
| `0x15`–`0x1F` | *reserved* | further recoveries from the 272-byte driver header | |
| `0x20`–`0x2F` | *reserved* | 802.11be / EHT (RU allocation, per-RU tone maps) | |
| `0x30`–`0x3F` | *reserved* | 802.11bf sensing metadata | |
| `0x40` | `NODE_TEMP_MC` | `i32` — SoC die temperature, millidegrees C | |
| `0x41` | `NODE_THROTTLE` | `u32` — firmware throttle bitmask | |
| `0x42` | `NODE_SPOOL_FREE` | `u64` — bytes free on the capture spool | |
| `0x43` | `NODE_LOAD_M` | `u32` — 1-minute load average × 1000 | |
| `0x44` | `NODE_NIC_TEMP_C` | `i32` — Wi-Fi NIC die temperature, **whole degrees** C | |
| `0x45`–`0x4F` | *reserved* | further node and host state | |

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

### `MONO_US` (`0x12`) — the clock a time step cannot move

`u64`, microseconds of `CLOCK_MONOTONIC` on the capturing host, from driver
header offset 200.

Every other clock in the record fails a different way. `FTM` wraps every
13.42 s and is free-running per radio. `US` wraps every 71.6 min. `UNIX_TS_NS`
is exactly the field an NTP step corrupts — and on a fleet with no RTC, nodes
boot in the past and get stepped, mid-capture, with **no symptom in the file**.
This is the only monotonic wall-time a record carries.

Measured on a real capture: strictly monotonic over its non-zero samples,
spanning 60.007 s against the host clock's 60.008 s — 3 ppm — with an implied
uptime of 110.9 h at the first sample.

#### Absent means "own transmission", not "unavailable"

This is the rule to read before using the field. Across 2,433 records, as an
exact biconditional with no exceptions:

| source MAC | `MONO_US` present | records |
|---|---|---:|
| the capturing node's own injector | **no** | 1,743 |
| three ambient transmitters | **yes** | 690 |

A locally generated frame never traverses the receive path that stamps this
clock. So an absent `MONO_US` is a **semantic marker**: this record is the
node's own transmission, looped back. It is the only per-record marker for that
fact the CSI stream carries — the time-transfer receiver counts the same thing
separately, as `own_transmissions`.

A consumer wanting only genuinely received frames therefore filters on
`MONO_US` being present. **Do not read a missing value as zero, and do not read
it as a broken clock.**

Verified on one node and one capture. A future capture that breaks the
biconditional is a finding, not noise.

### `VENDOR_HDR` (`0x14`) — the header, kept

The 272-byte driver header verbatim, at the offsets [Appendix A](#appendix-a-the-raw-driver-stream)
gives. The **whole** header, not only the bytes no type code claims: a field
promoted out of the blob later must keep the offset the appendix documents, and
that only holds if the blob is the header rather than a subset of it.

This exists because of what it would have saved. Per-frame bandwidth sat
unread in `rate_n_flags` for the whole life of the archive and was recoverable
only because that word happened to be stored already. The next field will not
be so lucky. With the blob, a recovery costs a decoder; without it, a re-capture.

Measured on a real capture: 203 of 272 header bytes are constant across
records, which makes this the most compressible thing in the file while the CSI
matrix barely compresses at all. That is what makes it affordable — see
[Storage](#storage-the-file-is-compressed-the-format-is-not).

Optional. A writer may omit it, and a reader must not assume it is present.

### `NODE_*` (`0x40`–`0x44`) — the conditions, not the measurement

A **sparse series**, never a per-record column. A writer emits these at an
interval and attaches them to the next record, so most records carry none and a
reader must not read absence as zero.

They are in the file, rather than only in whatever metrics store the capturer
runs, because of the format's own promise: a capture should be interpretable by
someone who has only the file and this document, and that reader has no metrics
store. Which conditions are named is not arbitrary — die temperature orders
phase drift, a throttled SoC is a different instrument, a spool at its floor is
how a long run loses its last hours, and load separates "the radio delivered
nothing" from "the host could not keep up".

**In a file, `csid` writes this series into the session block instead.** The
container is derived from the raw stream at teardown, so a per-record sample
attached during export would carry the teardown instant on every record — a
fabricated timestamp on a real measurement. The type codes above are for the
live datagram path, where a record is produced in the moment and the stamp is
implicit.

#### The two temperatures: two sensors, two units (`0x40` and `0x44`)

There are two die temperatures in this format and they are **not**
interchangeable. Getting this wrong is silent, so it is spelled out here rather
than left to the field names.

| Code | Field | Sensor | Unit | Source |
|---|---|---|---|---|
| `0x40` | `NODE_TEMP_MC` | host SoC | **millidegrees** C (`i32`) | `/sys/class/thermal/thermal_zone0/temp` |
| `0x44` | `NODE_NIC_TEMP_C` | Wi-Fi NIC die | **whole degrees** C (`i32`) | `iwlmvm/nic_temp` (firmware DTS) |

**A reader must not substitute one for the other, and must not rescale one into
the other.** A reader that treats `0x44` as millidegrees reports a card at
0.047 °C. A reader that treats `0x40` as degrees reports a node at 61,500 °C.
Both are absurd enough to catch, which is the only reason this pairing is
tolerable at all.

##### Why the units differ

**Each unit is the unit of its source.** The SoC's sysfs zone emits
millidegrees, so the record carries millidegrees. The driver's `nic_temp` emits
a whole number of degrees, so the record carries whole degrees. Multiplying the
NIC value by 1000 would produce a field that *looks* like the SoC's and asserts
three digits of precision the sensor never reported. The format's rule is that a
field carries the measurement, not a presentation of it, so neither side is
converted to match the other.

##### Why they are two sensors and not one

The active cooler sits on the SoC. The NIC sits under the M.2 HAT, in still air,
with a different thermal mass and no fan of its own. A node can therefore hold
the SoC comfortably in spec while the card climbs, and the card is the part that
produces the CSI. Reading the SoC and calling it "the temperature" measures the
enclosure's cooling, not the radio's state.

Neither value implies the other, and neither can be recovered from the other.
This is unlike `BW_ANTSEL` (`0x11`), which a reader may reconstruct from `rnf`
because they are the same bits.

##### The NIC reading is a firmware round trip, not a file read

`nic_temp` is not a cached sysfs value. Reading it makes the driver take
`mvm->mutex`, send a DTS measurement command and wait for the notification, so a
single read can cost up to a second. Two consequences bind a writer:

1. **Sample it far more slowly than the SoC zone.** The reference writer reads
   the SoC twice a second and the NIC once a minute during a capture.
2. **A failed read is the normal state of an idle radio.** `iwl_mvm_get_temp()`
   returns `-EIO` whenever the firmware is not running, and the debugfs file is
   mode `0400`. A writer records both as absence.

##### Absence is absence

`NODE_NIC_TEMP_C` is **absent** on every capture written before the field
existed, and on any tick where the firmware did not answer. It is never written
as `0`, and **a reader must not read its absence as a cold card, nor fill it
from the SoC value, nor carry the previous tick's value forward.** The same rule
already governs the rest of the `NODE_*` series.

##### Why this did not bump the version, and what that guarantees

Adding `0x44` is an added type code, which [the versioning
policy](#versioning-policy) states is not a version bump. The guarantee that
makes this safe is not a convention — it is checked:

- **The container `version` field is an equality test**, not a floor. A bump
  would make every existing file unreadable by the new build *and* every new
  file unreadable by every old build. An additive type code is therefore the
  only non-destructive way to extend the record.
- **Both reference readers skip unknown type codes**, and both have a test that
  says so. An old reader handed a new file ignores `0x44` and decodes every
  other field exactly as before.
- **The session-block schema is permissive**, not strict. An old writer's
  sidecar decodes with the field defaulted to absent, and a new writer omits the
  key entirely when it has no reading. The schema name stays `csid-session/1`:
  nothing validates the sidecar against an enumerated field list, and an
  optional added key is not a schema change.

##### Nothing in the archive is rewritten, and no re-derive can invent a value

The reading is taken **in the capture loop**, stamped with the moment it was
taken, and never anywhere else. Export and re-derive do not sample it. This
matters because `csid export` is run over archived `capture.raw` bytes to
re-derive containers into the current form: if the exporter sampled node state,
every re-derived capture would carry a NIC temperature measured on the day of
the re-derive, stamped onto a capture from months earlier. That is a fabricated
measurement wearing a real field's name, and the two-layer data model exists to
prevent exactly this.

Consequences for the existing corpus:

- **No existing `.csiq` or `capture.raw` is modified.** The field appears only in
  captures written after the change.
- **A re-derived capture keeps the node-state series its sidecar already had.**
  `export_session` reads `metadata.json` and never writes it.
- **A re-derived capture gains no NIC temperature.** The value was never
  measured for those bytes, and absence is the correct answer.

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

## The clocks

Every record can carry four timestamps. They are not redundant; they answer
different questions, and each fails in its own way.

| Field | Source | Resolution | Wraps | Use for |
|---|---|---|---|---|
| `FTM` | radio baseband, 320 MHz | 3.125 ns | ≈ 13.42 s | **all timing analysis** |
| `US` | firmware | 1 µs | ≈ 71.6 min | coarse cross-checks |
| `UNIX_TS_NS` | host kernel at delivery | ns (µs-jittered) | — | **wallclock anchoring** |
| `MONO_US` | host `CLOCK_MONOTONIC` | 1 µs | — | **surviving a clock step** |

`MONO_US` (`0x12`) is the newest and the one to reach for when the host's
wallclock cannot be trusted — a capturer with no RTC boots in the past and gets
stepped, which moves `UNIX_TS_NS` under a running capture and leaves no trace.
It is absent on a record the capturing node transmitted itself; see
[its section above](#mono_us-0x12--the-clock-a-time-step-cannot-move).

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

### Corrigendum: `SEQ` is a driver record counter (2026-08-24)

The paragraph below stood until 2026-08-24 and was **wrong about what the field
is**. It is preserved because a reader who acted on it deserves to see what
changed, not a silently edited page.

> `SEQ` is the low byte of the 802.11 sequence number. It is **not** a reliable
> completeness counter: it is 8 bits of a per-TID 12-bit field, observed over a
> subsampled frame population. Do not compute loss from it. True drop accounting
> requires a known injected cadence.

Measured over 2,433 records of a real capture carrying **four** distinct source
MACs:

| Observation | Result |
|---|---|
| Steps of exactly +1 | **2,432 of 2,432** |
| Steps across a **transmitter change** | 663, **all +1** |
| Header byte 77 | constant zero |

An 802.11 sequence number belongs to the transmitter. It cannot advance by one
across a change of transmitter, let alone across all 663 of them. `SEQ` is
therefore the **driver's own count of CSI reports**, and byte 77 being constant
confirms the counter is 8 bits wide rather than a truncation of something
larger.

**What this means for a reader.** A gap in `SEQ` is a *dropped report*, which is
the completeness signal the old text said did not exist. Two limits are real:

- it wraps every 256, so a gap is detectable modulo 256 and only for gaps
  **smaller than 256**;
- it counts what the driver delivered to userspace, so it cannot see a frame the
  radio never reported on — that denominator is still `frames_seen`.

IP-130 proposed a new `REC_COUNTER` type code for this. It was not allocated:
the field already exists, and a second code for one fact is how two answers to
one question begin. `0x13` stays reserved and unused.

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

## Storage: the file is compressed, the format is not

A CSIQ container is written as **`capture.csiq.zst`** — a zstd frame around an
ordinary, unchanged CSIQ byte stream. `version` is still `1`, the magic is still
`CSIQ`, and a reader that decompresses first meets exactly the bytes this
document describes.

This is deliberate, and it is the reason the version did not move. Compressing
the *record stream inside* the container would have changed framing, which the
versioning policy below correctly calls a version bump — and would have made
every existing reader fail on a purely additive change. Compressing the *file*
changes nothing about the format at all.

The extension is the single statement of which envelope a file uses, so a
directory listing cannot lie about it. Both forms exist and both are valid:

| Name | Written by |
|---|---|
| `capture.csiq` | any writer before the `VENDOR_HDR` era |
| `capture.csiq.zst` | a writer that keeps the driver header |

**Reader rule.** Decide by extension, and support both. A reader that meets a
`.zst` and has no decoder should say so in those terms rather than reporting a
corrupt container.

Measured, on a real 2,433-record capture with `VENDOR_HDR` kept on every record:

```
capture.raw        1,703,100 B    the lossless driver stream
capture.csiq.zst     796,578 B    46.8% of it, header blob included
```

The blob alone would have cost 661,776 B uncompressed. The whole compressed
container is smaller than that, which is what makes lossless provenance
affordable rather than a 44% tax.

**`capture.raw` is not retired by any of this.** It stays the source of truth,
which is what makes a re-derivation repeatable and a bad one cost compute only.

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
