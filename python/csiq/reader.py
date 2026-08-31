"""Reference decoder for CSIQ v1 containers, live datagrams, and raw streams.

Mirrors ``crates/csiq`` byte for byte. If the two ever disagree, the spec in
``docs/CSIQ-format-v1.md`` is authoritative and both are bugs.

This module is the parsing floor: pure standard library, no compiled
dependency, readable anywhere. NumPy is optional and only powers
:meth:`CsiRecord.matrix`. For the ergonomic layer that encodes the spec's
consumer rules as named methods, use :class:`csiq.Capture` instead.
"""

from __future__ import annotations

import json
import os
import struct
import sys
from dataclasses import dataclass, field
from typing import Any, BinaryIO, Iterator, Optional, Sequence

from .errors import (
    BadMagic,
    CsiqError,
    DesyncError,
    MalformedField,
    MissingRequiredField,
    NumpyUnavailable,
    TruncatedCapture,
    UnsupportedVersion,
    ZstdUnavailable,
)

try:  # NumPy is optional: without it, `matrix()` is unavailable but parsing works.
    import numpy as _np
except ImportError:  # pragma: no cover
    _np = None  # type: ignore[assignment]

MAGIC = b"CSIQ"
FORMAT_VERSION = 1
RECORD_TAG = 0xA1
LIVE_MAGIC = b"CL"
LIVE_VERSION = 1
FLAG_SESSION = 0x0001
FTM_HZ = 320_000_000

#: RSSI value meaning "this chain reported no measurement".
#: The firmware writes magnitude 0x7F, which negates to -127 dBm — Intel's
#: documented "not available" marker (IWL_NOISE_MEAS_NOT_AVAILABLE). It is not a
#: weak signal (-127 dBm is ~26 dB below a 20 MHz channel's thermal noise floor),
#: and the chain's CSI slice is a byte-identical stale duplicate — discard it.
RSSI_NO_MEASUREMENT = -127

# -- TLV type codes (see docs/CSIQ-format-v1.md) ------------------------------
T_FTM = 0x01
T_US = 0x02
T_UNIX_TS_NS = 0x03
T_RNF = 0x04
T_PHY = 0x05
T_NRX = 0x06
T_NTX = 0x07
T_NTONE = 0x08
T_SRC_MAC = 0x09
T_CHANNEL = 0x0A
T_WIDTH = 0x0B
T_RSSI = 0x0C
T_SEQ = 0x0D
T_CSI_MATRIX = 0x10
T_BW_ANTSEL = 0x11
T_MONO_US = 0x12
# 0x13 is deliberately unallocated: the driver header carries no record counter
# distinct from SEQ (0x0D), which IS one. See the SEQ corrigendum in the spec.
T_VENDOR_HDR = 0x14
T_NODE_TEMP_MC = 0x40
T_NODE_THROTTLE = 0x41
T_NODE_SPOOL_FREE = 0x42
T_NODE_LOAD_M = 0x43
# Whole degrees Celsius, NOT millidegrees like T_NODE_TEMP_MC: the driver's
# `nic_temp` reports the firmware's DTS reading as an integer. The two are
# different sensors in different units — the SoC under the cooler, and the
# AX210 under the HAT.
T_NODE_NIC_TEMP_C = 0x44

_MODULATION = {
    0: "cck",
    1: "legacy_ofdm",
    2: "ht",
    3: "vht",
    4: "he",
    5: "eht",
}

#: ``RATE_MCS_CHAN_WIDTH_*`` code -> nominal channel bandwidth in MHz.
#:
#: The driver's own codes, so no table sits between the firmware and the file.
#: A code absent here is carried verbatim and must NOT be coerced to 20 MHz.
_BANDWIDTH_MHZ = {0: 20, 1: 40, 2: 80, 3: 160, 4: 320}

# ``rate_n_flags`` v2 field positions, from ``rs.h`` in the pinned iax driver
# (``drivers/net/wireless/intel/iwlwifi/fw/api/rs.h``, upstream ref 20d21a7f).
# v1 and v2 disagree about almost every field above bit 7, so a v1 constant
# used here would misparse silently.
_RNF_CHAN_WIDTH_POS = 11  # bits 13-11
_RNF_CHAN_WIDTH_MSK = 0x7
_RNF_ANT_POS = 14  # bit 14 = antenna A, bit 15 = antenna B
_RNF_ANT_MSK = 0x3

