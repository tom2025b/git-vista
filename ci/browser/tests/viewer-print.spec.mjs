// The viewer's print output -- does the PAPER get the whole patch?
//
// The bug this pins, reported 2026-08-15: every printed page blank except one
// line at the top. Two correct features collided. Windowing (#362) mounts only
// the rows around the scroll position and expresses the rest of the document
// as two spacer <div>s; the print stylesheet un-clips `.viewer-body` so the
// content can flow across pages. Together, the spacers became real pages with
// no text in them -- the patch paginated to full length, and only the ~40
// mounted rows carried anything.
//
// WHY THIS TEST IS IN THE BROWSER SUITE AND NOT A UNIT TEST. The defect lives
// exactly where `cargo test` cannot see: the interaction between a wasm-only
// signal, Leptos' render flush, and the print stylesheet. A Rust test asserting
// "printing => full range" would have passed against the broken build, because
// the broken build's model was never wrong -- only the DOM was.
//
// The assertions therefore emulate the print media type rather than trusting
// any flag, and count what is actually mounted.

import { expect, test } from '@playwright/test'

import { openApp, openDiff } from './helpers.mjs'

/** Index 2 adds big.txt whole, so its diff is 4000 lines of "+". */
const BIG_PATCH = 2

const OPEN_BUDGET_MS = 8000

async function openViewer(page) {
  await openApp(page)
  await openDiff(page, BIG_PATCH)
  await page.getByRole('button', { name: 'Expand Full Diff' }).click()
  const body = page.locator('.viewer-body')
  await expect(body).toBeVisible({ timeout: OPEN_BUDGET_MS })
  // #387: `aria-busy` on `.viewer-modal` is the app's own readiness signal —
  // false exactly when the body has painted real content rather than the
  // "Loading…" placeholder (`features/readiness/core.rs::is_viewer_busy`).
  // This used to poll `body.textContent().length > 100`, a guess calibrated
  // to "Loading…" being short; the real signal replaces the guess rather
  // than tightening it.
  await expect(page.locator('.viewer-modal')).toHaveAttribute('aria-busy', 'false', {
    timeout: OPEN_BUDGET_MS,
  })
}

/** Rows mounted inside `.viewer-body`, and the height of the spacer divs.
 *
 *  The spacers are the direct-child <div>s with an inline pixel height -- the
 *  windowing pads. On paper they must be ZERO, because there is nothing to
 *  scroll past: every row is present. */
async function measure(page) {
  return page.evaluate(() => {
    const body = document.querySelector('.viewer-body')
    const pre = body.querySelector('pre.viewer-pre')
    const spacers = [...body.children].filter(
      (el) => el.tagName === 'DIV' && /height:\s*[\d.]+px/.test(el.getAttribute('style') || ''),
    )
    return {
      rows: pre ? pre.querySelectorAll('span').length : 0,
      textLength: body.textContent.length,
      spacerPx: spacers.reduce((n, el) => n + parseFloat(el.style.height || '0'), 0),
    }
  })
}

test.describe('viewer print output', () => {
  test('printing mounts the whole patch and drops the windowing spacers', async ({ page }) => {
    await openViewer(page)

    const onScreen = await measure(page)

    // Precondition. If the viewer were NOT windowed on screen, the print
    // assertions below would pass trivially and prove nothing -- this is the
    // "what would make this green while the mechanism is broken?" check.
    expect(
      onScreen.spacerPx,
      'precondition: on screen the viewer must be windowed, i.e. carry real ' +
        'spacer height. Without this the print assertions are vacuous.',
    ).toBeGreaterThan(1000)

    // Emulating the media type is what makes this a real test: it drives the
    // SAME `beforeprint` lifecycle a user's Ctrl+P does, rather than poking
    // an internal flag the app might ignore.
    await page.emulateMedia({ media: 'print' })
    await page.evaluate(() => window.dispatchEvent(new Event('beforeprint')))
    await expect.poll(async () => (await measure(page)).spacerPx, { timeout: 5000 }).toBe(0)

    const printed = await measure(page)

    console.log(
      '\nviewer print (fixture big.txt, 4000-line patch):\n' +
        `  on screen : ${onScreen.rows} rows, ${onScreen.spacerPx}px spacers\n` +
        `  printing  : ${printed.rows} rows, ${printed.spacerPx}px spacers\n`,
    )

    // 1. THE WHOLE PATCH IS MOUNTED. This is the defect, stated directly.
    expect(
      printed.rows,
      `only ${printed.rows} rows mounted for a 4000-line patch while printing — ` +
        'the pages past the mounted window will come out blank',
    ).toBeGreaterThan(3900)

    // 2. NO SPACERS. Any leftover spacer height is a blank band on paper.
    expect(printed.spacerPx, 'spacer height is blank paper when printing').toBe(0)

    // 3. THE END OF THE PATCH IS PRESENT -- not merely a bigger window.
    const text = await page.locator('.viewer-body').textContent()
    expect(text).toContain('line 0 of the large file')
    expect(
      text,
      'the last line of the patch must be in the print DOM, or the final pages are blank',
    ).toContain('line 3999 of the large file')

    // 4. AND IT GOES BACK. A viewer left fully mounted after printing has
    //    traded the print bug for the performance one #362 fixed.
    await page.evaluate(() => window.dispatchEvent(new Event('afterprint')))
    await page.emulateMedia({ media: 'screen' })
    await expect
      .poll(async () => (await measure(page)).rows, { timeout: 5000 })
      .toBeLessThan(600)
  })
})
