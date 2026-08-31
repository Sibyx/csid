# csiq — Python reader for the CSIQ Interchange Format v1

Reference implementation of the reader side of
[`docs/CSIQ-format-v1.md`](../docs/CSIQ-format-v1.md). If this and the Rust
[`crates/csiq`](../crates/csiq) ever disagree, **the spec wins and both are bugs**.

The parser is pure standard library. NumPy is optional and only powers the
complex-matrix view, so the format stays readable anywhere.

```console
$ pip install csiq              # parser only
$ pip install 'csiq[numpy]'     # + matrix() and chain()
$ pip install 'csiq[zstd]'      # + capture.csiq.zst on Python < 3.14
$ pip install 'csiq[fast]'      # + the PyO3 accelerator (optional)
```

## The accelerator

`csiq[fast]` installs a compiled backend behind the **same API**. It is selected
automatically when present; `CSIQ_BACKEND=python` forces the pure parser.

Measured on one real 88.7 MB capture (165,712 records, arm64 macOS, warm cache):

| Pass | Pure Python | Rust | Speedup |
|---|---:|---:|---:|
| parse only | 58.8 MB/s | 141.6 MB/s | 2.41x |
| parse + `matrix()` | 42.3 MB/s | 72.4 MB/s | 1.71x |

**It is not justified by bulk throughput, and the numbers say so.** At 141.6 MB/s
a 48 GB archive pass costs about 6 minutes of decode against a 7-hour ingest
budget — the earlier pure-path figure was 23 minutes, so the saving is real and
small. The accelerator earns its place on interactive latency and on being a thin
crate over a reference implementation that already exists, not on rescuing a
nightly job.

Two rules keep a third implementation of one format honest:

- `tests/test_backend_parity.py` decodes every fixture with both backends and
  requires identical output, **including phase** — comparing `|H|` would pass
  against a backend that mirrored every phase, which is the exact bug the spec
  spends a page on. It is a gate, not advice.
- The pure path is never removed, and a `.csiq.zst` always takes it.

It found a real divergence on its first run: the Rust reader renders an absent
`WIDTH` field as `Unknown(0)` while the Python reader defaults to `"NOHT"`. Same
fact, two sentinels. The binding renders it the reference reader's way; which
sentinel is *right* is a spec question, and the honest answer is probably
neither — absence should be absence.

## Two levels

**The parser** is the byte-level floor: a session dict and a lazy record
iterator, mirroring the Rust crate.

```python
from csiq import read_csiq

session, records = read_csiq("capture.csiq")
for rec in records:
    print(rec.ftm, rec.ntone, rec.rssi, rec.phy)
```

**The layer** is what you probably want. CSIQ documents eight consumer rules
whose violation is *silent* — the result looks healthy and is wrong. Each one is
a named method, with the measurement behind it in the docstring.

```python
from csiq import Capture

with Capture.open("capture.csiq.zst") as cap:
    print(cap.session.radio.channel, cap.envelope)

    for rec in cap.received():          # own transmissions excluded
        if not rec.fully_measured():    # a -127 chain is stale, not weak
            continue
        H = rec.H                       # chain-major, imaginary-first
        print(rec.bandwidth_mhz, rec.tone_spacing_khz)
```

## The rules, and where they live

| Rule | What breaks silently | API |
|---|---|---|
| The matrix is chain-major | impulse response smears | `rec.H`, `rec.chain(c)` |
| Coefficients are imaginary-first | every phase mirrored, `\|H\|` fine | `rec.H` |
| `-127` dBm is a sentinel | a stale block read as a weak signal | `rec.fully_measured()`, `rec.chains_measured()` |
| Absent `MONO_US` = own transmission | injected frames pollute the population | `cap.received()`, `rec.is_own_transmission` |
| Absent `BW_ANTSEL` is not 20 MHz | HE20 and VHT80 both carry 242 tones | `rec.bandwidth_mhz` |
| `WIDTH` describes the receiver | per-record bandwidth reported as a constant | `cap.session.radio.width` vs `rec.bandwidth_mhz` |
| `SEQ` is a report counter | a real completeness signal discarded | `rec.seq` |
| A `NODE_*` absence is not zero | a sparse series read as dense | `rec.node` |

## Versions: the header number is not the capability set

The container version is **1** and stays 1. Adding a type code is explicitly not
a version bump — readers skip what they do not know — so the number in the header
says nothing about which fields a file carries.

Two captures both stamping version 1 can differ by five type codes. Probe:

```python
with Capture.open(path) as cap:
    caps = cap.capabilities()
    print(cap.format_version)      # 1, always
    print(caps.names)              # what this file actually carries
    print(caps.probed)             # over how many records
```

This matters in practice. A capture from csid 0.1.0 carries no `MONO_US`, so
`received()` **raises `FieldNotRecorded`** rather than returning an empty
iterator — an empty result would read as "this capture received nothing", which
is a claim about the radio rather than about the writer.

## Errors

Everything subclasses `CsiqError`. Three encode distinctions the spec makes:

- `UnsupportedVersion` — refuse rather than guess.
- `ZstdUnavailable` — a missing decoder, not a corrupt file.
- `DesyncError` — the `0xA1` tag is a framing check; stop rather than emit garbage.

Plus `BadMagic`, `TruncatedCapture`, `MissingRequiredField`, `MalformedField`,
`FieldNotRecorded` and `NumpyUnavailable`.

## The raw stream

`capture.raw` is the lossless driver-native source of truth; CSIQ is derived from
it. Reading it needs the monitor width, which the raw header does not carry:

```python
from csiq import read_raw
for rec in read_raw("capture.raw", width="80MHz"):
    ...
```

## Tests

```console
$ python -m pytest tests/
```

They build CSIQ bytes by hand from the spec's layout tables rather than using the
reader, so a drift between writer and reader shows up as a failure. Each test
asserts both that the right answer comes back and that the appealing wrong answer
does not.
