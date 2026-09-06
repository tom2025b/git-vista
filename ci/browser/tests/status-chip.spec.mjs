// #141: production-shaped status DTOs make the English and action boundaries
// deterministic. This tests the real wasm view; it does not test Git's parser.
import { expect, test } from '@playwright/test'
import { forceOnline, openApp, runtime } from './helpers.mjs'

test.use({ hasTouch: true, viewport: { width: 1024, height: 768 } })
const clean = { branch: 'main', upstream: 'origin/main', ahead: 0, behind: 0,
  staged: [], unstaged: [], untracked: [], conflicted: [], scanned_at: 1788729000 }
const dirty = { ...clean, ahead: 2, behind: 3,
  staged: [{ path: 'a.txt', kind: 'Modified' }],
  unstaged: [{ path: 'a.txt', kind: 'Modified' }], untracked: ['b.txt', 'c.txt'] }

async function fixture(page, status) {
  await page.route('**/api/status?**', route => {
    expect(new URL(route.request().url()).searchParams.get('repo'), 'chip status pins the selected repository').toBeTruthy()
    return route.fulfill({ json: status })
  })
}
async function openActive(page) {
  await forceOnline(page)
  await page.goto(runtime().base)
  const entry = page.getByRole('button', { name: /fixture-repo/i }).first()
  await expect(entry).toBeVisible({ timeout: 20_000 })
  await entry.click()
  const active = page.getByRole('button', { name: /full git operations/ })
  await expect(active).toBeVisible({ timeout: 20_000 })
  await active.click()
  await expect(entry).toHaveCount(0)
  await expect(active).toHaveCount(0)
  await expect(page.locator('p.status.repo')).toContainText('fixture-repo', { timeout: 20_000 })
}
async function panel(page) {
  const chip = page.getByRole('button', { name: /^Repository status:/ })
  const box = await chip.boundingBox()
  expect(box.width).toBeGreaterThanOrEqual(44)
  expect(box.height).toBeGreaterThanOrEqual(44)
  await chip.tap()
  const detail = page.getByRole('dialog', { name: 'Repository status details' })
  await expect(detail).toBeVisible()
  return detail
}

test('tap explains staged, unstaged, untracked and diverged counts; Visualize contains no write buttons', async ({ page }) => {
  await fixture(page, dirty)
  await openApp(page)
  const detail = await panel(page)
  await expect(detail).toContainText('1 file is staged for the next commit; 1 file has tracked changes that are not staged; 2 files are new and not tracked by Git.')
  await expect(detail).toContainText('3 commits on origin/main are missing from main.')
  await expect(detail).toContainText('2 commits on main are not on origin/main yet.')
  await expect(detail).toContainText('The branches have diverged: each has commits the other does not.')
  await expect(detail.locator('[data-status-action]')).toHaveCount(0)
  await expect(detail.getByRole('button')).toHaveCount(1) // Close only, no disabled substitutes.
  await expect(detail.getByRole('button', { name: 'Close' })).toBeFocused()
  await page.keyboard.press('Tab')
  await expect(detail.getByRole('button', { name: 'Close' })).toBeFocused()
  await detail.getByRole('button', { name: 'Close' }).click()
  await expect(detail).toHaveCount(0)
  await expect(page.getByRole('button', { name: /^Repository status:/ })).toBeFocused()
})

test('Active offers guided staging and committing without writing on panel open', async ({ page }) => {
  let writes = 0
  page.on('request', req => { if (new URL(req.url()).pathname === '/api/stage') writes++ })
  await fixture(page, { ...dirty, ahead: 0, behind: 0 })
  await openActive(page)
  const detail = await panel(page)
  await expect(detail.getByRole('button', { name: 'Stage all changes', exact: true })).toBeVisible()
  await expect(detail).toContainText('Staging adds all modified, deleted, and new files to the next commit.')
  await expect(detail.getByRole('button', { name: 'Review and commit…', exact: true })).toBeVisible()
  expect(writes).toBe(0)
})

test('a behind-only clean branch opens the existing pull strategy picker without sending a pull', async ({ page }) => {
  let pulls = 0
  page.on('request', req => { if (new URL(req.url()).pathname === '/api/pull') pulls++ })
  await fixture(page, { ...clean, behind: 1 })
  await openActive(page)
  const detail = await panel(page)
  await expect(detail).toContainText('1 commit on origin/main is missing from main.')
  await detail.getByRole('button', { name: 'Choose how to pull…' }).click()
  await expect(detail).toHaveCount(0)
  await expect(page.getByText('Pull branch', { exact: true })).toBeVisible()
  expect(pulls).toBe(0)
})

test('a failed status request remains unknown and offers no write', async ({ page }) => {
  await page.route('**/api/status?**', route => route.fulfill({ status: 503, body: 'unavailable' }))
  await openActive(page)
  const detail = await panel(page)
  await expect(detail).toContainText('I can’t tell the repository’s status yet.')
  await expect(detail).not.toContainText('Your working tree is clean.')
  await expect(detail.locator('[data-status-action]')).toHaveCount(0)
})

test('clean zero counts describe a local reading and Escape returns focus', async ({ page }) => {
  await fixture(page, clean)
  await openApp(page)
  const detail = await panel(page)
  await expect(detail).toContainText('Your working tree is clean. There are no uncommitted changes.')
  await expect(detail).toContainText('The latest local reading reports main up to date with origin/main: no commits ahead or behind.')
  await expect(detail).toContainText('Zero can also mean Git could not calculate the comparison.')
  await page.keyboard.press('Escape')
  await expect(detail).toHaveCount(0)
  await expect(page.getByRole('button', { name: /^Repository status:/ })).toBeFocused()
})


test('staging uses the existing endpoint, then commit opens the staged-file review', async ({ page }) => {
  let staged = false
  await page.route('**/api/status?**', route => route.fulfill({ json: staged
    ? { ...clean, staged: [{ path: 'new.txt', kind: 'Added' }] }
    : { ...clean, untracked: ['new.txt'] } }))
  await page.route('**/api/stage', route => {
    expect(route.request().method()).toBe('POST')
    staged = true
    return route.fulfill({ status: 200, body: '' })
  })
  await openActive(page)
  const detail = await panel(page)
  await detail.getByRole('button', { name: 'Stage all changes', exact: true }).click()
  await expect(detail).toContainText('1 file is staged for the next commit;')
  expect(staged).toBe(true)
  await expect(detail.getByRole('button', { name: 'Stage all changes', exact: true })).toHaveCount(0)
  await page.keyboard.press('Shift+Tab')
  await expect(detail.getByRole('button', { name: 'Review and commit…', exact: true })).toBeFocused()
  await detail.getByRole('button', { name: 'Review and commit…', exact: true }).click()
  await expect(detail).toHaveCount(0)
  await expect(page.getByText('Commit staged changes', { exact: true })).toBeVisible()
  await expect(page.getByRole('listitem').filter({ hasText: /^added new\.txt$/ })).toBeVisible()
})