_WIDTH = {
    0: "NOHT",
    1: "HT20",
    2: "HT40-",
    3: "HT40+",
    4: "80MHz",
    5: "160MHz",
    6: "320MHz",
}
_WIDTH_CODE = {v: k for k, v in _WIDTH.items()}


@dataclass(frozen=True)
class PhyLabel:
    """Decoded ``rate_n_flags`` v2 label."""

    modulation: str
    mcs: int
    nss: int


@dataclass(frozen=True)
class BwAntsel:
    """Per-frame bandwidth and antenna selection, from ``rate_n_flags`` v2.

    Not to be confused with :attr:`CsiRecord.width`, which is the *configured
    monitor width* — a session constant that bounds what is decodable and
    describes no individual frame. An ambient channel interleaves PHY types
    frame by frame, so on those captures ``width`` is the wrong answer for
    every record and this is the right one.
    """

    #: The driver's ``RATE_MCS_CHAN_WIDTH_*`` code, carried verbatim.
    bandwidth_code: int
    #: Active-antenna bitmask: bit 0 = antenna A, bit 1 = antenna B. It is a
    #: mask, not an index. ``0`` means the word named no antenna, which is the
    #: normal state of a receive record.
    antenna_sel: int

    @property
    def bandwidth_mhz(self) -> Optional[int]:
        """Nominal channel bandwidth in MHz, or ``None`` for an unknown code.

        This is the *channel*, not the occupied tone span. A 20 MHz HE frame
        carries 242 tones at 78.125 kHz, occupying about 18.9 MHz.
        """
        return _BANDWIDTH_MHZ.get(self.bandwidth_code)

    @property
    def ant_a(self) -> bool:
        return bool(self.antenna_sel & 0b01)

    @property
    def ant_b(self) -> bool:
        return bool(self.antenna_sel & 0b10)


def decode_bw_antsel(rnf: int) -> Optional[BwAntsel]:
    """Decode bandwidth and antenna selection out of a ``rate_n_flags`` word.

    ``None`` for ``rnf == 0``, which is how the header reports "no rate
    information".

    This is the whole reason the archive needs no re-capture. ``RNF`` (``0x04``)
    is written verbatim into **every** record CSIQ has ever carried, and the
    ``BW_ANTSEL`` TLV is exactly this function applied at write time. Decoding
    an old file here recovers the same bits, not an approximation of them.
    """
    if rnf == 0:
        return None
    return BwAntsel(
        bandwidth_code=(rnf >> _RNF_CHAN_WIDTH_POS) & _RNF_CHAN_WIDTH_MSK,
        antenna_sel=(rnf >> _RNF_ANT_POS) & _RNF_ANT_MSK,
    )


