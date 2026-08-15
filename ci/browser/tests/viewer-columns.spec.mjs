// #362 step 3 -- is the measured column count the REAL wrap threshold?
//
// `features/diff/measure.rs` computes columns by measuring `.viewer-body`'s
// width and dividing by one monospace cell, taken from a 100-character probe
// carrying the same classes as the rendered patch. Its unit tests cover the
// arithmetic and its guards; they cannot cover the measurement, because
// nothing DOM-shaped compiles under `cargo test`.
//
// So this asserts the property the whole thing exists for: at the measured
// column count N, a line of N characters occupies ONE row and a line of N+1
// occupies TWO. If that holds, the number handed to `row_heights` is the
// browser's actual wrap point rather than a plausible guess.
//
// WHAT THIS PROVES AND WHAT IT DOES NOT. It proves the TECHNIQUE is sound --
// same selector, same probe, same arithmetic as the Rust. It does not execute
// the Rust function itself, which no browser test can do. If the two ever
// diverge it would be because the Rust stopped following the technique
// asserted here, so the technique is written out explicitly rather than
// hidden in a helper.

import { expect, test } from '@playwright/test'

import { openApp, openDiff } from './helpers.mjs'

const BIG_PATCH = 2

test.describe('#362 viewer column measurement', () => {
  test('the measured column count is exactly where the browser wraps', async ({ page }) => {
    await openApp(page)
    await openDiff(page, BIG_PATCH)
    await page.getByRole('button', { name: 'Expand Full Diff' }).click()
    await expect(page.locator('.viewer-body')).toBeVisible()

    const result = await page.evaluate(() => {
      // --- the measurement, mirroring measure.rs::measure_viewer ---
      const container = document.querySelector('.viewer-body')
      const width = container.getBoundingClientRect().width

      const probe = document.createElement('pre')
      probe.className = 'detail-diff viewer-pre'
      probe.setAttribute(
        'style',
        'position:absolute;visibility:hidden;left:-9999px;top:0;' +
          'margin:0;padding:0;border:0;white-space:pre;width:auto',
      )
      // 100 characters then divided: a single character's box is subject to
      // sub-pixel rounding, and a 0.4px error per cell is tens of columns of
      // drift across a wide viewer.
      probe.textContent = '0'.repeat(100)
      document.body.appendChild(probe)
      const charPx = probe.getBoundingClientRect().width / 100

      // --- columns_for(width, charPx) ---
      const columns = Math.floor(width / charPx)

      // --- the property under test ---
      // Wrap the probe at exactly the container width and see where a line of
      // N and a line of N+1 characters actually land. `pre-wrap` so it wraps
      // at all; a single unbroken run so word boundaries cannot confound it --
      // this is testing the COLUMN COUNT, not the word model, which
      // wrap-model.spec.mjs covers separately.
      probe.style.whiteSpace = 'pre-wrap'
      probe.style.wordBreak = 'break-word'
      probe.style.width = `${width}px`

      probe.textContent = 'x'
      const oneRow = probe.getBoundingClientRect().height

      probe.textContent = 'x'.repeat(columns)
      const atN = Math.round(probe.getBoundingClientRect().height / oneRow)

      probe.textContent = 'x'.repeat(columns + 1)
      const atNplus1 = Math.round(probe.getBoundingClientRect().height / oneRow)

      probe.remove()
      return { width, charPx, columns, atN, atNplus1, oneRow }
    })

    console.log(
      '\n#362 column measurement:\n' +
        `  .viewer-body width : ${result.width.toFixed(1)} px\n` +
        `  one cell           : ${result.charPx.toFixed(3)} px\n` +
        `  measured columns   : ${result.columns}\n` +
        `  rows at N chars    : ${result.atN}\n` +
        `  rows at N+1 chars  : ${result.atNplus1}\n`,
    )

    // Preconditions -- without these the two assertions below could pass on a
    // collapsed or unmeasurable container and mean nothing.
    expect(result.width, 'the viewer must have a real width to measure').toBeGreaterThan(50)
    expect(result.charPx, 'one monospace cell must have a real width').toBeGreaterThan(2)
    expect(result.columns, 'the measurement must yield a usable column count').toBeGreaterThan(10)

    // THE PROPERTY. N characters fit; N+1 do not.
    expect(
      result.atN,
      `${result.columns} characters must occupy ONE row at the measured column count — ` +
        'if this wraps, the measurement OVER-counts columns and every line will be ' +
        'measured shorter than it draws',
    ).toBe(1)
    expect(
      result.atNplus1,
      `${result.columns + 1} characters must occupy TWO rows — if this still fits, the ` +
        'measurement UNDER-counts columns and the scroll range will run long',
    ).toBe(2)
  })
})
