"""csiq — Python reference reader for the CSIQ Interchange Format v1.

This is the *reference implementation* of the reader side of the format spec
(``docs/CSIQ-format-v1.md``). It is deliberately dependency-light: the parser is
pure standard library, so the format can be read anywhere. NumPy is used when
available to hand back complex CSI matrices.

There are two levels, and most callers want the second.

**The parser** — :func:`read_csiq`, :func:`read_raw`, :func:`decode_live`. A
session dict and a lazy record iterator. This is the byte-level floor and it
mirrors ``crates/csiq``::

    from csiq import read_csiq

    session, records = read_csiq("capture.csiq")
    for rec in records:
        print(rec.ftm, rec.ntone, rec.rssi, rec.phy)

**The layer** — :class:`Capture`. The spec documents eight consumer rules whose
violation is *silent*: the result looks healthy and is wrong. Each one is a named
method here, with the measurement behind it in the docstring::

    from csiq import Capture

    with Capture.open("capture.csiq.zst") as cap:
        print(cap.session.radio.channel, cap.envelope)

        for rec in cap.received():         # own transmissions excluded (MONO_US)
            if not rec.fully_measured():   # a -127 chain is stale, not weak
                continue
            H = rec.H                      # chain-major, imaginary-first
            print(rec.bandwidth_mhz, rec.tone_spacing_khz)

The lossless driver-native stream is readable too, for when you want the source
of truth rather than the derived container::

    from csiq import read_raw
    for rec in read_raw("capture.raw", width="80MHz"):
        ...
"""

from .capture import (
    NARROW_SPACING_KHZ,
    WIDE_SPACING_KHZ,
    Capabilities,
    Capture,
    Clock,
    Envelope,
    bandwidth_mhz,
    is_own_transmission,
    tone_spacing_khz,
)
from .errors import (
    BadMagic,
    CsiqError,
    DesyncError,
    FieldNotRecorded,
    MalformedField,
    MissingRequiredField,
    NumpyUnavailable,
    TruncatedCapture,
    UnsupportedVersion,
    ZstdUnavailable,
)
from .reader import (
    FORMAT_VERSION,
    FTM_HZ,
    RSSI_NO_MEASUREMENT,
    BwAntsel,
    CsiRecord,
    FtmUnwrapper,
    PhyLabel,
    decode_bw_antsel,
    decode_live,
    ftm_to_seconds,
    parse_raw_record,
    read_csiq,
    read_raw,
)
from .session import (
    NO_FILTER,
    Build,
    Environment,
    Filter,
    Identity,
    Lifecycle,
    Radio,
    Session,
    Summary,
)


def backend() -> str:
    """Which parser a fresh read would use: ``"python"`` or ``"rust"``.

    ``"rust"`` when the optional accelerator (``csiq[fast]``) is installed and
    ``CSIQ_BACKEND=python`` is not set. Both must produce byte-identical output —
    the spec's own rule is that two implementations disagreeing means two bugs,
    and ``tests/test_backend_parity.py`` enforces it rather than trusting it.

    A ``.csiq.zst`` always takes the pure path: decompression is the stdlib's
    job and the accelerator reads plain containers only.
    """
    from . import _backend

    return _backend.selected()


__all__ = [
    "FORMAT_VERSION",
    "FTM_HZ",
    "NARROW_SPACING_KHZ",
    "NO_FILTER",
    "RSSI_NO_MEASUREMENT",
    "WIDE_SPACING_KHZ",
    "BadMagic",
    "Build",
    "BwAntsel",
    "Capabilities",
    "Capture",
    "Clock",
    "CsiRecord",
    "CsiqError",
    "DesyncError",
    "Environment",
    "Envelope",
    "FieldNotRecorded",
    "Filter",
    "FtmUnwrapper",
    "Identity",
    "Lifecycle",
    "MalformedField",
    "MissingRequiredField",
    "NumpyUnavailable",
    "PhyLabel",
    "Radio",
    "Session",
    "Summary",
    "TruncatedCapture",
    "UnsupportedVersion",
    "ZstdUnavailable",
    "backend",
    "bandwidth_mhz",
    "decode_bw_antsel",
    "decode_live",
    "ftm_to_seconds",
    "is_own_transmission",
    "parse_raw_record",
    "read_csiq",
    "read_raw",
    "tone_spacing_khz",
]

__version__ = "0.2.0"
