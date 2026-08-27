// #362 -- the full-screen viewer, before and after windowing.
//
// THE ARC, because these assertions inverted and the reason matters.
//
// This file was written to answer #362's step 1: does the unwindowed viewer
// actually cost anything, or is the alarm folklore? Measured, it was fine at
// fixture scale (a 4000-line patch filled in 590ms, 8,024 DOM nodes) and NOT
// fine at the cap: the fitted slope projected ~11,000ms and ~246,000 nodes at
// the 5,000,000-byte limit, against an 8,000ms budget. That measurement is
// what funded the work -- not the architectural argument the issue opened
// with, which was never measured.
//
// The viewer is now windowed, so the old assertions are gone: they pinned the
// unwindowed contract ("the END of the patch must be rendered -- uncapped
// means uncapped") and windowing deliberately breaks it. They failed loudly
// when the behaviour changed, which is exactly what they were for.
//
// What replaces them is the windowed contract, and it is strictly harder to
// satisfy: bounded DOM, an honest scroll range, and every line still
// reachable.

import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

import { expect, test } from '@playwright/test'

import { openApp, openDiff } from './helpers.mjs'

/** Index 2 adds big.txt whole, so its diff is 4000 lines of "+". */
const BIG_PATCH = 2
/** Index 0 is HEAD, the short positive-control patch. */
const SMALL_PATCH = 0

const OPEN_BUDGET_MS = 8000

/** The panel's bound, applied here for the same reason: a window that mounts
 *  hundreds of rows is not a window. 4000 lines must not become 4000 rows. */
const MOUNTED_ROW_BOUND = 600

async function openViewer(page, nth) {
  await openApp(page)
  await openDiff(page, nth)
  const expand = page.getByRole('button', { name: 'Expand Full Diff' })
  await expect(expand, 'the viewer entry point must exist').toBeVisible()
  const startedAt = await page.evaluate(() => performance.now())
  await expand.click()
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
  return startedAt
}

test.describe('#362 full-screen viewer', () => {
  test('renders a bounded window, keeps the scroll range honest, and loses nothing', async ({
    page,
  }) => {
    const startedAt = await openViewer(page, BIG_PATCH)

    const measured = await page.evaluate(
      ({ t0 }) => {
        const el = document.querySelector('.viewer-body')
        return {
          elapsedMs: Math.round(performance.now() - t0),
          mountedSpans: el.querySelectorAll('span').length,
          domNodes: el.querySelectorAll('*').length,
          scrollHeight: el.scrollHeight,
          textLength: el.textContent.length,
          heapMb: performance.memory
            ? Math.round(performance.memory.usedJSHeapSize / 1e6)
            : null,
        }
      },
      { t0: startedAt },
    )

    console.log(
      '\n#362 viewer, WINDOWED (fixture big.txt, 4000-line patch):\n' +
        `  time to filled body : ${measured.elapsedMs} ms\n` +
        `  mounted <span>      : ${measured.mountedSpans}   (was 8,020 unwindowed)\n` +
        `  DOM nodes in body   : ${measured.domNodes}   (was 8,024)\n` +
        `  scrollHeight        : ${measured.scrollHeight} px\n` +
        `  rendered text       : ${measured.textLength} chars   (was 163,061)\n` +
        `  JS heap             : ${measured.heapMb ?? 'n/a'} MB\n`,
    )
    mkdirSync(join(process.cwd(), '.measurements'), { recursive: true })
    writeFileSync(
      join(process.cwd(), '.measurements', '362-viewer.json'),
      JSON.stringify({ windowed: true, patchLines: 4000, ...measured }, null, 2),
    )

    // 1. BOUNDED. The point of the exercise.
    expect(
      measured.domNodes,
      `${measured.domNodes} nodes mounted for a 4000-line patch — a window that mounts ` +
        'everything is not a window',
    ).toBeLessThan(MOUNTED_ROW_BOUND)
    // ...and not bounded by rendering nothing, which would satisfy the line above.
    expect(measured.domNodes, 'something must actually be mounted').toBeGreaterThan(10)

    // 2. THE SCROLL RANGE IS STILL HONEST. This is what the pad_top/pad_bottom
    //    spacers are for, and it is the half that is easy to get wrong: a
    //    viewer that mounted 50 rows and reported a 900px document would be
    //    "windowed" and useless, because the scrollbar would lie about how
    //    much there is.
    expect(
      measured.scrollHeight,
      'the scroll range must still describe the WHOLE patch, not just the mounted window',
    ).toBeGreaterThan(10_000)

    // 3. STILL FAST.
    expect(measured.elapsedMs).toBeLessThan(OPEN_BUDGET_MS)

    // 4. NOTHING IS LOST. The start is mounted now; the end must be reachable
    //    by scrolling. A window that cannot reach its own end has traded a
    //    performance problem for a correctness one.
    const body = page.locator('.viewer-body')
    expect(await body.textContent()).toContain('line 0 of the large file')

    await page.evaluate(() => {
      const el = document.querySelector('.viewer-body')
      el.scrollTop = el.scrollHeight
    })
    await expect
      .poll(async () => (await body.textContent()) ?? '', { timeout: 5000 })
      .toContain('line 3999 of the large file')

    // And the window stayed bounded down there — not merely at the top.
    const atBottom = await page.evaluate(
      () => document.querySelector('.viewer-body').querySelectorAll('*').length,
    )
    expect(atBottom, 'the window must stay bounded at the end of the document too').toBeLessThan(
      MOUNTED_ROW_BOUND,
    )
  })

  test('mounted DOM no longer scales with patch size', async ({ page }) => {
    // Before windowing, this pair fitted a slope that projected past the
    // budget at the cap. That projection is what justified the work, so the
    // test that measured it now measures whether the work succeeded: mounted
    // cost must be roughly FLAT between a 3-line patch and a 4000-line one.
    await openViewer(page, SMALL_PATCH)
    const small = await page.evaluate(() => ({
      nodes: document.querySelector('.viewer-body').querySelectorAll('*').length,
    }))

    await openViewer(page, BIG_PATCH)
    const big = await page.evaluate(() => ({
      nodes: document.querySelector('.viewer-body').querySelectorAll('*').length,
      scrollHeight: document.querySelector('.viewer-body').scrollHeight,
    }))

    const patchRatio = 4000 / 3 // roughly: big.txt's lines vs the short patch's
    const nodeRatio = big.nodes / Math.max(small.nodes, 1)

    console.log(
      '\n#362 windowing effectiveness:\n' +
        `  small patch nodes : ${small.nodes}\n` +
        `  big patch nodes   : ${big.nodes}\n` +
        `  node ratio        : ${nodeRatio.toFixed(1)}x for a ~${Math.round(patchRatio)}x ` +
        'larger patch\n',
    )

    // The document really is much bigger — otherwise the flatness below is
    // trivially true and proves nothing.
    expect(
      big.scrollHeight,
      'precondition: the big patch must produce a much taller document',
    ).toBeGreaterThan(10_000)

    // Windowing means mounted cost tracks the VIEWPORT, not the document. A
    // ~1300x larger patch must not mount ~1300x the nodes; a small constant
    // factor is expected, since the overscan and the file-header rows scale a
    // little with content.
    expect(
      nodeRatio,
      `mounted nodes grew ${nodeRatio.toFixed(1)}x for a ~${Math.round(patchRatio)}x larger ` +
        'patch — if this tracks patch size, the window is not bounding anything',
    ).toBeLessThan(20)
  })
})
