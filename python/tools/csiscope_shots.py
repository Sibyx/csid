#!/usr/bin/env python3
"""Screenshot the live csiscope UI, whole page and per panel, over time.

The probe reads the numbers; this reads the pixels. A plot can be wrong in a
way the header is right about -- a wrong axis, a clipped envelope, a panel that
never redraws -- and only an image shows that.

Usage (run from the csid python/ directory):

    python tools/csiscope_shots.py --host monad01 --out shots/monad01 --shots 3 --gap 5
"""

from __future__ import annotations

import argparse
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

PANELS = [
    "p-capture", "p-metronome", "p-waterfall", "p-spectrum", "p-doppler",
    "p-phase", "p-cir", "p-constellation", "p-chains", "p-tones", "p-rssi",
    "p-jitter", "p-clocks", "p-bandplan", "p-ratio", "p-tonestats",
    "p-talkers", "p-classes", "p-mix", "p-validation", "p-stream",
]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=8088)
    ap.add_argument("--out", required=True)
    ap.add_argument("--shots", type=int, default=3)
    ap.add_argument("--gap", type=float, default=5.0)
    ap.add_argument("--settle", type=float, default=8.0)
    ap.add_argument("--panels", action="store_true", help="also crop every panel")
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    with sync_playwright() as pw:
        browser = pw.chromium.launch()
        page = browser.new_page(viewport={"width": 1920, "height": 1600},
                                device_scale_factor=2)
        page.goto(f"http://{args.host}:{args.port}/", wait_until="networkidle")
        page.wait_for_timeout(int(args.settle * 1000))
        for s in range(args.shots):
            tag = f"{s:02d}"
            page.screenshot(path=str(out / f"full-{tag}.png"), full_page=True)
            page.locator("#topbar").screenshot(path=str(out / f"strip-{tag}.png"))
            if args.panels:
                for pid in PANELS:
                    loc = page.locator(f"#{pid}")
                    if loc.count() == 0:
                        continue
                    try:
                        loc.screenshot(path=str(out / f"{pid}-{tag}.png"))
                    except Exception as exc:  # a hidden panel is not a failure
                        print(f"  skip {pid}: {exc}")
            print(f"{args.host}: shot {tag}")
            if s + 1 < args.shots:
                time.sleep(args.gap)
        browser.close()


if __name__ == "__main__":
    main()
