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

/**
 * The confirm modal's own confirm button, located by position rather than by
 * name.
 *
 * Deliberately not `getByRole('button', { name: 'Merge' })`: a withdrawn
 * confirmation folds its reason into `aria-label`, so the accessible name is
 * the *reason* rather than the label — which is the behaviour #65 asked for,
 * and which would make a name-based locator silently stop finding the button
 * in exactly the state this spec is about.
 */
function confirmButton(page) {
  // The LAST button in the action row. Cancel is first, Rebuild appears
  // between them when the plan is stale, and the confirmation is always last —
  // so `.last()` rather than "the sibling after Cancel", which stopped being
  // unambiguous the moment Rebuild existed.
  return page
    .getByRole('button', { name: 'Cancel' })
    .locator('xpath=following-sibling::button')
    .last()
}

/**
 * Open the merge confirmation with the change feed settled on **this**
 * repository.
 *
 * The wait is the part that matters, and it is not decoration. The app opens
 * its feed at start-up, against the launch selection; opening a different
 * repository makes the server close that stream and the client reconnect onto
 * the new one. Until that second stream has published, the client has no
 * snapshot at the plan's generation to difference against — and every verdict
 * correctly falls to the arm that claims least. Racing it would make this spec
 * assert the fallback and call it the feature.
 */
async function openMergeConfirmation(page) {
  await openMergePreviewRepo(page)
  const item = await openBranchMenu(page, PREVIEW_BRANCH)
  await item.click()
  await expect(
    page.getByText(`Merge ‘${PREVIEW_BRANCH}’ into ‘${PREVIEW_INTO}’?`),
  ).toBeVisible()
  // The plan has landed *and* its picture is drawn — a positive signal, so the
  // absence asserted next cannot be the absence of a dialog.
  await expect(page.getByText(PREVIEW_HEADING)).toBeVisible({ timeout: 20_000 })
  await expect(page.getByRole('img', { name: /^After:/ })).toBeVisible({ timeout: 30_000 })

  // Now wait for the notice to be **absent**, and that wait is the whole
  // synchronisation this spec needs.
  //
  // Absence is a positive signal here rather than a hopeful one, and that is
  // worth stating because a spec that waits on an absence is usually wrong.
  // Every unsettled state this feature can be in renders a notice: no feed yet
  // says "couldn't tell", and a feed that has not published a reading at this
  // plan's generation — which is what the app's start-up stream on the *launch*
  // repository looks like until it reconnects onto this one — says "the
  // repository changed". Only a client holding a reading at the plan's own
  // generation renders nothing at all.
  await expect(page.locator(STALE)).toHaveCount(0, { timeout: 20_000 })
  await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'false')
}

test.describe('#555 a plan the repository moved past', () => {
  test('says nothing while the plan is current, and stops saying it when the change is undone', async ({
    page,
  }) => {
    // The quiet case is half the feature — and asserting it FIRST would be
    // vacuous, because "the notice has not appeared yet" and "the notice will
    // never appear" look identical to a fresh page. So the quiet state is
    // proven by *returning* to it: make the repository move, wait for the
    // notice, put it back, and require the notice to go away again.
    await openMergeConfirmation(page)
    const notice = page.locator(STALE)

    git(['tag', 'briefly-there'])
    await expect(notice).toBeVisible({ timeout: 20_000 })
    await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'true')

    // Back to exactly the generation the plan was built against. This is a
    // repository that moved and moved back, and `enforce_fresh` would admit
    // the plan — so the panel must not be more pessimistic than the gate.
    git(['tag', '-d', 'briefly-there'])
    await expect(notice).toHaveCount(0, { timeout: 20_000 })
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

  test('the Rebuild it offers actually exists and produces a plan to approve again', async ({
    page,
  }) => {
    // #664 review, finding 6. Spec D4 requires Rebuild and Discard on a stale
    // plan; the first slice shipped only the SENTENCE telling the user to
    // rebuild. The tests above assert the wording and the disabled button, and
    // passed straight over the missing action — which is why this one asserts
    // the control, and asserts that using it gets the user somewhere.
    await openMergeConfirmation(page)
    const rebuild = page.getByRole('button', { name: 'Rebuild' })
    await expect(rebuild).toHaveCount(0, { timeout: 5_000 })

    git(['tag', 'stale-me'])
    try {
      await expect(page.locator(STALE)).toBeVisible({ timeout: 20_000 })
      await expect(rebuild).toBeVisible()
      await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'true')

      // Rebuilding replaces the plan with one built against the repository as
      // it is now — so the notice clears and the operation becomes offerable
      // again. It never runs anything: the user still has to confirm.
      await rebuild.click()
      await expect(page.getByRole('img', { name: /^After:/ })).toBeVisible({ timeout: 30_000 })
      await expect(page.locator(STALE)).toHaveCount(0, { timeout: 20_000 })
      await expect(confirmButton(page)).toHaveAttribute('aria-disabled', 'false')
      // And the dialog is still open, still asking. A rebuild that executed,
      // or that dismissed itself, would have destroyed the approval boundary
      // it exists to keep.
      await expect(
        page.getByText(`Merge ‘${PREVIEW_BRANCH}’ into ‘${PREVIEW_INTO}’?`),
      ).toBeVisible()
    } finally {
      git(['tag', '-d', 'stale-me'])
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
