"""Reference decoder for CSIQ v1 containers, live datagrams, and raw streams.

Mirrors ``crates/csiq`` byte for byte. If the two ever disagree, the spec in
``docs/CSIQ-format-v1.md`` is authoritative and both are bugs.
"""

from __future__ import annotations

import json
import struct
from dataclasses import dataclass, field
from typing import BinaryIO, Iterator, Optional, Sequence

try:  # NumPy is optional: without it, `matrix()` is unavailable but parsing works.
    import numpy as _np
except ImportError:  # pragma: no cover
    _np = None

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
#: and the chain's CSI slice is a stale duplicate — discard it.
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

_MODULATION = {
    0: "cck",
    1: "legacy_ofdm",
    2: "ht",
    3: "vht",
    4: "he",
    5: "eht",
}

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


class CsiqError(Exception):
    """Raised on malformed or unsupported CSIQ data."""


@dataclass(frozen=True)
class PhyLabel:
    """Decoded ``rate_n_flags`` v2 label."""

    modulation: str
    mcs: int
    nss: int


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

    def chains_measured(self) -> list:
        """Which RX chains actually measured, by RSSI.

        A ``False`` entry means the chain reported
        :data:`RSSI_NO_MEASUREMENT` and its CSI is a stale duplicate — exclude
        it rather than treating it as a very weak signal.
        """
        return [r != RSSI_NO_MEASUREMENT for r in self.rssi]

    def fully_measured(self) -> bool:
        """True when every reported chain carries a real measurement."""
        return bool(self.rssi) and all(r != RSSI_NO_MEASUREMENT for r in self.rssi)

    def coeff_count(self) -> int:
        """Number of complex coefficients (``ntone * nrx * ntx``)."""
        return self.ntone * self.nrx * self.ntx

    def matrix(self):
        """CSI as a complex array shaped ``[ntone, nrx * ntx]``.

        Amplitude is AGC-normalised — it carries channel *shape* only; take the
        absolute scale from :attr:`rssi`.
        """
        if _np is None:  # pragma: no cover
            raise RuntimeError("NumPy is required for matrix(); parsing works without it")
        n = self.coeff_count()
        if n == 0 or len(self.iq) != 2 * n:
            raise CsiqError(
                f"CSI payload does not match dimensions: {len(self.iq)} i16 for {n} coefficients"
            )
        flat = _np.asarray(self.iq, dtype=_np.int16).astype(_np.float32)
        # Stored imaginary-first, and chain-major (nrx*ntx blocks of ntone).
        # Both differ from the obvious reading — see the format spec.
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
            raise CsiqError("truncated TLV header")
        ty = payload[pos]
        (ln,) = struct.unpack_from("<I", payload, pos + 1)
        pos += 5
        if pos + ln > end:
            raise CsiqError("TLV value past end of payload")
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
        elif ty == T_CSI_MATRIX:
            rec.iq = list(struct.unpack(f"<{len(val) // 2}h", val))
        # Unknown types are skipped: forward compatibility is part of the spec.

    missing = {"ftm", "nrx", "ntx", "ntone"} - seen_required
    if missing:
        raise CsiqError(f"record missing required field(s): {sorted(missing)}")
    return rec


# -- container ----------------------------------------------------------------


def _read_exactly(fh: BinaryIO, n: int) -> bytes:
    buf = fh.read(n)
    if len(buf) != n:
        raise CsiqError(f"unexpected end of file (wanted {n} bytes, got {len(buf)})")
    return buf


def _as_binary(path_or_file) -> BinaryIO:
    """Accept a path (str, bytes, or os.PathLike) or an already-open binary file."""
    if hasattr(path_or_file, "read"):
        return path_or_file
    return open(path_or_file, "rb")


def read_csiq(path_or_file):
    """Open a ``.csiq`` container.

    Returns ``(session, records)`` where *session* is the embedded metadata dict
    (or ``None``) and *records* is a lazy iterator of :class:`CsiRecord`.
    """
    fh = _as_binary(path_or_file)

    magic = _read_exactly(fh, 4)
    if magic != MAGIC:
        raise CsiqError(f"bad magic {magic!r}: not a CSIQ file")
    version, flags, session_len = struct.unpack("<HHI", _read_exactly(fh, 8))
    if version != FORMAT_VERSION:
        raise CsiqError(f"unsupported CSIQ version {version} (this reader handles v{FORMAT_VERSION})")

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
                raise CsiqError(f"bad record tag 0x{tag[0]:02x} (stream desync)")
            (ln,) = struct.unpack("<I", _read_exactly(fh, 4))
            yield _decode_payload(_read_exactly(fh, ln))

    return session, _records()


def decode_live(datagram: bytes):
    """Decode one live-stream datagram → ``(session_uid, seq, CsiRecord)``."""
    if len(datagram) < 15:
        raise CsiqError("live datagram too short")
    if datagram[0:2] != LIVE_MAGIC:
        raise CsiqError(f"bad live magic {datagram[0:2]!r}")
    if datagram[2] != LIVE_VERSION:
        raise CsiqError(f"unsupported live version {datagram[2]}")
    session_uid, seq = struct.unpack_from("<QI", datagram, 3)
    return session_uid, seq, _decode_payload(datagram[15:])


# -- raw driver stream --------------------------------------------------------

# Little-endian offsets inside the 272-byte iax header. Kept in lockstep with
# `crates/csiq/src/raw.rs`.
_OFF_FTM = 8
_OFF_NRX = 46
_OFF_NTX = 47
_OFF_NTONE = 52
_OFF_RSSI_A = 60  # u8 magnitude; 61..63 reserved
_OFF_RSSI_B = 64  # u8 magnitude; 65..67 reserved
_OFF_SRC_MAC = 68
_OFF_SEQ = 76
_OFF_US = 88
_OFF_RNF = 92
_OFF_UNIX_TS_NS = 208
_OFF_CHANNEL = 216  # u8 (driver writes one byte)


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
        raise CsiqError("raw header shorter than base fields")
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
        channel = hdr[_OFF_CHANNEL]  # driver writes a single byte here

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


def read_raw(path_or_file, width: str = "NOHT") -> Iterator[CsiRecord]:
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
