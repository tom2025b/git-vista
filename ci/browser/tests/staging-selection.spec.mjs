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

  test('#808 a whole hunk survives an explicit line selection in the same file', async ({ page }) => {
    const viewer = await openStagingSelection(page)
    const whole = viewer.locator('.stage-hunk-check').first()
    await whole.click()
    await viewer.locator('.stage-line-check').nth(4).click() // alpha hunk 1, local 1
    await expect(whole).toHaveAttribute('aria-pressed', 'true')
    await expectLineSelection(viewer, [4]) // whole-hunk UI state stays distinct
    await expectSelectionPlan(page, viewer, [lineFile('alpha.txt', [
      { hunk: FIRST_HUNK, lines: FIRST_LINES },
      { hunk: SECOND_HUNK, lines: [1] },
    ])])
  })

  for (const direction of ['Stage', 'Unstage']) {
    test(`#808 ${direction} refuses an incomplete whole hunk in Preview and Apply`, async ({ page }) => {
      const viewer = await openStagingSelection(page)
      await viewer.locator('.viewer-close').click()
      await expect(viewer).not.toBeVisible()
      await page.route('**/api/staging/diff?**', route => route.fulfill({
        json: {
          generation: 'diff-v1:808-truncated',
          // The second hunk has visible changed lines but is missing its
          // declared trailing context. Both old and new counts are short.
          patch: `diff --git a/alpha.txt b/alpha.txt
--- a/alpha.txt
+++ b/alpha.txt
@@ -10 +10 @@
-old first
+new first
@@ -30,3 +30,3 @@
 before second
-old second
+new second
`,
          truncated: true,
        },
      }))
      await page.locator('circle.node-hit').first().click()
      await page.getByRole('button', { name: new RegExp(`Select Changes to ${direction}…`) }).click()
      await expect(viewer).toBeVisible()
      const whole = viewer.locator('.stage-hunk-check').nth(1)

      const sent = []
      await page.route('**/api/staging/preview', route => {
        sent.push('preview')
        return route.fulfill({ status: 500, body: 'unexpected preview request' })
      })
      await page.route('**/api/staging/apply', route => {
        sent.push('apply')
        return route.fulfill({ status: 500, body: 'unexpected apply request' })
      })
      const error = viewer.getByRole('alert')
      for (const action of ['Preview', `${direction} Selected`]) {
        await whole.click()
        await viewer.locator('.stage-line-check').first().click()
        await expect(error).toHaveCount(0)
        await viewer.getByRole('button', { name: action, exact: true }).click()
        await expect(error).toContainText('Cannot include the whole hunk 2 in alpha.txt')
        await expect(error).toContainText('complete changed lines')
        await expect(whole).toHaveAttribute('aria-pressed', 'true')
        expect(sent, 'an incomplete whole hunk must never reach Preview or Apply').toEqual([])
        await viewer.getByRole('button', { name: 'Clear selection', exact: true }).click()
        await expect(error).toHaveCount(0)
      }
    })
  }

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
