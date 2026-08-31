"""One test per consumer rule the spec documents as silently failing.

Each of these rules produces a result that *looks healthy* when it is broken.
Every test therefore asserts two things: the right answer comes back, and the
appealing wrong answer does not. A test that only checked the right answer would
pass against several of the implementations these rules exist to rule out.
"""

from __future__ import annotations

import math

import pytest

import csiq
from csiq import Capture, CsiRecord
from csiq.errors import (
    BadMagic,
    DesyncError,
    FieldNotRecorded,
    MissingRequiredField,
    UnsupportedVersion,
)

from conftest import container, record, write


# -- Rule 1: the matrix is chain-major ----------------------------------------


def test_matrix_is_chain_major_not_tone_interleaved(tmp_path):
    """``chain(c)`` is a contiguous block, not every c-th coefficient.

    The payload is ``nrx*ntx`` blocks of ``ntone``. Reading it tone-major smears
    the impulse response — chain-major is more compact in 99.4% of 5,186 records.
    """
    ntone, nrx, ntx = 3, 2, 1
    # chain 0 tones 0..2 then chain 1 tones 0..2, imaginary-first pairs.
    iq = [0, 1, 2, 3, 4, 5, 100, 101, 102, 103, 104, 105]
    path = write(tmp_path, container([record(ntone=ntone, nrx=nrx, ntx=ntx, iq=iq)]))

    with Capture.open(path) as cap:
        rec = next(cap.records())
        chain0, chain1 = rec.chain(0), rec.chain(1)

    # Chain 0 is the FIRST BLOCK of six i16, not the even-indexed coefficients.
    assert chain0.tolist() == [1 + 0j, 3 + 2j, 5 + 4j]
    assert chain1.tolist() == [101 + 100j, 103 + 102j, 105 + 104j]
    # The tone-major misreading would interleave the two chains.
    assert chain0.tolist() != [1 + 0j, 101 + 100j, 3 + 2j]
    # The matrix view is [ntone, nrx*ntx] built from that storage.
    assert rec.H.shape == (ntone, nrx * ntx)
    assert rec.H[:, 0].tolist() == chain0.tolist()


# -- Rule 2: each coefficient is imaginary first -------------------------------


def test_coefficients_are_imaginary_first(tmp_path):
    """``value = iq[i+1] + 1j*iq[i]``.

    The swapped order yields ``i*conj(H)``: ``|H|`` is untouched, so amplitude
    work looks perfectly healthy while every phase is mirrored. On real captures
    the correct order concentrates 21.5x more impulse-response energy at early
    delays; the swap inverts that to 0.48, an anti-causal channel.
    """
    path = write(tmp_path, container([record(ntone=1, nrx=1, ntx=1, iq=[7, 3])]))

    with Capture.open(path) as cap:
        value = next(cap.records()).H[0, 0]

    assert value == complex(3, 7)          # real=3 (second), imag=7 (first)
    assert value != complex(7, 3)          # the swapped reading
    # The magnitude is identical either way, which is exactly why the bug hides.
    # float32 storage, so compare at single precision rather than double.
    assert math.isclose(abs(value), abs(complex(7, 3)), rel_tol=1e-6)


# -- Rule 3: -127 dBm is a sentinel, not a weak signal -------------------------


def test_rssi_sentinel_is_not_a_weak_measurement(tmp_path):
    """A chain at ``-127`` carries a stale duplicate and must be discarded.

    -127 dBm sits ~26 dB below a 20 MHz channel's thermal noise floor, so it
    cannot be a measurement. Verified as an exact biconditional over 44,577
    records: a chain reads -127 iff its CSI block duplicates the previous one.
    """
    good = record(rssi=(-60, -64))
    half = record(rssi=(-60, csiq.RSSI_NO_MEASUREMENT))
    path = write(tmp_path, container([good, half]))

    with Capture.open(path) as cap:
        first, second = list(cap.records())

    assert first.fully_measured() and first.chains_measured() == [True, True]
    assert not second.fully_measured()
    assert second.chains_measured() == [True, False]
    # A record with one good chain stays usable single-chain.
    assert second.rssi_dbm[0] == -60
    # And the sentinel is never silently treated as a very weak reading.
    assert min(second.rssi_dbm) == csiq.RSSI_NO_MEASUREMENT


def test_measured_keeps_only_fully_measured_records(tmp_path):
    path = write(tmp_path, container([
        record(rssi=(-60, -64), mono_us=1),
        record(rssi=(-60, csiq.RSSI_NO_MEASUREMENT), mono_us=2),
    ]))
    with Capture.open(path) as cap:
        assert len(list(cap.measured())) == 1


# -- Rule 4: absent MONO_US means "own transmission" ---------------------------


