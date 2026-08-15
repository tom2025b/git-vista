// #362 step 1 -- DOES the unwindowed viewer actually cost anything?
//
// The full-screen viewer renders its patch with no windowing at all, against a
// cap of 5,000,000 bytes -- 25x the panel's 200,000. That sounds alarming, and
// the alarm has been repeated for long enough to feel like a finding.
//
// It has never been measured. docs/PERFORMANCE_BUDGETS.md says so in its own
// words: paint time and first-paint latency "are not measured anywhere in this
// document", and the viewer is explicitly outside its scope. The existing
// ladder proves the windowing ARITHMETIC is cheap; it says nothing about the
// drawing, and `grep -rn viewer ci/browser/` returned zero hits before this
// file -- the browser suite has only ever exercised the panel.
//
// So this measures before anything gets built. #362's own scope puts it first
// and says why: "Do this before funding the rest -- it converts a plausibility
// argument into a measured one." If the viewer opens comfortably at fixture
// scale, windowing it is speculative work; if it does not, there is a number to
// point at instead of an architecture diagram.
//
// WHAT THIS FILE DELIBERATELY DOES NOT DO. It does not assert that the viewer
// is windowed, and it does not fail if every row is mounted. Unwindowed is the
// documented current design. It asserts the things that would make unwindowed
// a REAL problem -- the viewer failing to open, or losing content -- and prints
// the measurements so the decision rests on numbers.

import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

import { expect, test } from '@playwright/test'

import { openApp, openDiff } from './helpers.mjs'

/** Fixture commit indices, newest first. Index 2 is the commit that adds
 *  big.txt whole, so its diff is BIG_FILE_LINES (4000) lines of "+" -- the
 *  largest single patch the fixture produces. A bare index here is exactly
 *  what silently broke a sibling spec when the fixture gained a commit, so it
 *  is named and its identity is asserted below rather than trusted. */
const BIG_PATCH = 2

/** Index 0 is HEAD -- the deliberately SHORT positive-control patch the sibling
 *  specs use. It is the small end of the scaling comparison below. */
const SMALL_PATCH = 0

/** Generous on purpose. This is a ceiling that says "the viewer is unusable",
 *  not a performance target -- a tight budget on a 7.6 GB box under load would
 *  measure the box, and this repo already has a flaky-timeout issue (#387)
 *  from exactly that mistake. */
const OPEN_BUDGET_MS = 8000

