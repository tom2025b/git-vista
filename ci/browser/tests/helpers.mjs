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
