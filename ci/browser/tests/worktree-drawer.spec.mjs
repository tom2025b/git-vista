// M11.03 (#548) — the worktree drawer must be REACHED, git's facts and this
// app's verdict must be visibly different things, a refusal must be readable,
// and switching desks must actually switch.
//
// Why this spec exists at all: `features/worktrees/core.rs` is host-tested and
// its tests prove every decision the drawer makes is correct. They cannot prove
// any of it is reached. The values are consumed by
// `features/worktrees/view.rs` and `activity.rs`, both
// `#[cfg(target_arch = "wasm32")]`, which `cargo test` never compiles — the
// exact shape of every defect in this suite's README table (#68d and #69c each
// had a fully-tested core with ZERO consumers, beside a green gate).
//
// The core suite's source census reads `view.rs` back and pins the mappings.
// That is strictly weaker than this file: it proves the source says the right
// thing, never that a browser renders it.
//
// The assertions, in the order the acceptance criteria matter:
//
//   A1  every desk is listed, INCLUDING the two this app refuses to open.
//       Hiding a refused sibling is the option the spec weighs and rejects.
//   A2  git's `locked` and this app's "can open" appear on the SAME row, in
//       visibly different pills. One "unusable" badge covering both is named
//       in the issue as a failure of this criterion.
//   A3  a refused desk's reason is VISIBLE TEXT — not a `title=`, not only an
//       `aria-label`. #65's finding: a tooltip-only reason never surfaces on a
//       tap and is never announced.
//   A4  switching to a serviceable desk works END TO END, including one that
//       was never registered in the catalog — the gap #651's body named and
//       left open. This is the load-bearing one: it is the whole reason
//       `/api/select-worktree` exists.
//   A5  the 44x44 touch floor (#65) on the one control.
//
// SERIAL, and it must stay serial: A4 switches the served repository, which is
// process-global state. Every assertion that depends on being in the main
// worktree therefore runs before it.

import { expect, test } from '@playwright/test'

import {
  OUTSIDE_ROOTS_SENTENCE,
  openWorktreeDrawer,
  openWorktreeRepo,
} from './helpers.mjs'

