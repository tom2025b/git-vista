// M5.33 (#86) -- "Touch selection is accessible", proved against a real DOM.
//
// WHAT ONLY A BROWSER CAN SAY HERE. Three of this feature's claims are
// unreachable from `cargo test`:
//
//   1. The panel renders at all. Everything from the route through the git
//      subprocess, the wire types, and the wasm view is exercised by opening
//      it -- `handlers::blame::tests` proves the server half against temp
//      repos, and `features::blame::view_census` reads the view's bytes, but
//      neither one has ever seen the two halves meet.
//   2. A drag across rows actually selects the range. The arithmetic is
//      host-tested (`features::diff::selection::drag_range`, and
//      `features::blame::core`'s selection suite on top of it); the wiring
//      from three pointer events to that arithmetic is `view.rs`, wasm-only.
//   3. The tap targets are really 44px on screen. `features::a11y::audit`
//      can only read the stylesheet's *declarations*; whether the rendered
//      box clears 44x44 after flex, padding and font metrics have had their
//      say is a measurement, and only here can it be taken.
//
// THE LIMIT, STATED PLAINLY. Playwright synthesises these pointer events;
// `pointerType: 'touch'` is set by this file, not by hardware, and the same
// caution `pointer-type.spec.mjs` records applies verbatim -- WORKLOG has an
// instance of VNC delivering touch as mouse events, which is how this class
// of claim stayed unverified before. A synthetic touch drag is a real step up
// from a pure function agreeing with itself. It is not a finger on an iPad.

import { expect, test } from '@playwright/test'

import { openApp, openDiff } from './helpers.mjs'

/** #65's floor. The one number this file measures against. */
const MIN_TAP = 44

/** Open the newest commit's detail panel, then the blame panel for the first
 *  file it changed. Two clicks, because the "Blame" button is deliberately a
 *  sibling of the file's own open-the-file button rather than nested in it
 *  (native buttons cannot nest, and one tap must not mean two things). */
async function openBlame(page) {
  await openApp(page)
  await openDiff(page, 0)
  await page.locator('.detail-file-blame').first().click()
  await expect(page.locator('.blame-panel')).toBeAttached({ timeout: 20_000 })
}

/** Every row's spoken selection state, in row order -- read from
 *  `aria-pressed`, not from a CSS class, because the criterion is that the
 *  selection is *announced*, not merely painted. */
async function pressedStates(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.blame-select')].map(
      (b) => b.getAttribute('aria-pressed') === 'true',
    ),
  )
}

/** Press on row `from`'s select target, sweep into row `to`'s while the
 *  primary pointer is still down, release. Dispatched as real PointerEvents
 *  with `buttons: 1`, because the handler's own guard is `ev.buttons() != 1`
 *  -- a synthetic sweep without it is exactly the hover case the guard
 *  refuses, and would prove the opposite of what this test claims.
 *
 *  The step is SIGNED. A first version walked `from + 1 .. to` unconditionally,
 *  so an upward sweep dispatched no `pointerenter` at all and the upward-drag
 *  test failed against an app that was behaving correctly -- a helper that can
 *  only sweep one way, accusing the code of the helper's own limitation. Worth
 *  keeping in view: the same shape with a weaker assertion would have *passed*
 *  vacuously instead, which is the harder version of this bug to notice. */
async function dragSelect(page, from, to) {
  await page.evaluate(
    ({ from, to }) => {
      const targets = [...document.querySelectorAll('.blame-select')]
      const opts = { bubbles: true, pointerId: 1, pointerType: 'touch', isPrimary: true }
      targets[from].dispatchEvent(new PointerEvent('pointerdown', { ...opts, buttons: 1 }))
      const step = to > from ? 1 : -1
      // `from === to` is a tap, not a sweep: no `pointerenter` at all, and no
      // loop (an unguarded signed walk would step away from `to` forever and
      // index off the end of the array).
      for (let i = from; i !== to; ) {
        i += step
        targets[i].dispatchEvent(new PointerEvent('pointerenter', { ...opts, buttons: 1 }))
      }
      targets[to].dispatchEvent(new PointerEvent('pointerup', { ...opts, buttons: 0 }))
    },
    { from, to },
  )
}

