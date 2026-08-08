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

  test.describe('long patch', () => {
    // What the contract actually is, and what it is NOT.
    //
    // An earlier version of these tests asserted "the container must not
    // scroll" and "the focused node must still be connected". Both were wrong,
    // and wrong in a way worth recording, because they would have rejected a
    // correct fix.
    //
    // Navigating to a hunk below the fold MUST scroll -- that is `reveal` doing
    // its job, and the alternative is focusing something the user cannot see.
    // And under virtualization, scrolling away from a node necessarily unmounts
    // it, so demanding the original element survive is demanding that
    // windowing not work. The distinction that matters is not "did it scroll"
    // but "did the app move focus, or did the browser scroll because nothing
    // handled the key".
    //
    // So: assert on where focus ENDS UP.

    test('ArrowDown moves focus to the next hunk, revealing it if needed', async ({ page }) => {
      await openApp(page)
      await openDiff(page, LONG_MULTI_HUNK)

      const before = await focusFirstHunk(page)
      expect(before.focused, 'a hunk header should be focusable').toBe(true)
      const beforeIdx = await page.evaluate(() =>
        document.activeElement.getAttribute('data-hunk-index'),
      )

      await page.keyboard.press('ArrowDown')
      // The reveal re-renders the window and focus is re-asserted on the next
      // frame, so read after the frame rather than synchronously.
      await page.waitForTimeout(250)

      const after = await page.evaluate(() => {
        const a = document.activeElement
        return {
          isHunk: !!(a.classList && a.classList.contains('diff-hunk')),
          tag: a.tagName,
          idx: a.getAttribute?.('data-hunk-index'),
        }
      })

      expect(after.isHunk, `focus should be on a hunk header, got <${after.tag}>`).toBe(true)
      expect(after.idx, 'focus should have moved to the NEXT hunk').toBe(
        String(Number(beforeIdx) + 1),
      )
    })

    test('the revealed hunk is actually visible in the viewport', async ({ page }) => {
      await openApp(page)
      await openDiff(page, LONG_MULTI_HUNK)
      await focusFirstHunk(page)

      await page.keyboard.press('ArrowDown')
      await page.waitForTimeout(250)

      // Focus without visibility is the failure this guards: an element can
      // hold focus while sitting outside the scroll container's visible box,
      // which reads to the user as "nothing happened".
      const visible = await page.evaluate((sel) => {
        const a = document.activeElement
        if (!a.classList?.contains('diff-hunk')) return { ok: false, why: 'focus is not on a hunk' }
        const box = a.getBoundingClientRect()
        const view = document.querySelector(sel).getBoundingClientRect()
        return {
          ok: box.top >= view.top - 1 && box.bottom <= view.bottom + 1,
          why: `hunk at ${Math.round(box.top)}..${Math.round(box.bottom)}, ` +
               `viewport ${Math.round(view.top)}..${Math.round(view.bottom)}`,
        }
      }, DIFF_SCROLLER)

      expect(visible.ok, `the focused hunk should be on screen — ${visible.why}`).toBe(true)
    })
  })
})