test.describe.serial('the worktree drawer', () => {
  test('lists every desk, including the ones this app refuses to open', async ({ page }) => {
    await openWorktreeRepo(page)
    const drawer = await openWorktreeDrawer(page)

    // A1. The two serviceable desks…
    await expect(drawer.getByText('desk-two', { exact: true })).toBeVisible()
    await expect(drawer.getByText('locked-desk', { exact: true })).toBeVisible()
    // …and the two refused ones, which is the half a "hide what we can't open"
    // implementation would silently drop.
    await expect(drawer.getByText('worktree-outside-desk', { exact: true })).toBeVisible()
    await expect(drawer.getByText('ghost-desk', { exact: true })).toBeVisible()

    // The branch each desk holds — the sentence M11.02's refusal points at.
    await expect(drawer.getByText('feature/desk-two', { exact: true })).toBeVisible()

    // And the row for the worktree we are actually in says so, rather than
    // offering to switch to where we already are.
    await expect(drawer.getByText('you are here', { exact: true })).toBeVisible()
  })

  test("git's flags and this app's verdict are two visibly different things", async ({ page }) => {
    await openWorktreeRepo(page)
    const drawer = await openWorktreeDrawer(page)

    // A2. The locked desk is the load-bearing row: git has flagged it, and
    // this app can still open it. A single "unusable" badge would have made
    // it unopenable for a reason nobody holds.
    const lockedRow = drawer.locator('.act-file').filter({ hasText: 'locked-desk' })
    await expect(lockedRow).toHaveCount(1)

    const gitPill = lockedRow.locator('.act-pill.act-terminal', { hasText: 'locked' })
    const appPill = lockedRow.locator('.act-pill.act-app', { hasText: 'can open' })
    await expect(gitPill).toBeVisible()
    await expect(appPill).toBeVisible()

    // Different classes is the structural half; different rendered colour is
    // the half a user actually perceives, so assert the colour too. Two pills
    // that resolve to the same colour would satisfy the class check and fail
    // the criterion.
    const gitColour = await gitPill.evaluate((el) => getComputedStyle(el).color)
    const appColour = await appPill.evaluate((el) => getComputedStyle(el).color)
    expect(gitColour).not.toBe(appColour)

    // And this row is still openable — the whole point.
    await expect(lockedRow.getByRole('button', { name: /Open ‘locked-desk’/ })).toBeVisible()
  })

  test('a refused desk states its reason as text a reader and a screen reader both get', async ({
    page,
  }) => {
    await openWorktreeRepo(page)
    const drawer = await openWorktreeDrawer(page)

    // A3. The fence sentence, visible in the document — not a tooltip.
    const outsideRow = drawer.locator('.act-file').filter({ hasText: 'worktree-outside-desk' })
    await expect(outsideRow).toHaveCount(1)
    await expect(outsideRow.getByText(OUTSIDE_ROOTS_SENTENCE)).toBeVisible()

    // Nothing is offered for it — a refusal is stated, not a greyed-out button
    // the user can wonder about.
    await expect(outsideRow.getByRole('button')).toHaveCount(0)

    // The missing desk is a DIFFERENT refusal with a different remedy. One
    // sentence covering both would read as one state, which it is not.
    const ghostRow = drawer.locator('.act-file').filter({ hasText: 'ghost-desk' })
    await expect(ghostRow.getByText(/git worktree prune/)).toBeVisible()
    // git's own flag is on that row too, separately from the app's verdict.
    await expect(ghostRow.locator('.act-pill.act-terminal', { hasText: 'prunable' })).toBeVisible()
    await expect(ghostRow.locator('.act-pill.act-app', { hasText: 'folder is gone' })).toBeVisible()
  })

  test("the one control clears #65's 44x44 touch floor", async ({ page }) => {
    await openWorktreeRepo(page)
    const drawer = await openWorktreeDrawer(page)

    // A5. Measured, not assumed from the class name.
    const open = drawer.getByRole('button', { name: /Open ‘desk-two’/ })
    const box = await open.boundingBox()
    expect(box.width).toBeGreaterThanOrEqual(44)
    expect(box.height).toBeGreaterThanOrEqual(44)
  })

  // MUST BE LAST: this switches the served repository, which is process-global.
  test('switching to a desk the catalog never held actually switches', async ({ page }) => {
    await openWorktreeRepo(page)
    const drawer = await openWorktreeDrawer(page)

    // A4. `desk-two` lives inside the repository's own allowed root, and was
    // never passed to the server as a repo — so the catalog does not hold it.
    // Before #548 this button answered `404 No such repository.`, which is the
    // gap #651's body named and deferred to here.
    await drawer.getByRole('button', { name: /Open ‘desk-two’/ }).click()

    // The drawer follows: the desk we switched to now says "you are here", and
    // it is no longer offered as somewhere to go.
    const openedRow = drawer.locator('.act-file').filter({ hasText: 'desk-two' })
    await expect(openedRow.getByText('you are here', { exact: true })).toBeVisible({
      timeout: 20_000,
    })
    await expect(openedRow.getByRole('button', { name: /Open ‘desk-two’/ })).toHaveCount(0)

    // And the rest of the app followed too, which is what "switches the app"
    // means: the graph's own header names the desk we switched to and the
    // branch that desk holds, not the ones the repository's main worktree has.
    //
    // Scoped to `.repo-branch` rather than a text match, and not for tidiness:
    // `feature/desk-two` legitimately appears FIVE times on this page once the
    // switch lands — the header, an SVG `<title>`, a stub label, and two cells
    // in the drawer itself — so a text locator resolves to all five and
    // Playwright refuses to guess. That is the same hazard `openDrawer`'s
    // comment records for the stash spec, and it is worth the extra
    // specificity here for a second reason: the header is *the* element that
    // must follow a switch, so naming it asserts more than "the string is
    // somewhere on the page" ever could.
    const graph = page.getByRole('region', { name: 'Commit history graph' })
    await expect(graph).toBeVisible()
    await expect(graph.locator('.repo-branch')).toHaveText(/feature\/desk-two/, {
      timeout: 20_000,
    })
    await expect(graph.getByText('desk-two', { exact: false }).first()).toBeVisible()
  })
})
