// #596: cherry-pick had no door. The server has run `GitOperation::CherryPick`
// since #576 — executor, planner funnel and graph preview all present — and
// nothing in the app could ask for it. Tom found it by right-clicking a commit
// and reading a ~20-item menu that did not offer the one thing he wanted.
//
// Why this spec is required rather than optional. The host tests in
// `features/dialogs/core.rs` prove `cherry_pick_offer` returns the right answer
// and `cherry_pick_confirm_prompt` writes honest copy — but `cargo test` never
// executes `crates/git-vista/src/menu/`, which is wasm-gated. So a perfectly
// green suite is compatible with `build_commit_items` returning a fourth item
// that `menu.rs` forgets to render: the exact absence #596 is about would ship
// again, with 1,100 tests passing over it. A test suite tests what exists, and
// only the browser can see whether this item exists.

import { expect, test } from '@playwright/test'

import { forceOnline, runtime } from './helpers.mjs'

const ITEM = 'Cherry-pick this commit'

// Active, not Visualize. `menu.rs` gates the whole write set behind
// `!read_only && online`, so in the read-only mode `openApp` selects, this item
// is legitimately absent — a spec written against it would fail for a reason
// that has nothing to do with #596.
async function openActiveApp(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const pickerEntry = page.getByRole('button', { name: /fixture-repo/i }).first()
  if (await pickerEntry.isVisible().catch(() => false)) {
    await pickerEntry.click()
  }
  const active = page.getByRole('button', { name: /full git operations/ })
  if (await active.isVisible().catch(() => false)) {
    await active.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
}

// Focus + Enter, not a click, for the reason `wip-collapse.spec.mjs` documents:
// the canvas starts with its top rows underneath the fixed topbar, so a
// coordinate click up there is intercepted by the header. Enter on a focused
// row opens the identical menu through the roving-tabindex path (#65).
async function openMenuOnRow(page, index) {
  await page.locator(`.node-hit[data-row-index="${index}"]`).focus()
  await page.keyboard.press('Enter')
  await expect(page.locator('.ctx-menu')).toBeVisible()
}

test.describe('#596 cherry-pick reaches the commit menu', () => {
  // Row 0 is the HEAD tip; row 1 is an ordinary commit behind it (the folded
  // WIP run sits at row 3 — see wip-collapse.spec.mjs for the fixture's shape).
  test('an ordinary commit offers the pick as a live item', async ({ page }) => {
    await openActiveApp(page)
    await openMenuOnRow(page, 1)

    const item = page.getByRole('button', { name: ITEM })
    await expect(item).toBeVisible()
    // Enabled, not merely present: the disabled variant renders as the same
    // label with `aria-disabled`, so a presence-only assertion would pass on a
    // menu where every commit refuses the operation.
    await expect(item).not.toHaveAttribute('aria-disabled', 'true')
  })

  // The other half of #65's rule, and the half that made this issue possible:
  // a blocked operation must SAY SO, on screen, rather than vanish. A menu that
  // silently omits the item teaches the reader that the app cannot cherry-pick
  // at all — which is precisely the wrong lesson, and precisely what happened.
  test('HEAD offers it disabled, with the reason visible on screen', async ({ page }) => {
    await openActiveApp(page)
    await openMenuOnRow(page, 0)

    const item = page.getByRole('button', { name: new RegExp(ITEM) })
    await expect(item).toBeVisible()
    await expect(item).toHaveAttribute('aria-disabled', 'true')
    // Visible text, not only a `title=` — a tooltip never surfaces on a tap
    // and is never announced.
    await expect(item).toContainText('tip of the current branch')
  })

  // The round trip #596 is actually asking for: the item must route through
  // `shell.open_confirm`, because that is what puts a cherry-pick in front of
  // the shared confirm dialog — and, with #594, in front of its `/api/preview`
  // panel. An item that opened its own bespoke dialog would pass both tests
  // above and still miss the point.
  test('choosing it opens the shared confirm dialog with honest copy', async ({ page }) => {
    await openActiveApp(page)
    await openMenuOnRow(page, 1)
    await page.getByRole('button', { name: ITEM }).click()

    // The confirm modal carries no class and no `role="dialog"` — it is styled
    // inline (`dialogs/confirm.rs`) — so it is identified by its own controls
    // rather than by a container selector. `exact` matters: without it,
    // "Cherry-pick" also matches the menu item that opened this.
    const confirm = page.getByRole('button', { name: 'Cherry-pick', exact: true })
    const cancel = page.getByRole('button', { name: 'Cancel' })
    await expect(confirm).toBeVisible()
    await expect(cancel).toBeVisible()

    // The measured failure mode, stated to the user before they commit to it:
    // git passes no `--allow-empty` here, so a conflict AND an already-applied
    // change both exit 1 and strand the repository mid-sequence.
    await expect(page.getByText(/CHERRY_PICK_HEAD/)).toBeVisible()
    await expect(page.getByText(/--abort/)).toBeVisible()
    // The destination is the live HEAD branch, not the tapped row.
    await expect(page.getByText(/onto ‘main’/)).toBeVisible()

    // Leave the fixture as it was found. This spec proves the door opens; it
    // deliberately does not walk through it, so nothing here depends on the
    // fixture's HEAD staying put for the specs that run after it.
    await cancel.click()
    await expect(confirm).toBeHidden()
  })
})
