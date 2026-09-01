// M10.08 A6 (#594) — the confirm dialog must actually DRAW the preview.
//
// Why this spec exists at all, and why it is the only thing that can close
// this issue: `/api/preview` shipped in #576 registered, authz-gated,
// contract-tested and wire-goldened, and **no frontend code called it**. Three
// audits, nineteen findings and six hardening rounds all checked the engine
// against its own contract; none re-read the acceptance list. Every one of
// those checks would still be green today with the panel deleted.
//
// The pure halves (`features/preview/core.rs`, `features/preview/scene.rs`)
// are host-tested and prove the decisions and the geometry are right. They
// cannot prove any of it is reached: the consumers are
// `dialogs/preview_panel.rs`, `dialogs/confirm.rs` and
// `features/preview/signals.rs`, all `#[cfg(target_arch = "wasm32")]`, which
// `cargo test` never compiles. That is the exact shape of the defect this
// issue is.
//
// The assertions, in the order the acceptance criteria matter:
//
//   A6  BOTH halves are drawn, before and after, and the after half is marked.
//   4   the preview INFORMS and never GATES — Confirm stays live throughout.
//   5   cancelling mid-preview leaves nothing behind, including when the reply
//       is still on the wire. That one is made deterministic by delaying
//       `/api/preview` in the route layer rather than by racing a real one.
//
// Nothing here ever confirms. The fixture must stay pre-merge — a spec that
// merged it could not tell a working preview from a picture of the past.

import { expect, test } from '@playwright/test'

import {
  openBranchMenu,
  openMergePreviewRepo,
  PREVIEW_BRANCH,
  PREVIEW_HEADING,
  PREVIEW_INTO,
} from './helpers.mjs'

/** The confirm modal, scoped so `getByText` cannot resolve into the graph. */
function dialog(page) {
  return page.getByText(`Merge ‘${PREVIEW_BRANCH}’ into ‘${PREVIEW_INTO}’?`)
}

