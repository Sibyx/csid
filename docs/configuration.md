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

# BLE co-capture: writes ble_rssi.parquet beside capture.csiq, on this node's
# clock. Off unless enabled. Defaults shown.
# [ble]
# enabled          = true
# adapter          = "hci0"
# required         = false   # true = a scanner that will not start fails the session
# scan_interval_ms = 100     # 2.5..10240; window == interval means always listening
# scan_window_ms   = 100
# hash_bytes       = 8       # bytes of the per-session digest kept (16 hex chars)
# restart_after_s  = 30      # no advertisement for this long => restart the scanner
# backoff_s        = 5
# gap_alert_s      = 5       # longer observation gaps are counted in the sidecar
# flush_every      = 256     # durable-log flush cadence (also flushed on a 2 s timer)

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

### `[ble]` — BLE co-capture

Runs a **passive** LE scan on the same node, for the whole session, stamping
each received advertisement with the same `unix_ts_ns` clock the CSI records
carry. That shared clock is the entire point: the calibration analysis joins
BLE observations onto CSI analysis windows with no offset to estimate.

Two artefacts land in the session directory and ship with it:

| File | Role |
|---|---|
| `ble_scan.jsonl` | Durable, append-only, crash-safe — the BLE analogue of `capture.raw`. Pruned by `csid-prune` after a verified sync. |
| `ble_rssi.parquet` | The contract artefact the analysis side reads. Written at session close from the log. |

`ble_rssi.parquet` columns, in order — **this is a schema contract**
(`ble-rssi/1`; a rename is a version bump):

| Column | Parquet type | Notes |
|---|---|---|
| `unix_ts_ns` | INT64, required | Host wallclock, same source as the CSI record stamp. |
| `host` | STRING, required | Capturing node. |
| `session_id` | STRING, required | csid session id — sessions concatenate without bookkeeping. |
| `adapter` | STRING, required | e.g. `hci0`. |
| `device_hash` | STRING, required | `hex(SHA-256(salt ‖ addr_type ‖ addr)[:hash_bytes])`. |
| `addr_kind` | STRING, required | `public` · `random_static` · `rpa_resolvable` · `rpa_non_resolvable` · `random_reserved` · `public_identity` · `random_identity` · `unknown`. |
| `pdu_type` | STRING, required | `adv_ind` · `adv_direct_ind` · `adv_scan_ind` · `adv_nonconn_ind` · `scan_rsp` · `unknown`. |
| `rssi_dbm` | INT32, **optional** | Null encodes the controller's "RSSI unavailable" sentinel rather than writing 127 dBm. |

The advertising **channel index** (37/38/39) is not a column: the HCI
Advertising Report does not carry it on any Bluetooth version.

**Addresses are never stored.** A 32-byte salt is drawn from the OS CSPRNG at
session open, held only in memory, and discarded at close — it is not in the
sidecar, the log, or the parquet. So a pseudonym is stable within a session
(per-device RSSI series work) and unlinkable across sessions (cross-session
tracking is impossible by construction). Because phones use rotating private
addresses, **distinct `device_hash` values are an upper bound on devices, not a
device count** — use `addr_kind` to bound the stable-identity population.

The scan is always passive; there is no knob to make it active. An active
scanner would transmit `SCAN_REQ` from this node, identifying it to the room
*and* injecting 2.4 GHz energy into the band the CSI capture is measuring.

| Key | Type | Notes |
|---|---|---|
| `enabled` | bool | Default `false`. |
| `adapter` | string | `hci<N>`. |
| `required` | bool | `false` (default): a scanner that will not start is logged, recorded as `summary.ble.status = "failed"`, and the CSI capture continues. `true`: the session fails at setup — use this for calibration sessions where a capture without BLE anchors is worthless. |
| `scan_interval_ms` | float | 2.5–10240 ms. |
| `scan_window_ms` | float | ≤ `scan_interval_ms`. Equal values = continuously listening. |
| `hash_bytes` | int | 4–32. Default 8 → 16 hex characters. |
| `restart_after_s` | float | Silence budget before the scanner is torn down and re-opened. Backs off exponentially (to 16×) across consecutive restarts that yield nothing, so a genuinely empty room does not churn the adapter all night. |
| `backoff_s` | float | Delay between restart attempts. |
| `gap_alert_s` | float | Observation gaps longer than this are counted into `summary.ble.gaps_over_alert`. |
| `flush_every` | int | Durable-log flush cadence in records (also flushed every 2 s). |

