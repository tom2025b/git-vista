// #765: capture the tag badge's frame identity before confirmation, and
// retain it when another tab moves this session onto a sibling worktree.
import { execFileSync } from 'node:child_process'
import { join } from 'node:path'
import { expect, test } from '@playwright/test'
import { openWorktreeDrawer, openWorktreeRepo, runtime } from './helpers.mjs'

const TAG = 'selector-765'
const git = (cwd, ...args) => execFileSync('git', args, { cwd, encoding: 'utf8' }).trim()

async function confirmTag(page) {
  const head = page.locator('.node-hit[data-row-index="0"]')
  await expect(head).toBeAttached({ timeout: 20_000 })
  await head.focus()
  await page.keyboard.press('Enter')
  const item = page.getByRole('button', { name: `Delete tag ‘${TAG}’` })
  await expect(item).toBeVisible()
  await item.click()
  await expect(page.getByText('Delete tag', { exact: true })).toBeVisible()
}

test('#765 preserves the captured tag selector across a second-tab worktree switch', async ({ context, page }) => {
  const { worktreeFixture } = runtime()
  const main = worktreeFixture.root
  const desk = join(main, '.desks', 'desk-two')
  git(main, 'tag', TAG)
  const before = git(main, 'rev-parse', `refs/tags/${TAG}`)
  expect(git(desk, 'rev-parse', `refs/tags/${TAG}`)).toBe(before)
  try {
    await openWorktreeRepo(page)
    // Read-side #752 uses the same accepted-frame WorktreeId as the graph.
    // Opening Activity gives an independent wire observation of that id.
    const listing = page.waitForRequest((r) => new URL(r.url()).pathname === '/api/tags')
    await openWorktreeDrawer(page)
    const repo = new URL((await listing).url()).searchParams.get('repo')
    expect(repo).toBeTruthy()
    await page.locator('.activity-panel .detail-close').click()
    await confirmTag(page)

    const mover = await context.newPage()
    await openWorktreeRepo(mover)
    const drawer = await openWorktreeDrawer(mover)
    await drawer.getByRole('button', { name: /Open ‘desk-two’/ }).click()
    await expect(drawer.locator('.act-file').filter({ hasText: 'desk-two' })
      .getByText('you are here', { exact: true })).toBeVisible({ timeout: 20_000 })
    await mover.close()

    const pending = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/delete-tag')
    await page.getByRole('button', { name: 'Delete', exact: true }).click()
    const refusal = await pending
    expect(refusal.request().postDataJSON()).toEqual({ repo, tag: TAG })
    expect(refusal.status()).toBe(412)
    expect((await refusal.json()).error.code).toBe('precondition_failed')
    await expect(page.getByText(/Repository selection changed/)).toBeVisible()
    expect(git(main, 'rev-parse', `refs/tags/${TAG}`)).toBe(before)
    expect(git(desk, 'rev-parse', `refs/tags/${TAG}`)).toBe(before)

    // Positive control: reopen the intended worktree and make a fresh intent.
    await openWorktreeRepo(page)
    await confirmTag(page)
    const deletion = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/delete-tag')
    await page.getByRole('button', { name: 'Delete', exact: true }).click()
    expect((await deletion).ok()).toBe(true)
    await expect.poll(() => git(main, 'tag', '--list', TAG)).toBe('')
    expect(git(desk, 'tag', '--list', TAG)).toBe('')
  } finally {
    if (git(main, 'tag', '--list', TAG)) git(main, 'tag', '-d', TAG)
  }
})
