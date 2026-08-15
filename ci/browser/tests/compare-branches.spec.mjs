// #366 -- the Compare-with-HEAD branch picker must reach the explicit
// RefVsRef viewer and show the answer for THAT mode, not merely any patch.

import { expect, test } from '@playwright/test'

import { forceOnline, runtime } from './helpers.mjs'

const BASE_BRANCH = 'base — new branch, no commits yet'
const BARE_COMMIT = /seed: large file for the virtualization budget/

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

async function openBaseComparison(page) {
  await page.getByRole('button', { name: BASE_BRANCH }).click()
  await page.getByRole('button', { name: 'Compare base with HEAD' }).click()
  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()
  return viewer
}

async function assertRefVsRefAnswer(viewer) {
  await expect(
    viewer.locator('.diff-del').filter({ hasText: '-one' }),
    'ref-vs-ref must remove the value parked on branch base',
  ).toHaveCount(1)
  await expect(
    viewer.locator('.diff-add').filter({ hasText: '+two' }),
    'ref-vs-ref must add the committed HEAD value',
  ).toHaveCount(1)

  const changedLines = viewer.locator('.diff-add, .diff-del')
  await expect(
    changedLines.filter({ hasText: /[+-]three/ }),
    'ref-vs-ref must not leak the staged index value',
  ).toHaveCount(0)
  await expect(
    changedLines.filter({ hasText: /[+-]four/ }),
    'ref-vs-ref must not leak the unstaged worktree value',
  ).toHaveCount(0)
}

async function failureMessage(fn) {
  try {
    await fn()
    return null
  } catch (e) {
    return String(e?.message ?? e)
  }
}

test.describe('#366 Compare branch with HEAD', () => {
  test('the item appears for a local branch and not for a bare commit', async ({ page }) => {
    await openActiveApp(page)

    await page.getByRole('button', { name: BASE_BRANCH }).click()
    await expect(page.getByRole('button', { name: 'Compare base with HEAD' })).toBeVisible()

    await page.keyboard.press('Escape')
    await page.getByRole('button', { name: BARE_COMMIT }).click()
    await expect(page.getByRole('button', { name: /Compare .* with HEAD/ })).toHaveCount(0)
  })

  test('the viewer renders the distinguishable ref-vs-ref answer and Escape closes it', async ({ page }) => {
    await openActiveApp(page)
    const viewer = await openBaseComparison(page)

    await expect(viewer.locator('.viewer-title')).toContainText('base → main')
    await assertRefVsRefAnswer(viewer)

    await page.keyboard.press('Escape')
    await expect(viewer).not.toBeAttached()
  })

  test('the distinguishing-content assertion goes red when the answer is altered', async ({ page }) => {
    await openActiveApp(page)
    const viewer = await openBaseComparison(page)

    await viewer.locator('.diff-add').filter({ hasText: '+two' }).evaluate((line) => {
      line.textContent = '+four\n'
    })

    const message = await failureMessage(() => assertRefVsRefAnswer(viewer))
    expect(message, 'the ref-vs-ref assertion must fail after its answer is altered').not.toBeNull()
    expect(message, 'the assertion must fail for the missing committed HEAD value').toMatch(
      /ref-vs-ref must add the committed HEAD value/,
    )
  })
})