def test_absent_mono_us_marks_an_own_transmission(tmp_path):
    """Not a missing value and not a broken clock.

    A locally generated frame never traverses the receive path that stamps
    CLOCK_MONOTONIC. Verified as an exact biconditional over 2,433 records.
    """
    path = write(tmp_path, container([
        record(ftm=1, mono_us=None),   # the node's own injected frame
        record(ftm=2, mono_us=9_000),  # genuinely received
    ]))

    with Capture.open(path) as cap:
        assert cap.mono_us_recorded()
        assert [r.ftm for r in cap.received()] == [2]
        assert [r.ftm for r in cap.own_transmissions()] == [1]
        own = next(cap.own_transmissions())
        assert own.mono_us is None
        assert own.mono_us != 0  # never read absence as zero


def test_writer_without_mono_us_refuses_rather_than_returning_nothing(tmp_path):
    """Type code 0x12 postdates much of the archive.

    On such a file every record looks like an own transmission, so the rule is
    inapplicable rather than false. Returning an empty iterator would read as
    "this capture received nothing", which is a claim about the radio.
    """
    path = write(tmp_path, container(
        [record(ftm=1), record(ftm=2)],
        session={"environment": {"csid_version": "0.1.0"}},
    ))

    with Capture.open(path) as cap:
        assert cap.mono_us_recorded() is False
        with pytest.raises(FieldNotRecorded) as excinfo:
            next(cap.received())
        assert "MONO_US" in str(excinfo.value)
        assert "0.1.0" in str(excinfo.value)
        # The records themselves are still readable.
        assert len(list(cap.records())) == 2


# -- Rule 5: absent BW_ANTSEL is not 20 MHz ------------------------------------


def test_bandwidth_falls_back_to_rnf_then_reports_absence(tmp_path):
    """Prefer 0x11; else decode 0x04; else ``None`` — never 20 MHz by default."""
    # RATE_MCS_CHAN_WIDTH lives at bits 11-13 of rate_n_flags v2; code 2 = 80 MHz.
    rnf_80 = 2 << 11
    path = write(tmp_path, container([
        record(ftm=1, bw_antsel=(1, 0)),        # explicit: code 1 = 40 MHz
        record(ftm=2, bw_antsel=None, rnf=rnf_80),  # recovered from RNF
        record(ftm=3, bw_antsel=None, rnf=None),    # genuinely unrecorded
    ]))

    with Capture.open(path) as cap:
        explicit, recovered, absent = list(cap.records())

    assert explicit.bandwidth_mhz == 40
    assert recovered.bandwidth_mhz == 80        # no re-capture needed
    assert absent.bandwidth_mhz is None
    assert absent.bandwidth_mhz != 20           # the appealing wrong default


# -- Rule 6: WIDTH describes the receiver, not the record ----------------------


def test_width_is_a_session_constant_and_bandwidth_is_per_record(tmp_path):
    """A 20 MHz frame on a 160 MHz monitor still reports its own bandwidth."""
    path = write(tmp_path, container(
        [record(width_code=5, bw_antsel=(0, 0))],  # monitor 160 MHz, frame 20 MHz
        session={"radio": {"width": "160MHz", "channel": 36}},
    ))

    with Capture.open(path) as cap:
        rec = next(cap.records())
        assert cap.session.radio.width == "160MHz"   # what the receiver could decode
        assert rec.bandwidth_mhz == 20               # what this frame actually was
        assert rec.width == "160MHz"                 # the raw field, unchanged


# -- The tone grid -------------------------------------------------------------


@pytest.mark.parametrize(
    "ntone, bw_code, expected_khz, label",
    [
        (242, 0, 78.125, "HE20 — 75.6 MHz would not fit in 20 MHz"),
        (242, 2, 312.5, "VHT80 — the same tone count, four times the spacing"),
        (52, 0, 312.5, "legacy OFDM in 20 MHz"),
    ],
)
def test_tone_spacing_separates_he20_from_vht80(tmp_path, ntone, bw_code, expected_khz, label):
    path = write(tmp_path, container([
        record(ntone=ntone, nrx=1, ntx=1, bw_antsel=(bw_code, 0))
    ]))
    with Capture.open(path) as cap:
        assert next(cap.records()).tone_spacing_khz == expected_khz, label


def test_tone_spacing_is_none_when_bandwidth_is_unknown(tmp_path):
    """A guess here silently rescales every frequency axis downstream."""
    path = write(tmp_path, container([record(ntone=242, bw_antsel=None, rnf=None)]))
    with Capture.open(path) as cap:
        assert next(cap.records()).tone_spacing_khz is None


# -- Rule 7: SEQ is a driver report counter ------------------------------------