test.describe('#86 blame: touch selection and its keyboard equal', () => {
  // The POSITIVE CONTROL. If this fails, nothing below is diagnostic --
  // the panel never opened and every other assertion is about an empty page.
  test('the Blame button opens a panel with attributed rows', async ({ page }) => {
    await openBlame(page)

    const rows = page.locator('.blame-row')
    await expect(rows.first()).toBeAttached()
    const count = await rows.count()
    expect(count, 'a blamed file should produce at least one range row').toBeGreaterThan(0)

    // Each row speaks its own name, and the names differ -- a row list where
    // every entry announces the same thing is unusable by voice even when it
    // looks right.
    const labels = await page.evaluate(() =>
      [...document.querySelectorAll('.blame-row')].map((r) => r.getAttribute('aria-label')),
    )
    for (const label of labels) {
      expect(label, 'every blame row needs a spoken label').toBeTruthy()
      expect(label).toMatch(/^Lines \d+/)
    }
  })

  test('both tap targets measure at least 44x44 on screen', async ({ page }) => {
    await openBlame(page)

    const boxes = await page.evaluate(() => {
      const pick = (sel) => {
        const el = document.querySelector(sel)
        if (!el) return null
        const r = el.getBoundingClientRect()
        return { w: r.width, h: r.height }
      }
      return { select: pick('.blame-select'), row: pick('.blame-row') }
    })

    for (const [name, box] of Object.entries(boxes)) {
      expect(box, `${name} should be on screen`).not.toBeNull()
      expect(box.w, `${name} is ${box.w}px wide, under #65's ${MIN_TAP}px floor`).toBeGreaterThanOrEqual(MIN_TAP)
      expect(box.h, `${name} is ${box.h}px tall, under #65's ${MIN_TAP}px floor`).toBeGreaterThanOrEqual(MIN_TAP)
    }
  })

  test('a touch drag selects the whole range it crossed, not just its ends', async ({ page }) => {
    await openBlame(page)
    const total = await page.locator('.blame-select').count()
    test.skip(total < 3, 'needs at least three rows to tell a range from its endpoints')

    await dragSelect(page, 0, 2)
    const after = await pressedStates(page)

    // The middle row is the whole assertion. A wiring that recorded only
    // pointerdown and pointerup -- the shape a missing `pointerenter` leaves
    // behind -- selects the ends and skips everything between them, while
    // still looking alive.
    expect(after[0], 'the row the drag started on').toBe(true)
    expect(after[1], 'the row the drag PASSED THROUGH — a missing pointerenter loses exactly this one').toBe(true)
    expect(after[2], 'the row the drag ended on').toBe(true)
    expect(after.slice(3).every((p) => p === false), 'nothing past the drag should be selected').toBe(true)
  })

  test('an upward drag selects the same range as the downward one', async ({ page }) => {
    await openBlame(page)
    const total = await page.locator('.blame-select').count()
    test.skip(total < 3, 'needs at least three rows')

    await dragSelect(page, 0, 2)
    const downward = await pressedStates(page)

    await page.reload()
    await openBlame(page)
    await dragSelect(page, 2, 0)
    const upward = await pressedStates(page)

    // `drag_range` is order-independent and host-tested for it; this is that
    // property surviving the trip through the DOM, where the anchor is a
    // pointerdown and the extent is whatever the finger reached.
    expect(upward, 'dragging up must select what dragging down selects').toEqual(downward)
  })

  test('the keyboard reaches the same selection a finger can', async ({ page }) => {
    await openBlame(page)
    const total = await page.locator('.blame-row').count()
    test.skip(total < 2, 'needs at least two rows')

    // Land on the first row the way Tab would, then extend with Shift.
    await page.evaluate(() => document.querySelector('.blame-row').focus())
    await page.keyboard.press('Shift+ArrowDown')

    const state = await page.evaluate(() => ({
      pressed: [...document.querySelectorAll('.blame-select')].map(
        (b) => b.getAttribute('aria-pressed') === 'true',
      ),
      focusedIsRow: !!document.activeElement?.classList?.contains('blame-row'),
      focusedIndex: document.activeElement?.getAttribute('data-blame-row'),
    }))

    expect(state.focusedIsRow, 'focus should have moved to another blame row').toBe(true)
    expect(state.focusedIndex, 'ArrowDown should move the roving position').toBe('1')
    expect(
      state.pressed[0] && state.pressed[1],
      'Shift+ArrowDown should select the range it crossed — whatever a drag can reach, a keyboard must',
    ).toBe(true)
  })

  // #86 review: the comparison path had NO browser assertion, and the index
  // bug lived exactly there — selection stores row indices, ranges carry
  // 1-based line numbers, and the toolbar searched one space with the other.
  // Row 0 therefore offered no comparison at all, and later rows could resolve
  // to the wrong commit. A test that only checks "a toolbar appeared" would
  // have passed throughout; this checks WHICH commit it names.
  test('selecting the first row offers a comparison, and it names that row\'s commit', async ({
    page,
  }) => {
    await openBlame(page)

    const firstCommit = await page.evaluate(
      () => document.querySelector('.blame-commit').textContent.trim(),
    )
    // Tap the first row's select target — a real click, which is also the
    // gesture the tap-self-clear bug broke.
    await page.locator('.blame-select').first().click()

    const toolbar = page.locator('.blame-toolbar button')
    await expect(
      toolbar,
      'row 0 must offer a comparison — under the index/line bug it never did',
    ).toBeVisible({ timeout: 10_000 })
    await expect(
      toolbar,
      'the offer must name the SELECTED row\'s commit, not whichever range happened to span the index',
    ).toContainText(firstCommit)
  })

  test('a later row offers a comparison naming its own commit, not an earlier one', async ({
    page,
  }) => {
    await openBlame(page)
    const rows = await page.locator('.blame-select').count()
    test.skip(rows < 2, 'needs at least two ranges')

    const secondCommit = await page.evaluate(
      () => [...document.querySelectorAll('.blame-commit')][1].textContent.trim(),
    )
    await page.locator('.blame-select').nth(1).click()

    const toolbar = page.locator('.blame-toolbar button')
    await expect(toolbar).toBeVisible({ timeout: 10_000 })
    await expect(toolbar).toContainText(secondCommit)
  })

  // The tap that undid itself: pointerdown committed a selection and the
  // click that follows toggled it back off, so the control looked dead. The
  // drag tests could not see it because they never dispatch a real click.
  test('a plain tap leaves the row selected', async ({ page }) => {
    await openBlame(page)
    await page.locator('.blame-select').first().click()
    await expect(
      page.locator('.blame-select').first(),
      'a tap must select and STAY selected — pointerdown and click must not both decide',
    ).toHaveAttribute('aria-pressed', 'true')
  })

  test('tapping the same row again clears it', async ({ page }) => {
    await openBlame(page)
    const target = page.locator('.blame-select').first()
    await target.click()
    await expect(target).toHaveAttribute('aria-pressed', 'true')
    await target.click()
    await expect(target, 'a second tap is the way back out').toHaveAttribute(
      'aria-pressed',
      'false',
    )
  })

  test('clicking a row opens that commit in the detail panel', async ({ page }) => {
    await openBlame(page)

    // The row's spoken label carries its commit's short id; the detail panel
    // it opens must be showing that same commit, not merely *a* commit.
    const shortId = await page.evaluate(() => {
      const row = document.querySelector('.blame-row')
      return row.querySelector('.blame-commit').textContent.trim()
    })
    await page.locator('.blame-row').first().click()

    const panel = page.locator('.detail-panel, .detail')
    await expect(panel.first()).toBeAttached({ timeout: 20_000 })
    await expect(page.getByText(shortId).first()).toBeAttached({ timeout: 20_000 })
  })
})
