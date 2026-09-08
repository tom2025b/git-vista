// #733 — a destructive write carries the WorktreeId of the status reading
// that produced its paths. The session selection is deliberately moved by a
// second tab after the first tab opens its confirmation. Both linked
// worktrees are dirty in the same tracked path, so the conditional path-state
// recheck cannot distinguish them; only the captured identity can return 412.

import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

import { expect, test } from '@playwright/test'

import { openWorktreeDrawer, openWorktreeRepo, runtime } from './helpers.mjs'

const COMMITTED = 'the committed line\n'
const MAIN_EDIT = 'edited in the main worktree\n'
const DESK_EDIT = 'edited in the linked worktree\n'

async function openHeadMenu(page) {
  const head = page.locator('.node-hit[data-row-index="0"]')
  await expect(head, 'the HEAD row must exist before its worktree menu can open').toBeAttached({
    timeout: 20_000,
  })
  await head.focus()
  await page.keyboard.press('Enter')
  await expect(page.locator('.ctx-menu')).toBeVisible()
}

test('#733 refuses a discard when another tab moves the selection after the status read', async ({
  context,
  page,
}) => {
  const { worktreeFixture } = runtime()
  const mainFile = join(worktreeFixture.root, 'tracked.txt')
  const deskFile = join(worktreeFixture.root, '.desks', 'desk-two', 'tracked.txt')
  writeFileSync(mainFile, MAIN_EDIT)
  writeFileSync(deskFile, DESK_EDIT)

  let statusRepo
  let discardBody
  await page.route('**/api/status/v2?**', async (route) => {
    statusRepo = new URL(route.request().url()).searchParams.get('repo')
    await route.continue()
  })
  page.on('request', (request) => {
    if (new URL(request.url()).pathname === '/api/discard-tracked-paths') {
      discardBody = request.postDataJSON()
    }
  })

  try {
    await openWorktreeRepo(page)
    await openHeadMenu(page)

    const discardItem = page.getByRole('button', { name: /Discard Changes…/ })
    await expect(discardItem).toBeVisible({ timeout: 20_000 })
    await expect(discardItem).not.toHaveAttribute('aria-disabled', 'true')
    await discardItem.click()
    await expect(page.getByText('Discard changes to tracked files', { exact: true })).toBeVisible()
    expect(statusRepo, 'the path list must have come from a repository-pinned status read').toBeTruthy()

    // A second browser tab shares the same server session but has independent
    // UI state, matching #588's per-session mutable selection. It can move the
    // server selection without closing or rebuilding the first tab's pending
    // confirmation.
    const mover = await context.newPage()
    await openWorktreeRepo(mover)
    const drawer = await openWorktreeDrawer(mover)
    await drawer.getByRole('button', { name: /Open ‘desk-two’/ }).click()
    const openedRow = drawer.locator('.act-file').filter({ hasText: 'desk-two' })
    await expect(openedRow.getByText('you are here', { exact: true })).toBeVisible({
      timeout: 20_000,
    })
    await expect(openedRow.getByRole('button', { name: /Open ‘desk-two’/ })).toHaveCount(0)
    await mover.close()

    const refusal = page.waitForResponse(
      (response) =>
        new URL(response.url()).pathname === '/api/discard-tracked-paths' &&
        response.status() === 412,
    )
    await page.getByRole('button', { name: 'Discard', exact: true }).click()
    await refusal

    expect(discardBody).toEqual({ repo: statusRepo, paths: ['tracked.txt'] })
    await expect(page.getByText(/Repository selection changed.*review its files/)).toBeVisible({
      timeout: 20_000,
    })
    // Both colliding edits survive. A live selector substituted at dispatch
    // would name desk-two, pass the precondition and erase DESK_EDIT instead.
    expect(readFileSync(mainFile, 'utf8')).toBe(MAIN_EDIT)
    expect(readFileSync(deskFile, 'utf8')).toBe(DESK_EDIT)
  } finally {
    writeFileSync(mainFile, COMMITTED)
    writeFileSync(deskFile, COMMITTED)
  }
})
