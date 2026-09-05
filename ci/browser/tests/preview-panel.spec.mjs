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
  openApp,
  openMergePreviewRepo,
  PREVIEW_BRANCH,
  PREVIEW_HEADING,
  PREVIEW_INTO,
} from './helpers.mjs'

/** The confirm modal, scoped so `getByText` cannot resolve into the graph. */
function dialog(page) {
  return page.getByText(`Merge ‘${PREVIEW_BRANCH}’ into ‘${PREVIEW_INTO}’?`)
}

test('#633: the repo opener waits for the selected repository\'s graph', async ({ page }) => {
  // The server selection is shared across this single-worker suite, so choose
  // repo A explicitly instead of assuming global setup (or the previous test)
  // left it selected. The repo-labelled wait also prevents openApp's generic
  // node check from accepting an older selection here.
  await openApp(page)
  await expect(page.locator('p.status.repo')).toContainText(/fixture-repo/i, {
    timeout: 20_000,
  })

  // Keep the picker from offering repo B until the launch repo (A) has drawn.
  // This makes the stale graph the old helper accidentally accepted a hard
  // precondition rather than a timing accident.
  let releaseCatalog
  const catalogGate = new Promise((resolve) => {
    releaseCatalog = resolve
  })
  await page.route('**/api/catalog?*', async (route) => {
    await catalogGate
    await route.continue()
  })

  // Learn B's opaque worktree id from its own Frame, then hold only B's first
  // commit page. The Frame is the authority for which repository a page
  // belongs to; request order or a guessed catalogue id would recreate the
  // same wait-for-the-wrong-thing mistake in the regression itself.
  let selectedWorktree
  await page.route('**/api/frame?*', async (route) => {
    const response = await route.fetch()
    const frame = await response.json()
    if (/merge-preview-repo/i.test(frame.repo_label ?? '')) {
      selectedWorktree = frame.worktree_id
    }
    await route.fulfill({ response })
  })

  let markSelectedPageRequested
  const selectedPageRequested = new Promise((resolve) => {
    markSelectedPageRequested = resolve
  })
  let releaseSelectedPage
  const selectedPageGate = new Promise((resolve) => {
    releaseSelectedPage = resolve
  })
  await page.route('**/api/commits?*', async (route) => {
    const requestedWorktree = new URL(route.request().url()).searchParams.get('repo')
    if (selectedWorktree && requestedWorktree === selectedWorktree) {
      markSelectedPageRequested()
      await selectedPageGate
    }
    await route.continue()
  })

  let openerFinished = false
  const opening = openMergePreviewRepo(page).then(() => {
    openerFinished = true
  })

  try {
    await expect(page.locator('p.status.repo')).toContainText(/fixture-repo/i, {
      timeout: 20_000,
    })
    await expect(page.locator('circle.node-hit').first()).toBeAttached()
    await page.evaluate(() => {
      window.__gv633StaleGraph = document.querySelector('section.graph svg').cloneNode(true)
    })
    releaseCatalog()

    await selectedPageRequested
    await expect(page.getByText('Loading history…', { exact: true })).toBeVisible()
    await page.evaluate(async () => {
      // Reattach repo A's real graph while B is still on the wire. This is the
      // stale-node state the race exposed, held long enough to make the helper
      // choose between "a node exists" and "the selected graph is ready".
      document.querySelector('section.graph').append(window.__gv633StaleGraph)
      await new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      )
    })
    await page.waitForTimeout(100)
    expect(
      openerFinished,
      'the opener must remain pending while the selected repository graph is still loading',
    ).toBe(false)

    releaseSelectedPage()
    await opening
    await expect(page.locator('p.status.repo')).toContainText(/merge-preview-repo/i)
    await expect(page.locator('circle.node-hit').first()).toBeAttached()
  } finally {
    // Let intercepted requests drain even when an assertion fails, so teardown
    // never inherits a route handler blocked on this test's proof gates.
    releaseCatalog()
    releaseSelectedPage()
  }
})

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

  test('#591: the animation reaches the real after state, not merely towards it', async ({
    page,
  }) => {
    // #623's root cause: a spec that samples a single point in time passes
    // trivially when the state was already there before the thing under test
    // ever ran. So this proves motion happened (the `new` pill is absent the
    // moment the animated scene first exists) AND that it settles on the
    // real endpoint (the pill appears, and the hypothetical commit's dot
    // lands on the exact pixel the static after picture draws it at) —
    // never one alone.
    await openMergePreviewRepo(page)
    const merge = await openBranchMenu(page, PREVIEW_BRANCH)
    await merge.click()
    await expect(dialog(page)).toBeVisible()

    const animated = page.getByRole('img', { name: /^An animation/ })
    await expect(animated).toBeVisible({ timeout: 30_000 })

    // Captured once, immediately, with no retry: `tween::REVEAL_AFTER`
    // (ADR 0121, decision 6) withholds every outcome-only pill — `new`
    // included — until progress crosses 0.92 of a 900ms transition, so a
    // `new` pill already present the instant this scene first mounts would
    // mean the gate never engaged at all.
    await expect(animated.getByText('new', { exact: true })).toHaveCount(0)

    // Reached, not merely not-yet-arrived: the same pill must actually show
    // up once the transition settles — well inside the ~900ms duration.
    await expect(animated.getByText('new', { exact: true })).toBeVisible({
      timeout: 3_000,
    })

    // And the settled dot is on the exact pixel the static after picture
    // draws it at — the tween's real endpoint, not an approximation of it.
    // The halo (a dashed ring, drawn only around the hypothetical commit) is
    // the one element both scenes render with the same distinguishing shape.
    const animatedHalo = animated.locator('circle[stroke-dasharray]').first()
    const after = page.getByRole('img', { name: /^After:/ })
    const afterHalo = after.locator('circle[stroke-dasharray]').first()
    await expect(afterHalo).toBeAttached()
    const [animatedPos, afterPos] = await Promise.all([
      animatedHalo.evaluate((el) => [el.getAttribute('cx'), el.getAttribute('cy')]),
      afterHalo.evaluate((el) => [el.getAttribute('cx'), el.getAttribute('cy')]),
    ])
    expect(animatedPos).toEqual(afterPos)
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
    const previewRequest = page.waitForRequest('**/api/preview')

    await openMergePreviewRepo(page)
    const merge = await openBranchMenu(page, PREVIEW_BRANCH)
    await merge.click()
    await expect(dialog(page)).toBeVisible()
    // The request is genuinely in flight — otherwise this test proves nothing
    // about a late reply, only about a preview that never started. Observe the
    // request itself instead of coupling that precondition to pending copy.
    await previewRequest

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

  test('a plan capability refusal makes no operation-availability promise', async ({ page }) => {
    await page.route('**/api/plan', (route) =>
      route.fulfill({
        status: 405,
        headers: { 'x-git-vista-listener-profile': 'read-only' },
        body: '',
      }),
    )
    await openMergeConfirm(page)

    // A plan 405 is a listener capability refusal, not evidence that the
    // operation can still run. Pin the response's structural facts and the
    // dangerous promise we must never append, without pinning every word.
    const failurePanel = page.getByText('No preview', { exact: true }).locator('..')
    await expect(failurePanel).toContainText('read-only LAN listener')
    await expect(failurePanel).toContainText('/api/plan')
    await expect(failurePanel).toContainText(/operation is unavailable/)
    await expect(failurePanel).not.toContainText(/unchanged|still available|ready either way/i)

    const confirm = page.getByRole('button', { name: 'Merge', exact: true })
    await expect(confirm).toBeEnabled()

    await page.getByRole('button', { name: 'Cancel' }).click()
  })
})