@dataclass
class CsiRecord:
    """One CSI record: three clocks, PHY label, per-chain RSSI, and the matrix.

    Timing rule: analyse on :attr:`ftm` (320 MHz, 3.125 ns, wraps ~13.42 s —
    use :class:`FtmUnwrapper`); anchor wallclock on :attr:`unix_ts_ns`.
    """

    ftm: int = 0
    us: int = 0
    unix_ts_ns: int = 0
    rnf: int = 0
    phy: Optional[PhyLabel] = None
    #: Per-frame bandwidth and antenna selection.
    #:
    #: Read from ``BW_ANTSEL`` (``0x11``) when the file carries it, otherwise
    #: decoded from ``RNF`` (``0x04``), which every file carries. The two are
    #: the same bits — see :func:`decode_bw_antsel` — so the fallback recovers
    #: the field on the whole archive rather than inventing it. ``None`` only
    #: when the record had no rate information at all.
    bw_antsel: Optional[BwAntsel] = None
    #: ``CLOCK_MONOTONIC`` microseconds — the clock an NTP step cannot distort.
    #:
    #: ``None`` means **this record is the node's own transmission looped back**,
    #: not that the clock was unavailable. Measured as an exact biconditional
    #: over 2,433 records: the field is zero if and only if the source MAC is the
    #: node's own injector. A locally generated frame never traverses the receive
    #: path that stamps this clock.
    mono_us: Optional[int] = None
    #: The 272-byte driver header verbatim, when the writer kept it (TLV 0x14).
    #:
    #: Lossless provenance: a field this build cannot name is still here at the
    #: offset Appendix A gives it, so the next recovery costs a decoder rather
    #: than a re-capture.
    vendor_hdr: Optional[bytes] = None
    #: Node and host state, when the sampler attached a tick to this record.
    #:
    #: A sparse series, never a per-record column. In a FILE this is normally
    #: empty — csid records the series in the session block instead, because the
    #: container is derived at teardown and a per-record stamp there would be
    #: fabricated. These fields carry state on the LIVE datagram path, where the
    #: record is produced in the moment.
    node: dict[str, int] = field(default_factory=dict)
    seq: int = 0
    nrx: int = 0
    ntx: int = 0
    ntone: int = 0
    #: Per-chain RSSI in dBm (negative). Zero means the chain reported nothing.
    rssi: Sequence[int] = field(default_factory=list)
    src_mac: bytes = b"\x00" * 6
    channel: int = 0
    width: str = "NOHT"
    iq: Sequence[int] = field(default_factory=list)

    @property
    def mac(self) -> str:
        """Source MAC as ``aa:bb:cc:dd:ee:ff``."""
        return ":".join(f"{b:02x}" for b in self.src_mac)

    def chains_measured(self) -> list[bool]:
        """Which RX chains actually measured, by RSSI.

        A ``False`` entry means the chain reported
        :data:`RSSI_NO_MEASUREMENT` and its CSI slice is a byte-identical copy
        of the previous record's — exclude it rather than treating it as a very
        weak signal. Verified as an exact biconditional over 44,577 records.
        """
        return [r != RSSI_NO_MEASUREMENT for r in self.rssi]

    def fully_measured(self) -> bool:
        """True when every reported chain carries a real measurement."""
        return bool(self.rssi) and all(r != RSSI_NO_MEASUREMENT for r in self.rssi)

    @property
    def rssi_dbm(self) -> tuple[int, ...]:
        """Per-chain RSSI in dBm, negative. ``-127`` marks "no measurement".

        The driver header carries the magnitude as a positive ``u8``; both
        reference readers negate at parse time, so a record always carries
        ordinary negative dBm and no consumer has to know that convention.
        Observed valid range on the reference hardware: -18 to -89 dBm.
        """
        return tuple(self.rssi)

    @property
    def is_own_transmission(self) -> bool:
        """True when this record is the capturing node's own frame, looped back.

        A locally generated frame never traverses the receive path that stamps
        ``CLOCK_MONOTONIC``, so an absent ``MONO_US`` is a semantic marker, not a
        missing value. Verified as an exact biconditional over 2,433 records:
        1,743 own-injector records carried none, all 690 ambient records did.

        Do not read a missing value as zero, and do not read it as a broken clock.
        """
        return self.mono_us is None

    @property
    def bandwidth_mhz(self) -> Optional[int]:
        """The bandwidth of THIS frame, in MHz, or ``None`` when unrecorded.

        Prefers ``BW_ANTSEL`` (0x11); falls back to decoding the same bits out of
        ``RNF`` (0x04), which the spec requires and which is what gives every
        pre-0.2.0 file per-frame bandwidth with no re-capture.

        **An absent value is not 20 MHz.** And this is not ``session.radio.width``
        — that is the configured monitor width, which bounds what the receiver
        could decode and describes no individual record.
        """
        if self.bw_antsel is not None:
            return self.bw_antsel.bandwidth_mhz
        if self.rnf:
            decoded = decode_bw_antsel(self.rnf)
            if decoded is not None:
                return decoded.bandwidth_mhz
        return None

    @property
    def tone_spacing_khz(self) -> Optional[float]:
        """Subcarrier spacing in kHz, derived from tone count and bandwidth.

        ``312.5`` when ``ntone * 312.5 kHz`` fits inside the channel, else
        ``78.125``. This is what separates 242 tones in 20 MHz (HE20 — 75.6 MHz
        would not fit) from 242 tones in 80 MHz (VHT80). ``None`` when the
        bandwidth is unknown, because a guess rescales every frequency axis
        downstream without saying so.
        """
        bw = self.bandwidth_mhz
        if bw is None or self.ntone <= 0:
            return None
        return 312.5 if self.ntone * 312.5 <= bw * 1000 else 78.125

    def chain(self, index: int) -> Any:
        """One chain's contiguous complex response, as ``[ntone]``.

        The payload is ``nrx*ntx`` contiguous blocks of ``ntone`` coefficients —
        chain-major, not tone-interleaved — so a chain is a slice rather than a
        stride. Reading it the other way yields a smeared impulse response:
        chain-major is more compact in 99.4% of 5,186 records.
        """
        if _np is None:  # pragma: no cover
            raise NumpyUnavailable("NumPy is required for chain(); parsing works without it")
        chains = self.nrx * self.ntx
        if not 0 <= index < chains:
            raise MalformedField(f"chain {index} out of range for {chains} chains")
        base = 2 * index * self.ntone
        flat = _np.asarray(self.iq[base : base + 2 * self.ntone], dtype=_np.int16).astype(_np.float32)
        if flat.size != 2 * self.ntone:
            raise MalformedField(f"chain {index} is short: {flat.size} i16 for {self.ntone} tones")
        # Imaginary first, then real. The other order yields i*conj(H), which
        # leaves |H| untouched and mirrors every phase.
        return flat[1::2] + 1j * flat[0::2]

    @property
    def H(self) -> Any:
        """The CSI matrix, ``[ntone, nrx*ntx]`` complex. Alias for :meth:`matrix`."""
        return self.matrix()

    def coeff_count(self) -> int:
        """Number of complex coefficients (``ntone * nrx * ntx``)."""
        return self.ntone * self.nrx * self.ntx

    def matrix(self) -> Any:
        """CSI as a complex array shaped ``[ntone, nrx * ntx]``.

        Amplitude is AGC-normalised — it carries channel *shape* only; take the
        absolute scale from :attr:`rssi`.
        """
        if _np is None:  # pragma: no cover
            raise NumpyUnavailable("NumPy is required for matrix(); parsing works without it")
        n = self.coeff_count()
        if n == 0 or len(self.iq) != 2 * n:
            raise MalformedField(f"CSI payload does not match dimensions: {len(self.iq)} i16 for {n} coefficients")
        flat = _np.asarray(self.iq, dtype=_np.int16).astype(_np.float32)
        # Two properties, and getting either wrong silently corrupts every
        # phase-derived result while leaving |H| looking perfectly healthy.
        #
        # 1. IMAGINARY FIRST. value = iq[i+1] + 1j*iq[i]. The other order yields
        #    i*conj(H): amplitude is untouched and every phase is mirrored. The
        #    tell is causality — the correct order concentrates 21.5x more
        #    impulse-response energy at early delays than late, while the swap
        #    inverts that ratio to 0.48, an anti-causal channel.
        # 2. CHAIN-MAJOR. The payload is nrx*ntx contiguous blocks of ntone, not
        #    tone-interleaved. Reading chain-major yields a more compact impulse
        #    response in 99.4% of 5,186 records.
        #
        # The returned VIEW is tone-major [ntone, nrx*ntx], so consumers keep a
        # stable shape without knowing the on-disk order. Use chain() for one
        # chain's contiguous response.
        cplx = flat[1::2] + 1j * flat[0::2]
        chains = self.nrx * self.ntx
        return _np.ascontiguousarray(cplx.reshape(chains, self.ntone).T)


