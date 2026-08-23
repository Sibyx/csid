#!/usr/bin/env python3
"""Sample the csiscope live WebSocket and dump decoded frames for offline audit.

The console renders what it computes; this reads the same bytes the browser
reads, so a disagreement between a plot and the numbers behind it is visible
without trusting the canvas.

Wire format (crates/csiscope/src/frame.rs):

    [u32 LE header_len][header JSON, space-padded][f32 section LE][u8 section]

The header declares every array as ``name: [element_offset, element_count]``
under ``f32`` (shared) and ``u8`` (per-client waterfall).

Usage (from the csid python/ directory or anywhere -- no repo paths inside):

    python tools/csiscope_probe.py --host monad01 --seconds 15 --out probe-monad01
"""

from __future__ import annotations

import argparse
import asyncio
import json
import struct
import time
from pathlib import Path

import numpy as np
import websockets


def decode(buf: bytes) -> tuple[dict, dict[str, np.ndarray], np.ndarray]:
    """Split one binary frame into (header, f32 arrays, waterfall bytes)."""
    (hlen,) = struct.unpack_from("<I", buf, 0)
    header = json.loads(buf[4 : 4 + hlen].decode("utf-8"))
    f32_start = 4 + hlen
    n_f32 = int(header.get("n_f32", 0))
    f32 = np.frombuffer(buf, dtype="<f4", count=n_f32, offset=f32_start)
    u8_start = f32_start + n_f32 * 4
    u8 = np.frombuffer(buf, dtype=np.uint8, offset=u8_start)

    arrays: dict[str, np.ndarray] = {}
    for name, (off, cnt) in (header.get("f32") or {}).items():
        arrays[name] = f32[off : off + cnt]
    wf_name = next(iter((header.get("u8") or {})), None)
    if wf_name is not None:
        off, cnt = header["u8"][wf_name]
        waterfall = u8[off : off + cnt]
    else:
        waterfall = np.empty(0, dtype=np.uint8)
    return header, arrays, waterfall


def strip_arrays(header: dict) -> dict:
    """Header without the two offset maps -- what the operator actually reads."""
    return {k: v for k, v in header.items() if k not in ("f32", "u8")}


async def sample(host: str, port: int, seconds: int, settings: dict | None) -> list[dict]:
    uri = f"ws://{host}:{port}/ws"
    kept: list[dict] = []
    async with websockets.connect(uri, max_size=64 * 1024 * 1024) as ws:
        if settings:
            await ws.send(json.dumps(settings))
        t0 = time.time()
        next_keep = t0
        seen = 0
        while time.time() - t0 < seconds:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=seconds)
            except asyncio.TimeoutError:
                break
            if isinstance(msg, str):
                kept.append({"kind": "text", "t": time.time() - t0, "body": json.loads(msg)})
                continue
            seen += 1
            now = time.time()
            if now < next_keep:
                continue
            next_keep = now + 1.0
            header, arrays, waterfall = decode(msg)
            kept.append(
                {
                    "kind": "frame",
                    "t": now - t0,
                    "bytes": len(msg),
                    "header": header,
                    "arrays": {k: v.copy() for k, v in arrays.items()},
                    "waterfall": waterfall.copy(),
                }
            )
        kept.append({"kind": "stat", "frames_seen": seen, "elapsed": time.time() - t0})
    return kept


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=8088)
    ap.add_argument("--seconds", type=int, default=15)
    ap.add_argument("--out", default="csiscope-probe")
    ap.add_argument("--settings", default=None, help="JSON ViewSettings patch to send first")
    args = ap.parse_args()

    settings = json.loads(args.settings) if args.settings else None
    kept = asyncio.run(sample(args.host, args.port, args.seconds, settings))

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    meta = []
    npz: dict[str, np.ndarray] = {}
    for i, item in enumerate(kept):
        if item["kind"] != "frame":
            meta.append(item)
            continue
        meta.append(
            {
                "kind": "frame",
                "i": i,
                "t": item["t"],
                "bytes": item["bytes"],
                "header": strip_arrays(item["header"]),
                "array_lens": {k: int(v.size) for k, v in item["arrays"].items()},
            }
        )
        for k, v in item["arrays"].items():
            npz[f"f{i}__{k}"] = v
        npz[f"f{i}__waterfall"] = item["waterfall"]

    (out / "meta.json").write_text(json.dumps(meta, indent=1, default=str))
    np.savez_compressed(out / "arrays.npz", **npz)
    n = sum(1 for m in meta if m.get("kind") == "frame")
    print(f"{args.host}: kept {n} frames -> {out}/meta.json + {out}/arrays.npz")


if __name__ == "__main__":
    main()
