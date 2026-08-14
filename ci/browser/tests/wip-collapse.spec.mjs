// #374: runs of consecutive `wip(#N): auto-checkpoint` commits fold into one
// expandable marker in the graph view. `cargo test` never executes
// `crates/git-vista/src` (wasm-gated) — the host tests in `collapse.rs`
// prove the algorithm, but nothing about whether `canvas.rs`'s wiring,
// `GraphFocus`'s re-anchoring, or the tap handler actually work. With this
// feature defaulting ON, an unverified wiring bug ships to every session on
// first load, which is why this spec is required, not optional (see the
// feature's design doc, "Testing").

import { expect, test } from '@playwright/test'

import { WIP_RUN_COUNT } from '../fixture.mjs'
import { openApp } from './helpers.mjs'

// The fixture seeds 4 real commits plus one run of WIP_RUN_COUNT checkpoint
// commits between commit 1 and commit 2 (fixture.mjs). Folded, the graph
// therefore renders: commit4(HEAD), commit3, commit2, one WipGroup, commit1
// = 4 real rows + 1 group row.
const EXPECTED_DISPLAY_ROWS = 5
// Expanded, the group's slot is replaced by its members: 4 real + WIP_RUN_COUNT.
// Asserted exactly, not as "more than before" — a loose assertion passes both
// when the run opens whole AND when only its head opens and the tail re-folds
// into a smaller group, which is precisely the defect this spec exists to
// catch. (It is also why WIP_RUN_COUNT must stay >= 3: at 2, an
// only-the-head-opened projection leaves a 1-commit tail, which is below
// MIN_RUN and so is indistinguishable from a correct expansion.)
const EXPECTED_EXPANDED_ROWS = EXPECTED_DISPLAY_ROWS - 1 + WIP_RUN_COUNT

test.describe('#374 WIP-checkpoint collapsing', () => {
  test('a run of checkpoints renders as one folded marker by default', async ({ page }) => {
    await openApp(page)
    // Default is ON, so the run is folded before any interaction.
    const marker = page.locator('.wip-group')
    await expect(marker).toHaveCount(1)
    await expect(marker.locator('.wip-group-label')).toContainText('WIP commits')
    await expect(marker.locator('.wip-group-label')).toContainText(String(WIP_RUN_COUNT))
    // The folded members are genuinely absent, not merely hidden.
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)
  })

  test('tapping the marker expands the run into individual commits', async ({ page }) => {
    await openApp(page)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)
    await page.locator('.wip-group .node-hit').click()
    await expect(page.locator('.wip-group')).toHaveCount(0)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_EXPANDED_ROWS)
  })

  test('the topbar toggle shows every checkpoint', async ({ page }) => {
    await openApp(page)
    await page.getByRole('button', { name: /WIP: folded/ }).click()
    await expect(page.locator('.wip-group')).toHaveCount(0)
    await expect(page.getByRole('button', { name: /WIP: shown/ })).toBeVisible()
  })

  test('a checkpoint commit can fold just its own run, not the whole graph', async ({
    page,
  }) => {
    // The topbar toggle is all-or-nothing. Once a run is open, the only way
    // back should not be un-folding every other run in the graph too, so each
    // member of an open run offers to fold that one section.
    await openApp(page)
    await page.locator('.wip-group .node-hit').click()
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_EXPANDED_ROWS)

    // Open the menu on the MIDDLE checkpoint of the run, not its first row:
    // the offer has to come from membership, not from being the run's head.
    //
    // Focus + Enter rather than a click, and not for convenience: the canvas
    // starts with its top rows underneath the fixed topbar, so a coordinate
    // click up there is intercepted by the header and silently lands
    // somewhere else. Enter on a focused row opens the identical menu through
    // the roving-tabindex path (#65), which is a real user route and needs no
    // hit-testing.
    await page.locator('.node-hit[data-row-index="4"]').focus()
    await page.keyboard.press('Enter')
    const fold = page.getByRole('button', {
      name: new RegExp(`Fold these ${WIP_RUN_COUNT} checkpoints`),
    })
    await expect(fold).toBeVisible()
    await fold.click()

    await expect(page.locator('.wip-group')).toHaveCount(1)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)
  })

  test('an ordinary commit is never offered the fold item', async ({ page }) => {
    await openApp(page)
    await page.locator('.node-hit[data-row-index="0"]').focus()
    await page.keyboard.press('Enter')

    // Assert the menu actually opened before asserting what is NOT in it —
    // without this the absence check passes just as happily when no menu
    // exists at all, which is exactly how it first passed.
    await expect(page.locator('.ctx-menu')).toBeVisible()
    await expect(page.getByRole('button', { name: /Fold these/ })).toHaveCount(0)
  })
})
