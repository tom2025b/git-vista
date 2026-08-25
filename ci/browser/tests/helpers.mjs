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

/**
 * Load the app and get as far as a rendered history graph.
 *
 * The server is started with the fixture as its repository, but the picker may
 * still be shown; handle both without branching on timing, which is the usual
 * source of flakiness here.
 */
export async function openApp(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const pickerEntry = page.getByRole('button', { name: /fixture-repo/i }).first()
  if (await pickerEntry.isVisible().catch(() => false)) {
    await pickerEntry.click()
  }

  // The mode dialog appears whenever a repository is (re)opened. "Visualize"
  // is read-only, which is all these tests need and cannot mutate the fixture.
  //
  // Match on "look only", not on /Visualize/: the topbar carries a mode BADGE
  // also labelled "Visualize", and it sits behind this dialog. A loose match
  // resolves to the badge and then waits forever for an element the dialog is
  // covering -- which is exactly what a 30s timeout looked like here.
  const visualize = page.getByRole('button', { name: /look only/ })
  if (await visualize.isVisible().catch(() => false)) {
    await visualize.click()
  }

  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
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

/** Open the Activity panel and wait for the Stashes section to have rendered. */
export async function openDrawer(page) {
  await page.getByRole('button', { name: /activity/i }).first().click()
  await expect(page.getByText('Stashes', { exact: true })).toBeVisible()
  // Wait on a ROW, not on the heading: the heading renders before the fetch
  // resolves, so asserting on it would let a spec proceed against a drawer
  // still showing "Loading stashes…" — and every assertion below would then
  // fail for the wrong reason.
  await expect(page.getByText(CONFLICTING_SUBJECT)).toBeVisible({ timeout: 20_000 })
}

