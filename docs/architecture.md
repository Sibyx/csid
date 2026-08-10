# Architecture

`csid` is one static binary that owns the whole userspace CSI path: netlink
consumption, radio control, session lifecycle, durable spooling, live streaming,
and CSIQ export.

## What csid does and does not own

```text
       ┌────────────────────────────────────────────────┐
       │  AX210 firmware   — stamps ftm at 320 MHz      │   ← precision lives here
       │  iwlwifi / iax    — CSI extraction, debugfs    │   ← IP-112 driver port
       └────────────────────────┬───────────────────────┘
                                │ nl80211 vendor events
       ┌────────────────────────┴───────────────────────┐
       │  csid  — everything from here up                │   ← this repository
       └────────────────────────────────────────────────┘
```

`csid` **does not** touch the firmware or kernel path. Every timing guarantee
the data has was established before a single userspace instruction ran. The
daemon's job is therefore not to *add* precision but to **not damage it**, and
to surround the data with enough provenance to stay interpretable.

## Threading model

```text
[RX thread]  pinned · SCHED_RR 50
   recv() → stamp unix_ts_ns → hand off.  Nothing else happens here.
      │
      ├── unbounded channel ──→ [durable thread] ──→ capture.raw   (LOSSLESS)
      │                                              verbatim driver bytes
      │
      └── bounded channel ────→ [live thread] ────→ CSIQ datagrams (BEST EFFORT)
          try_send, drop-newest                     /run/csid/live.sock

[main thread]  sd_notify READY/WATCHDOG/STOPPING · duration bound · stop flag

Siblings, never dependencies — each owns its socket, its thread and its file,
and shares ONLY the stop flag with the RX path above:

[inject thread]    AF_PACKET tx   paced illumination        (capture.mode = "inject")
[ble scan thread]  HCI raw   rx   ble_scan.jsonl            ([ble].enabled)
[timesync thread]  AF_PACKET rx   time_transfer.jsonl       ([timesync].enabled)
```

None of the three can apply backpressure to the RX thread — they hold no channel
into it. `[timesync]` in particular reads the transmit stamps in received frame
payloads on a **separate** `AF_PACKET` socket rather than tapping the CSI stream,
and its `ftm` column is filled at session close inside the `capture.raw` pass the
summary already makes. See [time-transfer.md](time-transfer.md).

### The invariant

**The live path can never block, slow, or cause loss on the durable path.**

This is the single most important property of the design, and it is what makes
shipping live streaming in v1 safe rather than reckless. It is enforced
structurally, not by discipline:

- The two consumers sit behind **separate channels with different policies**.
  The durable channel is unbounded; the live channel is bounded and its producer
  uses `try_send`, which cannot block.
- A slow, absent, or dead subscriber therefore manifests as a rising
  `live_dropped` counter — never as backpressure reaching the RX thread.
- The RX thread performs no parsing, no encoding, no I/O. CSIQ encoding happens
  on the live thread; raw framing on the durable thread.

The only backpressure that can reach the RX thread is the kernel's netlink
socket buffer (sized to 8 MiB), which at the measured ceiling of ~0.5 MB/s never
fills.

### Why realtime scheduling

Measured host delivery jitter is p50 19 µs / p95 57 µs / **p99.9 5.4 ms**. That
tail is scheduler stalls, not radio behaviour. `SCHED_RR` on the RX thread
targets exactly that tail. It is cosmetic for analysis — which runs on the
firmware `ftm` clock and is immune — but it tightens the wallclock anchor and
reduces the chance of a socket-buffer backlog. Failure to acquire realtime
priority is logged and non-fatal; an unprivileged run simply keeps the default
policy.

### Why not async

The data path is three threads and two channels. There is no concurrency problem
here that an async runtime solves: the rate is ~600 messages/second, the work per
message is a memcpy, and the one latency-sensitive thread wants a *dedicated
core and a realtime policy*, which is precisely what an async work-stealing
executor is designed not to give it. Plain OS threads make the scheduling
guarantees explicit.

## Session lifecycle

```text
validate config ─→ resolve tuning ─→ create session dir
      │
      ├─ ensure monitor iface, tune (retry once: 6 GHz -EIO quirk)
      ├─ debugfs: csi_interval, csi_addresses, csi_enabled=1
      ├─ write sidecar (OPEN)          ← provenance on disk before any capture
      ├─ open netlink source
      ├─ spawn durable / live / RX threads
      ├─ sd_notify READY
      │
      │   … capture … (watchdog pings, duration bound, stop flag)
      │
      ├─ stop flag → join RX → drain → fsync capture.raw
      ├─ debugfs: csi_enabled=0
      ├─ summarise (best effort — never invalidates the capture)
      ├─ write sidecar (CLOSE: complete | stopped | failed)
      └─ optional: export capture.csiq
```

The sidecar is written **before** capture begins so that a crashed or
power-cut session still leaves complete provenance on disk. It is rewritten at
close with the outcome and summary. A failure to write the close-time sidecar is
logged but never propagated — captured data is never invalidated by bookkeeping.

## The driver coupling, externalised

Exactly one thing in `csid` depends on the driver's ABI: which vendor OUI,
subcommand, and attribute ids carry the CSI blobs. Rather than bake those into
the binary, they live in `[driver]` configuration:

```toml
[driver]
vendor_oui       = 0x001735   # INTEL_OUI
csi_event_subcmd = 0x24       # IWL_MVM_VENDOR_CMD_CSI_EVENT
attr_csi_hdr     = 0x4d       # IWL_MVM_VENDOR_ATTR_CSI_HDR
attr_csi_data    = 0x4e       # IWL_MVM_VENDOR_ATTR_CSI_DATA
```

A driver revision that renumbers an attribute is then a config change, not a
recompile, and `csid doctor` prints the values in use so a mismatch is
diagnosable in one command. **These defaults must be verified against the
driver's `mvm/vendor-cmd.c` when bringing up new hardware or a new driver
revision** — see [deployment.md](deployment.md#first-bring-up).

## Crate layout

| Crate | Platform | Role |
|---|---|---|
| [`csiq`](../crates/csiq) | any | The format: TLV codec, container, live datagram, raw parser. No OS dependencies — this is the citable artifact and is separately vendorable. |
| [`csid`](../crates/csid) | Linux (capture) | The daemon. Portable modules (config, sinks, export, CLI) compile anywhere; netlink and realtime scheduling are `cfg(target_os = "linux")`. |

The split is deliberate: a consumer who wants to *read* CSIQ should not have to
build a netlink daemon. It also means the whole daemon structure type-checks on
a development macOS/Windows machine, with only the Linux capture internals
exercised on the target.

## Failure modes and what happens

| Failure | Behaviour |
|---|---|
| Tune fails | Retried once after 500 ms (6 GHz `-EIO` quirk); then session fails, sidecar `status=failed` |
| debugfs missing | Session fails before capture; `csid doctor` explains why |
| Netlink source dies mid-session | RX thread stops, channels drain, capture closes cleanly with what was captured |
| Live subscriber absent or slow | `live_dropped` rises; durable capture unaffected |
| Durable writer fails | Capture stops (data integrity is the priority), sidecar records the failure |
| Summary computation fails | Logged; sidecar written without summary; raw capture intact |
| CSIQ export fails | Logged; raw capture intact and re-exportable with `csid export` |
| SIGTERM / `systemctl stop` | Clean teardown, `status=stopped`, data flushed and fsynced |
