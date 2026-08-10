# csid

A systemd-native Wi-Fi **Channel State Information** capture daemon for Intel
AX210 radios on Linux 6.x, and **CSIQ** — a self-describing interchange format
for CSI.

[![CI](https://github.com/Sibyx/csid/actions/workflows/ci.yml/badge.svg)](https://github.com/Sibyx/csid/actions/workflows/ci.yml)

```console
# csid validate exp-c1 --probe     # is this capture even possible?
# csid run exp-c1                  # or: systemctl start csid@exp-c1
$ csid stream                      # watch it live
$ csiscope                         # ...or watch it properly, at :8088
$ csid export /var/lib/csid/monad05_exp-c1_20260722-093107
```

## Why this exists

CSI extraction tools are research artifacts: a binary, a shell script, and a
header layout you have to read the driver source to understand. They work for
one afternoon's experiment. They do not survive a 30-day unattended run across a
14-node fleet, and a capture from one is close to worthless a year later because
nothing recorded the channel, the width, the firmware, or the regulatory domain
that produced it.

`csid` is the boring, deployable version of that tooling:

- **One static binary.** Netlink consumption, radio control, session lifecycle,
  spooling, live streaming, and export. No Python runtime, no helper scripts, no
  `iaxcsi` dependency.
- **TOML configuration, validated before the radio is touched.** An impossible
  channel/width combination fails at `csid validate`, not four hours into a run.
- **systemd-native.** `Type=notify` with a real watchdog, `RuntimeMaxSec`
  session bounds, `OnSuccess=` shipping, timer-based offline healing.
- **Provenance that travels with the data.** Every session writes a sidecar
  recording radio config, firmware, kernel, driver module, regdomain and
  governor — written *before* capture, so even a crashed session is
  interpretable.
- **A documented format.** [CSIQ v1](docs/CSIQ-format-v1.md) is self-describing
  and versioned, with reference readers in Rust and Python.

## The data model

Two layers, on purpose:

| Layer | File | Role |
|---|---|---|
| **Source of truth** | `capture.raw` | The driver's bytes, verbatim. Lossless. Never rewritten, so a parser bug can never corrupt the archive. |
| **Interchange** | `capture.csiq` | Self-describing, versioned, provenance embedded. This is what you publish and share. |

Both are readable from Rust and Python. CSIQ is *derived* — you can always
re-export from the raw capture.

A session with `[ble].enabled` adds the same two layers for the BLE channel —
`ble_scan.jsonl` (durable, append-only) and `ble_rssi.parquet` (the interchange
form the analysis side reads). Both are stamped with the *same* wallclock the
CSI records carry, which is the reason the scanner lives in this daemon at all.
See [configuration.md](docs/configuration.md#ble--ble-co-capture) for the column
contract and the pseudonymisation scheme.

A session with `[timesync].enabled` adds the same two layers again for **time
transfer** — `time_transfer.jsonl` and `time_transfer.parquet`. Both of this
lab's transmitters already stamp their transmit time inside the payload (the
injector's `CSID | seq | tx_unix_ns`, the phone's MNDP header); recording those
beside each node's own receive stamp turns illumination into a time-transfer
channel. `csid fleet skew` then measures **inter-node clock skew to
microseconds** — one frame, many receivers, so the transmit instant cancels
exactly and nothing is bounded by the round-trip time that bounds
`csid fleet clock`. See [time-transfer.md](docs/time-transfer.md).

## Records carry three clocks

| Field | Source | Resolution | Use for |
|---|---|---|---|
| `ftm` | radio baseband, 320 MHz | **3.125 ns** | all timing analysis |
| `us` | firmware | 1 µs | coarse cross-checks |
| `unix_ts_ns` | host kernel at delivery | ns (µs-jittered) | wallclock anchoring |

**Analyse on `ftm`, anchor wallclock on `unix_ts_ns`.** The 320 MHz stamp is
applied in the RF plane before any host software runs, so it is immune to the
scheduling jitter that afflicts host timestamps (measured: p50 19 µs, p95 57 µs,
p99.9 5.4 ms). This also means **no clock-distribution protocol is needed across
a fleet**: nodes on the same channel stamp the same ambient frames on their own
320 MHz clocks, so pairwise offset and drift come out of the captures themselves
at sub-microsecond precision.

## Measured capability envelope

From the reference node (Raspberry Pi 5 + AX210, `iax` driver, regdomain
SK DFS-ETSI). `csid caps` prints this on any node.

| Axis | Measured |
|---|---|
| CSI types | legacy 52/56-tone, **HE20 242-tone**, **HE80 996-tone**; 1992 (HE160) unobserved for want of a source |
| MIMO | **2×2 confirmed** (242 tones × 4 chains) when the source transmits MIMO |
| Sustained rate | **608 Hz** unthrottled on a busy 20 MHz channel (~440 KB/s ≈ 1.5 GB/h) |
| Rate cap | `interval_us` is clean: 10 ms → ~88 Hz, 100 ms → ~9.8 Hz |
| Width trade-off | wider monitor width *lowers* total frame rate (~371 Hz at 20/40 vs ~215–225 Hz at 80/160) while unlocking wide-RU CSI |
| 6 GHz | tunes and captures on PSC channels and 80 MHz blocks |
| Amplitude | AGC-normalised — `\|H\|` is shape only; absolute scale from RSSI |

CSI type follows the *received frame*; monitor width only bounds what is
decodable.

## Live streaming

`csid` publishes parsed CSIQ records to a Unix datagram socket while capturing.
It is **best-effort by construction and can never affect the durable capture**:
the live path sits behind a bounded queue whose producer uses `try_send`, so a
slow or absent subscriber shows up as a rising `live_dropped` counter rather
than backpressure on the capture thread. See
[architecture.md](docs/architecture.md#the-invariant).

```python
import socket
from csiq import decode_live

s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.bind("/run/csid/live.sock")
while True:
    uid, seq, rec = decode_live(s.recv(262144))
    print(seq, rec.ntone, rec.rssi, rec.matrix().shape)
```

## The live console

`csiscope` is a second binary that subscribes to the same stream and serves a
browser console: waterfall, spectrum, sanitised phase, impulse response and
Doppler spectrogram, next to the node's configuration and unit control.

```console
$ csiscope                     # http://127.0.0.1:8088
$ csiscope --read-only         # views only, safe on an open network
```

It is a strict consumer — it never touches the radio and never writes to the
capture path — and it is **unauthenticated by design**, so it binds loopback
unless told otherwise.

One thing it makes unavoidable: an ambient channel carries several PHY types at
once (measured on channel 11: 74% legacy 52-tone, 26% HT 56-tone, interleaved
packet by packet), and views that mix them are not measurements of anything. So
every analytical panel is scoped to one **record class**, while the waterfall
can still show the whole channel on a shared frequency axis. Nothing is dropped
from the capture either way.

Full guide: [dashboard.md](docs/dashboard.md).

## Reading captures

```python
from csiq import read_csiq

session, records = read_csiq("capture.csiq")
print(session["radio"]["channel"], session["environment"]["firmware"])

for rec in records:
    H = rec.matrix()        # complex [ntone, nrx*ntx]
    ...
```

## Requirements

A **CSI-capable iwlwifi** — the `iax` or FeitCSI backport, installed via DKMS.
The in-tree `iwlwifi` emits no CSI vendor events. `csid doctor` checks this and
everything else in one command.

## Install

```console
$ cargo build --release
# install -m0755 target/release/csid /usr/local/bin/csid
# install -m0644 systemd/*.service systemd/*.timer /etc/systemd/system/
# install -m0644 config/config.toml /etc/csid/config.toml
```

Full walkthrough: [deployment.md](docs/deployment.md).

## Documentation

| Document | Contents |
|---|---|
| [CSIQ-format-v1.md](docs/CSIQ-format-v1.md) | The format specification |
| [architecture.md](docs/architecture.md) | Threading model, invariants, failure modes |
| [configuration.md](docs/configuration.md) | Complete TOML reference |
| [deployment.md](docs/deployment.md) | Install, systemd, bring-up, fleet notes |
| [dashboard.md](docs/dashboard.md) | The live console: views, record classes, security |
| [time-transfer.md](docs/time-transfer.md) | Inter-node skew + the phone affine fit from the illumination stream |

## Layout

```text
crates/csiq/     format library — no OS dependencies, builds anywhere
crates/csid/     the daemon — capture path is Linux-only
crates/csiscope/ the live console — a consumer of the stream, never a producer
python/csiq/     Python reference reader
docs/            specification and guides
systemd/         unit files
config/          example configuration
scripts/         sync and prune helpers
```

## Status

**Validated end to end on hardware** (Raspberry Pi 5 + AX210, kernel
6.8.0-1060-raspi, `iax` DKMS driver, firmware `78.3bfdc55f.0`, 2026-07-22):

- vendor-event registration accepted by the driver; CSI delivered unicast
- 2.4 GHz HT20 capture producing legacy 52-tone and HT 56-tone records with
  correct PHY labels (legacy/HT/VHT), per-chain RSSI, and real source MACs
- `capture.raw` → `capture.csiq` export, read back by the Python reference
  reader with byte-identical CSI payloads
- live streaming to an on-node subscriber **concurrent with** a durable capture:
  contiguous sequence numbers, zero drops, capture unaffected
- `csid doctor`, `validate`, `caps`, `export`, `stream` exercised on the node
- `csiscope` served against a live 8-hour capture on a busy 2.4 GHz channel
  (~290 Hz, interleaved legacy-52 and HT-56 records), deployed by the fleet's
  Ansible role

The format, configuration, export, and CLI layers are additionally covered by
tests that run on every platform (Linux, macOS, Windows) plus a cross-language
interop test in CI.

## Related work

`csid` consumes the CSI path established by the AX210 driver ports for Linux
6.x. It deliberately replaces only the *userspace* consumer: the firmware and
kernel path, where all timing precision originates, is untouched.

Prior art it learns from: the Halperin IWL5300 tool (a documented format is why
it was citable), FeitCSI (single-binary UX and live streaming), Nexmon CSI (CSI
on a socket), and PicoScenes (self-describing, richly-provenanced records — the
density target, though it is x86-only and unavailable on ARM).

## License

GPL-2.0-only. See [LICENSE](LICENSE).

## `collectord` — the UDP lab collector

A fourth crate in this workspace: the receiving end of the MonadCount mobile instrument's paced UDP
stream. It kernel-timestamps arrivals, answers the four-timestamp clock exchange, and writes
sessions in the same shape `csid` does so the existing sync machinery ships them. Lab-specific — it
is not part of the portable `csiq`/`csid` story. See [docs/collector.md](docs/collector.md).