class FtmUnwrapper:
    """Unwrap the 32-bit 320 MHz ``ftm`` clock into monotonic ticks."""

    def __init__(self) -> None:
        self._last: Optional[int] = None
        self._wraps = 0

    def push(self, raw: int) -> int:
        if self._last is not None and raw < self._last:
            self._wraps += 1
        self._last = raw
        return self._wraps * (1 << 32) + raw


#: True when a raw little-endian ``i16`` buffer can be viewed without a copy.
_NATIVE_LE = sys.byteorder == "little"


def _as_i16(raw: bytes) -> Sequence[int]:
    """View little-endian ``i16`` bytes as a sequence, without copying if we can.

    Building a Python list here is the single most expensive thing a reader does
    — measured at roughly three quarters of the cost of decoding a record — and
    most consumers only ever hand the result to NumPy, which reads the buffer
    directly. ``len()``, indexing, slicing and iteration all behave as before.

    A ``memoryview`` cast is native-endian, so on a big-endian host it would
    silently byte-swap every coefficient. There the bytes are unpacked properly
    instead: correctness first, and no such host is in the fleet.
    """
    if _NATIVE_LE:
        return memoryview(raw).cast("h")
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


def ftm_to_seconds(ticks: int) -> float:
    """Convert *unwrapped* ftm ticks to seconds."""
    return ticks / FTM_HZ


