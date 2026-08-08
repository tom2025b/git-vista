// #210 -- arrow keys must move between hunks, not scroll the container.
//
// The two tests below are a matched pair, and the pair is the point. The same
// keypress on the same widget behaves differently depending only on how LONG
// the patch is:
//
//   short patch  -> focus moves hunk to hunk. Works today.
//   long patch   -> the focused header is scrolled out of the virtualized
//                   window, unmounts, focus falls to <body>, and arrows become
//                   the container's native scrolling. Broken today.
//
// Either test alone is misleading. The first alone says "#210 works"; the
// second alone says "#210 is broken" and invites a fix to the focus model,
// which is not where the defect is. Together they locate it precisely: the
// focus model is correct, and virtualization unmounts the node it depends on.

import { expect, test } from '@playwright/test'

import { DIFF_SCROLLER, openApp, openDiff } from './helpers.mjs'

/** HEAD: a short multi-hunk patch that fits inside one render window. */
const SHORT_MULTI_HUNK = 0
/** A patch with a 2000-line file plus edits, so later hunks sit far below. */
const LONG_MULTI_HUNK = 1

async function focusFirstHunk(page) {
  return page.evaluate((sel) => {
    const hunk = document.querySelector('span.diff-hunk')
    hunk.focus()
    return {
      focused: document.activeElement === hunk,
      label: hunk.getAttribute('aria-label') ?? hunk.textContent.trim().slice(0, 40),
      scrollTop: document.querySelector(sel).scrollTop,
    }
  }, DIFF_SCROLLER)
}

async function readFocus(page) {
  return page.evaluate((sel) => {
    const a = document.activeElement
    return {
      activeIsHunk: !!(a.classList && a.classList.contains('diff-hunk')),
      activeTag: a.tagName,
      label: a.getAttribute?.('aria-label') ?? (a.textContent || '').trim().slice(0, 40),
      scrollTop: document.querySelector(sel).scrollTop,
      hunksInDom: document.querySelectorAll('span.diff-hunk').length,
    }
  }, DIFF_SCROLLER)
}

test.describe('#210 hunk keyboard navigation', () => {
  // The POSITIVE CONTROL. If this ever fails, the roving-focus model itself
  // regressed and the diagnosis below is wrong -- fix this first and re-derive.
  test('short patch: ArrowDown moves focus to the next hunk', async ({ page }) => {
    await openApp(page)
    await openDiff(page, SHORT_MULTI_HUNK)

    const before = await focusFirstHunk(page)
    expect(before.focused, 'a hunk header should be focusable').toBe(true)

    await page.keyboard.press('ArrowDown')
    const after = await readFocus(page)

    expect(after.activeIsHunk, `focus should stay on a hunk header, got <${after.activeTag}>`).toBe(true)
    expect(after.label, 'focus should have MOVED to a different hunk').not.toBe(before.label)
    expect(after.scrollTop, 'the container must not scroll natively').toBe(before.scrollTop)
  })

  test('short patch: Escape leaves the patch without closing the panel', async ({ page }) => {
    await openApp(page)
    await openDiff(page, SHORT_MULTI_HUNK)

    await focusFirstHunk(page)
    await page.keyboard.press('Escape')

    // Escape should leave the patch, not dismiss the panel -- an Escape that
    // closes everything is the iPad-hostile behaviour #210 explicitly avoids.
    await expect(page.locator(DIFF_SCROLLER)).toBeAttached()
    const after = await readFocus(page)
    expect(after.activeIsHunk, 'Escape should move focus off the hunk header').toBe(false)
  })

  test.describe('long patch (the open defect)', () => {
    // `test.fail()` rather than `skip`: Playwright expects these to fail and
    // reports an ERROR if they pass. So the day #210 is fixed, this file demands
    // attention instead of sitting green and forgotten -- which is precisely the
    // failure mode that let #210 survive as long as it has.
    test.fail()

    test('ArrowDown moves focus without scrolling', async ({ page }) => {
      await openApp(page)
      await openDiff(page, LONG_MULTI_HUNK)

      const before = await focusFirstHunk(page)
      expect(before.focused, 'a hunk header should be focusable').toBe(true)

      await page.keyboard.press('ArrowDown')
      const after = await readFocus(page)

      // Expected failure today: `activeTag` is BODY, `hunksInDom` may be 0, and
      // scrollTop has moved by roughly one viewport.
      expect(after.activeIsHunk, `focus should stay on a hunk header, got <${after.activeTag}>`).toBe(true)
      expect(after.scrollTop, 'the container must not scroll natively').toBe(before.scrollTop)
    })

    test('a focused hunk header is never unmounted out from under the user', async ({ page }) => {
      await openApp(page)
      await openDiff(page, LONG_MULTI_HUNK)
      await focusFirstHunk(page)

      // Scroll the way an arrow key would, then ask whether the thing that had
      // focus still exists. This is the mechanism itself, isolated from the
      // keyboard: `scroll_to_reveal` exists to make this safe and is not called.
      const survived = await page.evaluate(async (sel) => {
        const scroller = document.querySelector(sel)
        const focused = document.activeElement
        scroller.scrollTop = scroller.clientHeight * 2
        await new Promise((r) => setTimeout(r, 400))
        return { stillConnected: focused.isConnected, active: document.activeElement.tagName }
      }, DIFF_SCROLLER)

      expect(survived.stillConnected, 'the focused header was removed from the DOM').toBe(true)
      expect(survived.active, 'focus fell back to the document body').not.toBe('BODY')
    })
  })
})
