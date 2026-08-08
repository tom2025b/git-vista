// Does the code the Rust suite proves correct actually get REACHED?
//
// Each test here names the defect it exists to catch. All four defects it
// covers were invisible to `cargo test`, which never compiles
// `#[cfg(target_arch = "wasm32")]` code.

import { expect, test } from '@playwright/test'

import { DIFF_SCROLLER, openApp, openDiff, runtime } from './helpers.mjs'

/** Fixture commit indices, newest first. Index 0 is HEAD, the SHORT
 *  positive-control patch; index 1 is the long multi-hunk one. Named
 *  constants because a bare `0` here silently became the wrong commit
 *  when the fixture gained a commit, making a test unfalsifiable. */
const LONG_PATCH = 1

test.describe('status surfaces', () => {
  // #68d: `StatusSections` shipped with 20+ tests and zero consumers, so #68's
  // "touch cards and accessible list semantics" was false for weeks while the
  // suite stayed green. This asserts the semantics exist in the rendered DOM.
  test('status rows render as an accessible list with non-empty labels', async ({ page }) => {
    await openApp(page)
    await page.getByRole('button', { name: 'Activity' }).click()

    const items = page.getByRole('listitem')
    await expect(items.first()).toBeAttached({ timeout: 15_000 })

    const labels = await items.evaluateAll((els) =>
      els.map((e) => e.getAttribute('aria-label')),
    )
    expect(labels.length).toBeGreaterThan(0)
    // An empty or missing label is the failure mode that matters: the row is on
    // screen and looks fine, and VoiceOver reads nothing useful.
    for (const label of labels) {
      expect(label, 'every status row needs an aria-label').toBeTruthy()
      expect(label.trim().length).toBeGreaterThan(0)
    }
  })

  // #348: the topbar chip and the status panel disagreed about the same
  // working tree. Both now derive from one `chip_label`; this proves they
  // still agree against a fixture whose state is known exactly.
  test('the topbar chip agrees with the fixture working state', async ({ page }) => {
    const { fixture } = runtime()
    await openApp(page)

    const { staged, unstaged, untracked } = fixture.expected
    const chip = page.locator('.topbar').getByText(/\d+ staged/)
    await expect(chip).toBeVisible({ timeout: 15_000 })

    const text = (await chip.textContent()).trim()
    expect(text).toContain(`${staged} staged`)
    expect(text).toContain(`${unstaged} unstaged`)
    expect(text).toContain(`${untracked} untracked`)
  })

  // Split from the test above deliberately. The COUNTS being right is #348's
  // claim; the chip being announceable is #68's. Asserting both in one test
  // would let an accessibility regression hide behind correct numbers.
  //
  // Note what this asserts and what it does NOT. The chip's accessible name
  // currently comes from `title` (app/mod.rs, `<span class=class title=title>`),
  // which browsers do expose when nothing else supplies a name -- so the chip is
  // not silent, and asserting `aria-label` specifically would fail for a reason
  // that is a judgement call rather than a defect. That judgement (`title` is a
  // tooltip attribute, and touch devices never hover) is filed separately.
  test('the topbar chip has an accessible name, whatever supplies it', async ({ page }) => {
    await openApp(page)

    const chip = page.locator('.topbar').getByText(/\d+ staged/)
    await expect(chip).toBeVisible({ timeout: 15_000 })

    const name = await chip.evaluate((el) => {
      for (let e = el; e && e !== document.body; e = e.parentElement) {
        const n = e.getAttribute('aria-label') || e.getAttribute('title')
        if (n) return n
      }
      return null
    })
    expect(name, 'the status chip must be announceable, not just visible').toBeTruthy()
    expect(name).toMatch(/staged/)
  })
})

