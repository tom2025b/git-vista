// Shared navigation for the browser specs.
//
// These steps were derived by driving the real app, not from reading the source:
// a tap on a commit node opens a MENU, and the diff is behind that menu's
// "Show diff" item. Encoding that here keeps each spec about its own assertion.

import { readFileSync } from 'node:fs'
import { expect } from '@playwright/test'

import { RUNTIME_FILE } from '../global-setup.mjs'

export function runtime() {
  return JSON.parse(readFileSync(RUNTIME_FILE, 'utf8'))
}

/**
 * Make the page believe it has a network.
 *
 * The suite runs inside a network namespace with only loopback up (see run.sh
 * for why: the server's port is a compile-time constant). Chromium therefore
 * reports `navigator.onLine === false`, and the app's offline guard refuses to
 * open a repository at all -- a correct behaviour blocking a correct test.
 *
 * A veth pair to the host would be the honest fix, but it needs real root,
 * which an unprivileged user namespace does not grant. So this forges the one
 * signal instead, and pays for it explicitly:
 *
 *   THIS HARNESS CANNOT TEST THE OFFLINE GUARD. It fabricates the exact value
 *   that guard reads. The guard's own coverage must come from somewhere else --
 *   today that is a manual device pass.
 */
export async function forceOnline(page) {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'onLine', { get: () => true, configurable: true })
  })
}

/** The repository `openApp` opens, as both the picker row and the status line
 *  spell it (`global-setup.mjs` builds the fixture under this directory name). */
const APP_REPO = 'fixture-repo'

/**
 * Load the app and get as far as a rendered history graph.
 *
 * The picker is ALWAYS shown on load -- `picker_open` is seeded `true`
 * (app/mod.rs, ADR 0006 "ask every time"), and only a LAN session closes it
 * unasked. So "handle both cases" was never the shape of this problem: there
 * is one case, and the only question is whether the catalog has painted yet.
 *
 * That is why the old `if (await entry.isVisible())` was the bug (#623). It
 * sampled an INSTANT. Ask before the catalog fetch lands and the answer is
 * "no picker", the click is skipped, and the picker -- a full-viewport
 * `position:fixed; z-index:900` div (picker.rs) -- stays up for the rest of
 * the test.
 *
 * Nothing downstream noticed, which is the part worth remembering: the graph
 * renders BEHIND that overlay, so `toBeVisible()` on the region passed (it
 * tests layout, not occlusion) and `toBeAttached()` on a node passed (it
 * tests the DOM, not hit-testing). The suite then failed 30 seconds later in
 * whichever spec clicked first, as `<div> intercepts pointer events` -- one
 * cause wearing eleven different spec names, which is what #623 was named
 * for.
 *
 * So each step below WAITS for a state the app guarantees, and then asserts
 * the overlay is gone rather than assuming the click removed it.
 */
export async function openApp(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  // Not "if it is up" -- it is always up. Wait for the row to paint.
  const pickerEntry = page.getByRole('button', { name: /fixture-repo/i }).first()
  await expect(pickerEntry, 'the picker lists the fixture repository').toBeVisible({
    timeout: 20_000,
  })
  await pickerEntry.click()

  // The mode dialog follows a choice, so it is likewise guaranteed rather
  // than possible.
  //
  // Match on "look only", not on /Visualize/: the topbar carries a mode BADGE
  // also labelled "Visualize", and it sits behind this dialog. A loose match
  // resolves to the badge and then waits forever for an element the dialog is
  // covering -- which is exactly what a 30s timeout looked like here.
  const visualize = page.getByRole('button', { name: /look only/ })
  await expect(visualize, 'the mode dialog follows opening a repository').toBeVisible({
    timeout: 20_000,
  })
  await visualize.click()

  // Both overlays are gone, stated rather than assumed. `toHaveCount(0)` is an
  // assertion about the DOM, not a retry: after the two clicks above, neither
  // dialog has any reason to be mounted, and if one is, every later click in
  // this spec would have failed on it instead -- 30 seconds away, in a
  // different file, as a different symptom.
  await expect(pickerEntry, 'the picker must be dismissed, not merely unsampled').toHaveCount(0)
  await expect(visualize, 'the mode dialog must be dismissed, not merely unsampled').toHaveCount(0)

  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  // The seed matching the CURRENT epoch has arrived. `p.status.repo` renders
  // only inside `Some((e, Ok(seed))) if e == epoch` (app/mod.rs), so this is
  // the app's own statement that the graph now on screen belongs to the
  // selection just made -- not the previous epoch's nodes, still attached
  // while `/api/select` settles. `openPreview` below already waits on exactly
  // this signal for exactly this reason; openApp never did.
  await expect(page.locator('p.status.repo')).toContainText(APP_REPO, { timeout: 20_000 })
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
}