# -- TLV decoding -------------------------------------------------------------


def _decode_payload(payload: bytes) -> CsiRecord:
    rec = CsiRecord()
    pos, end = 0, len(payload)
    seen_required = set()

    while pos < end:
        if end - pos < 5:
            raise TruncatedCapture("truncated TLV header")
        ty = payload[pos]
        (ln,) = struct.unpack_from("<I", payload, pos + 1)
        pos += 5
        if pos + ln > end:
            raise MalformedField("TLV value past end of payload")
        val = payload[pos : pos + ln]
        pos += ln

        if ty == T_FTM:
            (rec.ftm,) = struct.unpack("<I", val)
            seen_required.add("ftm")
        elif ty == T_US:
            (rec.us,) = struct.unpack("<I", val)
        elif ty == T_UNIX_TS_NS:
            (rec.unix_ts_ns,) = struct.unpack("<Q", val)
        elif ty == T_RNF:
            (rec.rnf,) = struct.unpack("<I", val)
        elif ty == T_PHY:
            mod, mcs, nss = val[0], val[1], val[2]
            rec.phy = PhyLabel(_MODULATION.get(mod, f"unknown({mod})"), mcs, nss)
        elif ty == T_SEQ:
            rec.seq = val[0]
        elif ty == T_NRX:
            rec.nrx = val[0]
            seen_required.add("nrx")
        elif ty == T_NTX:
            rec.ntx = val[0]
            seen_required.add("ntx")
        elif ty == T_NTONE:
            (rec.ntone,) = struct.unpack("<H", val)
            seen_required.add("ntone")
        elif ty == T_SRC_MAC:
            rec.src_mac = bytes(val)
        elif ty == T_CHANNEL:
            (rec.channel,) = struct.unpack("<I", val)
        elif ty == T_WIDTH:
            (code,) = struct.unpack("<H", val)
            rec.width = _WIDTH.get(code, f"unknown({code})")
        elif ty == T_RSSI:
            rec.rssi = list(struct.unpack(f"<{len(val) // 2}h", val))
        elif ty == T_MONO_US:
            (rec.mono_us,) = struct.unpack("<Q", val)
        elif ty == T_VENDOR_HDR:
            rec.vendor_hdr = bytes(val)
        elif ty == T_NODE_TEMP_MC:
            (rec.node["temp_mc"],) = struct.unpack("<i", val)
        elif ty == T_NODE_THROTTLE:
            (rec.node["throttle_flags"],) = struct.unpack("<I", val)
        elif ty == T_NODE_SPOOL_FREE:
            (rec.node["spool_free_bytes"],) = struct.unpack("<Q", val)
        elif ty == T_NODE_LOAD_M:
            (rec.node["load_m"],) = struct.unpack("<I", val)
        elif ty == T_NODE_NIC_TEMP_C:
            (rec.node["nic_temp_c"],) = struct.unpack("<i", val)
        elif ty == T_BW_ANTSEL:
            if len(val) < 2:
                raise MalformedField("bw_antsel value shorter than two bytes")
            rec.bw_antsel = BwAntsel(bandwidth_code=val[0], antenna_sel=val[1])
        elif ty == T_CSI_MATRIX:
            rec.iq = _as_i16(val)
        # Unknown types are skipped: forward compatibility is part of the spec.

    missing = {"ftm", "nrx", "ntx", "ntone"} - seen_required
    if missing:
        raise MissingRequiredField(sorted(missing))

    # No 0x11 in this file — every file written before csid 0.2.0. The same
    # bits are in `rnf`, which the record already carries, so recover them
    # rather than reporting a field the capture genuinely holds as absent.
    if rec.bw_antsel is None:
        rec.bw_antsel = decode_bw_antsel(rec.rnf)
    return rec