test.describe('#362 full-screen viewer', () => {
  test('opens a near-cap patch without losing content, and we measure what it costs', async ({
    page,
  }) => {
    await openApp(page)
    await openDiff(page, BIG_PATCH)

    // PRECONDITION 1 -- the patch under test really is the big one. Without
    // this, every measurement below could be of a three-line diff and the
    // conclusion "the viewer is fine" would be unearned.
    const panelText = await page.locator('.detail-diff-scroll').textContent()
    expect(
      panelText,
      'precondition: commit index 2 must be the big.txt commit -- if the fixture ' +
        'gained a commit, BIG_PATCH is now pointing at the wrong one',
    ).toContain('line 0 of the large file')

    const expand = page.getByRole('button', { name: 'Expand Full Diff' })
    await expect(expand, 'the viewer entry point must exist').toBeVisible()

    // The measurement. `performance.now()` either side of the click, with the
    // viewer body's content settled -- not merely attached, since an empty
    // container attaches instantly and would report a flatteringly small
    // number.
    const started = await page.evaluate(() => performance.now())
    await expand.click()

    const body = page.locator('.viewer-body')
    await expect(body).toBeVisible({ timeout: OPEN_BUDGET_MS })
    await expect
      .poll(async () => (await body.textContent())?.length ?? 0, {
        timeout: OPEN_BUDGET_MS,
        message: 'the viewer body must actually fill with the patch, not just appear',
      })
      .toBeGreaterThan(10_000)

    const measured = await page.evaluate(
      ({ startedAt }) => {
        const el = document.querySelector('.viewer-body')
        return {
          elapsedMs: Math.round(performance.now() - startedAt),
          mountedSpans: el.querySelectorAll('span').length,
          domNodes: el.querySelectorAll('*').length,
          scrollHeight: el.scrollHeight,
          textLength: el.textContent.length,
          heapMb: performance.memory
            ? Math.round(performance.memory.usedJSHeapSize / 1e6)
            : null,
        }
      },
      { startedAt: started },
    )

    // Printed, not just asserted: the POINT of this test is the numbers. A
    // green tick tells whoever reads #362 nothing they can act on.
    console.log(
      '\n#362 viewer measurement (fixture big.txt, 4000-line patch):\n' +
        `  time to filled body : ${measured.elapsedMs} ms\n` +
        `  mounted <span>      : ${measured.mountedSpans}\n` +
        `  DOM nodes in body   : ${measured.domNodes}\n` +
        `  scrollHeight        : ${measured.scrollHeight} px\n` +
        `  rendered text       : ${measured.textLength} chars\n` +
        `  JS heap             : ${measured.heapMb ?? 'n/a'} MB\n`,
    )
    test.info().annotations.push({
      type: '#362 measurement',
      description: JSON.stringify(measured),
    })

    // Written to disk as well, because the whole value of this test is the
    // numbers and a Playwright reporter interleaves console output with its
    // own progress lines -- the first run of this spec printed its header and
    // swallowed the measurements. A file survives the run and can be quoted
    // into the issue.
    const out = join(process.cwd(), '.measurements')
    mkdirSync(out, { recursive: true })
    writeFileSync(
      join(out, '362-viewer.json'),
      JSON.stringify({ patchLines: 4000, budgetMs: OPEN_BUDGET_MS, ...measured }, null, 2),
    )

    // PRECONDITION 2 -- the render is genuinely large. This is what makes the
    // budget assertion meaningful: 4000 added lines at ~18px is far more than
    // a screenful, so a viewer that opened instantly by drawing nothing would
    // fail here rather than passing as a success.
    expect(
      measured.scrollHeight,
      'precondition: the viewer must be rendering far more than one screenful',
    ).toBeGreaterThan(10_000)

    // THE ACTUAL QUESTION. Not "is it windowed" but "is it usable".
    expect(
      measured.elapsedMs,
      `the viewer took ${measured.elapsedMs}ms to fill -- past this it is not a ` +
        'windowing nicety, it is a broken interaction',
    ).toBeLessThan(OPEN_BUDGET_MS)

    // CONTENT IS COMPLETE. The viewer's whole reason to exist is that it is
    // uncapped, so a viewer that silently truncated would defeat the feature
    // while looking fast. Both ends must be present.
    const text = await body.textContent()
    expect(text, 'the start of the patch must be rendered').toContain('line 0 of the large file')
    expect(text, 'the END of the patch must be rendered -- uncapped means uncapped').toContain(
      'line 3999 of the large file',
    )
  })

  // The measurement above is of a 163 KB patch. The viewer's cap is 5,000,000
  // bytes -- roughly THIRTY TIMES larger. Reporting "the viewer is fine" from
  // one point 3% of the way up the range would be the same confident
  // extrapolation this repo keeps getting caught by.
  //
  // A second point turns the extrapolation into something with a slope behind
  // it. If cost per rendered line is flat between a small patch and a 4000-line
  // one, linear projection to the cap is defensible arithmetic; if it is
  // already curving upward at 4000, the cap is worse than linear and the case
  // for windowing is stronger than a straight-line estimate suggests.
  //
  // A genuinely near-cap fixture is deliberately NOT built here: it would add
  // ~5 MB of diff text to a fixture that is rebuilt on every gate run, taxing
  // every future run to answer a question that gets answered once.
  test('cost per line does not blow up between a small patch and a large one', async ({ page }) => {
    const measure = async (nth) => {
      await openApp(page)
      await openDiff(page, nth)
      const expand = page.getByRole('button', { name: 'Expand Full Diff' })
      await expect(expand).toBeVisible()
      const startedAt = await page.evaluate(() => performance.now())
      await expand.click()
      const body = page.locator('.viewer-body')
      await expect(body).toBeVisible({ timeout: OPEN_BUDGET_MS })
      await expect
        .poll(async () => (await body.textContent())?.length ?? 0, { timeout: OPEN_BUDGET_MS })
        .toBeGreaterThan(100)
      return page.evaluate(
        ({ t0 }) => {
          const el = document.querySelector('.viewer-body')
          return {
            elapsedMs: Math.round(performance.now() - t0),
            nodes: el.querySelectorAll('*').length,
            chars: el.textContent.length,
          }
        },
        { t0: startedAt },
      )
    }

    const small = await measure(SMALL_PATCH)
    const big = await measure(BIG_PATCH)

    // Guard against the two points being the same size, which would make the
    // ratio meaningless while still passing every assertion below.
    expect(
      big.chars / Math.max(small.chars, 1),
      'precondition: the two patches must differ enough in size to have a slope',
    ).toBeGreaterThan(5)

    // DO NOT divide elapsed by chars. Opening the viewer costs a fixed amount
    // before a single character is drawn, and the small patch is 702 chars, so
    // a per-character rate computed from it is ~99% constant overhead. Measured
    // first time round: 250 us/char "small" vs 3.4 us/char "big", which reads
    // as the render getting 70x CHEAPER at scale. It is not; the constant is
    // simply spread thinner.
    //
    // Two points give the honest model directly: elapsed = fixed + slope*chars.
    const slopeMsPerChar = (big.elapsedMs - small.elapsedMs) / (big.chars - small.chars)
    const fixedMs = small.elapsedMs - slopeMsPerChar * small.chars
    const CAP_BYTES = 5_000_000

    const projection = {
      small,
      big,
      model: 'elapsedMs = fixedMs + slopeMsPerChar * chars, fitted on the two points above',
      fixedMs: Math.round(fixedMs),
      slopeMsPerChar: +slopeMsPerChar.toFixed(6),
      capBytes: CAP_BYTES,
      projectedCapMs: Math.round(fixedMs + slopeMsPerChar * CAP_BYTES),
      projectedCapNodes: Math.round((big.nodes / big.chars) * CAP_BYTES),
      budgetMs: OPEN_BUDGET_MS,
      note:
        'A two-point fit cannot see curvature. If the real cost is super-linear ' +
        '(more DOM -> slower layout, which is the usual shape), the cap is WORSE ' +
        'than this. Treat the projection as a floor, not an estimate.',
    }
    console.log('\n#362 scaling:\n' + JSON.stringify(projection, null, 2) + '\n')
    writeFileSync(
      join(process.cwd(), '.measurements', '362-scaling.json'),
      JSON.stringify(projection, null, 2),
    )

    // The slope must be positive and real: a zero or negative slope would mean
    // the two measurements disagree about which patch is bigger, and every
    // number derived from them would be noise wearing a decimal point.
    expect(
      slopeMsPerChar,
      'the larger patch must cost more than the smaller one, or this fit is noise',
    ).toBeGreaterThan(0)

    // THE FINDING, asserted so it cannot quietly stop being true. At the
    // measured slope the viewer's own cap projects PAST the budget, which is
    // what justifies windowing it -- not the 25x-cap architecture argument the
    // issue opened with, which was never measured.
    //
    // If this ever fails, the render got fast enough that the cap fits inside
    // the budget and #362's remaining scope should be reconsidered rather than
    // completed out of momentum.
    expect(
      projection.projectedCapMs,
      `at the measured slope a cap-sized patch projects to ${projection.projectedCapMs}ms, ` +
        `inside the ${OPEN_BUDGET_MS}ms budget -- if that is real, windowing the viewer ` +
        'is no longer justified by this measurement and #362 needs re-deciding',
    ).toBeGreaterThan(OPEN_BUDGET_MS)
  })
})
