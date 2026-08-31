"""Both backends, every fixture, identical output.

The spec's rule is that when two implementations of CSIQ disagree, the document
is authoritative and *both* are bugs. The PyO3 accelerator makes a third, which
raises that cost rather than lowering it. This file is what keeps it honest, and
it is **not advisory**: a divergence here fails the build.

Skipped in full when the accelerator is not installed, because the pure path is
the one that must always work and there is nothing to compare it against.
"""

from __future__ import annotations

import dataclasses
import os

import pytest

import csiq
from csiq import Capture, read_csiq
from csiq._backend import available

from conftest import container, record, write

pytestmark = pytest.mark.skipif(not available(), reason="csiq[fast] is not installed")


def _fields(rec) -> dict:
    """Every field of a record, by name.

    Not ``dataclasses.asdict``: that deep-copies, and ``iq`` is a zero-copy
    ``memoryview`` on both backends, which cannot be pickled. Copying it would
    also defeat the point of it being zero-copy.
    """
    return {f.name: getattr(rec, f.name) for f in dataclasses.fields(rec)}


def _both(path: str):
    """``(python_result, rust_result)`` for one file, in one process."""
    previous = os.environ.get("CSIQ_BACKEND")
    os.environ["CSIQ_BACKEND"] = "python"
    try:
        py_session, py_records = read_csiq(path)
        py = (py_session, [_fields(r) for r in py_records])
    finally:
        if previous is None:
            os.environ.pop("CSIQ_BACKEND", None)
        else:
            os.environ["CSIQ_BACKEND"] = previous

    rs_session, rs_records = read_csiq(path)
    return py, (rs_session, [_fields(r) for r in rs_records])


def _fixture(tmp_path) -> str:
    """One container exercising every field either backend can produce."""
    return write(tmp_path, container(
        [
            record(ftm=1, us=10, unix_ts_ns=1_700_000_000_000_000_000, seq=1,
                   rnf=(2 << 11) | (1 << 8), phy=(4, 7, 2), channel=36,
                   width_code=4, rssi=(-42, -55), bw_antsel=(2, 3),
                   mono_us=999_999, src_mac=b"\x02\x00\x00\x00\x00\x01",
                   node_temp_mc=54_321, nic_temp_c=61,
                   vendor_hdr=bytes(range(256)) + bytes(16),
                   ntone=8, nrx=2, ntx=1),
            # A record carrying only the required set, to check that absence
            # crosses the boundary as absence rather than as a default.
            record(ftm=2, rssi=None, bw_antsel=None, rnf=None,
                   mono_us=None, ntone=4, nrx=1, ntx=1),
            # The -127 sentinel, and a chain that did measure.
            record(ftm=3, rssi=(-60, csiq.RSSI_NO_MEASUREMENT), mono_us=5,
                   ntone=4, nrx=2, ntx=1),
        ],
        session={
            "session_id": "parity-fixture",
            "radio": {"band": "5", "channel": 36, "width": "80MHz",
                      "monitor": "wlp1s0mon0", "mac_filter": []},
            "environment": {"csid_version": "0.2.0",
                            "build": {"revision_source": "git",
                                      "csiq_format_version": 1}},
            "summary": {"records": 3, "empty_records": 0},
        },
    ))


def test_records_are_identical_field_by_field(tmp_path):
    path = _fixture(tmp_path)
    (py_session, py_records), (rs_session, rs_records) = _both(path)

    assert py_session == rs_session
    assert len(py_records) == len(rs_records) == 3
    for index, (left, right) in enumerate(zip(py_records, rs_records)):
        assert left.keys() == right.keys(), f"record {index}: field set differs"
        for key in left:
            # `iq` is a zero-copy buffer on both backends, so compare the values
            # it exposes rather than the view object.
            a, b = (list(left[key]), list(right[key])) if key == "iq" else (left[key], right[key])
            assert a == b, f"record {index}: {key} differs"
            assert type(a) is type(b), f"record {index}: {key} type differs"