def test_seq_is_readable_as_a_completeness_counter(tmp_path):
    """A gap in SEQ is a dropped report, detectable modulo 256."""
    path = write(tmp_path, container([
        record(ftm=1, seq=10), record(ftm=2, seq=11), record(ftm=3, seq=14),
    ]))
    with Capture.open(path) as cap:
        seqs = [r.seq for r in cap.records()]
    assert seqs == [10, 11, 14]
    gaps = sum((b - a - 1) % 256 for a, b in zip(seqs, seqs[1:]))
    assert gaps == 2


# -- Rule 8: a NODE_* absence is not a zero ------------------------------------


def test_node_series_is_sparse_and_absence_is_not_zero(tmp_path):
    """A writer emits these at an interval, so most records carry none."""
    path = write(tmp_path, container([
        record(ftm=1, node_temp_mc=54_321, nic_temp_c=61),
        record(ftm=2),
    ]))
    with Capture.open(path) as cap:
        sampled, quiet = list(cap.records())

    assert sampled.node["temp_mc"] == 54_321
    # Two sensors, two units. Never rescale one into the other.
    assert sampled.node["nic_temp_c"] == 61
    assert quiet.node == {}
    assert "temp_mc" not in quiet.node       # absence, not a cold reading
    assert quiet.node.get("temp_mc") is None


# -- Container-level refusals --------------------------------------------------


def test_bad_magic_is_refused_by_type(tmp_path):
    path = write(tmp_path, container([record()], magic=b"NOPE"))
    with pytest.raises(BadMagic):
        Capture.open(path)


def test_unknown_version_is_refused_rather_than_guessed(tmp_path):
    """The spec requires refusal: a bump means the meaning or framing changed."""
    path = write(tmp_path, container([record()], version=99))
    with pytest.raises(UnsupportedVersion) as excinfo:
        Capture.open(path)
    assert excinfo.value.found == 99
    assert excinfo.value.supported == 1


def test_desync_stops_rather_than_emitting_garbage(tmp_path):
    """The 0xA1 tag is a framing check, not decoration."""
    good = container([record(ftm=1)])
    path = write(tmp_path, good + b"\x00\x00\x00\x00\x00")
    with Capture.open(path) as cap:
        with pytest.raises(DesyncError):
            list(cap.records())


def test_missing_required_field_is_rejected_not_defaulted(tmp_path):
    path = write(tmp_path, container([record(omit=["ntone"])]))
    with Capture.open(path) as cap:
        with pytest.raises(MissingRequiredField) as excinfo:
            list(cap.records())
    assert excinfo.value.missing == ["ntone"]


def test_unknown_type_codes_are_skipped(tmp_path):
    """Forward compatibility: adding a type code is not a version bump."""
    from conftest import tlv

    body = record(ftm=7)
    # Splice a reserved 802.11bf code (0x30) the reader has never seen.
    import struct as _s
    payload = body[5:] + tlv(0x30, b"\xde\xad\xbe\xef")
    spliced = _s.pack("<BI", 0xA1, len(payload)) + payload
    path = write(tmp_path, container([spliced]))

    with Capture.open(path) as cap:
        assert next(cap.records()).ftm == 7


# -- Envelope, streams, and repeat reads ---------------------------------------


def test_envelope_is_decided_by_extension(tmp_path):
    path = write(tmp_path, container([record()]))
    with Capture.open(path) as cap:
        assert cap.envelope.form == "csiq"
        assert cap.envelope.compressed is False


def test_records_can_be_iterated_more_than_once(tmp_path):
    path = write(tmp_path, container([record(ftm=1), record(ftm=2)]))
    with Capture.open(path) as cap:
        first = [r.ftm for r in cap.records()]
        second = [r.ftm for r in cap]
    assert first == second == [1, 2]


def test_closed_capture_refuses_to_read(tmp_path):
    path = write(tmp_path, container([record()]))
    cap = Capture.open(path)
    cap.close()
    cap.close()  # idempotent
    with pytest.raises(csiq.CsiqError):
        list(cap.records())


def test_clock_unwraps_the_ftm_wrap(tmp_path):
    """FTM wraps every ~13.42 s; a value below its predecessor implies one wrap."""
    path = write(tmp_path, container([
        record(ftm=(1 << 32) - 1000), record(ftm=500),
    ]))
    with Capture.open(path) as cap:
        seconds = [clk.ftm_seconds() for _, clk in cap.clocked()]
    assert seconds[0] == 0.0
    # 1500 ticks at 320 MHz, not a jump backwards of 4.29 billion.
    assert math.isclose(seconds[1], 1500 / csiq.FTM_HZ)
    assert seconds[1] > 0


# -- Versions: the number in the header is not the capability set --------------


