// M4.31a (#428) -- the four-pane conflict view must be REACHED, and an absent
// stage must read as absent rather than as an empty pane.
//
// Why this spec exists at all: `features/conflicts/core.rs` is host-tested and
// its 16 tests prove the pane-state mapping is correct. They cannot prove it is
// reached. The mapping is consumed by `viewer.rs` and `activity.rs`, both
// `#[cfg(target_arch = "wasm32")]`, which `cargo test` never compiles -- the
// exact shape of every defect in this suite's README table (#68d and #69c each
// had a fully-tested core with ZERO consumers, beside a green gate).
//
// So the two assertions that matter here are:
//   1. a conflicted status row OPENS the viewer (the core is wired to a gesture)
//   2. the base pane of an ADD/ADD conflict says "not present", not "" (the
//      distinction ADR 0063 exists to protect survives rendering)

import { expect, test } from '@playwright/test'

import { forceOnline, runtime } from './helpers.mjs'

// `added-by-both.txt` has no stage 1 -- both sides created it, so there is no
// common ancestor. `both-modified.txt` has all three. See buildConflictFixture.
const ADD_ADD = 'added-by-both.txt'
const BOTH_MODIFIED = 'both-modified.txt'

async function openConflictRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  // The conflict repo is the second registered repository; pick it explicitly
  // rather than relying on which one the server defaulted to.
  const entry = page.getByRole('button', { name: /conflict-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  const active = page.getByRole('button', { name: /full git operations/ })
  if (await active.isVisible().catch(() => false)) {
    await active.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
}

async function openActivityPanel(page) {
  const toggle = page.getByRole('button', { name: /activity/i }).first()
  await toggle.click()
}

test('a conflicted path opens the four-pane view, and an absent base says so', async ({ page }) => {
  await openConflictRepo(page)
  await openActivityPanel(page)

  // 1. The row exists AND is a control. A plain <div> would fail here, which
  //    is the point: before #428 these rows were inert text.
  const row = page.getByRole('button', { name: new RegExp(`${ADD_ADD}.*inspect this conflict`) })
  await expect(row).toBeVisible()
  await row.click()

  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()
  await expect(viewer).toContainText(ADD_ADD)

  // 2. All four panes are present -- not three, and not a single merged blob.
  const panes = viewer.locator('.conflict-pane')
  await expect(panes).toHaveCount(4)
  for (const heading of ['Base', 'Ours', 'Theirs', 'Result (read-only)']) {
    await expect(viewer.getByRole('heading', { name: heading, exact: true })).toBeVisible()
  }

  // 3. THE assertion. An add/add conflict has no stage 1. The base pane must
  //    SAY that. An empty <pre> here would claim the ancestor existed and was
  //    blank -- a statement about the repository that is simply false.
  const basePane = panes.filter({ has: page.getByRole('heading', { name: 'Base', exact: true }) })
  await expect(basePane).toContainText(/not present on this side/i)
  await expect(basePane.locator('pre')).toHaveCount(0)

  // 4. The sides that DO exist show their real content, so assertion 3 is
  //    evidence about absence rather than about the view failing to load.
  const oursPane = panes.filter({ has: page.getByRole('heading', { name: 'Ours', exact: true }) })
  await expect(oursPane.locator('pre')).toContainText('ours created this')
  const theirsPane = panes.filter({
    has: page.getByRole('heading', { name: 'Theirs', exact: true }),
  })
  await expect(theirsPane.locator('pre')).toContainText('theirs created this')
})

test('a modify/modify conflict shows a real base, and the result pane holds git markers', async ({
  page,
}) => {
  await openConflictRepo(page)
  await openActivityPanel(page)

  await page
    .getByRole('button', { name: new RegExp(`${BOTH_MODIFIED}.*inspect this conflict`) })
    .click()

  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()
  const panes = viewer.locator('.conflict-pane')

  // The positive control for the previous test: here stage 1 DOES exist, so
  // the base pane holds the ancestor's text. If this rendered "not present"
  // too, the previous test would be passing on a broken renderer rather than
  // on a real absence.
  const basePane = panes.filter({ has: page.getByRole('heading', { name: 'Base', exact: true }) })
  await expect(basePane.locator('pre')).toContainText('the common ancestor')

  // The result pane is the working tree as git left it -- markers and all.
  // This is what makes it worth showing beside the three index stages.
  const resultPane = panes.filter({
    has: page.getByRole('heading', { name: 'Result (read-only)', exact: true }),
  })
  await expect(resultPane.locator('pre')).toContainText('<<<<<<<')
  await expect(resultPane.locator('pre')).toContainText('>>>>>>>')
})
