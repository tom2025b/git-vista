// #136: real journal writer -> activity token -> real historical frame/pages -> wasm UI.
import { expect, test } from '../../../ci/browser/node_modules/@playwright/test/index.mjs'
import { execFileSync } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { forceOnline, runtime } from '../../../ci/browser/tests/helpers.mjs'

async function openActive(page) {
  await forceOnline(page)
  await page.goto(runtime().base)
  await page.getByRole('button', { name: /fixture-repo/i }).first().click()
  await page.getByRole('button', { name: /full git operations/ }).click()
  await expect(page.locator('p.status.repo')).toContainText('fixture-repo')
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
}

async function createBranch(page, name) {
  const { base, fixture } = runtime()
  const protocol = await (await page.request.get(`${base}/api/protocol`)).json()
  const headers = { 'x-git-vista-protocol': String(protocol.protocol_version) }
  const session = await (await page.request.get(`${base}/api/session`, { headers })).json()
  headers['x-git-vista-csrf'] = session.csrf
  headers['x-git-vista-idempotency-key'] = randomUUID()
  const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: fixture.root, encoding: 'utf8' }).trim()
  const response = await page.request.post(`${base}/api/branch`, { headers, data: { name, commit } })
  expect(response.ok(), await response.text()).toBeTruthy()
}

async function observation(page, name) {
  await createBranch(page, name) // production journaling records shallow:false
  execFileSync('git', ['branch', '-D', name], { cwd: runtime().fixture.root })
  await page.getByRole('button', { name: 'Refresh', exact: true }).click()
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
  await page.getByRole('button', { name: 'Activity', exact: true }).click()
  const row = page.locator('.act-row').filter({ hasText: name }).filter({
    has: page.getByRole('button', { name: 'View graph at this activity', exact: true }),
  })
  await expect(row).toHaveCount(1)
  return row.getByRole('button', { name: 'View graph at this activity', exact: true })
}

test('historical paging draws deleted refs, carries its token, and exposes no write controls', async ({ page }) => {
  await openActive(page)
  const choose = await observation(page, 'past-136')
  const pages = []
  const writes = []
  await page.route('**/api/commits?**', async route => {
    const url = new URL(route.request().url())
    if (url.searchParams.has('as_of')) {
      pages.push(new URL(url))
      url.searchParams.set('limit', '2') // drive real stateless paging on a small fixture
      await route.continue({ url: String(url) })
    } else await route.continue()
  })
  page.on('request', req => { if (req.method() !== 'GET' && req.url().includes('/api/')) writes.push(req.url()) })
  await choose.click()
  const banner = page.locator('.historical-banner')
  await expect(banner).toContainText('Historical view · after activity at')
  await expect(banner).toContainText('view only')
  await expect(page.locator('svg')).toContainText('past-136')
  await expect.poll(() => pages.filter(p => p.searchParams.has('cursor')).length).toBeGreaterThan(0)
  expect(new Set(pages.map(p => p.searchParams.get('as_of'))).size).toBe(1)
  expect(pages.every(p => p.searchParams.get('repo'))).toBeTruthy()
  for (const name of ['Settings', 'Repos', 'Activity', 'Open URL…', 'Reset Test Repo']) {
    await expect(page.getByRole('button', { name, exact: true })).toHaveCount(0)
  }
  await page.locator('.node-hit[data-row-index="0"]').click()
  await expect(page.locator('.ctx-menu')).toBeVisible()
  await expect(page.locator('.ctx-menu')).not.toContainText('Create branch')
  await expect(page.locator('.ctx-menu')).not.toContainText('Create tag')
  await expect(page.locator('.ctx-menu')).not.toContainText('Cherry-pick')
  await page.keyboard.press('Escape')
  expect(writes).toEqual([])
  await page.screenshot({ path: '/tmp/gv136-historical.png', fullPage: true })
  await page.getByRole('button', { name: 'Return to live', exact: true }).click()
  await expect(banner).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Activity', exact: true })).toBeVisible()
  await expect(page.locator('svg')).not.toContainText('past-136')
})

test('a stale selection stays visibly historical with the real 409 and one return action', async ({ page }) => {
  await openActive(page)
  const choose = await observation(page, 'stale-136')
  await createBranch(page, 'after-token-136') // changes the exact fold after the selector was painted
  const refused = page.waitForResponse(r => r.url().includes('/api/frame?') && r.url().includes('as_of='))
  await choose.click()
  expect((await refused).status()).toBe(409)
  await expect(page.locator('.historical-banner')).toContainText('view only')
  await expect(page.locator('.status.error')).toContainText('activity moved')
  await expect(page.locator('circle.node-hit')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Return to live', exact: true })).toHaveCount(1)
  await page.getByRole('button', { name: 'Return to live', exact: true }).click()
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
  await expect(page.locator('.historical-banner')).toHaveCount(0)
})

test('returning during a pending historical seed ignores its late response', async ({ page }) => {
  await openActive(page)
  const choose = await observation(page, 'late-136')
  let release
  const pending = new Promise(resolve => { release = resolve })
  let reached = false
  await page.route('**/api/frame?**', async route => {
    if (new URL(route.request().url()).searchParams.has('as_of')) {
      reached = true
      await pending
    }
    await route.continue().catch(() => {})
  })
  await choose.click()
  await expect.poll(() => reached).toBe(true)
  await expect(page.locator('.historical-banner')).toBeVisible()
  await page.getByRole('button', { name: 'Return to live', exact: true }).click()
  release()
  await expect(page.locator('.historical-banner')).toHaveCount(0)
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
  await expect(page.locator('svg')).not.toContainText('late-136')
})