test.describe('the graph preview inside a confirmation', () => {
  test('A6: a merge confirmation draws both halves, with the new commit marked', async ({
    page,
  }) => {
    await openMergePreviewRepo(page)
    const merge = await openBranchMenu(page, PREVIEW_BRANCH)
    await merge.click()

    // The text arrives first and never waits for the picture — the panel fills
    // in beside a confirmation that is already readable.
    await expect(dialog(page)).toBeVisible()

    // BOTH halves. `after` alone would satisfy a careless reading of the
    // issue's body; A6's own words are "renders before/after", and the
    // protocol returns both *because* a canvas needs both — a caller given
    // only `after` cannot check a single lane number against anything.
    const before = page.getByRole('img', { name: /^Before:/ })
    const after = page.getByRole('img', { name: /^After:/ })
    await expect(before).toBeVisible({ timeout: 30_000 })
    await expect(after).toBeVisible()

    // The marks. `new` is a pill drawn only on a commit the operation would
    // create, and it appears in the AFTER half only — a `new` pill in the
    // before half would mean the two halves had been built from one graph.
    await expect(after.getByText('new', { exact: true })).toBeVisible()
    await expect(before.getByText('new', { exact: true })).toHaveCount(0)

    // The refs that land. The arrow is what separates "main is here" from
    // "main would move here"; a badge without it is the unmarked form.
    //
    // BOTH of them, and that is not padding. A merge moves the branch AND
    // HEAD, and `preview::ref_moves` has to be fed both or the after layout
    // reserves lane 0 for the wrong commit and colours the new commit off its
    // own hash — the two failures `git_vista_core::preview`'s module doc
    // spends its longest section on. A picture showing only `→main` would be
    // the visible symptom of exactly that.
    await expect(after.getByText(`→${PREVIEW_INTO}`)).toBeVisible()
    await expect(after.getByText('→HEAD')).toBeVisible()

    // The sentence beside the picture, for a reader who will not read a graph
    // and for a screen reader, which cannot. Asserted as the whole sentence,
    // not as a fragment: "one new commit" alone would still pass if the ref
    // moves had been dropped from the change list entirely, which is the half
    // of this summary a caller cannot re-derive for itself.
    await expect(page.getByText('one new commit and 2 refs move.')).toBeVisible()

    // The legend, so the marks can be decoded rather than guessed at.
    await expect(page.getByText('a commit this operation would create')).toBeVisible()

    // The panel's own heading. Asserted here, positively, because the third
    // test below asserts its ABSENCE — and a negative on a locator that never
    // matches anything is a test that cannot fail. This is the positive
    // control for that one.
    await expect(page.getByText(PREVIEW_HEADING)).toBeVisible()
  })

  test('4: the preview informs and never gates — Confirm stays live throughout', async ({
    page,
  }) => {
    await openMergePreviewRepo(page)
    const merge = await openBranchMenu(page, PREVIEW_BRANCH)
    await merge.click()
    await expect(dialog(page)).toBeVisible()

    // Before the picture lands.
    const confirm = page.getByRole('button', { name: 'Merge', exact: true })
    await expect(confirm).toBeEnabled()
    await expect(confirm).toHaveAttribute('aria-disabled', 'false')

    // And after it. These operations were all executable before previews
    // existed and must stay so; a preview that could disable Confirm would
    // have turned an informational panel into a gate.
    await expect(page.getByRole('img', { name: /^After:/ })).toBeVisible({ timeout: 30_000 })
    await expect(confirm).toBeEnabled()
    await expect(confirm).toHaveAttribute('aria-disabled', 'false')

    // Leave the fixture as we found it.
    await page.getByRole('button', { name: 'Cancel' }).click()
    await expect(dialog(page)).toHaveCount(0)
  })

  test('5: a reply that arrives after the dialog closed cannot paint the next one', async ({
    page,
  }) => {
    // Hold `/api/preview` open long enough that the cancel below is certainly
    // in front of the reply. Delaying the route rather than racing a real
    // response is what makes this deterministic: without it the test would
    // pass or fail on how fast this machine ran git.
    await page.route('**/api/preview', async (route) => {
      await new Promise((r) => setTimeout(r, 4000))
      await route.continue()
    })

    await openMergePreviewRepo(page)
    const merge = await openBranchMenu(page, PREVIEW_BRANCH)
    await merge.click()
    await expect(dialog(page)).toBeVisible()
    // The request is genuinely in flight — otherwise this test proves nothing
    // about a late reply, only about a preview that never started.
    await expect(page.getByText(/Drawing the result/)).toBeVisible()

    await page.getByRole('button', { name: 'Cancel' }).click()
    await expect(dialog(page)).toHaveCount(0)

    // A DIFFERENT confirmation, on the same branch, opened while the merge
    // preview is still on the wire. Checkout has no preview of its own, so
    // anything that appears here came from the cancelled merge.
    const checkout = page.getByRole('button', {
      name: new RegExp(`Checkout ‘${PREVIEW_BRANCH}’`),
    })
    await openBranchMenu(page, PREVIEW_BRANCH)
    await checkout.click()
    await expect(page.getByText(new RegExp(`Check out ‘${PREVIEW_BRANCH}’`))).toBeVisible()

    // Past the delay, with margin. The merge's reply has landed by now and
    // must have been dropped: a picture here would be the previous
    // operation's, drawn under a question about a different one — and it would
    // look entirely plausible, which is what makes it worth a test.
    await page.waitForTimeout(6000)
    await expect(page.getByText(PREVIEW_HEADING)).toHaveCount(0)
    await expect(page.getByRole('img', { name: /^After:/ })).toHaveCount(0)

    await page.getByRole('button', { name: 'Cancel' }).click()
  })
})

