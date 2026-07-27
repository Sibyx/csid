# Configuration

`csid` is configured entirely through TOML. There are two files:

| File | Scope |
|---|---|
| `/etc/csid/config.toml` | node-global: spool, sync, telemetry, driver ABI |
| `/etc/csid/experiments/<name>.toml` | one per experiment: radio, capture, stream, export |

`csid run <name>` resolves `<name>` to `/etc/csid/experiments/<name>.toml`, or
accepts a path directly. Both locations are overridable with `--config` and
`--experiments`.

**Validate before you deploy.** `csid validate <name>` parses the config,
resolves the band/channel/width to concrete frequencies, and reports exactly
what would be tuned — without touching the radio. Add `--probe` to also check
that the interface and debugfs knobs are present. Unknown keys are a hard error,
so a typo cannot silently disable a setting.

## Node-global: `/etc/csid/config.toml`

```toml
[node]
# Session spool root; one subdirectory per session.
spool = "/var/lib/csid"
# Override the reported hostname (defaults to the system hostname).
# hostname = "monad05"

[sync]
enabled          = true
remote           = "hetzner"                        # rclone remote name
bucket           = "monad-knowledge"
prefix           = "datasets/ax210-csi-captures"    # → <prefix>/<host>/<session_id>/
prune_after_days = 7                                # delete capture.raw this long after a verified sync

[otel]
enabled  = false
endpoint = "http://localhost:4317"                  # node-local Grafana Alloy

[driver]
# Driver ABI coupling — see docs/architecture.md. Defaults target iax (fflq).
vendor_oui       = 0x001735   # INTEL_OUI
csi_event_subcmd = 0x24       # IWL_MVM_VENDOR_CMD_CSI_EVENT
attr_csi_hdr     = 0x4d       # IWL_MVM_VENDOR_ATTR_CSI_HDR
attr_csi_data    = 0x4e       # IWL_MVM_VENDOR_ATTR_CSI_DATA
```

`[sync]` is descriptive: the shipping itself is done by the `csid-sync` unit
(rclone), which reads the same values. `csid` records them in the sidecar so a
session says where it was meant to land.

## Per-experiment: `/etc/csid/experiments/<name>.toml`

```toml
# Optional; defaults to the file stem. Appears in the session id.
experiment = "exp-c1"
# Free-form operator note recorded in the sidecar.
tag = "living room, blinds closed"

[radio]
interface   = "wlp1s0"        # the AX210 netdev
monitor     = "wlp1s0mon0"    # monitor iface (created if absent)
channel     = 36
# band is REQUIRED for 6 GHz (channel numbering overlaps 2.4 GHz);
# inferred for 2.4 and 5 GHz.
# band      = "6"
width       = "80MHz"         # NOHT | HT20 | HT40- | HT40+ | 80MHz | 160MHz
interval_us = 0               # 0 = unthrottled; otherwise a rate cap
mac_filter  = []              # source-MAC allowlist; empty = all

[capture]
mode     = "passive"          # "passive" | "inject" (paced illumination, see [inject])
duration = "30m"              # omit to run until stopped

# Only read when capture.mode = "inject". Defaults shown.
# [inject]
# rate_hz      = 25                    # absolute-deadline paced; missed slots skipped, never bunched
# frame_bytes  = 200                   # 802.11 MPDU size
# src_mac      = "ef:be:ad:de:ad:de"   # the analysis sentinel receivers filter on
# dst_mac      = "ff:ff:ff:ff:ff:ff"   # broadcast = unACKed; loss stays measurable
# bitrate_mbps = 6                     # legacy OFDM only (6/9/12/18/24/36/48/54)

[stream]
enabled     = true
transport   = "unix"          # "unix" (default) | "udp"
unix_socket = "/run/csid/live.sock"
# targets   = ["10.0.0.5:5555"]   # required when transport = "udp"
max_queue   = 4096            # bounded; overflow drops + counts, never blocks capture

[export]
on_close = true               # also write capture.csiq when the session ends
```

## Field reference

### `[radio]`

