// #360 -- the Update Required overlay must block FOCUS, not only pointers.
//
// The overlay exists because the client has decided it cannot safely parse this
// server's responses. Blocking clicks was verified on #244 by hit-testing four
// viewport points; no keyboard check was run, and the claim "verified
// non-dismissable" was recorded on the strength of the pointer half alone.
//
// It was not non-dismissable. `position: fixed` stops the mouse, but the app's
// controls stay in the DOM *after* the overlay, so Tab walked out into the
// topbar. A user who reaches "Refresh" and presses Enter has bypassed, through
// the keyboard, the exact decision this screen exists to enforce.
//
// That gap lands hardest on the people most likely to see this screen: an
// iPad-first app, a Magic Keyboard user, a VoiceOver user — all navigating by
// focus, for whom the barrier was simply absent.
//
// HOW THIS FORCES THE OVERLAY. `/api/protocol` is intercepted and answered with
// a version window this client falls outside. That drives the real negotiation
// path rather than poking app state, so the test exercises what a genuinely
// mismatched server would produce.

import { expect, test } from '@playwright/test'

import { openApp } from './helpers.mjs'

/** A window far above this client's PROTOCOL_VERSION, so the client reads as
 *  too old. Deliberately not "one above" — a wide gap cannot be mistaken for an
 *  off-by-one in the comparison. */
const FUTURE = {
  protocol_version: 99,
  min_client_protocol: 90,
  max_client_protocol: 99,
  server_version: '99.0.0',
}

async function openWithMismatch(page) {
  // Load the app NORMALLY first. The overlay blocks the mode dialog, so mocking
  // before load would leave openApp() clicking at a covered button — and it
  // would be testing a first-load path the code does not actually describe.
  //
  // This is the scenario the source names: "if the server is redeployed on an
  // incompatible protocol while this tab stays open, the next reload catches
  // it." So: get a working app, make the server incompatible, refresh.
  await openApp(page)

  // The client cache-busts with ?t=<epoch> (api.rs:532), so the pattern must
  // tolerate a query string — '**/api/protocol' alone matches nothing.
  await page.route(/\/api\/protocol(\?|$)/, (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(FUTURE),
    }),
  )

  // Refresh bumps the graph epoch, which is what the protocol resource is keyed
  // on — so this re-runs negotiation exactly as a real reload would.
  await page.getByRole('button', { name: /refresh/i }).first().click()

  const overlay = page.getByRole('alertdialog', { name: 'Update Required' })
  await expect(overlay, 'a protocol mismatch must raise the blocking overlay').toBeVisible({
    timeout: 20_000,
  })
  return overlay
}

test.describe('#360 Update Required blocks focus, not just pointers', () => {
  test('Tab cannot escape the overlay, and the app behind it is inert', async ({ page }) => {
    await openWithMismatch(page)

    // Precondition: the app really is still mounted behind the overlay. If it
    // were unmounted, "focus cannot reach it" would be true for a reason that
    // has nothing to do with this fix, and the test would prove nothing.
    const behind = await page.evaluate(
      () => document.querySelectorAll('.topbar button, .refresh').length,
    )
    expect(
      behind,
      'precondition: the app must still be mounted behind the overlay — otherwise ' +
        'this test passes trivially',
    ).toBeGreaterThan(0)

    // 1. THE APP IS INERT. This is the real mechanism: it removes the app from
    //    the tab order AND the accessibility tree, so a screen reader cannot
    //    reach it either.
    const inertCount = await page.evaluate(() => document.querySelectorAll('[inert]').length)
    expect(
      inertCount,
      'the overlay must mark its siblings inert — without it the app stays in the ' +
        'accessibility tree and VoiceOver walks straight into it',
    ).toBeGreaterThan(0)

    // ...and the overlay itself must NOT be inert, or the Reload button — the
    // one control the user needs — would be unreachable too.
    const overlayInert = await page.evaluate(() =>
      document.querySelector('[role="alertdialog"]').hasAttribute('inert'),
    )
    expect(overlayInert, 'the overlay itself must stay interactive').toBe(false)

    // 2. FOCUS STARTS INSIDE. A keyboard user should not have to hunt for it.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement?.textContent?.trim()), {
        timeout: 5000,
      })
      .toBe('Reload')

    // 3. TAB CANNOT LEAVE. Pressed repeatedly, because a trap that holds once
    //    and then leaks on the third press is not a trap.
    for (let i = 0; i < 6; i++) {
      await page.keyboard.press('Tab')
    }
    const after = await page.evaluate(() => {
      const el = document.activeElement
      return {
        text: el?.textContent?.trim() ?? '',
        insideOverlay: !!el?.closest('[role="alertdialog"]'),
        tag: el?.tagName ?? '',
      }
    })
    expect(
      after.insideOverlay,
      `after 6 Tab presses focus was on <${after.tag}> "${after.text}" — outside the ` +
        'overlay. Tab has walked into the app the overlay exists to block.',
    ).toBe(true)

    // 4. SHIFT+TAB TOO. Backwards is a separate code path in every engine.
    for (let i = 0; i < 4; i++) {
      await page.keyboard.press('Shift+Tab')
    }
    expect(
      await page.evaluate(() => !!document.activeElement?.closest('[role="alertdialog"]')),
      'Shift+Tab must not escape either',
    ).toBe(true)

    console.log(
      '\n#360 focus containment:\n' +
        `  app controls behind overlay : ${behind}\n` +
        `  elements marked inert       : ${inertCount}\n` +
        '  focus after 6x Tab          : inside overlay\n' +
        '  focus after 4x Shift+Tab    : inside overlay\n',
    )
  })

  test('the Reload button is still reachable and operable', async ({ page }) => {
    await openWithMismatch(page)
    // The failure mode of an over-eager trap: nothing is reachable, including
    // the one action offered. A blocking screen with no way out is worse than
    // one that leaks.
    const reload = page.getByRole('button', { name: 'Reload' })
    await expect(reload).toBeVisible()
    await expect(reload).toBeEnabled()
    // Focus containment is asserted in the first test; here the point is simply
    // that the trap did not make the escape hatch unreachable.
    await expect
      .poll(async () => page.evaluate(() => !!document.activeElement?.closest('[role="alertdialog"]')), { timeout: 10000 })
      .toBe(true)
  })
})
