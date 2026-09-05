// #664 review round 3 — a held rebuild-lease reply must not act on a
// confirmation the user already discarded.
//
// `dialogs/confirm.rs`'s `rebuild_lease` drives the force-with-lease Push
// confirmation's Rebuild button through its OWN two-request path (a lease
// plan does not come from `features::preview`'s normal fetch, so it does not
// get that path's generation guard for free — see that function's own doc
// comment). Before this round, neither `note_rebuild_failed` nor
// `note_rebuild_landed` checked whether the rebuild they were completing was
// still the live one, so a reply released after Cancel unconditionally wrote
// state and unconditionally re-opened the confirmation the user had just
// closed.
//
// This is the same class of gap `plan-freshness.spec.mjs`'s "a rebuild held
// in flight" tests close for the MERGE confirmation's `preview.rebuild` path
// — that path already had a generation guard (`Preview::fetch`'s own), which
// is exactly why those tests never caught this: `rebuild_lease` is a
// different function with a different, until-now-unguarded, completion.
//
// Both held-response shapes the round asked for: a held reply that would
// have SUCCEEDED, and one that would have FAILED. Neither should touch
// anything once the confirmation that started it is gone.

import { execFileSync } from 'node:child_process'

import { expect, test } from '@playwright/test'

import { openBranchMenu, openMergePreviewRepo, PREVIEW_BRANCH, runtime } from './helpers.mjs'

/** Run git in the merge-preview fixture — shared with `plan-freshness.spec.mjs`
 *  and `preview-panel.spec.mjs`, so every change here is undone before the
 *  test ends. */
function git(args) {
  const { mergePreviewFixture } = runtime()
  return execFileSync('git', ['-C', mergePreviewFixture.root, ...args], {
    encoding: 'utf8',
  }).trim()
}

/**
 * Open the force-with-lease Push confirmation for `PREVIEW_BRANCH`, with a
 * remote-tracking ref in place so a lease can be computed without a real
 * network — the same scratch setup `rebuild_lease` itself needs to run at
 * all.
 *
 * A freshly opened lease confirmation is never stale on its own — nothing
 * has moved yet, so `Rebuild` is not offered (D4: it appears only when the
 * plan on screen is no longer current). `tag` names an EXTRA ref this test
 * creates purely to trigger that: the server's sweep notices a repository
 * change nothing in the app made and the freshness verdict treats it as
 * enough to offer a rebuild, the same "a ref the plan does not name moving"
 * shape `plan-freshness.spec.mjs` already covers for the merge path.
 */
async function openForcePushConfirmation(page, tag) {
  // The expanded lease confirmation (every disclosure section open) is
  // taller than the default viewport, and `Rebuild` sits below the fold —
  // a real click needs it actually in view, not just present in the DOM.
  await page.setViewportSize({ width: 1280, height: 2400 })
  await openMergePreviewRepo(page)
  git(['remote', 'add', 'origin', runtime().mergePreviewFixture.root])
  git(['update-ref', `refs/remotes/origin/${PREVIEW_BRANCH}`, 'HEAD'])
  await openBranchMenu(page, PREVIEW_BRANCH)
  await page
    .getByRole('button', { name: `Force Push ‘${PREVIEW_BRANCH}’…`, exact: false })
    .click()
  await expect(page.getByText('What this plan says')).toBeVisible()
  git(['tag', tag])
  await expect(page.getByRole('button', { name: 'Rebuild', exact: true })).toBeVisible({
    timeout: 20_000,
  })
}

async function cleanupForcePushConfirmation(page, tag) {
  await page.unroute('**/api/plan').catch(() => {})
  git(['tag', '-d', tag])
  git(['remote', 'remove', 'origin'])
  await page.keyboard.press('Escape')
}

test.describe('#664 review round 3 — a canceled rebuild-lease reply does nothing', () => {
  test('a held SUCCESS reply does not re-open a canceled confirmation', async ({ page }) => {
    const tag = 'review-r3-success'
    await openForcePushConfirmation(page, tag)

    let releasePlan
    const planGate = new Promise((resolve) => {
      releasePlan = resolve
    })
    let arrived
    const firstRequestArrived = new Promise((resolve) => {
      arrived = resolve
    })
    let calls = 0
    await page.route('**/api/plan', async (route) => {
      calls++
      if (calls === 1) {
        arrived()
        await planGate
      }
      await route.continue()
    })

    try {
      await page.getByRole('button', { name: 'Rebuild', exact: true }).click()
      await firstRequestArrived
      await expect(page.getByText('Building a new plan', { exact: false })).toBeVisible()

      // Discard the confirmation while both of `rebuild_lease`'s requests are
      // still held — this is the window the fix closes.
      await page.getByRole('button', { name: 'Cancel', exact: true }).click()
      await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0)

      const secondRequestFinished = page.waitForResponse(
        (r) => r.url().endsWith('/api/plan') && calls === 2,
      )
      releasePlan()
      await secondRequestFinished
      // Give the wasm continuation a moment to run and (before the fix) call
      // `shell.open_confirm` — there is no UI signal to wait ON for "nothing
      // happened", so this is an explicit settle rather than a race.
      await page.waitForTimeout(300)

      await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0)
      await expect(page.getByText('What this plan says')).toHaveCount(0)
    } finally {
      releasePlan()
      await cleanupForcePushConfirmation(page, tag)
    }
  })

  test('a held FAILURE reply does not write over a canceled confirmation', async ({ page }) => {
    const tag = 'review-r3-failure'
    await openForcePushConfirmation(page, tag)

    let failPlan
    const planGate = new Promise((resolve) => {
      failPlan = resolve
    })
    let arrived
    const firstRequestArrived = new Promise((resolve) => {
      arrived = resolve
    })
    let calls = 0
    await page.route('**/api/plan', async (route) => {
      calls++
      if (calls === 1) {
        arrived()
        await planGate
        await route.abort('failed')
      } else {
        await route.continue()
      }
    })

    try {
      await page.getByRole('button', { name: 'Rebuild', exact: true }).click()
      await firstRequestArrived
      await expect(page.getByText('Building a new plan', { exact: false })).toBeVisible()

      await page.getByRole('button', { name: 'Cancel', exact: true }).click()
      await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0)

      failPlan()
      await page.waitForTimeout(300)

      // What this asserts, precisely: a held failure reply, released after
      // Cancel, produces no visible artifact on a dialog that is already
      // closed — no crash, no orphaned notice, nothing reopens. It does NOT
      // independently prove `note_rebuild_failed`'s internal token guard —
      // with the dialog already closed, nothing renders `Preview`'s plan
      // slot for either write to appear in, so a manual check (temporarily
      // removing the guard, see the PR) found this pair of assertions still
      // green either way. That guard's actual proof is the host-level
      // mutation-proof on `rebuild_token_is_current` in `preview/core.rs`,
      // which both `note_rebuild_failed` and `note_rebuild_landed` call —
      // this browser test's job is the end-to-end "nothing visibly breaks",
      // not a second proof of the same comparison.
      await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0)
      await expect(page.getByText("Couldn't build a new plan", { exact: false })).toHaveCount(0)
    } finally {
      failPlan()
      await cleanupForcePushConfirmation(page, tag)
    }
  })
})
