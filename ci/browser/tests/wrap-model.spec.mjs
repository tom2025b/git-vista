// #362 step 2 -- is the Rust wrap model right about what Chromium DRAWS?
//
// `rows::wrapped_rows` models `white-space: pre-wrap` + `word-break:
// break-word` and its unit tests assert specific row counts. Those tests prove
// the function matches MY arithmetic. They cannot prove it matches the
// browser, and the browser is the thing that decides where the scrollbar goes.
//
// That gap is exactly the class of failure this repo keeps finding: a model
// verified against its own assumptions, green forever, wrong in the DOM. So
// this renders the same strings through the app's real `.viewer-pre` rules and
// compares measured row counts against the numbers the Rust tests assert.
//
// If these ever disagree, ONE OF THE TWO IS WRONG and the windowing built on
// the model would put the wrong slice on screen.

import { expect, test } from '@playwright/test'

import { openApp } from './helpers.mjs'

/** Each case is [label, text, columns, expectedRows].
 *
 *  Every expectedRows here is copied from the assertions in
 *  `crates/git-vista/src/features/diff/rows.rs`. Deliberately duplicated
 *  rather than imported: the point is that two independent implementations --
 *  Rust and Chromium -- agree. Deriving one from the other would defeat it. */
const CASES = [
  ['a word moves down whole', 'let result = compute();', 22, 2],
  ['ordinary code, character model says 2', 'fn compute_all( value: u32)', 16, 2],
  ['word wrap needs MORE rows than character wrap', 'aaaaaaa bbbbbbb ccccccc', 12, 3],
  ['a word longer than the line breaks mid-word', 'x'.repeat(30), 10, 3],
  ['and its leftover takes a fourth row', 'x'.repeat(31), 10, 4],
  ['a long word starts fresh when the row is dirty', 'ab aaaaaaaaaaaaaaaaaaaa', 10, 3],
  ['indentation counts', '        indented', 12, 2],
  ['the same word without indent does not', 'indented', 12, 1],
  ['trailing spaces do not invent rows', 'abc        ', 5, 1],
]

/** East Asian Wide and emoji cases, held to a WEAKER contract on purpose.
 *
 *  These cannot be asserted exactly, and pretending otherwise would be false
 *  precision. A glyph's advance width depends on whichever font the browser
 *  falls back to, and this one draws CJK at slightly UNDER two cells —
 *  measured: ten ideographs wrapped at ten columns, but eleven still did not
 *  need a third row, which two-cell arithmetic says they should.
 *
 *  So the contract is directional rather than exact: the model must never
 *  measure FEWER rows than the browser draws. Under-measuring is the harmful
 *  direction — the scrollbar describes a shorter document than exists and a
 *  window keyed on those heights lands short. Over-measuring by a row costs a
 *  little wasted scroll range and nothing else. */
const WIDE_CASES = [
  ['ten ideographs', '中'.repeat(10), 10],
  ['eleven ideographs', '中'.repeat(11), 10],
  ['emoji are wide too', '😀'.repeat(6), 10],
]

