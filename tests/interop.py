#!/usr/bin/env python3
"""Cross-language interop test: Rust writes CSIQ, Python reads it back.

The format's central claim is that a capture is interpretable from the file plus
the specification alone. That is only credible if it is tested across
implementations, so this drives the real `csid` binary:

  1. synthesise a driver-native `capture.raw` (framing per docs Appendix A)
  2. run `csid export` to produce `capture.csiq`
  3. read the container with the Python reference reader
  4. assert every field survives, and that raw and CSIQ agree

Run from the repository root:  python3 tests/interop.py
"""

from __future__ import annotations

import json
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "python"))

from csiq import read_csiq, read_raw  # noqa: E402

NTONE = 242
NRX = 2
NTX = 2
FTM = 123_456
US = 555
UNIX_TS_NS = 1_700_000_000_000_000_000
RNF = 0x0442  # modulation type 4 (HE), mcs 2, nss 1
# The firmware reports RSSI as a positive magnitude; readers negate it into dBm.
RSSI_A = 43
RSSI_B = 44
CHANNEL = 36
SRC_MAC = bytes([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01])
SEQ = 7
RECORDS = 3


def synthesise_raw(path: Path) -> None:
    """Write a raw capture in the driver-native framing (docs Appendix A)."""
    hdr = bytearray(272)
    struct.pack_into("<I", hdr, 8, FTM)
    hdr[46] = NRX
    hdr[47] = NTX
    struct.pack_into("<H", hdr, 52, NTONE)
    struct.pack_into("<i", hdr, 60, RSSI_A)
    struct.pack_into("<i", hdr, 64, RSSI_B)
    hdr[68:74] = SRC_MAC
    hdr[76] = SEQ
    struct.pack_into("<I", hdr, 88, US)
    struct.pack_into("<I", hdr, 92, RNF)
    struct.pack_into("<Q", hdr, 208, UNIX_TS_NS)
    struct.pack_into("<I", hdr, 216, CHANNEL)

    coeffs = NTONE * NRX * NTX
    csi = b"".join(struct.pack("<h", (i % 200) - 100) for i in range(2 * coeffs))

    body = struct.pack(">I", len(hdr)) + bytes(hdr) + struct.pack(">I", len(csi)) + csi
    frame = struct.pack(">I", len(body)) + body
    path.write_bytes(frame * RECORDS)


def csid_binary() -> Path:
    for profile in ("release", "debug"):
        candidate = ROOT / "target" / profile / "csid"
        if candidate.is_file():
            return candidate
    raise SystemExit("csid binary not found — run `cargo build --release -p csid` first")


def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        session = Path(tmp) / "session"
        session.mkdir()

        synthesise_raw(session / "capture.raw")
        (session / "metadata.json").write_text(
            json.dumps(
                {
                    "schema": "csid-session/1",
                    "session_id": "interop_test",
                    "radio": {"width": "80MHz", "channel": CHANNEL},
                }
            )
        )

        subprocess.run(
            [str(csid_binary()), "export", str(session)],
            check=True,
            capture_output=True,
        )

        meta, records = read_csiq(session / "capture.csiq")
        recs = list(records)

        # -- the embedded session block survived the round trip
        assert meta is not None, "session block missing from the container"
        assert meta["session_id"] == "interop_test", meta
        assert meta["radio"]["channel"] == CHANNEL, meta

        assert len(recs) == RECORDS, f"expected {RECORDS} records, got {len(recs)}"

        r = recs[0]
        assert r.ftm == FTM, r.ftm
        assert r.us == US, r.us
        assert r.unix_ts_ns == UNIX_TS_NS, r.unix_ts_ns
        assert (r.nrx, r.ntx, r.ntone) == (NRX, NTX, NTONE), (r.nrx, r.ntx, r.ntone)
        assert r.channel == CHANNEL, r.channel
        assert r.width == "80MHz", r.width
        assert r.seq == SEQ, r.seq
        # Written into the synthetic header as the firmware writes it (a
        # positive magnitude); both readers must hand back dBm.
        assert r.rssi == [-RSSI_A, -RSSI_B], r.rssi
        assert r.src_mac == SRC_MAC, r.src_mac
        assert r.phy is not None and r.phy.modulation == "he", r.phy
        assert r.phy.mcs == 2 and r.phy.nss == 1, r.phy
        assert len(r.iq) == 2 * r.coeff_count(), len(r.iq)

        # -- the derived container agrees with the lossless source of truth
        raw_first = next(read_raw(session / "capture.raw", width="80MHz"))
        assert raw_first.ftm == r.ftm
        assert raw_first.ntone == r.ntone
        assert list(raw_first.iq) == list(r.iq), "raw and CSIQ CSI payloads differ"

        # -- optional NumPy view
        try:
            matrix = r.matrix()
            assert matrix.shape == (NTONE, NRX * NTX), matrix.shape
            shape = f", matrix {matrix.shape}"
        except RuntimeError:
            shape = " (NumPy absent; matrix view skipped)"

    print(f"interop OK: {RECORDS} records round-tripped rust -> csiq -> python{shape}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
