"""A minimal CSIQ v1 writer, so the tests need no capture archive.

The reader is the artefact under test, so the fixtures cannot use it to build
their input. These helpers lay the bytes out by hand from the spec's own layout
tables — which is also the point: if the writer here and the reader disagree,
one of them has drifted from ``docs/CSIQ-format-v1.md``.
"""

from __future__ import annotations

import json
import struct
from typing import Any, Iterable, Mapping

MAGIC = b"CSIQ"
RECORD_TAG = 0xA1
FLAG_SESSION = 0x0001

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
T_VENDOR_HDR = 0x14
T_NODE_TEMP_MC = 0x40
T_NODE_NIC_TEMP_C = 0x44


def tlv(type_code: int, value: bytes) -> bytes:
    """One TLV field: ``u8 type``, ``u32 len``, value."""
    return struct.pack("<BI", type_code, len(value)) + value


def record(
    *,
    ftm: int = 1_000,
    nrx: int = 2,
    ntx: int = 1,
    ntone: int = 4,
    iq: Iterable[int] | None = None,
    rssi: Iterable[int] | None = (-60, -64),
    rnf: int | None = None,
    bw_antsel: tuple[int, int] | None = None,
    mono_us: int | None = None,
    phy: tuple[int, int, int] | None = None,
    width_code: int | None = None,
    seq: int | None = None,
    src_mac: bytes | None = None,
    node_temp_mc: int | None = None,
    nic_temp_c: int | None = None,
    vendor_hdr: bytes | None = None,
    unix_ts_ns: int | None = None,
    us: int | None = None,
    channel: int | None = None,
    omit: Iterable[str] = (),
) -> bytes:
    """One framed record. ``omit`` drops a required field, to test refusal."""
    skip = set(omit)
    body = b""
    if "ftm" not in skip:
        body += tlv(T_FTM, struct.pack("<I", ftm))
    if "nrx" not in skip:
        body += tlv(T_NRX, struct.pack("<B", nrx))
    if "ntx" not in skip:
        body += tlv(T_NTX, struct.pack("<B", ntx))
    if "ntone" not in skip:
        body += tlv(T_NTONE, struct.pack("<H", ntone))

    if us is not None:
        body += tlv(T_US, struct.pack("<I", us))
    if unix_ts_ns is not None:
        body += tlv(T_UNIX_TS_NS, struct.pack("<Q", unix_ts_ns))
    if rnf is not None:
        body += tlv(T_RNF, struct.pack("<I", rnf))
    if phy is not None:
        body += tlv(T_PHY, bytes(phy))
    if seq is not None:
        body += tlv(T_SEQ, bytes([seq]))
    if src_mac is not None:
        body += tlv(T_SRC_MAC, src_mac)
    if channel is not None:
        body += tlv(T_CHANNEL, struct.pack("<I", channel))
    if width_code is not None:
        body += tlv(T_WIDTH, struct.pack("<H", width_code))
    if rssi is not None:
        vals = list(rssi)
        body += tlv(T_RSSI, struct.pack(f"<{len(vals)}h", *vals))
    if bw_antsel is not None:
        body += tlv(T_BW_ANTSEL, bytes(bw_antsel))
    if mono_us is not None:
        body += tlv(T_MONO_US, struct.pack("<Q", mono_us))
    if node_temp_mc is not None:
        body += tlv(T_NODE_TEMP_MC, struct.pack("<i", node_temp_mc))
    if nic_temp_c is not None:
        body += tlv(T_NODE_NIC_TEMP_C, struct.pack("<i", nic_temp_c))
    if vendor_hdr is not None:
        body += tlv(T_VENDOR_HDR, vendor_hdr)

    if iq is None:
        iq = list(range(2 * ntone * nrx * ntx))
    coeffs = list(iq)
    body += tlv(T_CSI_MATRIX, struct.pack(f"<{len(coeffs)}h", *coeffs))
    return struct.pack("<BI", RECORD_TAG, len(body)) + body


def container(
    records: Iterable[bytes] = (),
    session: Mapping[str, Any] | None = None,
    *,
    version: int = 1,
    magic: bytes = MAGIC,
) -> bytes:
    """A whole ``.csiq`` byte stream."""
    blob = json.dumps(session).encode("utf-8") if session is not None else b""
    flags = FLAG_SESSION if session is not None else 0
    head = magic + struct.pack("<HHI", version, flags, len(blob)) + blob
    return head + b"".join(records)


def write(tmp_path: Any, data: bytes, name: str = "capture.csiq") -> str:
    path = tmp_path / name
    path.write_bytes(data)
    return str(path)
