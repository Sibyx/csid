# Time transfer over the illumination stream

**What it answers:** is this fleet one timebase, to microseconds, measured on the
air rather than over ssh?

`csid fleet clock` already answers a weaker version of that question with a
four-timestamp ssh exchange. Its error is bounded by half the round-trip
*asymmetry*, which it cannot observe, so it honestly reports half the round-trip
*delay* as its uncertainty — milliseconds over a tailnet, on a good day. No
amount of sampling improves that bound.

This does better, for free, using an asset that was already on the air.

## The idea

Both of this lab's transmitters stamp their transmit time **inside the payload**,
and until now nothing read it back:

| Transmitter | Where | Layout |
|---|---|---|
| the csid injector (`crates/csid/src/inject.rs`) | raw 802.11 body | `b"CSID" ‖ u64 LE seq ‖ u64 LE tx_unix_ns` |
| the phone app (`LabPacket`, MNDP v1) | UDP payload | `b"MNDP" ‖ … ‖ u32 BE seq ‖ u64 BE t_mono_ns ‖ u64 BE t_wall_ms` |

A node with `[timesync].enabled` opens a second `AF_PACKET` socket on the
monitor interface, recognises both formats, and records each stamp beside its
own receive time. That yields two measurements:

### 1. Inter-node skew, to microseconds

One transmitted frame is received by every node in the room. Propagation across
10 m is 33 ns, so for one sequence number `s`:

```
t_A(s) − t_B(s)  =  (skew_A − skew_B)  +  (rx_A − rx_B)  +  O(30 ns)
                    └── what we want ──┘  └ per-node RX pipeline delay ┘
```

The transmit instant is the **same physical event** on both sides and cancels
*exactly*. There is no round trip, so there is no round-trip asymmetry to bound
the estimate.

**The median is the estimator, not the minimum.** The NTP-style minimum-delay
filter is right for one one-way delay and wrong for a difference of two:
`min(t_A − t_B)` picks the sample where A was fast *and* B was slow, so it
estimates `skew_AB + min(rx_A) − max(rx_B)` — biased low by the whole width of
B's jitter distribution. `min`/`max` are reported as the observed envelope only.

**One window, or the skews do not compose.** Every pair in a report is measured
over the same sequence window, printed above the table. Left to itself each pair
spans whatever interval its own two nodes shared, and a pair that ran twice as
long carries twice as much accumulated relative drift in its median — so
`skew(x,y) + skew(y,z) - skew(x,z)` is not zero and a node's offset depends on
which peer it was routed through. Measured on `coex-03_20260823-101235`, where
two nodes heard to seq 74,587 and four peers stopped at 30,366: the triangles
containing that long pair missed closure by 42-62 µs, against 1-11 µs for the
co-windowed ones. The window is the intersection of the spans of the nodes that
share at least `--min-common` sequences with the best-heard node. A node that
heard a *disjoint* stretch gets no vote — it cannot be placed on that timeline
under any window, so letting it clamp one would destroy every measurable pair.
A node that merely joined **late** does get a vote and does clamp, and what that
cost each pair is printed rather than swallowed.

**What survives and is not observable:** a fixed difference between the two
nodes' RX pipeline *floors* (driver path, interrupt coalescing, whether one node
got a kernel timestamp and the other did not). On identical hardware running one
image it is common-mode and cancels; on a mixed fleet it does not. The render
says so. Triangle closure over a co-windowed fleet is the closest handle on that
term: on the arm above it is 8-27 µs once the window is common.

### 2. Phone → fleet affine offset, continuously

The app's `ClockGate` registers `unix_ts_ns ≈ a·mono_ns + b` (pre-registration
§3.5) and gates a fold whose residual exceeds 0.25 s out of T3. That fit used to
come from a handful of RTT bursts. Every app datagram on the air carries
`(mono_ns, seq)`, so an illuminated session yields **thousands** of pairs.

One-way delay splits the claim in two, and the code reports them separately:

- **The slope is recoverable cleanly.** Every observation is `y = a·x + b + d`
  with `d ≥ d_min > 0` bounded and not growing with time, so its contribution to
  the slope is `O(spread(d)/T)` — about **1.1 ppb** for a 2 ms delay spread over
  30 minutes, three orders of magnitude below a consumer crystal's 10–50 ppm.
  The ppm figure is real.
- **The offset is biased by `d_min`, which one-way data cannot see.** The fit is
  placed on the **lower envelope** (Theil–Sen slope over a minimum-delay subset,
  then the intercept on the envelope), so what is reported is an *interval*, not
  a point. The offset an operator means — phone wallclock minus fleet wallclock
  — is estimated as `max(tx_wall − rx_unix)`, biased **early** by `d_min`, with
  the true value in `[est, est + one_way_floor]`.

**Does the bias matter at 250 ms?** No, by a wide margin. Measured on this
fleet: management-path RTT with `wlan0` power-save off is 10.6 ms, so a one-way
floor is ~5 ms — **2% of the G4b budget**. The injector's own 200-byte frame at
6 Mbps legacy OFDM occupies ~290 µs of air. If the budget were 1 ms this term
would dominate and `collectord`'s four-timestamp exchange would be mandatory
rather than complementary.

## Commands

