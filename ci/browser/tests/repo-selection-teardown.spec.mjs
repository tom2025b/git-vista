// #676 — repository selection tears down an open confirm dialog.
//
// A confirmation is about an operation in the repository the user is about
// to leave. `picker.rs`'s mode choice, `dialogs/confirm.rs`'s "open that
// worktree instead" path, and `dialogs/open_url.rs`'s clone flow each bump
// the graph epoch after a selection completes; this spec drives the first
// of those three end to end and asserts the confirm dialog the user left
// open is gone afterward, not merely that the epoch moved.
//
// The confirm backdrop is `position:fixed; z-index:30`, full viewport, and
// closes on any click landing on it (`confirm.rs`) — a real mouse click
// aimed at the topbar's "Repos" button lands on that backdrop first and
// closes the confirmation as a side effect, before the picker ever opens
// (traced in `~/projects/git-vista-reports/fable-2026-09-05-0845.md` §1d,
// "same tab, by mouse"). That path never reaches #676's fix at all. A
// keyboard user tabbing to "Repos" and pressing Enter does not go through
// hit-testing, so this spec drives it that way — `.focus()` + `Enter`,
// never `.click()` — to actually exercise the code path #676 changed rather
// than the accidental backdrop-close that already covers the mouse path.

import { expect, test } from '@playwright/test'

import { openMergePreviewRepo, PREVIEW_BRANCH, PREVIEW_INTO } from './helpers.mjs'

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
    const reposButton = page.getByRole('button', { name: 'Repos' })
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
})
