// #89: exercise the wasm wiring; provider fixtures stay outside graph loading.
import { expect, test } from '@playwright/test'
import { openApp } from './helpers.mjs'

const fixture = (page = 1) => ({
  repository: { name: 'octocat/Hello-World', web_url: 'https://github.com/octocat/Hello-World' },
  availability: 'ready', capabilities: { summaries: true, checks: true, reviews: true },
  pulls: [{ number: page === 1 ? 1347 : 1348, title: page === 1 ? 'Improve the documentation — café' : 'Second page draft', draft: page === 2, web_url: `https://github.com/octocat/Hello-World/pull/${page === 1 ? 1347 : 1348}` }],
  page, next_page: page === 1 ? 2 : null, retry_after_seconds: null,
})

const details = {
  number: 1347,
  availability: 'ready',
  check_rollup: 'failing',
  review_rollup: 'changes_requested',
  checks: [
    { name: 'build', state: 'success' },
    { name: 'browser', state: 'failure' },
  ],
  reviews: [
    { reviewer: 'hubot', state: 'approved' },
    { reviewer: 'octocat', state: 'changes_requested' },
  ],
  retry_after_seconds: null,
}

test('on-demand summaries, capabilities, paging and no reuse after close', async ({ page }) => {
  const requests = []
  await page.route('**/api/forge/pulls?**', async route => {
    const url = new URL(route.request().url())
    requests.push(url)
    const json = url.searchParams.has('number')
      ? details
      : fixture(Number(url.searchParams.get('page')))
    await route.fulfill({ json, headers: { 'Cache-Control': 'no-store' } })
  })
  await openApp(page)
  expect(requests).toHaveLength(0)
  await page.getByRole('button', { name: 'Pull requests', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: 'Pull requests', exact: true })
  await expect(dialog.getByRole('link', { name: '#1347 Improve the documentation — café' })).toHaveAttribute('href', 'https://github.com/octocat/Hello-World/pull/1347')
  await expect(dialog).toContainText('Checks: available · Reviews: available')
  expect(requests[0].searchParams.get('repo')).toBeTruthy()
  expect(requests.filter(url => url.searchParams.has('number'))).toHaveLength(0)
  await dialog.getByRole('button', { name: 'Checks and reviews', exact: true }).click()
  await expect(dialog).toContainText('Checks: failing · Reviews: changes requested')
  await expect(dialog.getByRole('list', { name: 'Checks for pull request #1347' })).toContainText('build · passed')
  await expect(dialog.getByRole('list', { name: 'Checks for pull request #1347' })).toContainText('browser · failed')
  await expect(dialog.getByRole('list', { name: 'Reviews for pull request #1347' })).toContainText('hubot · approved')
  await expect(dialog.getByRole('list', { name: 'Reviews for pull request #1347' })).toContainText('octocat · requested changes')
  expect(requests.find(url => url.searchParams.get('number') === '1347')).toBeTruthy()
  await dialog.getByRole('button', { name: 'Next', exact: true }).click()
  await expect(dialog).toContainText('Second page draft')
  await expect(dialog).toContainText('Draft')
  await expect(dialog.getByRole('button', { name: 'Next', exact: true })).toBeDisabled()
  await dialog.getByRole('button', { name: 'Close', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  await page.getByRole('button', { name: 'Pull requests', exact: true }).click()
  await expect(dialog).toContainText('Improve the documentation')
  await expect(dialog).not.toContainText('browser · failed')
  expect(requests.length).toBeGreaterThanOrEqual(3)
})

test('missing token is explicit and never looks like an empty PR page', async ({ page }) => {
  await page.route('**/api/forge/pulls?**', async route => {
    await route.fulfill({
      json: {
        ...fixture(),
        repository: null,
        availability: 'access_uncertain',
        pulls: [],
        next_page: null,
      },
      headers: { 'Cache-Control': 'no-store' },
    })
  })
  await openApp(page)
  await page.getByRole('button', { name: 'Pull requests', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: 'Pull requests', exact: true })
  await expect(dialog).toContainText('Pull request view unavailable. No GitHub token is configured')
  await expect(dialog).not.toContainText('No open pull requests')
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
