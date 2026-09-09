// #396 (split from #72) — does a half-typed commit message survive the page
// being thrown away and rebuilt?
//
// WHAT THIS PROVES, AND WHAT IT DOES NOT. The issue asks about iPad Safari
// suspending a background tab: WebKit discards the page and rebuilds it from
// scratch on return, keeping only what the app wrote to storage. Chromium on a
// Linux box cannot reproduce that, and this file does not pretend to. It runs
// the two closest things it CAN run, each of which destroys every in-memory
// signal and keeps only `localStorage`:
//
//   1. `page.reload()` — a fresh document, a fresh WASM module, the same
//      browser context. The same shape as a tab rebuilt on return.
//   2. A brand-new browser context seeded from the old one's `storageState()`
//      — a clean context that has never held the draft in its app signals.
//      This checks reconstruction from a serialized storage snapshot; it does
//      NOT prove Chromium flushed and recovered that snapshot from disk after
//      a browser-process kill.
//
// Real-iPad verification (the issue's second criterion) still needs an iPad,
// and stays open. See the verification report linked from the PR.
//
// Why a browser test at all: everything that DECIDES what to store and how to
// read it back is host-tested (`features/dialogs/commit.rs`), but the
// `localStorage` calls, the `set_draft_scope` effect that reads storage on the
// first Frame, and the Restore/Discard wiring are all `#[cfg(target_arch =
// "wasm32")]` — `signals.rs` says outright that nothing in the repository
// checks them. This suite is the only harness that reaches them.
//
// Every assertion here is required to go red in `harness-selfcheck.spec.mjs`:
// once with storage wiped before the reload (the mechanism removed), once with
// the box silently filled behind a correct banner (the mechanism weakened).

import { expect, test } from '@playwright/test'

import {
  DRAFT_KEY_PREFIX,
  draftBanner,
  expectDraftOffered,
  forceOnline,
  messageBox,
  runtime,
  storedDraftKeys,
} from './helpers.mjs'

/** Longer than the banner's 40-char preview, so "previewed" and "restored in
 *  full" are two different checks — a banner that showed the WHOLE text would
 *  pass the preview assertion only if the preview cut were broken, and a
 *  Restore that filled in only the preview would fail the value check. */
const DRAFT = 'wip(#396): typed before the tab was thrown away, then rebuilt'

/**
 * Open `fixture-repo` in ACTIVE mode. Not `helpers.openApp`, which chooses
 * Visualize: the menu hides every write item — "Commit Changes" included —
 * when `frame.read_only` is set, and the server sets it for exactly
 * `mode == Visualize` (state.rs `current()`). Nothing here submits, so the
 * shared fixture is never mutated.
 *
 * Every step waits for a state the app guarantees rather than sampling one
 * (#623/#644): the picker is always shown, the mode dialog always follows.
 */