| Key | Type | Notes |
|---|---|---|
| `interface` | string | Required. The AX210 netdev. |
| `monitor` | string | Monitor interface; created and brought up if missing. |
| `channel` | int | 802.11 channel number. |
| `band` | `"2.4"` \| `"5"` \| `"6"` | **Required for 6 GHz.** Inferred otherwise. |
| `width` | enum | Validated against the band: 2.4 GHz permits NOHT/HT20/HT40±only. |
| `interval_us` | int | Rate cap. `0` = unthrottled (~608 Hz ceiling on a busy 20 MHz channel); `10000` → ~88 Hz; `100000` → ~9.8 Hz. |
| `mac_filter` | string[] | `csi_addresses` debugfs knob. Empty clears the filter. |

Wide widths (80/160 MHz) require a centre frequency. `csid` derives it from the
built-in channel-group tables and **rejects a channel that is not a member of a
valid group**, so an impossible capture fails at `validate` time. Run
`csid caps` to print the tables.

### `[capture]`

| Key | Type | Notes |
|---|---|---|
| `mode` | string | `passive` (ambient traffic) or `inject` (passive capture **plus** the paced monitor-mode injector configured under `[inject]`). |
| `duration` | duration | Human format: `30s`, `10m`, `4h`. Omit to run until stopped. systemd's `RuntimeMaxSec` remains the outer bound. |

### `[inject]` (only read when `capture.mode = "inject"`)

Transmits paced 802.11 data frames on the monitor interface — the campaign's
illumination source. Replaces the hostapd soft-AP arm, whose multicast queue
released frames in beacon-aligned bunches (measured 2026-07-27: inter-arrival
p50 15.3 ms at a commanded 40 ms, CV 2.6); injection leaves only CSMA between
the pacing loop and the air. Works on any band the radio can tune — including
5 GHz, where the iax firmware crashes in AP mode. The payload carries a
sequence number and TX wallclock stamp, and the close-time sidecar records
`summary.inject = {sent, errors, skipped}`, so the receiver-side delivery
fraction is `receiver records ÷ sent`.

| Key | Type | Notes |
|---|---|---|
| `rate_hz` | int | 1..=1000. Absolute-deadline paced; missed slots are skipped, never bunched. |
| `frame_bytes` | int | 64..=1500. 802.11 MPDU size (radiotap not counted). |
| `src_mac` | string | Source MAC — the analysis sentinel receivers filter on. |
| `dst_mac` | string | Broadcast by default (unACKed, loss visible). |
| `bitrate_mbps` | int | Legacy OFDM only (6/9/12/18/24/36/48/54), via the radiotap RATE field, so receivers see the 52-tone `legacy_ofdm` class on both bands. CCK rates are rejected — they carry no CSI. |

### `[stream]`

| Key | Type | Notes |
|---|---|---|
| `enabled` | bool | Off by default. |
| `transport` | string | `unix` is the hardened v1 path. `udp` requires `targets`. |
| `unix_socket` | path | `csid` *sends* here; a consumer binds it (`csid stream` does). |
| `targets` | string[] | `host:port` list for `udp`. |
| `max_queue` | int | Bounded queue depth. Larger tolerates longer consumer stalls at the cost of memory; overflow always drops rather than blocks. |

### `[export]`

| Key | Type | Notes |
|---|---|---|
| `on_close` | bool | Write `capture.csiq` at session end. You can always run `csid export <session>` later — the raw capture is retained until pruned. |

## Session output

```text
/var/lib/csid/<host>_<experiment>_<YYYYmmdd-HHMMSS>/
├── capture.raw      # lossless driver-native bytes (source of truth)
├── capture.csiq     # self-describing container (if export.on_close)
└── metadata.json    # the sidecar — full provenance
```

## Environment variables

| Variable | Effect |
|---|---|
| `CSID_LOG` | `tracing` filter, e.g. `debug`, `csid::engine=trace`. Overrides `-v`. |
| `NOTIFY_SOCKET` | Set by systemd; enables `sd_notify`. Absent → no-op. |
| `WATCHDOG_USEC` | Set by systemd `WatchdogSec=`; `csid` pings at a third of it. |
| `JOURNAL_STREAM` | Set by systemd; selects structured journald logging over stderr. |
