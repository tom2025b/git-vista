// #473 -- a repository whose HEAD resolves to nothing must SAY so.
//
// Why this spec exists: `head_notice` is host-tested and its test proves the
// decision (only `Unresolvable` earns a notice; `Detached` and the rest do
// not). It cannot prove the decision is REACHED. The consumer is `app/mod.rs`,
// which is `#[cfg(target_arch = "wasm32")]` and which `cargo test` never
// compiles -- the same shape as every entry in this suite's README table, and
// the same shape as #473 itself: a fact the server knew and nothing drew.
//
// The two assertions that matter:
//   1. the broken repo SHOWS the notice (the mapping is wired to the view)
//   2. the healthy repo does NOT (a warning that fires on ordinary
//      repositories is a warning nobody reads -- and "detached" and "broken"
//      both arrive as head_branch: null, which is the whole defect)

import { expect, test } from '@playwright/test'

import { forceOnline, runtime } from './helpers.mjs'

const NOTICE = /HEAD is broken/i

async function openRepo(page, namePattern) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const entry = page.getByRole('button', { name: namePattern }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  // Selecting an entry opens the mode dialog ("Open 'x' as…"). It does not
  // appear when the repository is already the open one, so this is
  // conditional -- same shape as the conflict specs.
  const active = page.getByRole('button', { name: /full git operations/ })
  if (await active.isVisible().catch(() => false)) {
    await active.click()
  }
}

test.describe('#473 a HEAD that resolves to nothing', () => {
  test('the topbar says the repository is broken', async ({ page }) => {
    await openRepo(page, /broken-head-repo/i)

    // The repo label proves we are looking at the right repository before
    // asserting on what it says about HEAD.
    await expect(page.locator('p.status.repo')).toContainText(/broken-head-repo/i)
    await expect(page.locator('.head-broken')).toBeVisible()
    await expect(page.locator('.head-broken')).toHaveText(NOTICE)
  })

  test('a healthy repository says nothing of the kind', async ({ page }) => {
    await openRepo(page, /fixture-repo/i)

    await expect(page.locator('p.status.repo')).toContainText(/fixture-repo/i)
    // Present and correct, so the absence below is about HEAD's state and not
    // about the topbar having failed to render at all.
    await expect(page.locator('.repo-branch').first()).toBeVisible()
    await expect(page.locator('.head-broken')).toHaveCount(0)
  })
})