/**
 * Open the diff for the nth commit node, and wait for the patch to render.
 *
 * Waiting on a rendered hunk header rather than on the panel's frame matters:
 * the panel appears immediately and fetches its diff lazily, so asserting on the
 * frame would let a spec proceed against an empty patch.
 */
export async function openDiff(page, nth = 0) {
  await page.locator('circle.node-hit').nth(nth).click()
  await page.getByRole('button', { name: /Show diff/ }).click()
  await expect(page.locator('span.diff-hunk').first()).toBeAttached({ timeout: 20_000 })
}

/** The vertical scroller that wraps the windowed patch. */
export const DIFF_SCROLLER = '.detail-diff-scroll'

// --- #392: pasting a sign-in link over the URL of an already-open tab -------
//
// Shared with `harness-selfcheck.spec.mjs`, which requires the reload
// assertion below to go red when nothing reloads -- the exact shape of the
// defect (a same-document hash change the app never noticed).

/** Marks a particular loaded document, so a reload can be told from a
 *  same-document hash change: a hash change preserves `window`, a reload
 *  builds a fresh one. Playwright's navigation events do not distinguish the
 *  two, so an assertion built on them would pass against the unfixed app. */
export const RELOAD_SENTINEL = '__gv392_survived_the_hash_change'

/** Stamp the sentinel on the current document. */
export async function markPage(page) {
  await page.evaluate((key) => {
    window[key] = true
  }, RELOAD_SENTINEL)
}

/** Whether this is still the document `markPage` stamped. Resolves `false`
 *  rather than throwing while the context is being torn down by the reload
 *  the caller is waiting for. */
export function pageSurvived(page) {
  return page.evaluate((key) => window[key] === true, RELOAD_SENTINEL).catch(() => false)
}

/**
 * Set `location.hash`, tolerating the context teardown a reload causes.
 *
 * The assignment returns before the handler runs, so this normally resolves
 * cleanly -- but "Execution context was destroyed" here is the fix working,
 * not a failure, and must never fail the test asserting for it.
 */
export async function setHash(page, hash) {
  await page
    .evaluate((h) => {
      window.location.hash = h
    }, hash)
    .catch(() => {})
}

/** Collect the body of every `POST /api/session` the page makes from now on. */
export function watchSignIns(page) {
  const posts = []
  page.on('request', (r) => {
    if (r.method() === 'POST' && r.url().includes('/api/session')) {
      posts.push(r.postData() ?? '')
    }
  })
  return posts
}

// --- #478: the interleaved-chains grouping assertion ------------------------
//
// Shared with `harness-selfcheck.spec.mjs`, the same way `RELOAD_SENTINEL`
// above is, and for a stronger reason. That file's contract is that each
// assertion is run against a DOM broken in the exact way the real test claims
// to detect — which only holds if it runs the SAME assertion. When the two
// were separate copies they agreed only because someone had just made them
// agree; the next person to tighten the spec's version had no reason to know
// a self-check mirrored it.
//
// It landed as two copies once already, and the copy that mattered covered
// less: the self-check asserted only the length, so it stayed green over a
// reader (`allInnerTexts()` on SVG) under which the content checks it was
// guarding could never have passed at all.

/**
 * The interleaved graph folds into exactly TWO markers, and each carries its
 * OWN chain's length.
 *
 * `local`/`remote` are passed in rather than read from `fixture.mjs`: every
 * spec imports this module, and it has no business depending on one fixture's
 * shape. The caller owns the numbers.
 *
 * Reads `textContent`, not `innerText`. `.wip-group-label` is an SVG `<text>`
 * (`render/nodes.rs:356`) and SVG elements have no `innerText`, so
 * `allInnerTexts()` yields an array of `undefined` — silently, without
 * throwing. Both are real Playwright methods; only one of them works here.
 */