# -- container ----------------------------------------------------------------


def _read_exactly(fh: BinaryIO, n: int) -> bytes:
    try:
        buf = fh.read(n)
    except CsiqError:
        raise
    except Exception as exc:
        # A decompressor's own exception type must not escape: a caller writing
        # one `except CsiqError` would miss a corrupt `.zst` entirely, and the
        # traceback would name a library they did not import.
        raise MalformedField(f"could not read {n} bytes from the container: {exc}") from exc
    if len(buf) != n:
        raise TruncatedCapture(f"unexpected end of file (wanted {n} bytes, got {len(buf)})")
    return buf


def _as_binary(path_or_file: str | bytes | os.PathLike[str] | BinaryIO) -> BinaryIO:
    """Accept a path (str, bytes, or os.PathLike) or an already-open binary file.

    A path ending in ``.zst`` is decompressed transparently. **The file is
    compressed; the format is not** — the bytes inside a ``capture.csiq.zst``
    are an ordinary CSIQ v1 stream, and ``FORMAT_VERSION`` is still 1. Only the
    envelope changed, and the extension is the single statement of which
    envelope a given file uses.

    The archive holds both: everything captured before IP-139 Phase 6 is a plain
    ``capture.csiq``, everything after is compressed. Callers should not have to
    know which, so the decision is made here and nowhere else.
    """
    if isinstance(path_or_file, (str, bytes, os.PathLike)):
        # `os.fsdecode` rather than `fspath`, so a bytes path and a str path
        # take the same branch instead of needing two suffix literals.
        if os.fsdecode(path_or_file).lower().endswith(".zst"):
            return _open_zst(path_or_file)
        return open(path_or_file, "rb")
    return path_or_file


def _open_zst(path: str | bytes | os.PathLike[str]) -> BinaryIO:
    """Open a zstd-compressed container as a binary stream.

    Prefers the stdlib's ``compression.zstd`` (Python 3.14+) and falls back to
    the ``zstandard`` package. Raises a CSIQ error rather than an ImportError so
    a caller sees a format problem in format terms, with the fix named.
    """
    try:  # Python 3.14+
        from compression import zstd as _zstd  # type: ignore[import-not-found]

        return _zstd.ZstdFile(path, "rb")  # type: ignore[no-any-return]
    except ImportError:
        pass
    try:
        import zstandard
    except ImportError as exc:  # pragma: no cover - environment dependent
        raise ZstdUnavailable(
            f"{os.fspath(path)!r} is zstd-compressed and no zstd decoder is available. "
            "Install the `zstd` extra (or `zstandard`), or read the uncompressed sibling "
            "if one exists. The file itself is fine."
        ) from exc
    stream: BinaryIO = zstandard.open(path, "rb")
    return stream


def _from_fast(rec: Any) -> CsiRecord:
    """Rebuild a :class:`CsiRecord` from the accelerator's record.

    The scalars cross as themselves; only two need care. ``phy`` arrives as a
    tuple because :class:`PhyLabel` is a Python type the extension does not own,
    and ``iq`` is taken as ``iq_bytes`` and unpacked here — a list of Python ints
    is the expensive half of a record and the extension should not build one it
    might not be asked for.
    """
    phy = PhyLabel(*rec.phy) if rec.phy is not None else None
    bw = None
    if rec.bw_antsel is not None:
        mhz, antenna_sel = rec.bw_antsel
        code = next((c for c, m in _BANDWIDTH_MHZ.items() if m == mhz), 0xFF)
        bw = BwAntsel(bandwidth_code=code, antenna_sel=antenna_sel)
    raw = rec.iq_bytes
    return CsiRecord(
        ftm=rec.ftm,
        us=rec.us,
        unix_ts_ns=rec.unix_ts_ns,
        rnf=rec.rnf,
        phy=phy,
        seq=rec.seq,
        nrx=rec.nrx,
        ntx=rec.ntx,
        ntone=rec.ntone,
        rssi=list(rec.rssi),
        src_mac=rec.src_mac,
        channel=rec.channel,
        width=rec.width,
        iq=_as_i16(raw),
        bw_antsel=bw,
        mono_us=rec.mono_us,
        vendor_hdr=rec.vendor_hdr,
        node=dict(rec.node),
    )


