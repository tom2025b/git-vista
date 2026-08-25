// M3.24 (#77) — the stash drawer must be REACHED, and a conflicted pop must
// never report itself as complete.
//
// Why this spec exists at all: `features/stash/core.rs` is host-tested and its
// 18 tests prove every decision the drawer makes is correct. They cannot prove
// any of it is reached. The values are consumed by `features/stash/view.rs` and
// `activity.rs`, both `#[cfg(target_arch = "wasm32")]`, which `cargo test` never
// compiles — the exact shape of every defect in this suite's README table (#68d
// and #69c each had a fully-tested core with ZERO consumers, beside a green
// gate).
//
// The four assertions, in the order the criteria matter:
//
//   A4  a pop of the conflicting entry must NOT say "Popped", must say the
//       stash is still in the list, and must offer a route to resolve. This is
//       the load-bearing one: a pop that conflicts has applied something and
//       dropped nothing, so a UI that reports success has lied about the user's
//       data.
//   A1  "Show changes" is present on a row and reveals the patch, so a user can
//       look before dropping.
//   A2  an untracked file the push would NOT capture is named before the push.
//   --  the three fixture entries render, with git's two message forms told
//       apart (the `-m` form unmarked, the `WIP on` form marked "auto").
//
// SERIAL, and it must stay serial: the A4 test pops a stash, which mutates the
// fixture's list and leaves `collision.txt` conflicted. Every read assertion
// therefore runs before it. Sharing a repo with another spec would break that
// spec for reasons unrelated to what it tests — see buildStashFixture's comment.

import { expect, test } from '@playwright/test'

import {
  CONFLICTING_PATH,
  CONFLICTING_SUBJECT,
  openDrawer,
  openStashRepo,
  UNTRACKED,
} from './helpers.mjs'

test.describe.serial('the stash drawer', () => {
  test('the drawer lists the fixture entries and tells git\'s two message forms apart', async ({
    page,
  }) => {
    await openStashRepo(page)
    const drawer = await openDrawer(page)

    // Every query is scoped to the drawer. Page-wide would resolve to four
    // elements for the WIP subject below — see openDrawer's comment.
    // The `-m` messages appear as the user typed them.
    await expect(drawer.getByText(CONFLICTING_SUBJECT)).toBeVisible()
    await expect(drawer.getByText('half-finished refactor')).toBeVisible()

    // The bare `git stash` entry is git's own `WIP on main: <sha> <subject>`.
    // The subject survives and the sha does NOT — the row already shows the
    // stash's own oid, and a second unexplained hash beside it is noise.
    // This is the string that appears FOUR times on the page — git copied the
    // seed commit's subject into the stash message verbatim. Scoped, it is one.
    await expect(drawer.getByText('seed: two tracked files')).toBeVisible()

    // Exactly one row is marked as git-authored rather than user-authored.
    // `toHaveCount(1)` and not `toBeVisible`: the pill existing on every row
    // would mean the automatic/typed distinction had collapsed, which is a
    // claim about whose words the user is reading.
    await expect(drawer.getByText('auto', { exact: true })).toHaveCount(1)
  })

  test('A1: a stash can be read before it is applied or dropped', async ({ page }) => {
    await openStashRepo(page)
    const drawer = await openDrawer(page)

    const show = drawer.getByRole('button', { name: 'Show changes' }).first()
    await expect(show).toBeVisible()
    await show.click()

    // The patch itself, from `GET /api/stash/show`. Asserting on a real diff
    // line rather than on the <pre> existing: the element appears before the
    // fetch resolves.
    await expect(drawer.getByText(/^\+.*the stashed edit/m)).toBeVisible({ timeout: 20_000 })

    // Tapping the open one closes it, so the control is its own undo.
    await show.click()
    await expect(drawer.getByText(/^\+.*the stashed edit/m)).toHaveCount(0)
  })

  test('A2: an untracked file the push would leave behind is named first', async ({ page }) => {
    await openStashRepo(page)
    const drawer = await openDrawer(page)

    // Default state: untracked files are NOT included, so the preview must say
    // so before the button is pressed. This is the whole of A2 — a flag that
    // merely exists satisfies the letter and misses the failure, which is a
    // user believing they stashed a new file that git left on disk.
    await expect(drawer.getByText(`${UNTRACKED} — NOT stashed`)).toBeVisible()

    // Ticking the box moves it to the captured list. Without this half the
    // assertion above would pass against a preview that always warns.
    await drawer.getByText('Include untracked files').click()
    await expect(drawer.getByText(`${UNTRACKED} — NOT stashed`)).toHaveCount(0)
    await expect(drawer.getByText(UNTRACKED, { exact: true })).toBeVisible()
  })

  // --- A4. Runs LAST because it mutates the fixture. ------------------------
  test('A4: a pop that conflicts is not reported as complete', async ({ page }) => {
    // The only test here that writes: an apply, a conflict scan, and a wasm
    // boot, against a box that serialises heavy builds. The config's 30s
    // per-test budget is not enough for that chain, and an inner wait longer
    // than the outer budget can only ever fail as a test timeout — which
    // reports nothing about the assertion.
    test.setTimeout(90_000)
    await openStashRepo(page)
    const drawer = await openDrawer(page)

    // The conflicting entry is the newest, so its row's Pop is the first one.
    await drawer.getByRole('button', { name: 'Pop', exact: true }).first().click()

    // The notice. `git stash apply` on this entry exits 1 with the conflict
    // markers already written, so the composed pop halts at the gate and the
    // verdict is Conflicted — never Popped.
    const notice = drawer.getByText(/NOT popped/)
    await expect(notice).toBeVisible({ timeout: 45_000 })

    // The three claims that make this criterion, asserted positively rather
    // than by the absence of a success message:
    //   1. the entry survived,
    await expect(drawer.getByText(/still in your list/)).toBeVisible()
    //   2. the working tree DID change (the markers are in it), and
    await expect(
      drawer.getByText('Your working tree has changes from this stash.'),
    ).toBeVisible()
    //   3. A3 — the conflicted path routes into the SHARED conflict view.
    const resolve = drawer.getByRole('button', {
      name: `${CONFLICTING_PATH} — resolve this conflict`,
    })
    await expect(resolve).toBeVisible()

    // And the word that must not appear. A `toHaveCount(0)` on the success
    // wording is the negative the whole slice exists for: it is what fails if
    // someone ever reports the pop from "the request returned" instead of from
    // PopVerdict::is_complete().
    await expect(drawer.getByText(/^Popped the stash/)).toHaveCount(0)

    // The entry is still listed, which is the drawer agreeing with the notice.
    await expect(drawer.getByText(CONFLICTING_SUBJECT)).toBeVisible()

    // Following the route opens the four-pane conflict view (#428) — not a
    // second, stash-shaped conflict UI.
    // NOT scoped to the drawer: the four-pane viewer opens outside it, which is
    // the whole point of the route. Anchored to `.viewer-modal` / `.conflict-pane`
    // — the same locators `conflict-panes.spec.mjs` uses, verified against
    // `viewer.rs:319,662`. An earlier draft of this line guessed at
    // `getByRole('region', { name: /conflict/i })`, which does not exist: the
    // viewer is a modal div, not a named landmark. Asserting on the panes rather
    // than on the path text also means it cannot be satisfied by the path still
    // sitting in the drawer behind the modal.
    await resolve.click()
    const viewer = page.locator('.viewer-modal')
    await expect(viewer).toBeVisible({ timeout: 20_000 })
    await expect(viewer.locator('.conflict-pane')).toHaveCount(4)
  })
})
