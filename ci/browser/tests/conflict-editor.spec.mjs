// M4.31c (#432) -- the line/block resolver must be REACHABLE and must actually
// resolve, and neither fact is visible to `cargo test`.
//
// The parsing and composing live in `features/conflicts/markers.rs` with 10
// host tests, so what a choice PRODUCES is already pinned. What those tests
// cannot see is whether a renderer ever asks: `viewer.rs` is consumed only in
// wasm. That is the same gap #428's and #430's specs were written for, and the
// same shape as every defect in this suite's README table -- a fully-tested
// core with zero consumers beside a green gate.
//
// So the assertions here are about reaching the surface and the round trip:
//   1. the editor opens on a text conflict
//   2. per-block choices compose into the result box
//   3. applying it writes exactly that content and clears the conflict
//   4. it is NOT offered for a conflict the server would refuse

import { expect, test } from '@playwright/test'

import { forceOnline, runtime } from './helpers.mjs'

// This spec drives its OWN repository (`editor-repo`, buildEditorFixture) and
// that is not tidiness. Applying a resolution MUTATES the fixture, and this
// file sorts before `conflict-panes.spec.mjs`: sharing conflict-repo emptied
// that spec's conflicts and failed all four of its tests. Its own comment says
// it mutates the shared fixture and must run last — two specs cannot both be
// last, so this one brought its own.
//
// Two paths, one per test, so the two are not racing each other either.
const FIRST = 'first.txt'
const SECOND = 'second.txt'

async function openConflictRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const entry = page.getByRole('button', { name: /editor-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  const active = page.getByRole('button', { name: /full git operations/ })
  if (await active.isVisible().catch(() => false)) {
    await active.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  await page.getByRole('button', { name: /activity/i }).first().click()
}

test('the line-by-line resolver opens, composes a choice, and applies it', async ({ page }) => {
  await openConflictRepo(page)

  await page
    .getByRole('button', { name: new RegExp(`${FIRST}.*inspect this conflict`) })
    .click()
  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()

  // 1. The editor is offered for a text conflict, and is behind a click --
  //    the marker file is a second read, and a whole-side resolve should not
  //    pay for it.
  const open = viewer.getByRole('button', { name: /Resolve line by line/ })
  await expect(open).toBeVisible()
  await open.click()

  // 2. Exactly one conflict in this fixture, and it starts UNCHOSEN. That is
  //    the state `compose` refuses to produce a file from, and the apply
  //    button must reflect it rather than letting a guess through.
  const apply = viewer.getByRole('button', { name: 'Apply this resolution' })
  await expect(viewer.locator('.conflict-blk-conflict')).toHaveCount(1)
  await expect(viewer.locator('.conflict-blk-state').first()).toContainText('not chosen yet')
  await expect(apply).toBeDisabled()

  // 3. Choosing a side fills the result box with THAT side's text. The
  //    fixture writes "our version" / "their version" (buildConflictFixture),
  //    so the assertion can tell the two apart -- a composer that returned the
  //    wrong side would sail past a mere "is non-empty" check, and returning
  //    the wrong side is the worst failure a merge tool has.
  await viewer.getByRole('button', { name: 'Theirs', exact: true }).click()
  await expect(apply).toBeEnabled()

  const composed = viewer.locator('.conflict-compose')
  await expect(composed).toHaveValue(/their version/)
  await expect(composed).not.toHaveValue(/our version/)
  // And the markers are gone -- the whole point of composing rather than
  // handing back what git wrote.
  await expect(composed).not.toHaveValue(/<<<<<<</)
  await expect(composed).not.toHaveValue(/>>>>>>>/)

  // 4. Applying it resolves the path for real: the viewer closes and the
  //    conflicted list loses this row. Both, because #429 found that
  //    refreshing one resource left the other claiming a conflict that was
  //    already resolved.
  await apply.click()
  await expect(viewer).toBeHidden()
  await expect(
    page.getByRole('button', { name: new RegExp(`${FIRST}.*inspect this conflict`) }),
  ).toHaveCount(0)
})

test('hand-editing the result takes over from the buttons', async ({ page }) => {
  // #432's "safe manual editing". The rule under test is not that typing
  // works -- it is that a later button press must NOT silently discard what
  // was typed. An editor that threw away someone's edit to re-apply a choice
  // would be the worst kind of data loss: quiet, and only noticed later.
  await openConflictRepo(page)

  await page
    .getByRole('button', { name: new RegExp(`${SECOND}.*inspect this conflict`) })
    .click()
  const viewer = page.locator('.viewer-modal')
  await viewer.getByRole('button', { name: /Resolve line by line/ }).click()

  const composed = viewer.locator('.conflict-compose')
  await composed.fill('a resolution typed entirely by hand\n')

  // The status line says so, and the per-block buttons go inert.
  await expect(viewer.locator('.conflict-blk-state').last()).toContainText('edited by hand')
  await expect(viewer.getByRole('button', { name: 'Ours', exact: true })).toBeDisabled()

  // Apply is enabled even though no block was ever chosen -- a hand-edited
  // file is a complete answer on its own.
  const apply = viewer.getByRole('button', { name: 'Apply this resolution' })
  await expect(apply).toBeEnabled()
  await apply.click()
  await expect(viewer).toBeHidden()
})