// The three arms with no picture in them. Every one is a real answer the
// engine computes, and none can be produced against a healthy host: a
// conflicted merge would need a fixture whose whole point is the opposite, and
// "this host's git is too old" cannot be arranged at all. So the response is
// fulfilled in the route layer.
//
// What that does and does not prove is worth being exact about. It does NOT
// test the engine — #576's own suites do, against the real thing, and the wire
// goldens pin the payloads these bodies are copied from. It tests the last
// layer, which is the one this issue exists about: that an arm the server
// takes trouble to distinguish is still distinguishable by the time a person
// reads it, instead of being flattened into a spinner or a generic error.
test.describe('the arms with no picture', () => {
  /** Answer the next `/api/preview` with `body`, without touching the server. */
  async function answerPreviewWith(page, body) {
    await page.route('**/api/preview', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(body),
      })
    })
  }

  /** Open the merge confirmation and wait for the dialog's own text. */
  async function openMergeConfirm(page) {
    await openMergePreviewRepo(page)
    const merge = await openBranchMenu(page, PREVIEW_BRANCH)
    await merge.click()
    await expect(dialog(page)).toBeVisible()
  }

  test('a conflict names its paths, and Confirm stays live', async ({ page }) => {
    await answerPreviewWith(page, {
      outcome: 'conflict',
      paths: ['src/main.rs', 'docs/notes.md'],
    })
    await openMergeConfirm(page)

    // A conflict is a live established fact — real git ran the real three-way
    // merge and it does not apply. The paths are the content of that fact, and
    // flattening them into "preview failed" would throw away the distinction
    // the server spent #576 establishing, at the last possible moment.
    await expect(page.getByText('This would conflict')).toBeVisible()
    await expect(page.getByText('src/main.rs')).toBeVisible()
    await expect(page.getByText('docs/notes.md')).toBeVisible()

    // And it must not read as a refusal of the operation. Merging into a
    // conflict is a thing a person may deliberately choose to do.
    await expect(page.getByText(/still available/)).toBeVisible()
    const confirm = page.getByRole('button', { name: 'Merge', exact: true })
    await expect(confirm).toBeEnabled()
    await expect(confirm).toHaveAttribute('aria-disabled', 'false')

    await page.getByRole('button', { name: 'Cancel' }).click()
  })

  test('an unavailable host gives its named reason and its remedy', async ({ page }) => {
    await answerPreviewWith(page, {
      outcome: 'unavailable',
      reason: { unavailable: 'git_too_old', found: '2.34.1', minimum: '2.38' },
    })
    await openMergeConfirm(page)

    // The version the host actually has and the one the feature needs, both on
    // screen. "Too old" without the two numbers sends a reader nowhere.
    await expect(page.getByText(/2\.34\.1/)).toBeVisible()
    await expect(page.getByText(/Upgrade git to 2\.38 or newer/)).toBeVisible()

    // The load-bearing half of criterion 4: a host that cannot DRAW a merge
    // can still PERFORM one, and every one of these operations worked before
    // previews existed.
    const confirm = page.getByRole('button', { name: 'Merge', exact: true })
    await expect(confirm).toBeEnabled()
    await expect(confirm).toHaveAttribute('aria-disabled', 'false')
    await expect(page.getByText(/still available/)).toBeVisible()

    await page.getByRole('button', { name: 'Cancel' }).click()
  })

  test('an unsupported operation says no host can draw it', async ({ page }) => {
    await answerPreviewWith(page, {
      outcome: 'unsupported',
      operation: 'RebaseBranch',
    })
    await openMergeConfirm(page)

    // Permanent, and about the OPERATION rather than this host — which is a
    // different sentence from the one above, deliberately, because the two
    // send a reader to different places. `Unsupported` sends them nowhere,
    // and says so.
    await expect(page.getByText('No picture for this one')).toBeVisible()
    await expect(page.getByText(/RebaseBranch/)).toBeVisible()
    await expect(page.getByText(/no preview on any host/)).toBeVisible()

    const confirm = page.getByRole('button', { name: 'Merge', exact: true })
    await expect(confirm).toBeEnabled()

    await page.getByRole('button', { name: 'Cancel' }).click()
  })

  test('a round trip that fails is not reported as a fact about the repository', async ({
    page,
  }) => {
    await page.route('**/api/preview', (route) => route.fulfill({ status: 500, body: 'boom' }))
    await openMergeConfirm(page)

    // Distinct from `Unavailable`, and that distinction is the whole reason
    // `PreviewSlot::Failed` exists: the server saying "this host's git is too
    // old" is a fact about the repository, and the fetch never arriving is a
    // fact about the connection. Telling a user the second when the first is
    // true sends them somewhere useless.
    await expect(page.getByText('No preview')).toBeVisible()
    await expect(page.getByText(/says nothing about the operation itself/)).toBeVisible()

    const confirm = page.getByRole('button', { name: 'Merge', exact: true })
    await expect(confirm).toBeEnabled()

    await page.getByRole('button', { name: 'Cancel' }).click()
  })
})