def _read_csiq_fast(path: str) -> tuple[dict[str, Any] | None, Iterator[CsiRecord]]:
    """Read through the accelerator, mapping its errors onto the typed hierarchy.

    Both backends must raise the same class for the same bad bytes, or a caller
    cannot write one ``except``. The Rust side reports one error string, so the
    mapping happens here, in one place.
    """
    from . import _backend

    fast = _backend.fast_module()
    try:
        blob, records = fast.read_csiq(path)
    except ValueError as exc:
        raise _classify(str(exc)) from exc

    session = json.loads(blob) if blob else None

    def _iter() -> Iterator[CsiRecord]:
        while True:
            try:
                rec = next(records)
            except StopIteration:
                return
            except ValueError as exc:
                raise _classify(str(exc)) from exc
            yield _from_fast(rec)

    return session, _iter()


def _classify(message: str) -> CsiqError:
    """Map the Rust reader's message onto the typed error hierarchy."""
    lowered = message.lower()
    if "bad magic" in lowered:
        return BadMagic(message)
    if "unsupported csiq version" in lowered:
        return UnsupportedVersion(-1, FORMAT_VERSION)
    if "tag" in lowered and "record" in lowered:
        return DesyncError(message)
    if "truncated" in lowered or "unexpected end" in lowered:
        return TruncatedCapture(message)
    if "missing" in lowered and "field" in lowered:
        return MissingRequiredField([message])
    return MalformedField(message)


def read_csiq(
    path_or_file: str | bytes | os.PathLike[str] | BinaryIO,
) -> tuple[dict[str, Any] | None, Iterator[CsiRecord]]:
    """Open a ``.csiq`` container.

    Returns ``(session, records)`` where *session* is the embedded metadata dict
    (or ``None``) and *records* is a lazy iterator of :class:`CsiRecord`.
    """
    # The accelerator takes a path, not a stream, and only handles the plain
    # envelope — zstd is the stdlib's job on the Python side. Anything else
    # falls through to the pure parser, which is the only path that must work.
    if isinstance(path_or_file, (str, bytes, os.PathLike)):
        from . import _backend

        name = os.fsdecode(path_or_file)
        if _backend.selected() == "rust" and not name.lower().endswith(".zst"):
            return _read_csiq_fast(name)

    fh = _as_binary(path_or_file)

    magic = _read_exactly(fh, 4)
    if magic != MAGIC:
        raise BadMagic(f"bad magic {magic!r}: not a CSIQ file")
    version, flags, session_len = struct.unpack("<HHI", _read_exactly(fh, 8))
    if version != FORMAT_VERSION:
        raise UnsupportedVersion(version, FORMAT_VERSION)

    session = None
    if session_len:
        blob = _read_exactly(fh, session_len)
        if flags & FLAG_SESSION:
            session = json.loads(blob.decode("utf-8"))

    def _records() -> Iterator[CsiRecord]:
        while True:
            tag = fh.read(1)
            if not tag:
                return
            if tag[0] != RECORD_TAG:
                raise DesyncError(f"bad record tag 0x{tag[0]:02x} (stream desync)")
            (ln,) = struct.unpack("<I", _read_exactly(fh, 4))
            yield _decode_payload(_read_exactly(fh, ln))

    return session, _records()


def decode_live(datagram: bytes) -> tuple[int, int, CsiRecord]:
    """Decode one live-stream datagram → ``(session_uid, seq, CsiRecord)``."""
    if len(datagram) < 15:
        raise TruncatedCapture("live datagram too short")
    if datagram[0:2] != LIVE_MAGIC:
        raise BadMagic(f"bad live magic {datagram[0:2]!r}")
    if datagram[2] != LIVE_VERSION:
        raise UnsupportedVersion(datagram[2], LIVE_VERSION)
    session_uid, seq = struct.unpack_from("<QI", datagram, 3)
    return session_uid, seq, _decode_payload(datagram[15:])


