// #392 -- a sign-in link pasted over the URL of a LIVE tab must sign it in.
//
// Why this spec exists: `bootstrap_fragment::token_in_fragment` is host-tested
// and its test proves the decision (only a non-empty `s=` is a token). It
// cannot prove the decision is REACHED. The consumer is the `hashchange`
// listener in `session.rs`, installed from `app/mod.rs` -- both
// `#[cfg(target_arch = "wasm32")]`, which `cargo test` never compiles. Same
// shape as every row of this suite's README table.
//
// The defect was silence: editing only the fragment is a same-document
// navigation, so nothing reloaded, startup never re-ran, and the token sat
// visibly in the address bar doing nothing. That is the exact motion a server
// restart forces, because a restart rotates the token.
//
// # Why each negative test ends by reloading on purpose
//
// "Nothing happened" is the weakest possible assertion: it also passes when
// the detector is broken, or when the page simply had not got there yet. So
// each negative drives a real token through the same detector immediately
// afterwards and requires the reload it just proved absent. If the sentinel
// could never die, the second half fails and the negative is not believed.
// `harness-selfcheck.spec.mjs` makes the same demand of the positive test.

import { expect, test } from '@playwright/test'

import { markPage, openApp, pageSurvived, runtime, setHash, watchSignIns } from './helpers.mjs'

/** Shaped like the real thing (64 lowercase hex, per the server's
 *  `SECRET_BYTES`) but not it: the one bootstrap token was spent by global
 *  setup and is single-use, so no spec can hold a live one. It does not need
 *  to be live -- the defect is that the exchange is never ATTEMPTED. */
const DEAD_TOKEN = 'a'.repeat(64)

test.describe('#392 a token pasted into a live tab', () => {
  test('re-runs sign-in, carrying the pasted token', async ({ page }) => {
    await openApp(page)
    const posts = watchSignIns(page)
    await markPage(page)
    expect(await pageSurvived(page), 'the sentinel must be set before the paste').toBe(true)

    await setHash(page, `#s=${DEAD_TOKEN}`)

    // The reload is the whole fix: startup is the only path that redeems a
    // token, and only a reload re-runs it.
    await expect.poll(() => pageSurvived(page)).toBe(false)

    // ...and it redeemed THIS token, not merely re-checked the cookie. A
    // reload that skipped the exchange would leave the pasted link unspent.
    await expect.poll(() => posts.some((body) => body.includes(DEAD_TOKEN))).toBe(true)

    // A dead token must not cost the tab its session: `establish_session`
    // falls through to `GET /api/session`, the cookie is still live, and the
    // app comes back rather than dropping to the sign-in screen.
    await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

    // The token is stripped on redemption, so it is not left sitting in the
    // address bar (or in history) after the reload it caused.
    await expect.poll(() => page.evaluate(() => window.location.hash)).toBe('')
  })

  test('a fragment carrying no token leaves the tab alone', async ({ page }) => {
    await openApp(page)
    const posts = watchSignIns(page)
    await markPage(page)

    await setHash(page, '#tab=diff')
    await page.waitForTimeout(1000)

    expect(await pageSurvived(page), 'a tokenless fragment must not reload').toBe(true)
    expect(posts, 'a tokenless fragment must not spend a sign-in attempt').toEqual([])

    // Positive control: the same detector, the same tab, a real token.
    await setHash(page, `#s=${DEAD_TOKEN}`)
    await expect.poll(() => pageSurvived(page)).toBe(false)
  })

  test('an empty s= leaves the tab alone', async ({ page }) => {
    await openApp(page)
    const posts = watchSignIns(page)
    await markPage(page)

    // `s=` with nothing after it could never sign anyone in, so reloading on
    // it would destroy a working tab's state for nothing.
    await setHash(page, '#s=')
    await page.waitForTimeout(1000)

    expect(await pageSurvived(page), 'an empty token must not reload').toBe(true)
    expect(posts, 'an empty token must not spend a sign-in attempt').toEqual([])

    await setHash(page, `#s=${DEAD_TOKEN}`)
    await expect.poll(() => pageSurvived(page)).toBe(false)
  })

  test('the suite is driving the server this spec assumes', async () => {
    // Cheap guard against a green run over a stale `.runtime.json` -- the same
    // failure mode `harness-selfcheck` exists for.
    expect(runtime().base).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):8080/)
  })
})
