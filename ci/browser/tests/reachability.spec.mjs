// Does the code the Rust suite proves correct actually get REACHED?
//
// Each test here names the defect it exists to catch. All four defects it
// covers were invisible to `cargo test`, which never compiles
// `#[cfg(target_arch = "wasm32")]` code.

import { expect, test } from '@playwright/test'

import { DIFF_SCROLLER, openApp, openDiff, runtime } from './helpers.mjs'

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
    await openDiff(page, 0)

    const counts = await page.evaluate(async (sel) => {
      const scroller = document.querySelector(sel)
      const at = async (top) => {
        scroller.scrollTop = top
        await new Promise((r) => setTimeout(r, 300))
        return document.querySelectorAll(`${sel} .diff-line, ${sel} span`).length
      }
      const total = scroller.scrollHeight
      return {
        total,
        samples: [await at(0), await at(total / 2), await at(total)],
      }
    }, DIFF_SCROLLER)

    // The window is a function of viewport height, not patch length, so the
    // bound holds at every scroll position rather than only at the top.
    for (const n of counts.samples) {
      expect(n, 'rendered element count must stay bounded').toBeLessThan(600)
    }
    // Guard against the opposite failure: a selector that matches nothing would
    // satisfy the bound above while proving nothing at all.
    expect(Math.max(...counts.samples)).toBeGreaterThan(0)
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
