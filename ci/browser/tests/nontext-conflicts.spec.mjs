// M4.31d (#430) -- binary and delete/modify conflicts must be given their own
// surface, and the controls that cannot work must not be offered.
//
// Why a browser spec when `features/conflicts/core.rs` already has 27 host
// tests: those prove `ResolutionSurface` computes the right answer. They cannot
// prove a renderer ASKS it. `viewer.rs` is consumed only in wasm, which
// `cargo test` never compiles -- the same gap #428's spec was written for, and
// the same shape as every defect in this suite's README table.
//
// So the assertions here are about REACHING the surface:
//   1. a binary conflict's note says a line merge is impossible (not just its size)
//   2. a delete/modify conflict names which side deleted
//   3. the control for a side that holds nothing is WITHHELD, with a reason,
//      rather than offered and then refused by the server with a 409

import { expect, test } from '@playwright/test'

import { forceOnline, runtime } from './helpers.mjs'

const BINARY = 'logo.png'
const DELETED = 'doomed.txt'

async function openNonTextRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  // Pick it explicitly rather than relying on which repo the server defaulted
  // to -- there are three registered now.
  const entry = page.getByRole('button', { name: /nontext-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  const active = page.getByRole('button', { name: /full git operations/ })
  if (await active.isVisible().catch(() => false)) {
    await active.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  await page.getByRole('button', { name: /activity/i }).first().click()
}

test('a binary conflict explains that no line merge is possible', async ({ page }) => {
  await openNonTextRepo(page)

  await page.getByRole('button', { name: new RegExp(`${BINARY}.*inspect this conflict`) }).click()
  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()

  // THE assertion. Before #430 the only thing on screen about a binary
  // conflict was the pane's "Binary file (N bytes)" -- a size, not an
  // explanation, and nothing said why the text panes were empty.
  const note = viewer.locator('.conflict-note')
  await expect(note).toBeVisible()
  await expect(note).toContainText(/binary/i)
  await expect(note).toContainText(/no line-by-line merge/i)

  // Choosing a whole side IS the honest resolution for binary and the server
  // accepts it, so these must still be offered. A note that withheld them
  // would leave a binary conflict unresolvable.
  await expect(viewer.getByRole('button', { name: 'Take ours' })).toBeVisible()
  await expect(viewer.getByRole('button', { name: 'Take theirs' })).toBeVisible()

  // And no pane may render the bytes as text.
  await expect(viewer).not.toContainText('PNG')
})

test('a delete/modify conflict names the deleting side and withholds the empty one', async ({
  page,
}) => {
  await openNonTextRepo(page)

  await page.getByRole('button', { name: new RegExp(`${DELETED}.*inspect this conflict`) }).click()
  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()

  // #430's second acceptance criterion: WHICH side deleted, named.
  const note = viewer.locator('.conflict-note')
  await expect(note).toBeVisible()
  await expect(note).toContainText(/They deleted this file/)
  await expect(note).toContainText(/our side still has it/)

  // The claim the honesty review killed. The wire carries deletion flags, not
  // modification flags, so no sentence may say the surviving side "changed"
  // anything. Asserted here as well as in the host tests because this is the
  // only layer that sees what a user actually reads.
  await expect(note).not.toContainText(/changed it/)

  // THE defect this slice fixes, at the layer where it was visible. `theirs`
  // deleted the file, so `ConflictedFile::refuses` answers TakeTheirs with
  // SideAbsent (protocol conflict.rs:343). Before #430 the button was rendered
  // anyway and the user got a 409 for pressing it.
  await expect(viewer.getByRole('button', { name: 'Take theirs' })).toHaveCount(0)
  await expect(viewer.locator('.conflict-withheld')).toContainText(/no version of this file/i)

  // The side that DOES hold content is still offered, so the assertion above
  // is evidence about withholding rather than about the controls failing to
  // render at all.
  await expect(viewer.getByRole('button', { name: 'Take ours' })).toBeVisible()
  await expect(viewer.getByRole('button', { name: 'Delete file' })).toBeVisible()
})