```bash
# On a node (the cockpit runs this over ssh):
csid timesync report --window 60             # what this node heard
csid timesync report --json --window 60      # what `csid fleet skew` parses
csid timesync export <session-dir>           # rebuild the parquet from the log

# From the bench laptop:
csid fleet skew                              # AUTHORITATIVE — µs, on the air
csid fleet skew --window 120 --min-common 50
csid fleet skew --tx ef:be:ad:de:ad:de       # pin the transmitter
csid fleet clock                             # FALLBACK — ms, over ssh
```

Exit codes follow the cockpit's convention: `0` certified, `1` measured and
outside budget, `3` not measured (a silent node, an unreachable node, or no
stamped transmitter on the air at all).

### Which one decides

When both have a number for the same window, **`csid fleet skew` decides.** Use
`csid fleet clock` when no stamped transmitter is on the air, when a node's own
`chronyd` state is the question, or as a cross-check. Its uncertainty is never
better than the node's chrony root dispersion, and that widening is what makes
falling back to it safe.

## The artefact

```text
<session>/
  time_transfer.jsonl     durable, append-only, written as frames arrive
  time_transfer.parquet   the contract artefact, written at close
```

Same split, same reasoning, as `ble_rssi.parquet`: a session that lost power
before its close-time export is still readable from the log, and
`csid timesync export` rebuilds the parquet from it.

Schema `time-transfer/1` — **a contract** with `monad_knowledge.csi.timesync`, so
a column rename is a schema bump, not a refactor:

| Column | Type | Null? | Meaning |
|---|---|---|---|
| `unix_ts_ns` | INT64 | required | Receiver wallclock at frame delivery |
| `host` | string | required | Receiving node |
| `session_id` | string | required | csid session this landed in |
| `rx_stamp_src` | string | required | `kernel` (SCM_TIMESTAMPNS) or `userspace` |
| `tx_kind` | string | required | `csid` or `app` |
| `tx_id` | string | required | Sentinel MAC, or the app's session UUID |
| `tx_mac` | string | required | 802.11 `addr2` — who transmitted |
| `seq` | INT64 | required | Payload sequence number |
| `tx_stamp_ns` | INT64 | required | The payload's transmit stamp |
| `tx_clock` | string | required | `unix` or `mono` — **never mix these** |
| `tx_wall_ns` | INT64 | null ok | App `wallMillis` × 1e6; null for `csid` |
| `ftm` | INT64 | null ok | Paired CSI 320 MHz counter; null when unpaired |
| `ftm_lag_ns` | INT64 | null ok | `csi.unix_ts_ns − unix_ts_ns` of that pairing |

`rx_stamp_src` is not decoration. A userspace stamp carries the scheduler's
wake-up jitter, which is the same order as the skew being measured — the lesson
`collectord` already learned. A session that fell back **must not be pooled**
with kernel-stamped ones.

`ftm` is filled at session close by pairing each row against `capture.raw` on
time (nearest CSI record from the same transmitter, within `ftm_tolerance_us`,
default 2000 µs against a 40 ms inter-frame spacing at 25 Hz). Pairing on time
rather than on the driver header's sequence byte is deliberate: that byte's
relationship to the 12-bit 802.11 sequence-control field is driver-coupled and
has never been verified on hardware. `ftm_lag_ns` is recorded so a reader can
judge each pairing rather than trust it. **An unpaired row keeps `ftm = null`** —
normal, since CSI is only produced for frames the radio actually sounded.

## What this does not touch

The capture hot path. The receiver has its own socket, its own thread and its own
file, and shares only the stop flag. The CSI RX thread, the durable sink and the
live sink are byte-for-byte unchanged, and the `ftm` column is filled inside the
`capture.raw` pass the close-time summary already made.

## Reading it back

```python
from monad_knowledge.csi.timesync import load_fleet, node_offsets, align_to_reference

df   = await load_fleet(["csid:monad01/<sid>", "csid:monad02/<sid>", ...])
offs = node_offsets(df)          # host, offset_ns, uncertainty_ns, within_budget
aligned = align_to_reference(my_per_node_frame, offs)   # adds unix_ts_ns_aligned
```

`offset_ns` is what you **add** to a node's timestamps to put them on the
reference's timeline. A node that shared too few packets with the reference gets
`offset_ns = NaN` and `within_budget = False` — it is *unmeasured*, and
`align_to_reference` leaves its aligned column `NaN` rather than asserting a
zero offset nobody measured.

## Configuration

```toml
[timesync]
enabled          = true
required         = true    # fail at setup on profiles whose analysis POOLS NODES
ftm_tolerance_us = 2000
one_way_floor_us = 5000    # measured mgmt RTT 10.6 ms, power-save off
```

Ansible renders this from `csid_timesync_default` (fleet-wide, on) plus
per-profile `timesync_enabled` / `timesync_required`.

## Known limits — verify these on hardware

1. **An encrypted experiment SSID hides the phone's stamps.** WPA2/WPA3 encrypts
   the payload over the air, so a monitor-mode receiver sees ciphertext. Those
   frames are counted as `protected_frames` in the sidecar rather than dropped
   silently, and a large count with zero `rows_app` is the diagnosis. The
   injector's own frames are unaffected (they are unencrypted by construction),
   so inter-node skew still works. For an encrypted SSID, `collectord`'s
   four-timestamp exchange is the route for the phone side — it receives the
   datagram as a real UDP peer, above the crypto.
2. **A-MSDU aggregates and IPv6 are not parsed.** Counted as unrecognised.
3. **`hwtimestamp` / `SO_TIMESTAMPNS` support is per-driver.** The fallback to a
   userspace stamp is recorded per row, never assumed.
