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

// This fixture is shared with other specs (preview-panel, plan-freshness).
// A failure mid-test — say, at the "git bisect good" poll — would otherwise
// leave BISECT_START and a detached HEAD behind for whichever spec runs
// next against the same checkout. Reset unconditionally; "we are not
// bisecting" is the expected, harmless case on a clean pass.
test.afterEach(() => {
  const { mergePreviewFixture } = runtime()
  try {
    execFileSync('git', ['-C', mergePreviewFixture.root, 'bisect', 'reset'], {
      encoding: 'utf8',
    })
  } catch {
    // Not bisecting — nothing to clean up.
  }
})

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
  expect(count, 'the fixture must have at least three commits to bisect between').toBeGreaterThanOrEqual(3)

  // Name the endpoints by fixture subject rather than assuming adjacent graph
  // rows form a useful range. main~1 is strictly between this bad tip and the
  // shared base, so Git must check out a real candidate before it can finish.
  const bad = page.getByRole('button', { name: /main: add 2\.txt$/ })
  const good = page.getByRole('button', { name: /seed: the shared base$/ })
  await expect(bad).toBeAttached()
  await expect(good).toBeAttached()
  const candidateOid = git(['rev-parse', 'main~1'])

  // Mark the tip commit bad, mark an older one good — the two-step anchor
  // flow `menu/bisect_items.rs`'s module doc describes.
  await bad.click()
  await page
    .getByRole('button', { name: 'Mark bad (start bisect here)' })
    .click()
  await good.click()
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
  expect(
    git(['rev-parse', 'HEAD']),
    'the chosen endpoints must leave a real intermediate candidate to test',
  ).toBe(candidateOid)

  // The menu row only opens the command: the mark itself applies to Git's
  // current HEAD. Require the request to succeed before consulting the log;
  // Git can append `git bisect good` before rejecting an invalid mark, so the
  // substring alone is not a success oracle.
  await bad.click()
  const markFinished = page.waitForResponse(
    (response) =>
      new URL(response.url()).pathname === '/api/bisect/mark' &&
      response.request().method() === 'POST',
  )
  await page.getByRole('button', { name: 'Bisect: mark good' }).click()
  const markResponse = await markFinished
  expect(
    markResponse.ok(),
    `bisect mark must return 2xx, got HTTP ${markResponse.status()}`,
  ).toBe(true)

  // After a successful mark, real git state grows a second log line.
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
