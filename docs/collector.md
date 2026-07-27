# `collectord` — the UDP lab collector

Plays the **Collector** role of the MonadCount wireless-laboratory model: receive every stream,
stamp arrival with the reference clock, and be the single source of time for a session.

It exists because the numbers the phone experiments turn on can only be produced at the *receiving*
end. The phone knows what it commanded and what it handed to the kernel; only the collector knows
what actually arrived, when, and in what order.

## What it does

1. **Records arrivals.** Every MNDP datagram gets a kernel arrival timestamp (`SO_TIMESTAMPNS`) and
   one row in `arrivals.tsv`. Delivered rate, inter-arrival CV, max gap and loss all fall out of
   that file.
2. **Answers the clock exchange.** A `TIME_REQUEST` is replied to with the kernel arrival stamp
   (`t2`) and the send instant (`t3`), which is what lets the phone compute its offset against this
   node. The request's sequence number is echoed — the phone rejects a mismatched reply, because a
   stale one looks *fast* and would bias its minimum-delay filter the wrong way.
3. **Writes sessions in `csid`'s shape**, so the fleet's existing sync machinery ships them.

It does not analyse, does not talk to a database, and decides nothing about an experiment.

## Why kernel timestamps are not optional

A timestamp taken in userspace, after the scheduler has decided to wake the process, carries that
scheduler's jitter — which is the same order of magnitude as the quantity being measured. `doctor`
reports whether kernel stamping is active, and every sidecar records it in
`environment.kernel_timestamps`. **A userspace-stamped session must not be pooled with
kernel-stamped ones**: it measures the collector's scheduler as much as the channel.

On non-Linux hosts the fallback is used so the crate builds and can be exercised on a laptop. It is
reported, never silently substituted.

## Session lifecycle

A phone never announces the end of a session — it runs out of battery, walks out of range, or is
killed by the OS. Silence is therefore the only end-of-session signal: after
`session.idle_timeout_seconds` the session is closed, its sidecar flips from `receiving` to
`complete`, and only then does `collector-sync` ship it. A session cut short by a restart is
`stopped`, which still ships but says plainly that the recording was interrupted.

```text
<spool>/<session-uuid>/
  arrivals.tsv    one row per datagram: kernel arrival, sequence, phone monotonic, bytes
  exchanges.tsv   one row per answered clock exchange: t1, t2, t3, sequence
  metadata.json   the sidecar; its `status` is what makes the session shippable
```

## Where sessions land

`collector-sync` ships to

```text
s3://<bucket>/datasets/monad-app-sessions/<participant>/<session>/collector/
```

deliberately *beside* the phone's own artefacts for the same session, so an analysis opens one
prefix and finds both ends of the link. The participant comes from the `SESSION_HELLO` the phone
repeats with every clock burst; a session that never sent one ships under `unknown-participant`,
which is visible rather than lost.

The spool is **not** shared with `csid`: `csid-prune` deletes by csid's own filenames, so a shared
spool would leave these artefacts growing without bound.

## Running it

```bash
collectord --config /etc/collector/config.toml validate   # check a candidate config first
collectord --config /etc/collector/config.toml doctor     # what can this host actually do?
collectord --config /etc/collector/config.toml run
```

Deployment is the `csid` Ansible role with `csid_collector_enabled: true` — set on the node running
the experiment AP, which is the only address a phone joined to an isolated experiment SSID can
reach.

## Protocol

MNDP v1, defined once in `crates/collector/src/proto.rs` and mirrored in the app's `LabPacket.kt`.
The two must stay byte-identical. Types: `1` DATA, `2` TIME_REQUEST, `3` TIME_RESPONSE,
`4` SESSION_HELLO.
