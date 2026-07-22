"""csiq — Python reference reader for the CSIQ Interchange Format v1.

This is the *reference implementation* of the reader side of the format spec
(``docs/CSIQ-format-v1.md``). It is deliberately dependency-light: NumPy is used
when available to hand back complex CSI matrices, but the parser itself is pure
standard library, so the format can be read anywhere.

Typical use::

    from csiq import read_csiq

    session, records = read_csiq("capture.csiq")
    print(session["session_id"], session["radio"]["channel"])
    for rec in records:
        H = rec.matrix()          # complex ndarray [ntone, nrx*ntx] (needs NumPy)
        print(rec.ftm, rec.ntone, rec.rssi, rec.phy)

The raw driver-native stream is also readable, for when you want the lossless
source of truth rather than the derived container::

    from csiq import read_raw
    for rec in read_raw("capture.raw", width="80MHz"):
        ...
"""

from .reader import (  # noqa: F401
    CsiRecord,
    CsiqError,
    FtmUnwrapper,
    PhyLabel,
    decode_live,
    ftm_to_seconds,
    read_csiq,
    read_raw,
)

__all__ = [
    "CsiRecord",
    "CsiqError",
    "FtmUnwrapper",
    "PhyLabel",
    "decode_live",
    "ftm_to_seconds",
    "read_csiq",
    "read_raw",
    "FTM_HZ",
    "FORMAT_VERSION",
]

FTM_HZ = 320_000_000
FORMAT_VERSION = 1
__version__ = "0.1.0"