export async function expectEachChainHasItsOwnMarker(page, { local, remote }) {
  // A precondition, not a formality: if both chains were the same length the
  // per-chain checks below would pass against a projection that put each
  // chain in the OTHER one's marker, and this assertion would discriminate
  // nothing.
  expect(local, 'the two chains must differ in length, or the counts prove nothing').not.toBe(
    remote,
  )
  const labels = await page.locator('.wip-group-label').allTextContents()
  expect(labels, 'each chain must fold into its own marker').toHaveLength(2)
  expect(labels[0], 'each marker must carry its own chain length').toContain(String(local))
  expect(labels[1], 'each marker must carry its own chain length').toContain(String(remote))
}

// --- #77: the stash drawer -------------------------------------------------
//
// Shared between `stash-drawer.spec.mjs` and `harness-selfcheck.spec.mjs`.
// They live HERE and not in the spec file because importing a spec file
// re-executes its top-level `test.describe`, which registers its tests a
// second time under the importing file — so the stash suite would have run
// twice, and the self-check's run of it would have popped the fixture the
// real spec needs.

/** The entry that cannot be applied cleanly. Newest, so it is the first row. */
export const CONFLICTING_SUBJECT = 'will not apply cleanly'
/** The path that entry collides on. */
export const CONFLICTING_PATH = 'collision.txt'
/** Left in the working tree by the fixture and never stashed by it. */
export const UNTRACKED = '1 untracked file'

/**
 * Open the stash repo in FULL mode.
 *
 * Not Visualize like `helpers.openApp`: this spec writes. The mode matters for
 * a second reason worth stating — in Visualize the drawer deliberately still
 * lists and still inspects, but every write is refused with a reason, so a
 * spec that landed in the wrong mode would find the Pop button rendered as a
 * `<span>` and fail with a confusing "not a button".
 */
