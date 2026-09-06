// M5.34 (#87, ADR 0129) — the bisect menu items reach real git state.
//
// The host-side `contract_suite` pipeline tests
// (`bisect_start_executes_through_the_pipeline` and its two siblings) drive
// `GitOperation::BisectStart`/`Mark`/`Reset` directly through the planner,
// bypassing the HTTP handler entirely — they prove the executors are
// correct, never that a click reaches them. This spec is the other half:
// it drives the actual `POST /api/bisect/*` routes through a real browser
// click, and asserts against `git bisect log`'s own output on disk, not
// against "no error appeared". A handler that constructed the wrong
// `GitOperation` (started when asked to mark, marked "good" when the button
// said "bad") would show no error either — the request would still succeed,
// just do the wrong git thing — which is exactly what only this leg, not
// the pipeline tests, can catch.

import { execFileSync } from 'node:child_process'

import { expect, test } from '@playwright/test'

import { openMergePreviewRepo, runtime } from './helpers.mjs'

/** Run git in the merge-preview fixture — shared with several other specs,
 *  so every change here is undone before the test ends. */
function git(args) {
  const { mergePreviewFixture } = runtime()
  return execFileSync('git', ['-C', mergePreviewFixture.root, ...args], {
    encoding: 'utf8',
  }).trim()
}

/** `git bisect log`'s own text when a session is running, or throws
 *  ("We are not bisecting") when it is not. */
function bisectLogOrEmpty() {
  try {
    return git(['bisect', 'log'])
  } catch {
    return ''
  }
}

test('the bisect menu items start, mark and reset a real bisect session', async ({ page }) => {
  await openMergePreviewRepo(page)

  // Precondition: no bisect is already running in this fixture.
  expect(bisectLogOrEmpty()).toBe('')

  const nodes = page.locator('circle.node-hit')
  const count = await nodes.count()
  expect(count, 'the fixture must have at least two commits to bisect between').toBeGreaterThanOrEqual(2)

  // Mark the tip commit bad, mark an older one good — the two-step anchor
  // flow `menu/bisect_items.rs`'s module doc describes.
  await nodes.nth(0).click()
  await page
    .getByRole('button', { name: 'Mark bad (start bisect here)' })
    .click()
  await nodes.nth(1).click()
  await page
    .getByRole('button', { name: 'Start bisect: this commit is good' })
    .click()

  // Real git state: `git bisect start` actually ran.
  await expect
    .poll(bisectLogOrEmpty, {
      timeout: 10_000,
      message: 'git bisect log must show a start line after the request succeeds',
    })
    .toContain('git bisect start')

  // Mark the new candidate good — real git state grows a second log line.
  await nodes.nth(0).click()
  await page.getByRole('button', { name: 'Bisect: mark good' }).click()
  await expect
    .poll(bisectLogOrEmpty, { timeout: 10_000 })
    .toContain('git bisect good')

  // Reset — real git state: the session is gone, not just the UI's.
  await nodes.nth(0).click()
  await page.getByRole('button', { name: 'Bisect: reset' }).click()
  await expect
    .poll(bisectLogOrEmpty, {
      timeout: 10_000,
      message: 'git bisect reset must actually clear .git/BISECT_START',
    })
    .toBe('')
})
