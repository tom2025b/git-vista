// #364 item 1 -- does pointer TYPE actually change behaviour, or only arithmetic?
//
// `geometry::drag_threshold` returns 12px of slop for "touch" and 4px for
// everything else, and its unit tests pin exactly those numbers. Those tests
// prove the FUNCTION. They cannot prove the WIRE: `gestures.rs:173` is the only
// caller, it lives behind `#[cfg(target_arch = "wasm32")]`, and `cargo test`
// never compiles it. Before this file, `grep pointerType` across the crate and
// the browser suite returned zero hits outside that pure helper -- so #69's
// "Finger, Pencil ... navigation work" rested on correct arithmetic behind an
// untested connection.
//
// WHAT IS OBSERVED, and why it is the honest signal. The threshold decides
// tap-versus-pan for the CAMERA, not for hunk headers. A pan changes
// `<g transform="translate(tx ty) scale(s)">` inside `.graph-svg`
// (app/canvas.rs:591). So the transform string is the app's own answer to "did
// you treat that as a drag?" -- read out of the DOM rather than inferred.
//
// THE LIMIT, STATED PLAINLY SO NOBODY OVER-READS THIS FILE. Playwright
// synthesises these events; `pointerType` is set by the test, not by hardware.
// This proves the branch is wired and behaves differently per type. It does NOT
// prove a real finger or a real Apple Pencil on a real iPad. The repo's own
// history is the reason for the caution: WORKLOG records that VNC delivered
// touch as mouse events, which is exactly how this stayed unverified. A
// synthetic pointerType is a genuine step up from a pure function agreeing with
// itself -- it is not the same as the device.

import { expect, test } from '@playwright/test'

import { openApp } from './helpers.mjs'

/** Between the two thresholds on purpose: past mouse/pen's 4px, short of
 *  touch's 12px. This is the ONLY distance at which the two types must
 *  disagree, so it is the whole experiment. */
const BETWEEN = 8

/** Comfortably past touch's 12px, to prove touch can still pan at all. */
const BEYOND = 24

/** The camera transform, straight out of the DOM. */
async function transform(page) {
  return page.evaluate(() => {
    const g = document.querySelector('.graph-svg > g')
    return g ? g.getAttribute('transform') : null
  })
}

/** Press, move, release on `.graph-svg` as a given pointerType.
 *
 *  Dispatched as real PointerEvents rather than page.mouse, because
 *  page.mouse can only ever be "mouse" — the one thing this test needs to vary
 *  is precisely the property page.mouse hard-codes. Two moves, not one: the
 *  handler compares against the position recorded at pointerdown, and a single
 *  large jump would not show that the threshold is being consulted per move. */
async function swipe(page, pointerType, dx) {
  await page.evaluate(
    ({ pointerType, dx }) => {
      const svg = document.querySelector('.graph-svg')
      const r = svg.getBoundingClientRect()
      const x = r.left + r.width / 2
      const y = r.top + r.height / 2
      const opts = (cx, cy) => ({
        pointerId: 1,
        pointerType,
        isPrimary: true,
        clientX: cx,
        clientY: cy,
        bubbles: true,
        cancelable: true,
        buttons: 1,
      })
      svg.dispatchEvent(new PointerEvent('pointerdown', opts(x, y)))
      svg.dispatchEvent(new PointerEvent('pointermove', opts(x + dx / 2, y)))
      svg.dispatchEvent(new PointerEvent('pointermove', opts(x + dx, y)))
      svg.dispatchEvent(new PointerEvent('pointerup', opts(x + dx, y)))
    },
    { pointerType, dx },
  )
  // The camera is a signal; give Leptos a frame to write the transform.
  await page.waitForTimeout(120)
}

test.describe('#364 pointer type changes behaviour, not just arithmetic', () => {
  test('an 8px move pans for mouse and pen, but is still a tap for touch', async ({ page }) => {
    await openApp(page)
    await expect(page.locator('.graph-svg')).toBeVisible()

    const start = await transform(page)
    expect(start, 'precondition: the camera must expose a transform to observe').not.toBeNull()

    // --- MOUSE: 8px is past its 4px threshold, so this must pan.
    await swipe(page, 'mouse', BETWEEN)
    const afterMouse = await transform(page)
    expect(
      afterMouse,
      `a ${BETWEEN}px mouse move did not pan the camera. Either drag_threshold is not ` +
        'being consulted, or pointer events are not reaching gestures.rs at all — in ' +
        'which case the touch assertion below would pass for the wrong reason.',
    ).not.toBe(start)

    // --- PEN: same 4px threshold as mouse. Reload so each type starts clean;
    //     asserting against a camera another gesture already moved would make
    //     "did it change?" depend on gesture order.
    await page.reload()
    await expect(page.locator('.graph-svg')).toBeVisible()
    const beforePen = await transform(page)
    await swipe(page, 'pen', BETWEEN)
    expect(
      await transform(page),
      'pen uses the same 4px threshold as mouse — a Pencil that needs a finger’s ' +
        'slop would feel imprecise, which is the distinction #115 introduced',
    ).not.toBe(beforePen)

    // --- TOUCH: 8px is INSIDE its 12px slop, so this must NOT pan.
    await page.reload()
    await expect(page.locator('.graph-svg')).toBeVisible()
    const beforeTouch = await transform(page)
    await swipe(page, 'touch', BETWEEN)
    expect(
      await transform(page),
      `a ${BETWEEN}px touch move panned the camera, but touch has ${12}px of slop — ` +
        'a finger wobbling on a tap would drag the graph out from under it (#115)',
    ).toBe(beforeTouch)

    // --- AND TOUCH STILL WORKS. Without this, the assertion above is satisfied
    //     just as well by touch events being ignored entirely, which would be a
    //     far worse bug than the one being guarded against.
    await swipe(page, 'touch', BEYOND)
    expect(
      await transform(page),
      `a ${BEYOND}px touch move must pan — past 12px it is unambiguously a drag. If ` +
        'this fails, touch is not reaching the handler and the previous assertion ' +
        'proved nothing.',
    ).not.toBe(beforeTouch)

    console.log(
      '\n#364 pointer-type behaviour (synthetic events, not hardware):\n' +
        `  mouse ${BETWEEN}px : panned      (threshold 4px)\n` +
        `  pen   ${BETWEEN}px : panned      (threshold 4px)\n` +
        `  touch ${BETWEEN}px : stayed put  (threshold 12px)\n` +
        `  touch ${BEYOND}px : panned      (past threshold)\n`,
    )
  })
})