# -- raw driver stream --------------------------------------------------------

# Little-endian offsets inside the 272-byte iax header. Kept in lockstep with
# `crates/csiq/src/raw.rs`.
_OFF_FTM = 8
_OFF_NRX = 46
_OFF_NTX = 47
_OFF_NTONE = 52
_OFF_RSSI_A = 60
_OFF_RSSI_B = 64
_OFF_SRC_MAC = 68
_OFF_SEQ = 76
_OFF_US = 88
_OFF_RNF = 92
_OFF_UNIX_TS_NS = 208
_OFF_CHANNEL = 216


def _decode_rnf(rnf: int) -> Optional[PhyLabel]:
    if rnf == 0:
        return None
    return PhyLabel(
        _MODULATION.get((rnf >> 8) & 0x07, f"unknown({(rnf >> 8) & 0x07})"),
        rnf & 0x0F,
        ((rnf >> 4) & 0x03) + 1,
    )


def parse_raw_record(hdr: bytes, csi: bytes, width: str = "NOHT") -> CsiRecord:
    """Parse one raw header + CSI payload into a :class:`CsiRecord`."""
    if len(hdr) < _OFF_RNF + 4:
        raise TruncatedCapture("raw header shorter than base fields")
    nrx = hdr[_OFF_NRX]
    ntx = hdr[_OFF_NTX]
    # The firmware reports RSSI as a positive magnitude (the convention Intel
    # uses for the __le32 RSSI fields in fw/api/stats.h); negate it so records
    # carry ordinary dBm. Zero is preserved as zero — it means "this chain
    # reported nothing", not 0 dBm. Kept in lockstep with `raw::rssi_dbm`.
    (rssi_a,) = struct.unpack_from("<i", hdr, _OFF_RSSI_A)
    (rssi_b,) = struct.unpack_from("<i", hdr, _OFF_RSSI_B)
    rssi_a, rssi_b = -rssi_a, -rssi_b
    (rnf,) = struct.unpack_from("<I", hdr, _OFF_RNF)

    unix_ts_ns, channel = 0, 0
    if len(hdr) >= _OFF_CHANNEL + 1:
        (unix_ts_ns,) = struct.unpack_from("<Q", hdr, _OFF_UNIX_TS_NS)
        channel = hdr[_OFF_CHANNEL]  # u8; Appendix A reserves 217-219

    return CsiRecord(
        ftm=struct.unpack_from("<I", hdr, _OFF_FTM)[0],
        us=struct.unpack_from("<I", hdr, _OFF_US)[0],
        unix_ts_ns=unix_ts_ns,
        rnf=rnf,
        phy=_decode_rnf(rnf),
        seq=hdr[_OFF_SEQ],
        nrx=nrx,
        ntx=ntx,
        ntone=struct.unpack_from("<H", hdr, _OFF_NTONE)[0],
        rssi=[rssi_a, rssi_b][: min(nrx, 2)],
        src_mac=bytes(hdr[_OFF_SRC_MAC : _OFF_SRC_MAC + 6]),
        channel=channel,
        width=width,
        iq=list(struct.unpack(f"<{len(csi) // 2}h", csi[: len(csi) // 2 * 2])),
    )


def read_raw(path_or_file: str | bytes | os.PathLike[str] | BinaryIO, width: str = "NOHT") -> Iterator[CsiRecord]:
    """Iterate the lossless driver-native stream (``capture.raw``).

    ``width`` is a session property absent from the raw header; pass the monitor
    width the capture used (it is recorded in ``metadata.json``).
    """
    fh = _as_binary(path_or_file)
    while True:
        prefix = fh.read(4)
        if not prefix or len(prefix) < 4:
            return
        (msg_len,) = struct.unpack(">I", prefix)
        body = _read_exactly(fh, msg_len)
        (hdr_len,) = struct.unpack_from(">I", body, 0)
        hdr = body[4 : 4 + hdr_len]
        (csi_len,) = struct.unpack_from(">I", body, 4 + hdr_len)
        csi = body[8 + hdr_len : 8 + hdr_len + csi_len]
        yield parse_raw_record(hdr, csi, width)