def test_container_version_does_not_describe_the_field_set(tmp_path):
    """Two writers both stamping version 1 can differ by five type codes.

    Adding a type code is explicitly not a version bump, so the header number
    cannot tell a consumer whether ``MONO_US`` is available — and that, not the
    number, is what decides whether an analysis is possible.
    """
    old_writer = write(tmp_path, container(
        [record(ftm=1, bw_antsel=None, rnf=None)],
        session={"environment": {"csid_version": "0.1.0"}},
    ), name="old.csiq")
    new_writer = write(tmp_path, container(
        [record(ftm=1, mono_us=5, bw_antsel=(0, 0), vendor_hdr=b"\x00" * 272,
                node_temp_mc=54_000, nic_temp_c=61)],
        session={"environment": {"csid_version": "0.2.0",
                                 "build": {"revision_source": "git",
                                           "csiq_format_version": 1}}},
    ), name="new.csiq")

    with Capture.open(old_writer) as old, Capture.open(new_writer) as new:
        # Same container version.
        assert old.format_version == new.format_version == 1
        # Entirely different capability sets.
        assert old.capabilities().mono_us is False
        assert new.capabilities().mono_us is True
        assert old.capabilities().vendor_hdr is False
        assert new.capabilities().vendor_hdr is True
        assert new.capabilities().nic_temp is True
        assert new.session.environment.build.recorded is True
        assert old.session.environment.build.recorded is False


def test_capabilities_report_how_many_records_were_probed(tmp_path):
    """Absence is only ever evidence over the records examined."""
    path = write(tmp_path, container([record(ftm=i) for i in range(1, 6)]))
    with Capture.open(path) as cap:
        caps = cap.capabilities(probe=3)
    assert caps.probed == 3
    assert "RSSI (0x0C)" in caps.names


# -- Session block: the group table is idealised -------------------------------


def test_flat_session_block_still_types(tmp_path):
    """Pre-0.2.0 sidecars put identity and lifecycle fields at the top level.

    A view that only reads named groups returns empty here and looks fine.
    """
    path = write(tmp_path, container([record()], session={
        "session_id": "monad02_x_20260816-080102",
        "experiment": "drift-overnight-100",
        "schema": "csid-session/1",
        "started_at": "2026-08-16T08:01:02Z",
        "ended_at": None,
        "status": "capturing",
        "radio": {"band": "2.4", "channel": 11, "width": "HT20",
                  "monitor": "wlp1s0mon0", "mac_filter": []},
        "environment": {"csid_version": "0.1.0", "hostname": "monad02"},
        "summary": None,
        "timesync": {"required": False},
    }))

    with Capture.open(path) as cap:
        s = cap.session

    assert s.identity.session_id == "monad02_x_20260816-080102"
    assert s.lifecycle.started_at == "2026-08-16T08:01:02Z"
    assert s.radio.channel == 11
    # `monitor` is an interface name, not a boolean.
    assert s.radio.monitor == "wlp1s0mon0"
    assert s.radio.mac_filter == ()
    # An unknown group is reported, never dropped.
    assert s.groups_not_typed == ["timesync"]
    assert s.raw["timesync"] == {"required": False}


def test_capturing_status_on_an_old_writer_is_not_a_truncated_capture(tmp_path):
    """Before csid 0.2.0 the embedded status said `capturing` forever."""
    old = write(tmp_path, container([record()], session={
        "status": "capturing", "environment": {"csid_version": "0.1.0"}}), name="a.csiq")
    new = write(tmp_path, container([record()], session={
        "status": "completed", "environment": {"csid_version": "0.2.0"}}), name="b.csiq")

    with Capture.open(old) as cap:
        assert cap.session.lifecycle.status == "capturing"
        assert cap.session.lifecycle.status_is_trustworthy is False
    with Capture.open(new) as cap:
        assert cap.session.lifecycle.status_is_trustworthy is True


def test_empty_fingerprint_is_not_no_filter(tmp_path):
    """`no-filter` is a recorded fact; `""` is the absence of a record."""
    unrecorded = write(tmp_path, container([record()], session={
        "filter": {"fingerprint": ""}}), name="a.csiq")
    unfiltered = write(tmp_path, container([record()], session={
        "filter": {"fingerprint": "no-filter"}}), name="b.csiq")

    with Capture.open(unrecorded) as cap:
        assert cap.session.filter.filtering_known is False
        assert cap.session.filter.filtered is None   # not False
    with Capture.open(unfiltered) as cap:
        assert cap.session.filter.filtering_known is True
        assert cap.session.filter.filtered is False


def test_empty_records_absent_is_not_zero(tmp_path):
    """A pre-counter sidecar records no value; printing 0 asserts a measurement."""
    path = write(tmp_path, container([record()], session={"summary": {"records": 100}}))
    with Capture.open(path) as cap:
        assert cap.session.summary.records == 100
        assert cap.session.summary.empty_records is None
        assert cap.session.summary.useful_records is None
