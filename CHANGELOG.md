# Changelog

All notable changes to `csid`, `csiq` and `csiscope` are recorded here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), the
version is the workspace version in `Cargo.toml`, and every capture node stamps
that version into its sidecar as `environment.csid_version`. That stamp is the
reason a bump is not cosmetic: the measurement lake groups sessions by it, and a
feature that ships under an unchanged number is invisible in the archive.

Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A
sidecar or CSIQ field that is added with a serde default is a **minor** change;
a field whose meaning changes, or a schema identifier that bumps, is **major**.

The Python reader (`python/`, `csiq` on PyPI) carries its own version and its
own changelog in `python/README.md`.

## [Unreleased]

### Changed (breaking: needs a major version bump at release)

- **`csid ble-continuous` salts from a daily fleet key.** Every node derives
  the salt of a segment as `HMAC-SHA256(day_key, "csid-ble-fleet-salt/1" ‖
  segment_s ‖ index)`, `index = floor(unix_s / segment_s)`, so one address has
  one pseudonym on every node inside a segment and pseudonyms can be joined
  across nodes. `day_key` is the UTC day's key of a chain every node starts
  from one seed and moves forward itself: `day_key(d+1) =
  SHA-256("csid-ble-fleet-ratchet/1" ‖ day_key(d))`. Moving forward rewrites
  the key file atomically before the new key is used, so a finished day's key
  is gone and that day cannot be linked again. A frame whose day is before
  the chain's day (a node that booted in the past) gets a random salt and
  `hash_scope = "segment"`; a jump of more than 366 days is treated as a broken
  clock and leaves the chain alone. Segments are wall-clock aligned (:00 and
  :30 for `30m`), the segment length must divide a day, the first segment
  after a start is shorter, and the index is read from each frame's own
  timestamp. `--fleet-key` (default `/etc/csid/ble-fleet.key`) is the chain
  file `csid-ble-fleet-key/2` (`day <n>` / `key <64 hex>`); the scanner refuses
  to start without it and refuses a file readable by group or others. The
  in-session scanner (`csid run`) keeps its random per-session salt.