Health is reported three ways, because the readiness audit's finding was that
silent degradation is the real risk:

- `systemctl status` gains a `· ble <n> obs, <x> Hz` suffix, so a scanner that
  stopped producing is visible without opening the spool;
- the journal logs `ble scanning` every 10 s with the rate, restart count and
  worst gap;
- `summary.ble` in `metadata.json` grades the channel `ok` / `degraded`
  (restarted, errored, gapped, or short rows) / `failed` (no observations at
  all), alongside the raw counters.

**`bluetoothd` shares the adapter.** `csid` uses an `HCI_CHANNEL_RAW` socket, so
a running `bluetooth.service` performing its own discovery will change the scan
parameters underneath it. On a capture node, mask the service or leave the
adapter unmanaged. `csid doctor` reports whether a passive scan starts cleanly.

### `[timesync]` — time transfer over the illumination stream

Records the transmit stamp both of this lab's transmitters already put inside
their payloads, beside this node's own receive stamp. That turns illumination
into a time-transfer channel: `csid fleet skew` differences two nodes' receive
stamps for the *same* transmitted frame, so the transmit instant cancels exactly
and the inter-node skew estimate is **not** bounded by the round-trip time that
bounds `csid fleet clock`. Full argument: [time-transfer.md](time-transfer.md).

| Key | Type | Notes |
|---|---|---|
| `enabled` | bool | Off by default. Opens one extra `AF_PACKET` socket and one extra thread on the monitor interface. |
| `required` | bool | Fail the session at setup if the receiver cannot start. Set it on profiles whose analysis **pools nodes** — a capture with no measurable inter-node skew cannot be certified against gate G4b's 250 ms budget. |
| `flush_every` | int | Durable-log flush interval in rows (also flushed on a 2 s timer). |
| `ftm_tolerance_us` | int | How near a CSI record must be to a received frame to be credited with its `ftm`. Default 2000 µs, far below the 40 ms inter-frame spacing at 25 Hz, so a pairing is unambiguous. Validation rejects anything above 20 000 µs. |
| `one_way_floor_us` | int | Upper bound on the minimum one-way delay. Widens the reported phone-offset interval; **never** changes the fit. Default 5000 µs, from this fleet's measured 10.6 ms management RTT with `wlan0` power-save off. |

Artefacts: `time_transfer.jsonl` (durable) and `time_transfer.parquet` (schema
`time-transfer/1`, the contract `monad_knowledge.csi.timesync` reads).
`summary.timesync` in `metadata.json` grades the channel `ok` / `degraded` /
`failed` and carries the diagnosis counters — in particular `protected_frames`,
which is large with `rows_app` zero when the experiment SSID is **not open**: a
monitor-mode receiver cannot read an encrypted payload, so the phone's stamps
are invisible from the air (`collectord` still sees them, as a real UDP peer).

`rx_stamp_source` is the field to read first. `userspace` means `SO_TIMESTAMPNS`
was refused and every receive stamp carries the scheduler's wake-up jitter —
the same order as the skew being measured. Such a session must not be pooled
with `kernel`-stamped ones.

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
├── capture.csiq        # self-describing container (if export.on_close)
├── ble_scan.jsonl      # BLE durable log        (if [ble].enabled)
├── ble_rssi.parquet    # BLE interchange        (if [ble].enabled)
├── time_transfer.jsonl # time-transfer log      (if [timesync].enabled)
├── time_transfer.parquet # time-transfer interchange
├── markers.jsonl       # block boundaries       (if `csid marker` was used)
└── metadata.json       # the sidecar — full provenance
```

## Environment variables

| Variable | Effect |
|---|---|
| `CSID_LOG` | `tracing` filter, e.g. `debug`, `csid::engine=trace`. Overrides `-v`. |
| `NOTIFY_SOCKET` | Set by systemd; enables `sd_notify`. Absent → no-op. |
| `WATCHDOG_USEC` | Set by systemd `WatchdogSec=`; `csid` pings at a third of it. |
| `JOURNAL_STREAM` | Set by systemd; selects structured journald logging over stderr. |
