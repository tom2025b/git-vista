import { expect, test } from '@playwright/test'
import { readFile } from 'node:fs/promises'
import { openApp, openDiff, openMergePreviewRepo, openBranchMenu, runtime } from './helpers.mjs'

// The netns harness forces onLine=true at boot. Replace that override AND
// cut transport: neither a fabricated event alone nor a failed socket alone
// exercises the app's actual offline-state wiring.
async function connectivity(page, context, online) {
  await context.setOffline(!online)
  await page.evaluate((up) => {
    Object.defineProperty(navigator, 'onLine', { get: () => up, configurable: true })
    window.dispatchEvent(new Event(up ? 'online' : 'offline'))
  }, online)
}

async function confirmMerge(page) {
  await (await openBranchMenu(page, 'feature')).click()
  await expect(page.getByText('Merge ‘feature’ into ‘main’?')).toBeVisible()
  await expect(page.getByRole('img', { name: /^After:/ })).toBeVisible()
  return page.getByRole('button', { name: 'Merge', exact: true })
}

test('offline refuses an already-open git confirmation and never replays on reconnect', async ({ page, context }) => {
  await openMergePreviewRepo(page)
  const confirm = await confirmMerge(page)
  await expect(confirm).toBeEnabled()
  const writes = []
  page.on('request', request => {
    if (request.method() === 'POST' && new URL(request.url()).pathname === '/api/merge') writes.push(request)
  })
  // Even the positive control cannot modify the shared fixture.
  await page.route('**/api/merge', route => route.fulfill({ status: 400, body: 'test refusal' }))
  await connectivity(page, context, false)
  await expect(page.getByText(/This device reports it is offline/)).toBeVisible()
  await confirm.click()
  await expect(page.getByText('Your device reports it is offline. Reconnect to the network, then try again.', { exact: true })).toBeVisible()
  expect(writes).toHaveLength(0)
  await connectivity(page, context, true)
  await expect(page.getByText(/This device reports it is offline/)).toHaveCount(0)
  await page.waitForTimeout(1000)
  expect(writes, 'reconnection must not replay the refused intent').toHaveLength(0)
  await page.keyboard.press('Escape')
  await (await confirmMerge(page)).click()
  await expect.poll(() => writes.length, { message: 'a fresh online action reaches the same transport' }).toBe(1)
})

test('a write that loses connectivity gets no offline retry or reconnect replay', async ({ page, context }) => {
  await openMergePreviewRepo(page)
  let attempts = 0
  const requests = []
  page.on('request', request => {
    if (request.method() === 'POST' && new URL(request.url()).pathname === '/api/merge') requests.push(request)
  })
  await page.route('**/api/merge', async route => {
    attempts++
    await connectivity(page, context, false)
    await route.abort('internetdisconnected')
  })
  await (await confirmMerge(page)).click()
  await expect(page.getByText('Your device reports it is offline. Reconnect to the network, then try again.', { exact: true })).toBeVisible()
  expect(attempts).toBe(1)
  expect(requests, 'the immediate retry must be refused before fetch').toHaveLength(1)
  await connectivity(page, context, true)
  await page.waitForTimeout(1000)
  expect(requests).toHaveLength(1)
})

test('a previously loaded client refuses the live v9 server window', async ({ page }) => {
  await openApp(page)
  await page.route('**/api/protocol?*', route => route.fulfill({
    contentType: 'application/json',
    headers: { 'cache-control': 'no-store' },
    body: JSON.stringify({ protocol_version: 9, min_client_protocol: 9, max_client_protocol: 9, server_version: 'stale-v9' }),
  }))
  // Keep the loaded wasm document: exactly the stale/cached-client deployment
  // scenario. No service worker is introduced merely to manufacture a cache.
  await page.getByRole('button', { name: /refresh/i }).first().click()
  const overlay = page.getByRole('alertdialog', { name: 'Update Required' })
  await expect(overlay).toBeVisible()
  await expect(overlay).toContainText('server accepts v9–v9')
  await expect(overlay).toContainText('newer')
  expect(await page.locator('[inert]').count()).toBeGreaterThan(0)
  await page.keyboard.press('Escape')
  await expect(overlay).toBeVisible()
})

test('PWA assets revalidate and diff reads create no persistent response cache', async ({ page }) => {
  await openApp(page)
  const { base } = runtime()
  const manifest = await page.request.get(`${base}/manifest.webmanifest`)
  expect(manifest.headers()['cache-control']).toContain('no-cache')
  expect(await manifest.json()).toMatchObject({ start_url: '/', display: 'standalone' })
  const icon = await page.request.get(`${base}/icon-180.png`)
  expect(icon.headers()['content-type']).toContain('image/png')
  await expect(page.locator('link[rel="apple-touch-icon"]')).toHaveAttribute('href', '/icon-180.png')
  const before = await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } }))
  const diffResponse = page.waitForResponse(response => /\/api\/diff\/[^?]+\?/.test(response.url()))
  await openDiff(page)
  expect((await diffResponse).headers()['cache-control']).toContain('no-store')
  expect(await page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } }))).toEqual(before)
  expect(await page.evaluate(() => caches.keys())).toEqual([])
  expect(await page.evaluate(async () => (await navigator.serviceWorker.getRegistrations()).length)).toBe(0)
})

test('browser data controls export only preferences and clear owned data in both stores', async ({ page }) => {
  await openMergePreviewRepo(page)
  await page.evaluate(() => {
    localStorage.setItem('git-vista.icons', 'text')
    localStorage.setItem('git-vista.node-icons', 'private-value-canary')
    for (const storage of [localStorage, sessionStorage]) {
      storage.setItem('gv-commit-draft:private-repo', 'private-draft-canary')
      storage.setItem('git-vista.comparison', 'private-comparison-canary')
      storage.setItem('other-application', 'keep-me')
    }
  })
  await page.getByRole('button', { name: /settings/i }).click()
  const section = page.getByRole('region', { name: 'Browser data' })
  const downloadPromise = page.waitForEvent('download')
  await section.getByRole('button', { name: 'Export preferences' }).click()
  const download = await downloadPromise
  const exported = JSON.parse(await readFile(await download.path(), 'utf8'))
  expect(exported).toMatchObject({ format: 'git-vista-preferences', version: 1, preferences: { 'git-vista.icons': 'text' } })
  expect(JSON.stringify(exported)).not.toContain('private-')
  await section.getByRole('button', { name: 'Clear saved browser data…' }).click()
  await section.getByRole('button', { name: 'Keep saved data' }).click()
  expect(await page.evaluate(() => localStorage.getItem('gv-commit-draft:private-repo'))).toBe('private-draft-canary')
  await section.getByRole('button', { name: 'Clear saved browser data…' }).click()
  await Promise.all([
    page.waitForEvent('load'),
    section.getByRole('button', { name: 'Delete saved data and reload' }).click(),
  ])
  const stores = await page.evaluate(() => [Object.entries(localStorage), Object.entries(sessionStorage)])
  for (const entries of stores) {
    expect(entries).toContainEqual(['other-application', 'keep-me'])
    expect(entries.filter(([key]) => key.startsWith('git-vista.') || key.startsWith('gv-commit-draft:'))).toEqual([])
  }
})