test.describe('diff rendering', () => {
  // #69c: `CumulativeHeights` shipped with 9 tests and zero consumers, so #69's
  // "rendering is virtualized" was false. A math-only budget test cannot see
  // that; counting mounted elements can.
  test('a long patch renders a bounded window, not every line', async ({ page }) => {
    await openApp(page)
    // Index 1, NOT 0. Index 0 is HEAD, the SHORT positive-control patch, on
    // which a bound of "fewer than 600 mounted rows" passes whether or not
    // virtualization exists at all. An earlier version of this test opened 0
    // and was therefore vacuous -- it could not have failed. Caught in
    // adversarial review, and worth the comment: the constant was correct when
    // written and became wrong when the fixture gained a commit.
    await openDiff(page, LONG_PATCH)

    const observed = await page.evaluate(async (sel) => {
      const scroller = document.querySelector(sel)
      const at = async (top) => {
        scroller.scrollTop = top
        await new Promise((r) => setTimeout(r, 350))
        const rows = document.querySelectorAll(`${sel} span`).length
        return { top: scroller.scrollTop, rows, text: scroller.textContent }
      }
      const total = scroller.scrollHeight
      return {
        total,
        top: await at(0),
        middle: await at(Math.floor(total / 2)),
        bottom: await at(total),
      }
    }, DIFF_SCROLLER)

    // 1. PRECONDITION: this really is a long patch. Without this the bound
    //    below is unfalsifiable -- the whole failure the old version had.
    //    2000 bulk lines at ~18.1px is ~36000px; require well over a screenful.
    expect(
      observed.total,
      'precondition: the patch under test must be long enough for windowing to matter',
    ).toBeGreaterThan(10_000)

    // 2. The window stays bounded at every scroll position, not just the top.
    for (const s of [observed.top, observed.middle, observed.bottom]) {
      expect(s.rows, `mounted rows at scrollTop ${s.top} must stay bounded`).toBeLessThan(600)
      // A selector matching nothing would satisfy the bound while proving
      // nothing, so require real content too.
      expect(s.rows, `something must be mounted at scrollTop ${s.top}`).toBeGreaterThan(10)
    }

    // 3. CONTENT IS PRESERVED, not merely bounded. A renderer that dropped the
    //    body of the patch would pass 1 and 2 handsomely. The fixture writes
    //    `bulk line N` for N in 0..2000, so each region has a known sentinel.
    expect(observed.top.text, 'the start of the patch should be rendered at the top').toContain(
      'bulk line 0',
    )
    expect(
      observed.bottom.text,
      'the end of the patch should be rendered at the bottom',
    ).toContain('bulk line 1999')
    // The middle must show middle content AND must NOT still be showing the
    // start -- that difference is what distinguishes a real window from a
    // static render of the first screenful.
    expect(observed.middle.text, 'the middle of the patch should be rendered').not.toContain(
      'bulk line 0',
    )
  })

  // #350: `scroll_to_reveal` was built and mutation-proven, then never called
  // from the focus path. The observable consequence is here: a hunk header can
  // be scrolled entirely out of the DOM, which is what breaks keyboard
  // navigation (#210).
  test('hunk headers unmount when scrolled outside the window', async ({ page }) => {
    await openApp(page)
    // Commit 1 is the long multi-hunk patch; a short one keeps every header
    // mounted and would make this test pass for the wrong reason.
    await openDiff(page, 1)

    const observed = await page.evaluate(async (sel) => {
      const scroller = document.querySelector(sel)
      const seen = []
      const stops = [0, 0.25, 0.5, 0.75].map((f) => Math.floor(scroller.scrollHeight * f))
      for (const top of stops) {
        scroller.scrollTop = top
        await new Promise((r) => setTimeout(r, 300))
        seen.push({ top: scroller.scrollTop, hunks: document.querySelectorAll('span.diff-hunk').length })
      }
      return seen
    }, DIFF_SCROLLER)

    // This documents CURRENT behaviour so the fix has something to flip. When
    // #210 is fixed by revealing before focusing, a focused header should never
    // be unmounted out from under the user -- at which point this expectation
    // becomes the wrong one and should be replaced by the keyboard spec's.
    const everEmpty = observed.some((s) => s.hunks === 0)
    expect(
      everEmpty,
      'if this now fails, virtualization no longer unmounts hunk headers — ' +
        'good news; update this test and the #210 keyboard spec together',
    ).toBe(true)
  })
})
