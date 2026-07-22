# Deployment

## Requirements

- Linux 6.x with a **CSI-capable iwlwifi** — the `iax` (or FeitCSI) backport
  built via DKMS. The in-tree `iwlwifi` does **not** emit CSI vendor events.
- An Intel AX210 (or AX200) radio.
- Root, or `CAP_NET_ADMIN` plus read/write access to
  `/sys/kernel/debug/ieee80211/*/iwlwifi/iwlmvm`.

Verify all of it in one command:

```console
# csid doctor --interface wlp1s0
[ok] kernel: 6.8.0-1060-raspi
[ok] iwlwifi module: /lib/modules/6.8.0-1060-raspi/updates/dkms/iwlwifi.ko.zst
[ok] backport compat module: loaded
[ok] capture interface: wlp1s0
[ok] debugfs CSI knobs: /sys/kernel/debug/ieee80211/phy1/iwlwifi/iwlmvm
[ok] regdomain: country SK: DFS-ETSI
[ok] cpu governor: performance
[ok] spool directory: /var/lib/csid
[info] driver ABI: oui=0x001735 subcmd=0x24 hdr_attr=0x4d data_attr=0x4e
[info] wiphy index: 1 (registration target)
```

A `FAIL` on *iwlwifi module* pointing outside `updates/dkms` means the in-tree
driver is loaded and no CSI will ever arrive — the most common bring-up mistake.

## Build

```console
$ cargo build --release
$ install -m0755 target/release/csid /usr/local/bin/csid
```

Cross-compiling for a Pi 5 from an x86 host:

```console
$ rustup target add aarch64-unknown-linux-gnu
$ cargo build --release --target aarch64-unknown-linux-gnu
```

The binary is self-contained: no Python runtime, no shared library beyond libc,
and `iw`/`ip` from `iproute2`/`iw` for interface setup.

## Install

```console
# install -d /etc/csid/experiments /var/lib/csid /run/csid
# install -m0644 config/config.toml            /etc/csid/config.toml
# install -m0644 config/experiments/smoke.toml /etc/csid/experiments/smoke.toml
# install -m0644 systemd/*.service systemd/*.timer /etc/systemd/system/
# systemctl daemon-reload
```

## First bring-up

The shipped defaults are taken from the `iax` (fflq) sources and **confirmed on
hardware** (Pi 5 + AX210, kernel 6.8.0-1060-raspi, firmware `78.3bfdc55f.0`), so
a stock `iax` node needs no ABI tuning at all.

The one thing that can change per driver revision is the **vendor-event ABI** —
which OUI, subcommand, and attribute ids carry the CSI blobs. If a capture runs
but records never arrive:

1. Confirm the driver actually emits events:
   `sudo cat /sys/kernel/debug/ieee80211/phy1/iwlwifi/iwlmvm/csi_enabled` should be `1`
   during a session, and the channel must carry traffic (`csid caps` lists which
   channels were busy on the reference node).
2. Check the ABI constants against the driver source — `mvm/vendor-cmd.c`, the
   `IWL_MVM_VENDOR_CMD_CSI_EVENT` subcommand and its attribute enum — and set
   them in `[driver]`.
3. Re-run with `-vv` (`CSID_LOG=trace`) to see family resolution and the
   registration handshake.

Once a session produces records, the ABI is settled for that driver build.

## Running a session

```console
# systemctl start csid@smoke          # unattended, from /etc/csid/experiments/smoke.toml
# journalctl -u csid@smoke -f
# systemctl stop csid@smoke           # clean teardown, status=stopped
```

Interactively:

```console
# csid validate smoke --probe
# csid run smoke --duration 60s
```

## systemd units

`csid@.service` is a template — the instance name is the experiment.

| Concern | Mechanism |
|---|---|
| session runtime bound | `RuntimeMaxSec=` (defaults to the config `duration`) |
| liveness | `Type=notify` + `WatchdogSec=30`; `csid` pings while records flow |
| post-session shipping | `OnSuccess=csid-sync.service` |
| offline healing | `csid-sync.timer` — `OnUnitActiveSec=15min`, `Persistent=yes` |
| pruning | `csid-prune.timer` — daily; raw older than the grace window after sync |
| privileges | `AmbientCapabilities=CAP_NET_ADMIN`, `ProtectSystem=strict`, `NoNewPrivileges` |
| scheduling | `CPUSchedulingPolicy=rr`, `CPUSchedulingPriority=50` |
| logging | journald only; the sidecar is the per-session record |

Check the hardening posture with:

```console
# systemd-analyze security csid@smoke
```

## Storage and shipping

```text
/var/lib/csid/<host>_<experiment>_<stamp>/
├── capture.raw      # lossless source of truth
├── capture.csiq     # self-describing, publishable
└── metadata.json    # provenance
```

`csid-sync` ships completed sessions with `rclone copy --checksum` to
`<bucket>/<prefix>/<host>/<session_id>/` and writes a `.synced` marker. It uses
rclone's **env-var S3 backend** (`CSID_S3_*` from `/etc/csid/sync.env`), so there
is no `rclone.conf` to distribute.
It is idempotent — the marker is checked before copying — so the
`OnSuccess=` hook and the 15-minute timer can both fire safely. A node that is
offline for days simply catches up when it reconnects.

`csid-prune` deletes `capture.raw` once `.synced` is older than
`prune_after_days`. **`metadata.json` and `.synced` are kept forever**, leaving a
greppable on-node index of every session the node ever ran.

At the unthrottled ceiling a busy channel produces roughly 1.5 GB/h, so bound
long runs with `interval_us` or rely on the prune window.

## Live consumers

```console
$ csid stream --socket /run/csid/live.sock --limit 20
```

Anything that can bind a Unix datagram socket can consume the stream; decode
with `csiq.decode_live` (Python) or `csiq::live::decode` (Rust). Gaps in `seq`
are sender-side drops — see [CSIQ-format-v1.md](CSIQ-format-v1.md#live-datagrams).

## Fleet notes

`csid` is deliberately single-node. Multi-node orchestration belongs to the
experiment layer: fan out `systemctl start csid@<exp>` over SSH or Ansible.

No clock-distribution protocol is needed. Nodes tuned to the same channel stamp
the same ambient frames on their own 320 MHz clocks, so pairwise offset and
drift are recoverable from the captures themselves at sub-microsecond precision;
NTP-disciplined `unix_ts_ns` in the sidecar is enough to pair sessions.
