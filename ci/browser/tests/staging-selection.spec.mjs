// The DOM handlers added by #771 must reach the per-line model. Refs #357.
import { expect, test } from '@playwright/test'
import {
  BETA_HUNK, FIRST_HUNK, FIRST_LINES, SECOND_HUNK,
  expectLineSelection, expectSelectionPlan, lineFile, openStagingSelection,
} from './staging-selection.mjs'

test.describe('#357 staging line selection', () => {
  test('pointer clicks toggle added and removed lines independently across hunks and files', async ({ page }) => {
    const viewer = await openStagingSelection(page)
    const lines = viewer.getByRole('button', { name: 'Select this line for staging', exact: true })
    await expectLineSelection(viewer, [])
    await lines.nth(0).click() // alpha hunk 0, removed line, local 1
    await expectLineSelection(viewer, [0])
    await expectSelectionPlan(page, viewer, [lineFile('alpha.txt', [{ hunk: FIRST_HUNK, lines: [1] }])])
    await lines.nth(1).click() // added line, local 2
    await expectLineSelection(viewer, [0, 1])
    await lines.nth(0).click() // deselect just the removed line
    await expectLineSelection(viewer, [1])
    await expectSelectionPlan(page, viewer, [lineFile('alpha.txt', [{ hunk: FIRST_HUNK, lines: [2] }])])
    await lines.nth(1).click()
    await expectLineSelection(viewer, []) // last toggle clears the action gate

    await lines.nth(4).click() // alpha hunk 1, local 1
    await lines.nth(7).click() // beta hunk 0, local 2 (not flat hunk 2)
    await expectLineSelection(viewer, [4, 7])
    await expectSelectionPlan(page, viewer, [
      lineFile('alpha.txt', [{ hunk: SECOND_HUNK, lines: [1] }]),
      lineFile('beta.txt', [{ hunk: BETA_HUNK, lines: [2] }]),
    ])
    await viewer.getByRole('button', { name: 'Clear selection', exact: true }).click()
    await expectLineSelection(viewer, [])
  })

  for (const key of ['Enter', 'Space']) {
    test(`Shift+${key} selects exactly the changed lines in the focused hunk`, async ({ page }) => {
      const viewer = await openStagingSelection(page)
      const header = viewer.locator('.stage-hunk-text').first()
      const selected = [lineFile('alpha.txt', [{ hunk: FIRST_HUNK, lines: FIRST_LINES }])]
      await expectLineSelection(viewer, [])
      await header.click() // production click-to-focus path, not injected focus
      await expect(header).toBeFocused()
      await page.keyboard.press(`Shift+${key}`)
      await expectLineSelection(viewer, [0, 1, 2, 3])
      await expectSelectionPlan(page, viewer, selected)

      await header.click()
      await page.keyboard.press(`Shift+${key}`) // select-all is idempotent
      await expectLineSelection(viewer, [0, 1, 2, 3])
      await expectSelectionPlan(page, viewer, selected)
      await viewer.locator('.stage-line-check').nth(2).click()
      await expectLineSelection(viewer, [0, 1, 3])
      await expectSelectionPlan(page, viewer, [lineFile('alpha.txt', [{ hunk: FIRST_HUNK, lines: [1, 2, 5] }])])

      await viewer.getByRole('button', { name: 'Clear selection', exact: true }).click()
      await expectLineSelection(viewer, [])
      await header.click()
      await page.keyboard.press(key) // plain Activate still selects a whole hunk
      await expectSelectionPlan(page, viewer, [{ path: 'alpha.txt', selection: { select: 'hunks', hunks: [FIRST_HUNK] } }])
      await header.click()
      await page.keyboard.press(`Shift+${key}`) // narrows an existing whole hunk
      await expectLineSelection(viewer, [0, 1, 2, 3])
      await expectSelectionPlan(page, viewer, selected)
    })
  }
})
