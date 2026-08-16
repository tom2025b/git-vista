// The commit-details panel: full-screen, and printable on its own.
//
// Requested 2026-08-15 -- read a commit's details full-screen, print them, and
// get the patch separately from the viewer (which already prints it uncapped).
//
// WHAT MAKES THIS WORTH A TEST. The panel's patch is WINDOWED: only the rows
// near the scroll position are in the DOM and the rest of the document's height
// is spacer divs. That is exactly the shape that produced #390's blank pages
// when a print stylesheet un-clipped it. This surface avoids the defect by
// excluding the patch entirely rather than by unwindowing it -- so the thing to
// pin is that the patch really is absent from the print output, and that what
// remains is the details, not an empty page.
//
// The assertions emulate the print media type and read computed styles, rather
// than trusting the signal that requested the print. A test that asserted "the
// attribute is set" would pass against a stylesheet that printed nothing.

import { expect, test } from '@playwright/test'

import { openApp, openDiff } from './helpers.mjs'

const BIG_PATCH = 2

/** Is this element actually rendered, per the browser's own computed style?
 *  `display: none` anywhere up the ancestor chain collapses the box, so
 *  offsetParent/rects are the honest check rather than reading one property. */
async function isRendered(page, selector) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel)
    if (!el) return false
    const r = el.getBoundingClientRect()
    return getComputedStyle(el).display !== 'none' && r.width > 0 && r.height > 0
  }, selector)
}

test.describe('commit details: full screen and print', () => {
  test('Full Screen widens the panel and gives it back', async ({ page }) => {
    await openApp(page)
    await openDiff(page, BIG_PATCH)

    const panel = page.locator('.detail-panel')
    await expect(panel).toBeVisible()

    const docked = (await panel.boundingBox()).width
    const viewport = page.viewportSize().width

    // Precondition: docked really is a sidebar. Without this, "full screen is
    // wider" could be trivially true on a narrow viewport where the panel
    // already spans the window, and the test would prove nothing.
    expect(
      docked,
      'precondition: the docked panel must be narrower than the window, ' +
        'or the full-screen assertion below is vacuous',
    ).toBeLessThan(viewport * 0.8)

    await page.getByRole('button', { name: 'Full Screen' }).click()
    await expect.poll(async () => (await panel.boundingBox()).width).toBeGreaterThan(docked)

    const full = (await panel.boundingBox()).width
    expect(full, 'full screen must actually fill the window').toBeGreaterThan(viewport * 0.95)

    // ...and it is a toggle, not a one-way door.
    await page.getByRole('button', { name: 'Exit Full Screen' }).click()
    await expect.poll(async () => (await panel.boundingBox()).width).toBe(docked)
  })

  test('printing the panel keeps the details and drops the patch', async ({ page }) => {
    await openApp(page)
    await openDiff(page, BIG_PATCH)
    await expect(page.locator('.detail-panel')).toBeVisible()

    // The patch must be on screen BEFORE printing -- otherwise "the patch is
    // absent from print" is true for the wrong reason.
    await expect(page.locator('.detail-panel .detail-diff').first()).toBeAttached()
    expect(
      await isRendered(page, '.detail-panel .detail-diff-scroll'),
      'precondition: the patch must be visible on screen, or its absence ' +
        'from the printout proves nothing',
    ).toBe(true)

    // Drive the real stylesheet: emulate print AND stamp the surface the way
    // print_detail() does.
    await page.emulateMedia({ media: 'print' })
    await page.evaluate(() => document.documentElement.setAttribute('data-print', 'detail'))

    const printed = await page.evaluate(() => {
      const panel = document.querySelector('.detail-panel')
      return {
        panelShown: getComputedStyle(panel).display !== 'none',
        panelPosition: getComputedStyle(panel).position,
        text: panel.textContent,
      }
    })

    // 1. THE PANEL SURVIVES. Every other print surface hides it; this one must
    //    not, and that rule was narrowed by hand, so it is worth pinning.
    expect(printed.panelShown, 'the panel is the print target — it must render').toBe(true)

    // 2. IT IS UN-TRAPPED. A panel left `position: fixed` prints its first
    //    page and silently drops the rest.
    expect(
      printed.panelPosition,
      'a fixed-position panel prints one page and loses the remainder',
    ).toBe('static')

    // 3. THE PATCH IS GONE. The whole point: the patch is windowed, and a
    //    windowed surface prints its spacers as blank pages (#390).
    expect(
      await isRendered(page, '.detail-panel .detail-diff-scroll'),
      'the patch must NOT print — it is windowed, and its spacers would come ' +
        'out as blank pages. Print it from the viewer instead.',
    ).toBe(false)

    // 4. THE CHROME IS GONE. Buttons are screen affordances.
    expect(await isRendered(page, '.detail-panel .detail-actions'), 'buttons are not paper').toBe(false)

    // 5. AND THE DOCUMENT IS NOT EMPTY -- the failure mode that would satisfy
    //    3 and 4 while producing a blank sheet.
    expect(
      printed.text.length,
      'the printout must still carry the commit details',
      // The file list header is the one string that proves the Changes
      // section survived while the patch under it did not.
    ).toBeGreaterThan(100)
    expect(printed.text, 'the file list must print — it is not the patch').toMatch(/Changes —/)

    await page.emulateMedia({ media: 'screen' })
  })
})