test.describe('#362 wrap model vs Chromium', () => {
  test('the Rust row model agrees with what the browser actually draws', async ({ page }) => {
    // Load the real app so the real stylesheet and font stack are in play. A
    // hand-built page would measure a font nobody ships.
    await openApp(page)

    const results = await page.evaluate((cases) => {
      // A probe styled exactly like `.viewer-pre`, sized in `ch` so "columns"
      // means the same thing to the browser as it does to the model. `ch` is
      // the width of "0" in the current font -- exact for a monospace stack,
      // which is what the viewer uses.
      const probe = document.createElement('pre')
      probe.className = 'detail-diff viewer-pre'
      probe.style.position = 'absolute'
      probe.style.visibility = 'hidden'
      probe.style.left = '-9999px'
      probe.style.top = '0'
      probe.style.margin = '0'
      probe.style.padding = '0'
      probe.style.border = '0'
      document.body.appendChild(probe)

      // Row height from a single known-unwrapped line, rather than assuming a
      // number: line-height varies with the stylesheet and any change to it
      // would silently corrupt every count below.
      probe.style.width = '200ch'
      probe.textContent = 'x'
      const oneRow = probe.getBoundingClientRect().height

      const out = []
      for (const [label, text, columns, expected] of cases) {
        probe.style.width = `${columns}ch`
        probe.textContent = text
        const h = probe.getBoundingClientRect().height
        out.push({
          label,
          columns,
          expected,
          measured: Math.round(h / oneRow),
          rawHeight: Math.round(h),
          oneRow: Math.round(oneRow),
        })
      }
      probe.remove()
      return out
    }, CASES)

    // Report every case before asserting, so a failure shows the whole picture
    // rather than dying on the first disagreement.
    const table = results
      .map(
        (r) =>
          `  ${r.measured === r.expected ? 'ok  ' : 'DIFF'} ` +
          `${String(r.expected).padStart(2)} expected / ${String(r.measured).padStart(2)} drawn ` +
          `@ ${String(r.columns).padStart(3)} cols — ${r.label}`,
      )
      .join('\n')
    console.log(`\n#362 wrap model vs Chromium:\n${table}\n`)

    // Sanity: the probe must actually be measuring something. A zero row
    // height would make every count 0 and every comparison meaningless.
    expect(results[0].oneRow, 'the probe must have a real line height').toBeGreaterThan(4)

    const disagreements = results.filter((r) => r.measured !== r.expected)
    expect(
      disagreements,
      'the Rust model and Chromium must agree on every case — a disagreement means ' +
        'the windowing built on this model will render the wrong slice:\n' +
        disagreements
          .map((d) => `  "${d.label}": model says ${d.expected}, browser drew ${d.measured}`)
          .join('\n'),
    ).toEqual([])
  })

  test('wide characters are never UNDER-measured, whatever font the browser picks', async ({
    page,
  }) => {
    await openApp(page)

    const drawn = await page.evaluate((cases) => {
      const probe = document.createElement('pre')
      probe.className = 'detail-diff viewer-pre'
      Object.assign(probe.style, {
        position: 'absolute',
        visibility: 'hidden',
        left: '-9999px',
        top: '0',
        margin: '0',
        padding: '0',
        border: '0',
        width: '200ch',
      })
      document.body.appendChild(probe)
      probe.textContent = 'x'
      const oneRow = probe.getBoundingClientRect().height

      const out = cases.map(([label, text, columns]) => {
        probe.style.width = `${columns}ch`
        probe.textContent = text
        return {
          label,
          columns,
          measured: Math.round(probe.getBoundingClientRect().height / oneRow),
        }
      })
      probe.remove()
      return out
    }, WIDE_CASES)

    // The model's own answers, computed here from the same two-cells rule the
    // Rust side uses. Kept as a literal restatement rather than an import,
    // for the same reason as the table above: two implementations agreeing is
    // the evidence, and deriving one from the other destroys it.
    const modelRows = { 'ten ideographs': 2, 'eleven ideographs': 3, 'emoji are wide too': 2 }

    console.log(
      '\n#362 wide-character check (model must be >= drawn):\n' +
        drawn
          .map(
            (d) =>
              `  ${modelRows[d.label] >= d.measured ? 'ok  ' : 'UNDER'} ` +
              `model ${modelRows[d.label]} / drawn ${d.measured} @ ${d.columns} cols — ${d.label}`,
          )
          .join('\n') +
        '\n',
    )

    for (const d of drawn) {
      expect(
        modelRows[d.label],
        `"${d.label}": the model measured ${modelRows[d.label]} rows but the browser drew ` +
          `${d.measured}. Under-measuring means the scroll range falls short of the rendered ` +
          'document — the failure mode windowing exists to avoid.',
      ).toBeGreaterThanOrEqual(d.measured)
    }
  })
})
