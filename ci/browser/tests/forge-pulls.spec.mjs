// #89: exercise the wasm wiring; provider fixtures stay outside graph loading.
import { expect, test } from '@playwright/test'
import { openApp } from './helpers.mjs'

const fixture = (page = 1) => ({
  repository: { name: 'octocat/Hello-World', web_url: 'https://github.com/octocat/Hello-World' },
  availability: 'ready', capabilities: { summaries: true, checks: false, reviews: false },
  pulls: [{ number: page === 1 ? 1347 : 1348, title: page === 1 ? 'Improve the documentation — café' : 'Second page draft', draft: page === 2, web_url: `https://github.com/octocat/Hello-World/pull/${page === 1 ? 1347 : 1348}` }],
  page, next_page: page === 1 ? 2 : null, retry_after_seconds: null,
})

test('on-demand summaries, capabilities, paging and no reuse after close', async ({ page }) => {
  const requests = []
  await page.route('**/api/forge/pulls?**', async route => {
    const url = new URL(route.request().url())
    requests.push(url)
    await route.fulfill({ json: fixture(Number(url.searchParams.get('page'))), headers: { 'Cache-Control': 'no-store' } })
  })
  await openApp(page)
  expect(requests).toHaveLength(0)
  await page.getByRole('button', { name: 'Pull requests', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: 'Pull requests', exact: true })
  await expect(dialog.getByRole('link', { name: '#1347 Improve the documentation — café' })).toHaveAttribute('href', 'https://github.com/octocat/Hello-World/pull/1347')
  await expect(dialog).toContainText('Checks: unavailable · Reviews: unavailable')
  expect(requests[0].searchParams.get('repo')).toBeTruthy()
  await dialog.getByRole('button', { name: 'Next', exact: true }).click()
  await expect(dialog).toContainText('Second page draft')
  await expect(dialog).toContainText('Draft')
  await expect(dialog.getByRole('button', { name: 'Next', exact: true })).toBeDisabled()
  await dialog.getByRole('button', { name: 'Close', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  await page.getByRole('button', { name: 'Pull requests', exact: true }).click()
  await expect(dialog).toContainText('Improve the documentation')
  expect(requests.length).toBeGreaterThanOrEqual(3)
})

test('a stalled provider panel closes and the local graph remains usable', async ({ page }) => {
  let release
  const pending = new Promise(resolve => { release = resolve })
  await page.route('**/api/forge/pulls?**', async route => { await pending; await route.fulfill({ json: fixture() }).catch(() => {}) })
  await openApp(page)
  await page.getByRole('button', { name: 'Pull requests', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: 'Pull requests', exact: true })
  await expect(dialog).toContainText('Loading pull requests…')
  await dialog.getByRole('button', { name: 'Close', exact: true }).click()
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  await page.locator('.node-hit[data-row-index="1"]').focus()
  await page.keyboard.press('Enter')
  await expect(page.locator('.ctx-menu')).toBeVisible()
  release()
  await expect(dialog).toHaveCount(0)
})
