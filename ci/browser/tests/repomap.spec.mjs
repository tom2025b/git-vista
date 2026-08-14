import { expect, test } from '@playwright/test'
import { forceOnline, runtime } from './helpers.mjs'

// #380: the mindmap repo picker. A second view of the same catalog — the map's
// leaf click must run the exact open path a list row runs.
test.describe('#380 mindmap repo picker', () => {
  test('the picker toggles to a map whose leaves open a repository', async ({ page }) => {
    await forceOnline(page)
    const { base } = runtime()
    await page.goto(base)
    await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

    // The picker opens on load. Toggle to the map.
    await page.getByRole('button', { name: 'View: list' }).click()
    await expect(page.getByRole('button', { name: 'View: map' })).toBeVisible()

    const svg = page.locator('svg.repomap')
    await expect(svg).toBeVisible()

    // Every catalog entry renders as a leaf; the fixture repo is among them.
    const leaves = page.locator('.repomap-leaf')
    expect(await leaves.count()).toBeGreaterThan(0)
    const fixtureLeaf = leaves.filter({ hasText: /fixture-repo/ }).first()
    await expect(fixtureLeaf).toBeVisible()

    // Clicking the leaf runs the same open path as a list row: the mode
    // screen (with its "look only" choice) appears.
    await fixtureLeaf.click()
    await expect(page.getByRole('button', { name: /look only/ })).toBeVisible()
  })

  test('the map lists exactly as many leaves as the list has rows', async ({ page }) => {
    await forceOnline(page)
    const { base } = runtime()
    await page.goto(base)
    await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

    // Count list rows first (buttons whose row opens a repo carry the
    // hook-policy disclosure line; count via the row container's buttons
    // minus the action buttons). Simplest honest proxy: the picker rows are
    // the buttons inside the scroll region before toggling.
    const listButtons = await page
      .locator('div[style*="overflow-y:auto"] button')
      .filter({ hasNotText: /^Delete$/ })
      .count()

    await page.getByRole('button', { name: 'View: list' }).click()
    const leaves = await page.locator('.repomap-leaf').count()

    expect(leaves).toBe(listButtons)
  })
})