- **Seal `ble-continuous/2`.** `hash_scope` is `"fleet-segment"` (was
  `"segment"`), and a `salt` block carries `scheme`, `key_schedule`,
  `segment_s`, `segment_index`, `key_day`, `key_id` (a 4-byte public
  fingerprint of that day's key), `nominal_start_utc`, `nominal_end_utc` and
  `persisted: false`. Every `/1` field keeps its name and meaning. The footer
  of `ble_rssi.parquet` gains `salt_derivation`, `salt_segment_s`,
  `salt_segment_index`, `salt_key_day` and `salt_key_id`, and
  `pseudonym_scope` states the scope.
- **`ble_rssi.parquet` is `ble-rssi/4`: new nullable `oui` column.** The IEEE
  OUI (`aa:bb:cc`) of a public address (`addr_type` 0x00 or 0x02); null for
  every random address, which has none. The other 24 address bits are never
  stored. `/1`–`/3` files read with `oui` null.
- **Do not join `/1` segments across nodes.** Their salts are random per node,
  so equal hashes across nodes in `/1` data are coincidences.
- **CI publishes the fleet's binaries.** The `cross-build aarch64` job is
  replaced by `pi release (aarch64)` on native `ubuntu-24.04-arm` (pinned: the
  node image is Ubuntu 24.04, glibc 2.39). It tests `csiq`, `csid`,
  `csiscope` and `collector` on aarch64, builds them, and uploads
  `csid-aarch64-<commit>` (`csid`, `csiscope`, `collectord`, `SHA256SUMS`,
  `COMMIT`, `GLIBC`, `VERSION`, 90 days). monad-knowledge's `roles/csid`
  installs that artifact; nodes no longer compile. `fmt` and `clippy` are now
  separate jobs, so a style or lint failure no longer hides the test result.
- **Lint fixes that had kept CI red.** `rawsock::FRAME_BUF` is now the one
  buffer size the census and time-transfer receivers use (each had its own
  copy, and the shared one was dead code under `-D warnings`); one test builds
  its `InjectConfig` with struct-update syntax. `cargo fmt --check` (15 files)
  and clippy in `csiscope` (4 × `neg_cmp_op_on_partial_ord`, 1 ×
  `unnecessary_cast`) still fail; the `!(a < b)` forms may be deliberate NaN
  handling and were left alone.

### Added

- **BLE heartbeats carry the interval statistics blescand used to log.** The
  in-session `ble scanning` line and the continuous `continuous BLE scanning`
  line now both carry `devices` (distinct pseudonyms in the 10 s interval),
  `devices_total` (distinct pseudonyms since the salt was drawn: the session
  for `csid@`, the 30-minute segment for `csid-ble`) and `rssi_median` (omitted
  when no report in the interval had an RSSI). The continuous line also gains
  `rate_hz`. The monitoring stack's `monad_ble:*` recording rules read these;
  before this, the continuous owner logged only a cumulative `observations`.
- **`BLE segment sealed`** is logged once per continuous segment after its
  `session.json` is durable, with `rows` and `devices_total`. These two
  entries are journal fields only.
- **`ble_rssi.parquet` gains `company_id` (schema `ble-rssi/3`).** The 16-bit
  Bluetooth SIG company identifier that opens the first Manufacturer Specific
  Data structure of an advertisement (Apple 0x004C, Microsoft 0x0006, …), or
  null. It gives a vendor breakdown of the population and no identity: every
  device of a manufacturer shares it, and the manufacturer payload after it is
  still not read. The durable log carries it as an optional field, so older
  logs keep parsing. The footer adds `csid.company_id_source`. The continuous
  segment sidecar now takes its `observations_schema` from the same constant,
  where it was a literal. A schema identifier bump is a **major** change under
  the policy above.

## [0.3.1] - 2026-09-16

Three defects found while reading why the fleet's matrix arms reported
failure on 2026-09-15 and 2026-09-16 with every node capturing.

### Fixed

- **`session-quality --locate-only` no longer fails on a foreign sidecar.**
  It parsed every `metadata.json` in the spool and propagated the first parse
  error. monad05 held a zero-byte sidecar from a session opened on a full disk
  on 2026-08-18, so from 2026-09-15 20:12 UTC every matrix arm whose
  comparison transmitter was monad05 died at its locate step after both
  cohorts had started (entries 2026-09-15-07, -08 and 2026-09-16-02). The
  locate now opens only directories named `_<profile>_`, skips segment
  directories (`-segNNNN`, which repeat the root's run id and would have
  become a second match at the first rotation), and logs an unreadable
  sidecar by path instead of failing.
- **The sidecar is written atomically** (temp file plus rename). `fs::write`
  truncates and then writes, so ENOSPC or a crash between the two left an
  empty `metadata.json` that `csid-sync`, `csid-prune` and `session-quality`
  each misread in a different way.

### Changed

- **The journal heartbeat ticks every 60 s and names its run.** The cadence
  was `WATCHDOG_USEC / 3`, so raising `WatchdogSec` to 900 s on 2026-09-06
  silently moved the `capturing` line to every 300 s; the
  `monad_csi:capture_active` recording rule and the arm playbooks' verify
  step were both sized for the old value. The line now carries `run_id` and
  `session_id` as fields (`F_RUN_ID`, `F_SESSION_ID` in the journal), so a
  Loki reader can count liveness per planned run rather than per unit.

## [0.3.0] - 2026-09-04

Three instruments added after the transmitter identity in the CSI header was
found to go dark ten minutes into every 0.2.0 session (see the diary of
2026-09-04 in `monad-knowledge`). **None of the three has run on a node yet**;
the host and Linux container test suites pass (305 tests).

### Added

- **Frame census** (`[census]`, `census.rs`, `rawsock.rs`). A second
  `AF_PACKET` thread on the monitor interface classifies every received frame
  by type, subtype, transmitter address (`addr2`) and BSSID, and writes
  per-minute counts to `frame_census.jsonl` (schema `frame-census/1`). The
  sidecar's `summary.census` names the busiest transmitters and the beaconing
  BSSIDs for the whole session, and counts the three frames that make
  beamforming-feedback sensing possible: `ndpa`, `vht_bfi`, `he_bfi`. Nothing
  per frame is kept and no payload is decoded. Exists because the CSI header's
  `src_mac` is firmware-written and reads as the fill `ef:be:ad:de:ad:de` for
  most of a session on this hardware, and the driver's side-channel copy of the
  true `addr2` is disabled upstream as not frame-aligned (`flq-mvm.c`).
- **Channel survey** (`[survey]`, `survey.rs`). At session open and again at
  close, `iw dev <interface> link` and `iw dev <interface> scan` on the
  **management** radio, recorded in the sidecar as `survey.at_open` and
  `survey.at_close`: every BSS heard with frequency, channel, band, width,
  signal and SSID, plus the node's own association. Validation refuses the
  capture interface or the monitor as the survey interface. A survey never
  fails a session. New CLI: `csid survey --interface wlan0 [--json]`.
- **Associated capture** (`capture.mode = "sta"`, `[sta]`) — IP-139 Phase 7,
  items 2 to 4. No monitor interface is created and no tune is commanded; the
  channel, width and centre are observed off the association and the profile's
  radio block is overwritten with them so every downstream reader sees one
  tuning. The sidecar carries `radio.observed = true`, `radio.bssid` and
  `radio.ssid`. `[sta].require_assoc` (default true) fails a session whose
  interface is not associated; `false` waits up to a minute for the
  supplicant. `[sta].ssid` and `[sta].bssid` refuse any other link.
  `[timesync]` and `[census]` are refused in this mode, because both need the
  monitor interface. The on-hardware capability gate (Phase 7 item 1) is
  authored as a fleet bench and has not run.
- `caps::freq_to_channel`, the inverse of `channel_to_freq`, and
  `WidthCfg::from_observed`, both needed to read a link back into a tuning.
- `radio::read_link` and `radio::ObservedLink`.
- `sidecar::CensusMeta`, `sidecar::CensusSummary`, `sidecar::SurveyMeta`, and
  `Sidecar::set_survey_close`.

### Changed

- The `AF_PACKET` receive socket that `timesync::rx` opened privately now
  lives in `rawsock.rs` and is shared with the census. Behaviour of the
  time-transfer receiver is unchanged; its `RxSocket::open` gained a caller
  name for error text.
- `Sidecar::open` takes the observed link and the open-time survey as two
  more arguments.
- `ExperimentConfig::validate` skips `caps::validate_radio` in STA mode (the
  radio block's channel is not a tune there) and validates the new blocks.
- Documentation: `docs/configuration.md` gains the `[census]`, `[survey]` and
  `[sta]` field references; the module map in `lib.rs` gains three rows.

### Compatibility

- Every new sidecar field carries a serde default and every new block is
  `skip_serializing_if` absent, so a 0.2.0 reader parses a 0.3.0 sidecar and a
  0.3.0 daemon reads its whole back catalogue. `csid-session/1` is unchanged.
- The CSIQ container is unchanged at the version 0.2.0 wrote.
- The profile template in the fleet's Ansible role renders `[census]` and
  `[survey]` on by default; a node still on 0.2.0 that receives such a profile
  fails validation on the unknown table (`deny_unknown_fields`). Deploy the
  binary before the profiles.

## [0.2.0] - 2026-08-24

The first bump. Everything between the initial release and this one shipped
under the `0.1.0` literal, which is why the whole archive carried one distinct
`csid_version` until this release; the build provenance below is the fix.
Reconstructed from the commit history and the sidecar's own field notes.

### Added

- **Build provenance** in the sidecar (`environment.build`: revision,
  revision source, build time, rustc, profile, CSIQ format version), baked in
  by `build.rs`. A build that cannot name its revision says so.
- **Achieved-tuning readback** (`radio.achieved_control_freq_mhz`,
  `achieved_width_mhz`, `achieved_center_freq_mhz`) read from
  `iw dev … info` after every tune, because `iw` exiting 0 was not the same
  fact as the radio holding a wide width. `center_freq_mhz` alone says what
  csid asked for.
- **`empty_records`** in the summary and the status document: of `records`,
  how many carried an all-zero I/Q matrix. Counted on the raw bytes.
- **Filter fingerprint** (`filter`): the PHY selection in force, recorded
  whether or not any of it is set, so two differently-filtered captures cannot
  pool by accident. Every field is currently unset and the sidecar says so.
- **Per-frame bandwidth** in CSIQ (TLV `0x11`, `BW_ANTSEL`): the bandwidth
  and antenna-selection bits of `rate_n_flags`, so a mixed-width class can be
  told apart per frame. Files written before this carry no `0x11` and the
  reader falls back to the session width.
- **Segment sealing embeds the finalised session block** in each segment's
  CSIQ, and `status` is true at close; before this the export read the
  deliberately-`capturing` sidecar from disk.
- **Node and host state series** (`summary.node_state`): SoC temperature,
  throttle flags, spool free bytes, load and NIC die temperature, sampled in
  the capture loop and stamped relative to session open.
- **Per-segment transmitter census** (`summary.transmitters`) by the CSI
  header's source MAC, so delivery can be read mid-run. (Superseded for
  identity by the frame census in 0.3.0; still correct for the injector.)
- `monitor_tx_rate` debugfs knob driven from `[inject]`, with
  `config::validate_monitor_tx_rate` refusing HE, EHT, CCK and undecodable
  words at config load and `allow_untransmittable_rate` as the deliberate
  bench override.
- `nic_temp` readback from `iwlmvm` debugfs.
- Python bindings (`csiq-py`, the `csiq-fast` backend) and the pure-Python
  `csiq` reader's `capture` module.
- `docs/CSIQ-format-v1.md`: the format's design, layout and versioning policy.

### Fixed

- Inter-node skew: pairs are measured over one common sequence window, so
  `skew(x,y) + skew(y,z) - skew(x,z)` closes (`timesync/skew.rs`).

## [0.1.0] - 2026-07-22

Initial release of `csid`, `csiq` and `csiscope`: the systemd-native capture
daemon for the Intel AX210 on the `iax` driver, the self-describing CSIQ
container, and the live console.

Shipped under this literal without a bump, between 2026-07-22 and 2026-08-24:
paced monitor-mode injection (`capture.mode = "inject"`), BLE co-capture with
per-session pseudonyms and lab-namespace matching, time transfer over the
illumination stream (`time_transfer.parquet`, inter-node skew, the phone affine
fit), segment rotation with sync-while-running, the fleet cockpit (`csid fleet
…`), block markers, thermal benchmarking and the observability export.

[0.3.0]: https://github.com/Sibyx/csid/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Sibyx/csid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Sibyx/csid/releases/tag/v0.1.0
