// #676 — repository selection tears down an open confirm dialog.
//
// A confirmation is about an operation in the repository the user is about
// to leave. `picker.rs`'s mode choice, `dialogs/confirm.rs`'s "open that
// worktree instead" path, `dialogs/open_url.rs`'s clone flow, and
// `features/worktrees/view.rs`'s drawer `open_button` (the fourth site,
// found by grok's review of PR #677 — the drawer is the OTHER
// `/api/select-worktree` caller fable's item C did not name) each bump the
// graph epoch after a selection completes. This spec drives two of those
// four end to end and asserts the confirm dialog the user left open is gone
// afterward, not merely that the epoch moved.
//
// PICKER TEST: the confirm backdrop is `position:fixed; z-index:30`, full
// viewport, and closes on any click landing on it (`confirm.rs`) — a real
// mouse click aimed at the topbar's "Repos" button lands on that backdrop
// first and closes the confirmation as a side effect, before the picker
// ever opens (traced in
// `~/projects/git-vista-reports/fable-2026-09-05-0845.md` §1d, "same tab, by
// mouse"). That path never reaches #676's fix at all. A keyboard user
// tabbing to "Repos" and pressing Enter does not go through hit-testing, so
// this spec drives it that way — `.focus()` + `Enter`, never `.click()` —
// to actually exercise the code path #676 changed rather than the
// accidental backdrop-close that already covers the mouse path.
//
// DRAWER TEST: the drawer's "Open" button sits inside the Activity panel
// (`Overlay::Activity`), a DIFFERENT dock from Confirm (`Overlay::Confirm`)
// — the two coexist in the DOM rather than one evicting the other, confirmed
// by probing before writing this assertion (both report `isVisible()`).
// But visible is not clickable: the confirm backdrop sits at a higher
// z-index (grok's review of PR #677 measured it, activity ~15-21 under
// confirm's 30) and a real `.click()` on "Open 'desk-two'" is intercepted
// by that backdrop — the same "<div> intercepts pointer events" shape #623
// was named for, and the identical reason the picker test above cannot use
// `.click()` on "Repos" either. So this test drives it the same way:
// `.focus()` + `Enter`, never `.click()`.
//
// `confirm.rs`'s own "open that worktree instead" path and `open_url.rs`'s
// clone flow are covered ONLY by `repo_selection_teardown_census.rs`'s text
// match, not by a browser run — stated here plainly rather than left
// implied: that census proves the call is present in the right place, not
// that a browser actually executes it. If either call is ever wrapped in
// dead code the way `crates/git-vista/src/picker.rs`'s mutation-proof arm 2
// mutated the picker call, the census alone would not catch it.

import { expect, test } from '@playwright/test'

import {
  openMergePreviewRepo,
  openWorktreeDrawer,
  openWorktreeRepo,
  PREVIEW_BRANCH,
  PREVIEW_INTO,
} from './helpers.mjs'

test.describe('#676 — selecting a repository closes an open confirmation', () => {
  test('re-selecting the same repository through the picker closes the merge confirm', async ({
    page,
  }) => {
    await openMergePreviewRepo(page)

    // Open the merge confirmation and leave it open.
    const nodes = page.locator('circle.node-hit')
    const count = await nodes.count()
    let mergeItem = null
    for (let i = 0; i < count; i++) {
      await nodes.nth(i).click()
      const item = page.getByRole('button', { name: new RegExp(`Merge ‘${PREVIEW_BRANCH}’`) })
      if (await item.isVisible().catch(() => false)) {
        mergeItem = item
        break
      }
      await page.keyboard.press('Escape')
    }
    expect(mergeItem, `no commit offered a merge item for ${PREVIEW_BRANCH}`).not.toBeNull()
    await mergeItem.click()
    const confirmText = page.getByText(`Merge ‘${PREVIEW_BRANCH}’ into ‘${PREVIEW_INTO}’?`)
    await expect(confirmText, 'the merge confirmation must be open before this spec means anything').toBeVisible()

    // A keyboard user reaches "Repos" without hitting the backdrop a mouse
    // click would land on first — `.focus()` skips Playwright's own
    // actionability/hit-test check, matching that.
    const reposButton = page.getByRole('button', { name: 'Repos', exact: true })
    await reposButton.focus()
    await page.keyboard.press('Enter')

    // The picker follows — re-select the same repository, same mode, which
    // is picker.rs's `mode_view` choice handler: the exact call site #676
    // changed.
    const entry = page.getByRole('button', { name: /merge-preview-repo/i }).first()
    await expect(entry, 'the picker must actually have opened').toBeVisible({ timeout: 20_000 })
    await entry.click()
    const full = page.getByRole('button', { name: /full git operations/ })
    await expect(full).toBeVisible({ timeout: 20_000 })
    await full.click()

    // The repository re-selection has landed (a fresh Ready render for the
    // same repo), and the confirmation from before it is gone — not merely
    // unattended, gone — which is #676's whole claim.
    await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
    await expect(confirmText, 'the stale merge confirmation must not survive a repository re-selection').toHaveCount(
      0,
    )
  })

  test('opening a desk through the worktree drawer closes an open confirm from the same drawer', async ({
    page,
  }) => {
    await openWorktreeRepo(page)
    const drawer = await openWorktreeDrawer(page)

    // Open the two-tap "Close Worktree" confirmation for a desk this test
    // never actually closes — it only needs the confirmation OPEN, sharing
    // the same `Shell::confirm_op` the picker test's merge confirmation
    // does.
    const removableRow = drawer.locator('.act-file').filter({ hasText: 'removable-desk' })
    await removableRow.getByRole('button', { name: /Close ‘removable-desk’/ }).click()
    const confirmText = page.getByText('Close the worktree ‘removable-desk’?')
    await expect(confirmText, 'the remove-worktree confirmation must be open before this spec means anything').toBeVisible()

    // The drawer and the confirm coexist in the DOM, but the confirm's
    // backdrop sits above the drawer and intercepts a real click — so this
    // reaches "Open 'desk-two'" the same way the picker test above reaches
    // "Repos": keyboard, never `.click()`.
    const openDeskTwo = drawer.getByRole('button', { name: /Open ‘desk-two’/ })
    await openDeskTwo.focus()
    await page.keyboard.press('Enter')

    // The switch landed, and the stale removal confirmation — about a
    // worktree that has nothing to do with the desk just opened — is gone.
    await expect(page.getByText('you are here', { exact: true })).toBeVisible({ timeout: 20_000 })
    await expect(
      confirmText,
      'a confirmation about the desk just left must not survive opening a different desk',
    ).toHaveCount(0)
  })
})
