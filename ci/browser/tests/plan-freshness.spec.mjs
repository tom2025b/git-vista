// M12.05 (#555) — a plan the repository has moved past must SAY so, and must
// stop offering the button.
//
// Why this spec exists, and why nothing else can close this criterion: the
// decision is `features/freshness/core.rs`, host-tested to fourteen tests, and
// every one of them would still pass with the notice deleted from the dialog.
// The three consumers — `features/freshness/signals.rs`,
// `features/preview/signals.rs` and `dialogs/confirm.rs` — are all
// `#[cfg(target_arch = "wasm32")]`, which `cargo test` never compiles. That is
// the exact shape of every defect in this suite's README table.
//
// A source census in `features::freshness::core::suite` pins those three files
// to the functions they must call. This spec proves the whole path actually
// runs in a browser: the SSE stream connects, the sweep on the server notices a
// change nothing in the app made, and the panel changes what it says and what
// it offers.
//
// The two arms asserted, because they are the two the spec's D4 separates:
//
//   MovedElsewhere  a ref moved that this plan does not name. Still refused —
//                   `enforce_fresh` compares the whole digest — and said
//                   differently, because "the branch you are about to
//                   force-push moved" and "somebody's tag landed" feel nothing
//                   alike and conflating them trains people to dismiss the
//                   notice.
//   Moved           a ref this plan names moved. Named, loudly.
//
// Nothing here ever confirms, and every external change is undone before the
// test ends — the fixture is shared with `preview-panel.spec.mjs`, which
// requires it to stay pre-merge.

import { execFileSync } from 'node:child_process'

import { expect, test } from '@playwright/test'

import {
  openBranchMenu,
  openMergePreviewRepo,
  PREVIEW_BRANCH,
  PREVIEW_HEADING,
  PREVIEW_INTO,
  runtime,
} from './helpers.mjs'

/** The staleness notice, by the class `dialogs/preview_panel.rs` gives it. */
const STALE = '.plan-stale'

/** Run git in the merge-preview fixture — the *external* change the app never
 *  made, which is the only kind this feature is about. */
function git(args) {
  const { mergePreviewFixture } = runtime()
  return execFileSync('git', ['-C', mergePreviewFixture.root, ...args], {
    encoding: 'utf8',
  }).trim()
}

/** The confirm modal's own confirm button. */
function confirmButton(page) {
  return page.getByRole('button', { name: /^Merge$/ })
}

async function openMergeConfirmation(page) {
  await openMergePreviewRepo(page)
  const item = await openBranchMenu(page, PREVIEW_BRANCH)
  await item.click()
  await expect(
    page.getByText(`Merge ‘${PREVIEW_BRANCH}’ into ‘${PREVIEW_INTO}’?`),
  ).toBeVisible()
  // Wait for the plan to land: until it does there is no generation on screen
  // and the feature correctly says nothing at all.
  await expect(page.getByText(PREVIEW_HEADING)).toBeVisible({ timeout: 20_000 })
}

test.describe('#555 a plan the repository moved past', () => {
  test('says nothing at all while the plan is still current', async ({ page }) => {
    await openMergeConfirmation(page)
    // The quiet case is half the feature. A notice that appears on an
    // untouched repository is a notice nobody reads — and it would also be
    // this feature failing open in the direction that costs nothing to detect.
    await expect(page.locator(STALE)).toHaveCount(0)
    await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'false')
    await page.keyboard.press('Escape')
  })

  test('a ref the plan does not name is said gently and still withdraws the button', async ({
    page,
  }) => {
    await openMergeConfirmation(page)
    await expect(page.locator(STALE)).toHaveCount(0)

    // Somebody else's tag lands. Nothing this merge depends on moved.
    git(['tag', 'landed-from-outside'])
    try {
      const notice = page.locator(STALE)
      await expect(notice).toBeVisible({ timeout: 20_000 })
      await expect(notice).toContainText('not in a way this operation depends on')
      // Still refused: the execution gate compares the WHOLE digest, so leaving
      // this button live would be offering a button whose purpose is to fail.
      await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'true')
    } finally {
      git(['tag', '-d', 'landed-from-outside'])
      await page.keyboard.press('Escape')
    }
  })

  test('a ref the plan names is named back, loudly', async ({ page }) => {
    const before = git(['rev-parse', PREVIEW_INTO])
    await openMergeConfirmation(page)
    await expect(page.locator(STALE)).toHaveCount(0)

    // The branch this merge writes to moves under the plan. This is the case
    // where what the picture shows and what the operation would do have come
    // apart.
    git(['commit', '--allow-empty', '-qm', 'landed on main from outside'])
    try {
      const notice = page.locator(STALE)
      await expect(notice).toBeVisible({ timeout: 20_000 })
      await expect(notice).toContainText(`refs/heads/${PREVIEW_INTO}`)
      await expect(notice).toContainText('moved while this was on screen')
      await expect(notice).toContainText('Rebuild it and review it again')
      await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'true')
    } finally {
      git(['reset', '--hard', before])
      await page.keyboard.press('Escape')
    }
  })
})
