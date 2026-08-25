// Does this harness actually catch anything?
//
// A test that has never been observed to fail is a hypothesis, not a check.
// This repository has shipped six of them, and the four defects that motivated
// the whole `ci/browser/` suite each sat behind a green, correct, useless test.
// So the suite checks ITSELF here: each assertion below is run against a DOM
// deliberately broken in the exact way the real test claims to detect, and is
// required to FAIL.
//
// The mutation is applied to the live DOM rather than to source, which means no
// rebuild, no writes to the working tree, and a runtime of seconds. It proves
// the assertion is load-bearing. It does NOT prove the assertion would catch a
// source-level regression that produces a different DOM shape -- for that, see
// the mutation-testing path used for the Rust core.

import { expect, test } from '@playwright/test'

import { DIFF_SCROLLER, forceOnline, markPage, openApp, openDiff, pageSurvived, runtime, setHash } from './helpers.mjs'

/** Open the #478 interleaved-twin repository. Duplicated from
 *  `wip-collapse.spec.mjs` rather than imported, for the same reason every
 *  other mutation here is written out: this file must be able to fail on its
 *  own terms, and a shared opener that broke would silently turn these
 *  self-checks into "the page never loaded" passes. */
async function openTwinRepo(page) {
  await forceOnline(page)
  await page.goto(runtime().base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()
  const entry = page.getByRole('button', { name: /interleaved-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()
  const visualize = page.getByRole('button', { name: /look only/ })
  if (await visualize.isVisible().catch(() => false)) await visualize.click()
  await expect(page.locator('p.status.repo')).toContainText(/interleaved-repo/i)
  await expect(page.locator('.wip-group')).toHaveCount(2)
}

/** Open the #77 stash repository and its drawer. Duplicated rather than
 *  imported from `helpers.mjs`, for the same reason `openTwinRepo` above is:
 *  this file must be able to fail on its own terms, and a shared opener that
 *  broke would silently turn these self-checks into "the page never loaded"
 *  passes.
 *
 *  Opened in VISUALIZE, not full mode. The self-check only reads the drawer,
 *  and a read-only session cannot mutate the fixture that
 *  `stash-drawer.spec.mjs` — which runs after this file — depends on. */
async function openStashDrawer(page) {
  await forceOnline(page)
  await page.goto(runtime().base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()
  const entry = page.getByRole('button', { name: /stash-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()
  const visualize = page.getByRole('button', { name: /look only/ })
  if (await visualize.isVisible().catch(() => false)) await visualize.click()
  await expect(page.locator('p.status.repo')).toContainText(/stash-repo/i)
  await page.getByRole('button', { name: /activity/i }).first().click()
  // Wait on a ROW, not the heading: the heading renders before the fetch
  // resolves, so this would otherwise proceed against "Loading stashes…" and
  // the mutation below would be applied to an empty drawer.
  await expect(page.getByText('will not apply cleanly')).toBeVisible({ timeout: 20_000 })
}

/**
 * Run `fn` and return the message it threw, or `null` if it did not throw.
 *
 * Returning the MESSAGE rather than a boolean is the whole point. A bare
 * "did it throw" check accepts any exception at all -- a navigation timeout, a
 * typo in a selector, a missing fixture -- as proof that the named assertion
 * detected the named defect. It does not. Each caller below therefore matches
 * the failure it expected, so a self-check can only pass for the right reason.
 * Raised in adversarial review of this file.
 */
async function failureMessage(fn) {
  try {
    await fn()
    return null
  } catch (e) {
    return String(e?.message ?? e)
  }
}

/** Assert `fn` failed, AND failed with the expected signature. */
function expectFailedBecause(message, pattern, what) {
  expect(message, `${what} must FAIL after the mutation, but it passed`).not.toBeNull()
  expect(
    message,
    `${what} failed, but for the wrong reason — expected ${pattern}, got:\n${message}`,
  ).toMatch(pattern)
}

test.describe('harness self-check — every assertion must be able to go red', () => {
  test('the accessible-list assertion fails when the labels are stripped', async ({ page }) => {
    await openApp(page)
    await page.getByRole('button', { name: 'Activity' }).click()
    await expect(page.getByRole('listitem').first()).toBeAttached({ timeout: 15_000 })

    // The mutation: exactly the defect #68d shipped -- rows present, names gone.
    await page.evaluate(() => {
      for (const el of document.querySelectorAll('[role="listitem"]')) {
        el.removeAttribute('aria-label')
      }
    })

    const msg = await failureMessage(async () => {
      const labels = await page
        .getByRole('listitem')
        .evaluateAll((els) => els.map((e) => e.getAttribute('aria-label')))
      for (const l of labels) expect(l, 'every status row needs an aria-label').toBeTruthy()
    })
    expectFailedBecause(msg, /every status row needs an aria-label/, 'the status-row assertion')
  })

  test('the bounded-window assertion fails when the window is unbounded', async ({ page }) => {
    await openApp(page)
    await openDiff(page, 1)

    // The mutation: exactly the defect #69c left in place -- every line mounted.
    // Injecting 2000 spans simulates the un-virtualized rendering the real test
    // exists to forbid.
    await page.evaluate((sel) => {
      const host = document.querySelector(sel)
      const frag = document.createDocumentFragment()
      for (let i = 0; i < 2000; i++) {
        const s = document.createElement('span')
        s.textContent = `injected ${i}`
        frag.appendChild(s)
      }
      host.appendChild(frag)
    }, DIFF_SCROLLER)

    const msg = await failureMessage(async () => {
      const n = await page.evaluate(
        (sel) => document.querySelectorAll(`${sel} span`).length,
        DIFF_SCROLLER,
      )
      expect(n, 'mounted rows must stay bounded').toBeLessThan(600)
    })
    expectFailedBecause(msg, /mounted rows must stay bounded/, 'the windowing assertion')
  })

  test('the keyboard assertion fails when focus is taken away', async ({ page }) => {
    await openApp(page)
    await openDiff(page, 0)

    await page.evaluate(() => document.querySelector('span.diff-hunk').focus())
    // The mutation: exactly what the long-patch defect does -- focus to <body>.
    //
    // `document.body.focus()` does NOT work here and is a trap worth naming:
    // <body> has no tabindex, so it is not focusable, `.focus()` is a silent
    // no-op, and focus stays exactly where it was. The mutation then tests
    // nothing while looking like it tested something -- which this self-check
    // caught on its first run. `blur()` is the reliable way to send focus back
    // to the document, and is what actually happens when a focused node
    // unmounts.
    await page.evaluate(() => document.activeElement.blur())

    const msg = await failureMessage(async () => {
      const isHunk = await page.evaluate(
        () => document.activeElement.classList?.contains('diff-hunk') ?? false,
      )
      expect(isHunk, 'focus must be on a hunk header').toBe(true)
    })
    expectFailedBecause(msg, /focus must be on a hunk header/, 'the hunk-focus assertion')
  })

  test('the chip assertion fails when the chip carries no name', async ({ page }) => {
    await openApp(page)
    const chip = page.locator('.topbar').getByText(/\d+ staged/)
    await expect(chip).toBeVisible({ timeout: 15_000 })

    await chip.evaluate((el) => {
      for (let e = el; e && e !== document.body; e = e.parentElement) {
        e.removeAttribute('aria-label')
        e.removeAttribute('title')
      }
    })

    const msg = await failureMessage(async () => {
      const name = await chip.evaluate((el) => {
        for (let e = el; e && e !== document.body; e = e.parentElement) {
          const n = e.getAttribute('aria-label') || e.getAttribute('title')
          if (n) return n
        }
        return null
      })
      expect(name, 'the chip must be announceable').toBeTruthy()
    })
    expectFailedBecause(msg, /the chip must be announceable/, 'the announceability assertion')
  })

  test('the #478 two-markers assertion fails when the two are folded into one', async ({
    page,
  }) => {
    await openTwinRepo(page)

    // The mutation is the WRONG FIX the issue names, expressed in the DOM:
    // one marker standing for both chains, carrying their combined count.
    // Relaxing the lane check would produce exactly this — a group claiming a
    // chain that does not exist.
    await page.evaluate(() => {
      const markers = [...document.querySelectorAll('.wip-group')]
      markers[0].querySelector('.wip-group-label').textContent = '\u22ef 8 WIP commits \u22ef'
      markers[1].remove()
    })

    const msg = await failureMessage(async () => {
      const labels = await page.locator('.wip-group-label').allInnerTexts()
      expect(labels, 'each chain must fold into its own marker').toHaveLength(2)
    })
    expectFailedBecause(msg, /each chain must fold into its own marker/, 'the #478 grouping assertion')
  })

  test('the #478 upward-edge assertion fails when that edge is not drawn', async ({ page }) => {
    await openTwinRepo(page)

    // The mutation is the defect itself: `visible_edges` culling an edge whose
    // endpoints arrive out of order, so the line into the folded fork point is
    // never drawn. Removing the upward path from the DOM is what that looks
    // like from outside the app.
    await page.evaluate(() => {
      for (const p of document.querySelectorAll('section.graph svg path')) {
        const n = (p.getAttribute('d') ?? '').match(/-?\d+(\.\d+)?/g)
        if (n && n.length >= 4 && Number(n[n.length - 1]) < Number(n[1])) p.remove()
      }
    })

    const msg = await failureMessage(async () => {
      const spans = await page.evaluate(() =>
        [...document.querySelectorAll('section.graph svg path')]
          .map((p) => (p.getAttribute('d') ?? '').match(/-?\d+(\.\d+)?/g))
          .filter((n) => n && n.length >= 4)
          .map((n) => [Number(n[1]), Number(n[n.length - 1])]),
      )
      expect(
        spans.filter(([from, to]) => to < from),
        'exactly one edge must run upward',
      ).toHaveLength(1)
    })
    expectFailedBecause(msg, /exactly one edge must run upward/, 'the #478 upward-edge assertion')
  })

  test('the #77 pop assertion fails when a conflicted pop claims success', async ({ page }) => {
    // The load-bearing negative of the whole stash slice: a pop that conflicts
    // has applied something and dropped nothing, so a UI reporting "Popped"
    // there has lied about the user's data. `stash-drawer.spec.mjs` asserts
    // that wording is ABSENT, and an absence assertion is the easiest kind to
    // pass for the wrong reason -- it also passes when the notice never renders
    // at all, or when the selector is wrong.
    //
    // The mutation injects the success wording into the live DOM and requires
    // the absence assertion to notice. It deliberately does NOT drive a real
    // pop: a pop mutates the stash repo, this file runs before
    // stash-drawer.spec.mjs (workers: 1, fullyParallel: false, alphabetical),
    // and consuming that fixture here is exactly what broke conflict-panes when
    // the #432 editor shared conflict-repo.
    await openStashDrawer(page)

    // The exact sentence `PopVerdict::Popped` produces, added to a drawer whose
    // pop has NOT completed.
    await page.evaluate(() => {
      const p = document.createElement('p')
      p.className = 'detail-status'
      p.textContent = 'Popped the stash. It has been removed from your stash list.'
      document.body.appendChild(p)
    })

    const msg = await failureMessage(async () => {
      await expect(
        page.getByText(/^Popped the stash/),
        'a conflicted pop must never claim it completed',
      ).toHaveCount(0)
    })
    expectFailedBecause(
      msg,
      /a conflicted pop must never claim it completed/,
      'the #77 pop-completion assertion',
    )
  })

  test('the #392 reload assertion fails when the tab does not reload', async ({ page }) => {
    await openApp(page)
    await markPage(page)

    // The mutation is the defect itself, and needs no DOM surgery: a fragment
    // carrying no token is a hash change the app is *supposed* to ignore, so
    // the document survives -- which is precisely what #392 looked like for
    // every fragment, token or not. If the reload assertion could not tell
    // that apart, `token-paste.spec.mjs` would pass against the unfixed app.
    await setHash(page, '#tab=diff')

    const msg = await failureMessage(async () => {
      await expect
        .poll(() => pageSurvived(page), {
          timeout: 3_000,
          message: 'the tab must have reloaded',
        })
        .toBe(false)
    })
    expectFailedBecause(msg, /the tab must have reloaded/, 'the #392 reload assertion')
  })
})