export async function openStashRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const entry = page.getByRole('button', { name: /stash-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  const full = page.getByRole('button', { name: /full git operations/ })
  if (await full.isVisible().catch(() => false)) {
    await full.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
}

/**
 * Open the Activity panel and return the drawer's own region.
 *
 * Returns the region rather than nothing, and every caller queries INSIDE it.
 * A page-wide `getByText` cannot be used here: git copies a commit's subject
 * verbatim into its `WIP on <branch>: <sha> <subject>` stash message, so the
 * fixture's stash subject is also the seed commit's subject and the same string
 * resolves in the graph's SVG `<title>`, in the activity feed, AND in the
 * drawer — four elements, which Playwright refuses to guess between. That is a
 * real failure this suite caught on its first run against a browser; the
 * assertion was right and the reader was wrong.
 */
export async function openDrawer(page) {
  await page.getByRole('button', { name: /activity/i }).first().click()
  const drawer = page.getByRole('region', { name: 'Stashes' })
  await expect(drawer).toBeVisible()
  // Wait on a ROW inside the drawer, not on the region: the region renders
  // before the fetch resolves, so asserting on it alone would let a spec
  // proceed against "Loading stashes…".
  await expect(drawer.getByText(CONFLICTING_SUBJECT)).toBeVisible({ timeout: 20_000 })
  return drawer
}

// --- #594: the graph preview inside a confirmation ------------------------
//
// Shared between `preview-panel.spec.mjs` and `harness-selfcheck.spec.mjs`,
// for the reason the stash helpers above are: importing a spec file
// re-registers its tests under the importing file, so the self-check would
// run the real spec a second time.

/** The branch the merge-preview fixture offers to merge. */
export const PREVIEW_BRANCH = 'feature'
/** The branch it would be merged into — the one that fixture checks out. */
export const PREVIEW_INTO = 'main'
/** The repository label its accepted history Frame renders above the graph. */
const PREVIEW_REPO = 'merge-preview-repo'

/**
 * Open the merge-preview repo in FULL mode.
 *
 * Not Visualize like `openApp`: a merge is a write, so in Visualize the menu
 * offers no merge item at all and there would be no confirmation to hang a
 * preview off. `api::preview_request` refuses in that mode too, deliberately —
 * see its module doc.
 */
export async function openMergePreviewRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const entry = page.getByRole('button', { name: /merge-preview-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  const full = page.getByRole('button', { name: /full git operations/ })
  if (await full.isVisible().catch(() => false)) {
    await full.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  // A node alone is not readiness: the previous repository's graph remains
  // attached briefly while `/api/select` settles and the new history epoch
  // starts. The repo line is rendered from the accepted Frame in the same
  // Ready arm that mounts its graph, so it identifies whose nodes follow.
  await expect(page.locator('p.status.repo')).toContainText(PREVIEW_REPO, {
    timeout: 20_000,
  })
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
}

/**
 * Open the context menu on whichever commit carries `branch`, and return it.
 *
 * Found by walking the nodes rather than by reading a badge. The badge is an
 * SVG `<text>` beside the dot, not a child of it, so mapping badge -> node
 * would mean matching on coordinates; and the branch items are built from what
 * the MENU knows (`MenuData::branches`), which is the thing under test here
 * anyway. Walking asks the app the question directly.
 *
 * Throws with the branch named rather than timing out anonymously: a fixture
 * whose branch is missing is a fixture problem, and "no such menu item" is a
 * far more useful message than a 30-second wait on a locator.
 */
export async function openBranchMenu(page, branch) {
  const nodes = page.locator('circle.node-hit')
  const count = await nodes.count()
  for (let i = 0; i < count; i++) {
    await nodes.nth(i).click()
    const item = page.getByRole('button', { name: new RegExp(`Merge \u2018${branch}\u2019`) })
    if (await item.isVisible().catch(() => false)) {
      return item
    }
    await page.keyboard.press('Escape')
  }
  throw new Error(
    `no commit in this graph carries the branch ${branch} — ` +
      `walked ${count} nodes and none offered a merge item for it`,
  )
}

/** The preview panel's heading, when a picture (or a pending one) is showing. */
export const PREVIEW_HEADING = 'What this would do'

// --- #548: the worktree drawer ---------------------------------------------

/** The drawer's landmark label — mirrors `worktrees::view::DRAWER_REGION_LABEL`. */
export const WORKTREE_REGION_LABEL = 'Worktrees'

/** The fence sentence, mirrored from `Serviceable::refusal` and pinned there
 *  too (`the_fence_sentence_is_the_one_the_issue_names`), so a reword is a
 *  deliberate edit in both places rather than a spec failing for a reason
 *  nobody expects. */
export const OUTSIDE_ROOTS_SENTENCE =
  'This worktree is outside the folders you allowed, so it cannot be opened.'

/**
 * Open the worktree repo in FULL mode.
 *
 * Full, not Visualize, for the reason `openStashRepo` is: this spec switches
 * the served repository, and the drawer inherits the session's mode rather
 * than escalating it — so a spec that landed in Visualize would be asserting
 * about a different posture than the one it means to.
 */
export async function openWorktreeRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const entry = page.getByRole('button', { name: /worktree-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  const full = page.getByRole('button', { name: /full git operations/ })
  if (await full.isVisible().catch(() => false)) {
    await full.click()
  }
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
}

/**
 * Open the Activity panel and return the worktree drawer's own region.
 *
 * Scoped for the reason `openDrawer` is: a branch name shown in the drawer is
 * also drawn in the graph's SVG titles, so a page-wide locator would resolve
 * to several elements and Playwright would refuse to guess.
 *
 * Waits on a ROW, not on the region: the region renders before the census
 * fetch resolves, so asserting on it alone would let a spec proceed against
 * "Loading worktrees…".
 */
export async function openWorktreeDrawer(page) {
  await page.getByRole('button', { name: /activity/i }).first().click()
  const drawer = page.getByRole('region', { name: WORKTREE_REGION_LABEL })
  await expect(drawer).toBeVisible()
  await expect(drawer.getByText('desk-two', { exact: true })).toBeVisible({ timeout: 20_000 })
  return drawer
}
