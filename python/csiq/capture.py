"""``Capture`` — the ergonomic layer over the reference reader.

WHY THIS EXISTS. :func:`csiq.read_csiq` returns a dict and a generator, which is
the right shape for a reference parser and the wrong shape for a consumer. CSIQ
v1 documents eight rules whose violation is **silent** — the result looks healthy
and is wrong — and none of them is expressible in a dict plus a generator. Every
consumer therefore re-derives them, and the ones who have not read the spec write
correct-looking code that no test they would think to write can catch.

This module makes each of those rules a named method with the spec's own
measurement in its docstring. Reading the docstring teaches the rule.

    from csiq import Capture

    with Capture.open("capture.csiq.zst") as cap:
        print(cap.session.radio.channel)
        for rec in cap.received():        # own transmissions excluded
            H = rec.H                     # chain-major, imaginary-first
            if not rec.fully_measured():
                continue                  # a -127 chain is stale, not weak

WHAT IT DOES NOT DO. No filtering, calibration or phase sanitisation. Those are
analysis choices and the format deliberately does not describe them.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any, BinaryIO, Iterable, Iterator, Optional

from .errors import CsiqError, FieldNotRecorded
from .reader import (
    FORMAT_VERSION,
    FTM_HZ,
    _as_binary,
    RSSI_NO_MEASUREMENT,
    CsiRecord,
    FtmUnwrapper,
    read_csiq,
)
from .session import Session

#: Subcarrier spacing in kHz for the two OFDM numerologies CSIQ can carry.
NARROW_SPACING_KHZ = 78.125
WIDE_SPACING_KHZ = 312.5


def bandwidth_mhz(rec: CsiRecord) -> Optional[int]:
    """The bandwidth of the frame this record describes, in MHz, or ``None``.

    Function form of :attr:`csiq.CsiRecord.bandwidth_mhz`, for a caller holding a
    bare record. See that property for the reader rule and why an absent value
    is not 20 MHz.
    """
    return rec.bandwidth_mhz


def tone_spacing_khz(rec: CsiRecord) -> Optional[float]:
    """Subcarrier spacing in kHz, or ``None`` when the bandwidth is unknown.

    Function form of :attr:`csiq.CsiRecord.tone_spacing_khz`.
    """
    return rec.tone_spacing_khz


def is_own_transmission(rec: CsiRecord) -> bool:
    """True when this record is the capturing node's own frame, looped back.

    Function form of :attr:`csiq.CsiRecord.is_own_transmission`. An absent
    ``MONO_US`` is a semantic marker, verified as an exact biconditional over
    2,433 records — not a missing value, and never a zero.
    """
    return rec.is_own_transmission


class Clock:
    """The four clocks a record can carry, with the unwrap already applied.

    They are not redundant. Each answers a different question and fails its own
    way, so the rule is: **analyse on FTM, anchor wallclock on UNIX_TS_NS.**

    ============  ==================  ==========  ============================
    Field         Source              Wraps       Use for
    ============  ==================  ==========  ============================
    ``ftm``       radio, 320 MHz      ~13.42 s    all timing analysis
    ``us``        firmware            ~71.6 min   coarse cross-checks
    ``unix_ts_ns``host kernel         never       wallclock anchoring
    ``mono_us``   host MONOTONIC      never       surviving a clock step
    ============  ==================  ==========  ============================

    ``ftm`` is stamped in the RF plane before any host software runs, so it is
    immune to scheduling jitter. It is also free-running per radio, so it is not
    comparable across nodes — that is what ``unix_ts_ns`` is for.

    This class holds the unwrap state, so it is per-iteration and stateful.
    Records must be pushed in file order.
    """

    __slots__ = ("_unwrapper", "_first_ticks", "_ticks")

    def __init__(self) -> None:
        self._unwrapper = FtmUnwrapper()
        self._first_ticks: Optional[int] = None
        self._ticks = 0

    def push(self, rec: CsiRecord) -> int:
        """Feed one record in file order; returns its unwrapped FTM ticks."""
        self._ticks = self._unwrapper.push(rec.ftm)
        if self._first_ticks is None:
            self._first_ticks = self._ticks
        return self._ticks

    @property
    def ftm_ticks(self) -> int:
        """Unwrapped 320 MHz ticks for the most recently pushed record."""
        return self._ticks

    def ftm_seconds(self) -> float:
        """Seconds since the first pushed record, on the radio's own clock.

        Relative by construction. The FTM clock is free-running per radio, so an
        absolute value from it means nothing outside this one capture.
        """
        if self._first_ticks is None:
            return 0.0
        return (self._ticks - self._first_ticks) / FTM_HZ


@dataclass(frozen=True, slots=True)
class Capabilities:
    """Which optional fields this capture's writer actually emits.

    **This is the version answer a self-describing format gives.** The container
    version is 1 and stays 1: the spec's own policy is that adding a type code is
    not a version bump, because readers skip what they do not know. So the number
    stamped in the header cannot tell a consumer whether ``MONO_US`` is available
    — and that, not the number, is what changes whether an analysis is possible.

    Two writers both stamping version 1 can differ by five type codes. A capture
    from csid 0.1.0 carries no ``MONO_US``, so own transmissions cannot be told
    from received frames; one from a current build carries it, and the same code
    then answers a question it could not answer before. Probe, do not assume.

    ``probed`` says how many records were examined. Absence is only ever evidence
    over the records looked at, and this field is what stops that from being
    forgotten.
    """

    mono_us: bool = False
    bw_antsel: bool = False
    vendor_hdr: bool = False
    node_state: bool = False
    nic_temp: bool = False
    rssi: bool = False
    phy: bool = False
    src_mac: bool = False
    probed: int = 0

    @property
    def names(self) -> tuple[str, ...]:
        """The optional fields that were present, as spec names."""
        mapping = (
            ("mono_us", "MONO_US (0x12)"),
            ("bw_antsel", "BW_ANTSEL (0x11)"),
            ("vendor_hdr", "VENDOR_HDR (0x14)"),
            ("node_state", "NODE_* (0x40-0x43)"),
            ("nic_temp", "NODE_NIC_TEMP_C (0x44)"),
            ("rssi", "RSSI (0x0C)"),
            ("phy", "PHY (0x05)"),
            ("src_mac", "SRC_MAC (0x09)"),
        )
        return tuple(label for attr, label in mapping if getattr(self, attr))


@dataclass(frozen=True, slots=True)
class Envelope:
    """How the container was stored. Decided by extension, as the spec requires."""

    #: ``"csiq"`` or ``"csiq.zst"``.
    form: str
    compressed: bool

    def __str__(self) -> str:
        return self.form


class Capture:
    """One CSIQ container, opened.

    Use :meth:`open` as a context manager. Records are streamed lazily and the
    container is single-pass — iterating twice re-reads the file, which
    :meth:`records` does for you.
    """

    __slots__ = ("_path", "_session", "_envelope", "_streams", "_closed", "_caps")

    def __init__(self, path: str | bytes | os.PathLike[str]) -> None:
        self._path = os.fsdecode(path)
        self._closed = False
        self._streams: list[BinaryIO] = []
        self._caps: Optional[Capabilities] = None
        with self._stream() as fh:
            blob, _ = read_csiq(fh)
        self._session = Session.from_mapping(blob)
        compressed = self._path.lower().endswith(".zst")
        self._envelope = Envelope("csiq.zst" if compressed else "csiq", compressed)

    def _stream(self) -> BinaryIO:
        """A fresh decoded byte stream over the container.

        ``.zst`` is dispatched by extension inside the reader, which is the one
        place that decision is made.
        """
        return _as_binary(self._path)

    # -- construction ---------------------------------------------------------

    @classmethod
    def open(cls, path: str | bytes | os.PathLike[str]) -> "Capture":
        """Open a ``.csiq`` or ``.csiq.zst`` container.

        The envelope is decided by **extension**, which the spec makes the single
        statement of which form a file uses, so a directory listing cannot lie
        about it. A ``.zst`` opened without a decoder raises
        :class:`~csiq.errors.ZstdUnavailable`, never a corruption error — the
        file is fine and the dependency is not.
        """
        return cls(path)

    def __enter__(self) -> "Capture":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        """Close every stream this capture opened. Idempotent."""
        while self._streams:
            stream = self._streams.pop()
            try:
                stream.close()
            except Exception:  # pragma: no cover - a closed stream is the goal
                pass
        self._closed = True

    # -- metadata -------------------------------------------------------------

    @property
    def path(self) -> str:
        return self._path

    @property
    def session(self) -> Session:
        """The embedded session block, typed, with the raw mapping retained."""
        return self._session

    @property
    def envelope(self) -> Envelope:
        """Which storage form this file uses — ``csiq`` or ``csiq.zst``."""
        return self._envelope

    def __repr__(self) -> str:
        sid = self._session.identity.session_id or os.path.basename(self._path)
        return f"<Capture {sid} [{self._envelope}]>"

    # -- records --------------------------------------------------------------

    def records(self) -> Iterator[CsiRecord]:
        """Every record in file order, including the node's own transmissions.

        Re-reads the container on each call, so two iterations are independent.
        """
        if self._closed:
            raise CsiqError("capture is closed")
        stream = self._stream()
        self._streams.append(stream)
        _, records = read_csiq(stream)
        try:
            yield from records
        finally:
            stream.close()
            if stream in self._streams:
                self._streams.remove(stream)

    def __iter__(self) -> Iterator[CsiRecord]:
        return self.records()

    def capabilities(self, *, probe: int = 2000) -> Capabilities:
        """Which optional type codes this capture actually carries.

        Decided by examining the first ``probe`` records, and cached. Bounded
        because the answer is a property of the WRITER rather than of the
        traffic: a build that emits a field emits it wherever it applies.

        Read this before an analysis that depends on an optional field, and
        report what was missing rather than silently producing a narrower result.
        """
        if self._caps is None:
            found = {k: False for k in
                     ("mono_us", "bw_antsel", "vendor_hdr", "node_state", "nic_temp",
                      "rssi", "phy", "src_mac")}
            seen = 0
            for rec in self.records():
                seen += 1
                found["mono_us"] |= rec.mono_us is not None
                found["bw_antsel"] |= rec.bw_antsel is not None
                found["vendor_hdr"] |= rec.vendor_hdr is not None
                found["node_state"] |= bool(rec.node)
                found["nic_temp"] |= "nic_temp_c" in rec.node
                found["rssi"] |= bool(rec.rssi)
                found["phy"] |= rec.phy is not None
                found["src_mac"] |= rec.src_mac != b"\x00" * 6
                if seen >= probe or all(found.values()):
                    break
            self._caps = Capabilities(probed=seen, **found)
        return self._caps

    @property
    def format_version(self) -> int:
        """The container format version. Always 1, and that is not a shortcut.

        Adding a type code is not a version bump, so this number says nothing
        about which fields a file carries — see :meth:`capabilities` for that.
        A reader meeting a version it does not implement refuses the file rather
        than guessing, which is why this is checked at open time and not here.
        """
        return FORMAT_VERSION

    def mono_us_recorded(self, *, probe: int = 2000) -> bool:
        """Whether this capture's writer emits ``MONO_US`` at all.

        Shorthand for ``capabilities().mono_us``. Type code ``0x12`` postdates
        much of the archive; on a file whose writer never emitted it, *every*
        record looks like an own transmission, so the rule that separates
        received frames from injected ones is inapplicable rather than false.
        """
        return self.capabilities(probe=probe).mono_us

    def _require_mono_us(self) -> None:
        if not self.mono_us_recorded():
            version = self._session.environment.csid_version or "unknown"
            raise FieldNotRecorded(
                "MONO_US (0x12)",
                f"This capture was written by csid {version}, which predates the field. "
                "Own transmissions cannot be told from received frames here — use "
                "records() and say in the result that the split was unavailable.",
            )

    def received(self) -> Iterator[CsiRecord]:
        """Records the radio genuinely received.

        Excludes the capturing node's own transmissions, identified by an absent
        ``MONO_US`` — see :func:`is_own_transmission` for the biconditional and
        the counts behind it. This is the population almost every analysis wants,
        and the one nothing in the format's raw API hands you.

        Raises :class:`~csiq.errors.FieldNotRecorded` when the writer never
        emitted ``MONO_US``, rather than yielding nothing.
        """
        self._require_mono_us()
        return (rec for rec in self.records() if not rec.is_own_transmission)

    def own_transmissions(self) -> Iterator[CsiRecord]:
        """The complement of :meth:`received` — the node's own injected frames.

        A caller who wants these should say so, which is why the filter is not a
        boolean flag on one method. Raises the same error on a writer that never
        emitted ``MONO_US``, where every record would qualify.
        """
        self._require_mono_us()
        return (rec for rec in self.records() if rec.is_own_transmission)

    def measured(self) -> Iterator[CsiRecord]:
        """Received records on which **every** reported chain measured.

        A chain reading :data:`~csiq.RSSI_NO_MEASUREMENT` carries a
        byte-identical stale copy of an earlier frame, not a weak signal. A
        record whose other chain is valid stays usable single-chain, so this is
        the strict population — use :meth:`received` plus
        ``rec.chains_measured()`` when you want to keep those.

        Falls back to every record when the writer never emitted ``MONO_US``, so
        this method stays usable on the older half of the archive. Check
        :meth:`mono_us_recorded` when the distinction matters to your result.
        """
        source = self.received() if self.mono_us_recorded() else self.records()
        return (rec for rec in source if rec.fully_measured())

    def clocked(self) -> Iterator[tuple[CsiRecord, Clock]]:
        """Every record paired with the running :class:`Clock`.

        The clock is one stateful object reused across the iteration, so read
        ``clock.ftm_seconds()`` inside the loop rather than collecting the pairs.
        """
        clock = Clock()
        for rec in self.records():
            clock.push(rec)
            yield rec, clock

    # -- export ---------------------------------------------------------------

    def to_arrow(self, *, records: Optional[Iterable[CsiRecord]] = None) -> Any:
        """Per-record scalars as a PyArrow table. Needs the ``arrow`` extra.

        The CSI matrix is deliberately **not** a column. CSIQ is an archival and
        interchange layer, not a columnar analytics format — convert the matrix
        to Parquet or Zarr yourself, with a layout that suits your analysis.
        """
        try:
            import pyarrow as pa
        except ImportError as exc:  # pragma: no cover
            raise CsiqError("to_arrow() needs PyArrow; install the `arrow` extra") from exc
        return pa.table(self._columns(records))

    def to_pandas(self, *, records: Optional[Iterable[CsiRecord]] = None) -> Any:
        """Per-record scalars as a pandas DataFrame. Needs the ``pandas`` extra."""
        try:
            import pandas as pd
        except ImportError as exc:  # pragma: no cover
            raise CsiqError("to_pandas() needs pandas; install the `pandas` extra") from exc
        return pd.DataFrame(self._columns(records))

    def _columns(self, records: Optional[Iterable[CsiRecord]]) -> dict[str, list[Any]]:
        source = self.records() if records is None else records
        cols: dict[str, list[Any]] = {
            k: []
            for k in (
                "ftm",
                "ftm_seconds",
                "us",
                "unix_ts_ns",
                "mono_us",
                "seq",
                "channel",
                "bandwidth_mhz",
                "tone_spacing_khz",
                "ntone",
                "nrx",
                "ntx",
                "modulation",
                "mcs",
                "nss",
                "rssi_a",
                "rssi_b",
                "fully_measured",
                "own_transmission",
            )
        }
        # None rather than True everywhere on a writer that never emitted the
        # field: "every frame was self-transmitted" is a claim, and a false one.
        mono = self.mono_us_recorded()
        clock = Clock()
        for rec in source:
            clock.push(rec)
            rssi = list(rec.rssi) + [None, None]
            cols["ftm"].append(rec.ftm)
            cols["ftm_seconds"].append(clock.ftm_seconds())
            cols["us"].append(rec.us)
            cols["unix_ts_ns"].append(rec.unix_ts_ns or None)
            cols["mono_us"].append(rec.mono_us)
            cols["seq"].append(rec.seq)
            cols["channel"].append(rec.channel)
            cols["bandwidth_mhz"].append(bandwidth_mhz(rec))
            cols["tone_spacing_khz"].append(tone_spacing_khz(rec))
            cols["ntone"].append(rec.ntone)
            cols["nrx"].append(rec.nrx)
            cols["ntx"].append(rec.ntx)
            cols["modulation"].append(rec.phy.modulation if rec.phy else None)
            cols["mcs"].append(rec.phy.mcs if rec.phy else None)
            cols["nss"].append(rec.phy.nss if rec.phy else None)
            cols["rssi_a"].append(rssi[0])
            cols["rssi_b"].append(rssi[1])
            cols["fully_measured"].append(rec.fully_measured())
            cols["own_transmission"].append(rec.is_own_transmission if mono else None)
        return cols


__all__ = [
    "NARROW_SPACING_KHZ",
    "Capabilities",
    "WIDE_SPACING_KHZ",
    "Capture",
    "Clock",
    "Envelope",
    "bandwidth_mhz",
    "is_own_transmission",
    "tone_spacing_khz",
]