def test_absence_crosses_the_boundary_as_absence(tmp_path):
    """A field the writer omitted must be ``None`` on both sides, never a default."""
    path = _fixture(tmp_path)
    (_, py_records), (_, rs_records) = _both(path)
    bare_py, bare_rs = py_records[1], rs_records[1]

    for field in ("bw_antsel", "mono_us", "vendor_hdr", "phy"):
        assert bare_py[field] is None
        assert bare_rs[field] is None
    assert bare_py["node"] == bare_rs["node"] == {}
    assert bare_py["rssi"] == bare_rs["rssi"] == []
    assert list(bare_py["iq"]) == list(bare_rs["iq"])


def test_derived_rules_agree_on_both_backends(tmp_path):
    """Parity on the raw fields is not enough — the rules built on them must agree."""
    path = _fixture(tmp_path)
    previous = os.environ.get("CSIQ_BACKEND")

    def snapshot() -> list[tuple]:
        with Capture.open(path) as cap:
            return [
                (
                    rec.bandwidth_mhz,
                    rec.tone_spacing_khz,
                    rec.is_own_transmission,
                    rec.fully_measured(),
                    tuple(rec.chains_measured()),
                    rec.mac,
                )
                for rec in cap.records()
            ]

    os.environ["CSIQ_BACKEND"] = "python"
    try:
        pure = snapshot()
    finally:
        if previous is None:
            os.environ.pop("CSIQ_BACKEND", None)
        else:
            os.environ["CSIQ_BACKEND"] = previous
    fast = snapshot()

    assert pure == fast


def test_matrix_is_identical_including_phase(tmp_path):
    """The two silent errors are phase errors, so parity must be checked on phase.

    Comparing ``|H|`` would pass against a backend that mirrored every phase,
    which is precisely the bug the spec spends a page on.
    """
    numpy = pytest.importorskip("numpy")
    path = _fixture(tmp_path)
    previous = os.environ.get("CSIQ_BACKEND")

    def matrices():
        with Capture.open(path) as cap:
            return [rec.H for rec in cap.records()]

    os.environ["CSIQ_BACKEND"] = "python"
    try:
        pure = matrices()
    finally:
        if previous is None:
            os.environ.pop("CSIQ_BACKEND", None)
        else:
            os.environ["CSIQ_BACKEND"] = previous
    fast = matrices()

    for left, right in zip(pure, fast):
        assert left.shape == right.shape
        numpy.testing.assert_array_equal(left, right)
        numpy.testing.assert_array_equal(numpy.angle(left), numpy.angle(right))


@pytest.mark.parametrize("broken, expected", [
    ("magic", csiq.BadMagic),
    ("version", csiq.UnsupportedVersion),
])
def test_both_backends_raise_the_same_class(tmp_path, broken, expected):
    """A caller must be able to write one ``except`` clause."""
    if broken == "magic":
        data = container([record()], magic=b"NOPE")
    else:
        data = container([record()], version=99)
    path = write(tmp_path, data)

    previous = os.environ.get("CSIQ_BACKEND")
    os.environ["CSIQ_BACKEND"] = "python"
    try:
        with pytest.raises(expected):
            Capture.open(path)
    finally:
        if previous is None:
            os.environ.pop("CSIQ_BACKEND", None)
        else:
            os.environ["CSIQ_BACKEND"] = previous

    with pytest.raises(expected):
        Capture.open(path)


def test_zst_always_takes_the_pure_path(tmp_path):
    """Decompression is the stdlib's job; the accelerator reads plain containers."""
    path = write(tmp_path, container([record()]), name="capture.csiq.zst")
    # Not a real zstd frame, so the pure path must fail on the envelope rather
    # than the accelerator failing on the magic — which is how we know which
    # path it took.
    with pytest.raises(csiq.CsiqError):
        Capture.open(path)
