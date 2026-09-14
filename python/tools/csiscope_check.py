"""Browser regressions against a local csiscope with a replay already running.

Run: python tools/csiscope_check.py --url http://127.0.0.1:18088/ --out shots/check
The replay supplies measurements; this probe never starts or controls a radio.
"""

import argparse
from pathlib import Path

from playwright.sync_api import sync_playwright


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as pw:
        browser = pw.chromium.launch()
        page = browser.new_page(viewport={"width": 1440, "height": 1000}, device_scale_factor=2)
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        # Inspect module state only in this test page. Production exports none.
        def instrument(route):
            response = route.fetch()
            route.fulfill(response=response, body=response.text() +
                          "\nwindow.scopeProbe = {S, heat, scheduleRender};\n")

        page.route("**/app.js", instrument)
        page.goto(args.url)
        page.wait_for_function("window.scopeProbe?.S.frame != null")
        page.wait_for_timeout(1500)
        assert page.locator('[data-tab="help"]').count() == 0
        if page.evaluate("scopeProbe.S.public"):
            assert not page.locator('[data-tab="node"]').is_visible()
            assert page.locator('#c-window').is_disabled()
            assert page.evaluate("!/([0-9a-f]{2}:){5}[0-9a-f]{2}/i.test(JSON.stringify(scopeProbe.S.frame.h))")
        else:
            page.locator('#c-chain').select_option('1')
            page.wait_for_function('scopeProbe.S.frame.h.geometry.chain === 1')
            page.wait_for_timeout(300)
        page.locator("#btn-pause").click()
        page.wait_for_timeout(500)
        before = page.evaluate("({w: scopeProbe.heat.waterfall.filled, d: scopeProbe.heat.doppler.filled})")
        assert before["w"] > 0
        sizes = []
        for width in [1280, 1440, 1100, 1440]:
            page.set_viewport_size({"width": width, "height": 1000})
            page.wait_for_timeout(150)
            sizes.append(page.locator("#p-waterfall").bounding_box()["height"])
        after = page.evaluate("({w: scopeProbe.heat.waterfall.filled, d: scopeProbe.heat.doppler.filled})")
        assert before == after, (before, after, "resize fabricated history")
        assert max(sizes) <= 445, sizes
        # A new palette must also recolour history; the legend describes the
        # entire heatmap. A transport seam wrapping the ring stays blank.
        page.evaluate('''async () => {
          const {Heatmap} = await import('./plot.js');
          const h = new Heatmap(document.createElement('canvas'), 'row', 4);
          h.push(new Uint8Array([100, 120, 140]), 3, 1);
          const before = [...h.bctx.getImageData(0, 1, 1, 1).data];
          h.setRamp('magma');
          const after = [...h.bctx.getImageData(0, 1, 1, 1).data];
          if (String(before) === String(after) || h.filled !== 3) throw Error('palette/history');
          h.gap();
          if (h.valid[3] || h.valid[0] || !h.valid[1]) throw Error('wrapped gap');
        }''')
        settings = page.evaluate("JSON.stringify(scopeProbe.S.settings)")
        page.evaluate("scopeProbe.S.ws.close()")
        page.wait_for_timeout(2500)
        assert page.evaluate("JSON.stringify(scopeProbe.S.settings)") == settings
        assert page.evaluate("scopeProbe.heat.waterfall.filled") >= before["w"]
        page.locator("#p-waterfall .help-tip").focus()
        assert page.locator("#help-p-waterfall").is_visible()
        page.keyboard.press("Escape")
        page.screenshot(path=str(args.out / "scope-desktop.png"))
        page.set_viewport_size({"width": 390, "height": 844})
        page.wait_for_timeout(300)
        assert page.locator('main').bounding_box()['width'] >= 380
        assert not page.locator('#rail').is_visible()
        page.locator('#btn-settings').click()
        assert page.locator('#rail').is_visible()
        page.locator('#btn-settings').click()
        page.screenshot(path=str(args.out / "scope-mobile.png"))
        assert not errors, errors
        print({"resize_history": after, "panel_heights": sizes, "reconnect_settings": "preserved", "errors": errors})
        browser.close()


if __name__ == "__main__":
    main()