async function openRepoActive(page) {
  await forceOnline(page)
  await page.goto(runtime().base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()
  const entry = page.getByRole('button', { name: /fixture-repo/i }).first()
  await expect(entry, 'the picker lists the fixture repository').toBeVisible({ timeout: 20_000 })
  await entry.click()
  const active = page.getByRole('button', { name: /full git operations/ })
  await expect(active, 'the mode dialog follows opening a repository').toBeVisible({
    timeout: 20_000,
  })
  await active.click()
  await expect(entry, 'the picker must be dismissed, not merely unsampled').toHaveCount(0)
  await expect(active, 'the mode dialog must be dismissed, not merely unsampled').toHaveCount(0)
  await expect(page.locator('p.status.repo')).toContainText(/fixture-repo/i, { timeout: 20_000 })
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
}

/** Tap HEAD (index 0, newest first — `reachability.spec.mjs` pins this) and
 *  choose "Commit Changes". Anchored at the END: the item's accessible name
 *  starts with its nerd-font glyph, and the disabled variant's aria-label
 *  goes on to name its reason — so `$` resolves only the enabled item. */
async function openCommitDialog(page) {
  await page.locator('circle.node-hit').nth(0).click()
  await page.getByRole('button', { name: /Commit Changes$/ }).click()
  await expect(messageBox(page), 'the commit modal must mount').toBeVisible()
}

/**
 * Restore must fill the box with the WHOLE draft. On failure, say what the
 * modal looked like: this step failed ONCE in the first fifteen local runs (banner
 * state unknown, box empty for the full 10s), and could not be reproduced —
 * whoever meets it next should not have to guess which of "the click never
 * reached the handler", "the offer was re-seeded from storage" or "Discard
 * was hit" it was.
 */
async function expectRestoredOrExplain(page) {
  try {
    await expect(messageBox(page), 'Restore fills the box with the WHOLE draft').toHaveValue(DRAFT)
  } catch (e) {
    const diag = {
      bannerCount: await draftBanner(page).count(),
      storedKeys: await storedDraftKeys(page),
      statusTexts: await page.getByRole('status').allTextContents(),
      boxValue: await messageBox(page).inputValue().catch(() => '<no box>'),
    }
    throw new Error(`${e.message}\n\nmodal state at failure: ${JSON.stringify(diag, null, 2)}`)
  }
}

test.describe('#396 a commit draft survives the page being rebuilt', () => {
  test('typed → written to localStorage → offered after a reload and in a fresh context → restore fills the box → discard removes it', async ({
    page,
    context,
    browser,
  }) => {
    // Four app loads of up to 20s each is more than the 30s default.
    test.setTimeout(180_000)

    await test.step('precondition: no draft on record, and typing writes one', async () => {
      await openRepoActive(page)
      expect(await storedDraftKeys(page), 'a clean fixture has no draft in storage').toEqual([])
      await openCommitDialog(page)
      await expect(draftBanner(page), 'no banner over an empty store').toHaveCount(0)

      // `fill` dispatches `input`, which is the event `set_message` persists on.
      await messageBox(page).fill(DRAFT)
      await expect(messageBox(page)).toHaveValue(DRAFT)

      const keys = await storedDraftKeys(page)
      expect(keys, 'exactly one draft key, scoped to this repository').toHaveLength(1)
      expect(keys[0].length, 'the key carries a worktree id, not an empty scope').toBeGreaterThan(
        DRAFT_KEY_PREFIX.length,
      )
      const stored = await page.evaluate((k) => JSON.parse(window.localStorage.getItem(k)), keys[0])
      expect(stored.message, 'the stored record carries the full text').toBe(DRAFT)
      expect(typeof stored.saved_at_ms, 'the stored record carries a timestamp').toBe('number')
    })

    await test.step('proxy 1 — reload: a fresh document offers the draft, never auto-fills', async () => {
      await page.reload()
      await openRepoActive(page)
      await openCommitDialog(page)
      await expectDraftOffered(page, DRAFT)

      await page.getByRole('button', { name: 'Restore the saved draft into the message box' }).click()
      await expectRestoredOrExplain(page)
      await expect(draftBanner(page), 'Restore dismisses the banner').toHaveCount(0)
      // Restore does not touch storage (signals.rs): the copy is still there
      // for the next rebuild, which is what step 3 relies on.
      expect(await storedDraftKeys(page)).toHaveLength(1)
    })

    let survivor
    await test.step('proxy 2 — a clean browser context seeded from serialized storage offers it too', async () => {
      // Cookies (the session) and localStorage (the draft), but no app signals.
      const state = await context.storageState()
      const fresh = await browser.newContext({ storageState: state })
      survivor = await fresh.newPage()
      await openRepoActive(survivor)
      await openCommitDialog(survivor)
      await expectDraftOffered(survivor, DRAFT)
    })

    await test.step('Discard removes the stored draft, and a further rebuild has nothing to offer', async () => {
      await survivor.getByRole('button', { name: 'Discard the saved draft' }).click()
      await expect(draftBanner(survivor), 'Discard dismisses the banner').toHaveCount(0)
      await expect(messageBox(survivor), 'Discard leaves the (already empty) box empty').toHaveValue('')
      expect(await storedDraftKeys(survivor), 'Discard deletes the key').toEqual([])

      // The negative control that keeps the positive one honest: the banner
      // is contingent on storage, not a fixture of the dialog.
      await survivor.reload()
      await openRepoActive(survivor)
      await openCommitDialog(survivor)
      await expect(draftBanner(survivor), 'nothing stored, nothing offered').toHaveCount(0)
      await expect(messageBox(survivor)).toHaveValue('')
      await survivor.context().close()
    })
  })
})
