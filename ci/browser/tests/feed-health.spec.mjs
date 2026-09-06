// #663 (M12 follow-up; ADR 0094 §7): the change feed's health, drawn
// permanently and quietly in the topbar.
//
// THE CLAIM UNDER TEST is entirely about the HEALTHY state, not the degraded
// one: "an indicator that appears only when something is wrong is
// indistinguishable, at a glance, from one that has stopped working."
// Asserting only that a degraded state renders would prove nothing about
// that claim — a conditional affordance would pass such a test too. So this
// spec asserts the affordance is present and quiet on a healthy feed FIRST,
// then asserts it changes once the feed degrades, in that order, on the
// SAME connection.
//
// # Why a real local server, not `route.fulfill()`
//
// `route.fulfill()` sends a complete, immediately-closed response — there is
// no way through Playwright's routing API alone to hold an SSE connection
// open and push a second event onto it later. Tried first and measured
// broken: the client's own `onerror` handler (`features/freshness/signals.rs`)
// clears its log on EVERY close, including the ordinary close a fulfilled
// mock produces, and a second snapshot chained through a reconnect gets
// recorded and then wiped again by the very next close-triggered `clear()`
// within the same tick — whichever state a Playwright poll happens to catch
// is a race, not a fact about the app (confirmed: 9 rapid reconnect cycles
// in under 10s, degraded state never once observed).
//
// So this spec runs a tiny real HTTP server (Node's own `http` module) that
// answers with genuine SSE headers and keeps the socket open across two
// writes, and redirects the app's request to it with `route.continue({url})`
// — a real, live, non-mocked connection, held open exactly as long as a real
// server would hold one, with the health changing on it the way it actually
// would.

import http from 'node:http'

import { expect, test } from '@playwright/test'

import { openApp } from './helpers.mjs'

/** One `event: snapshot` SSE block. */
function sseBlock(json) {
  return `event: snapshot\ndata: ${json}\n\n`
}

const WATCHING_SNAPSHOT =
  '{"seq":1,"generation":"gen-1","health":{"state":"watching","watches":5,' +
  '"budget":{"provenance":"undetermined","watches":5}},' +
  '"changed":{"kind":"unknown"},"at":1000}'

const SWEEP_ONLY_SNAPSHOT =
  '{"seq":2,"generation":"gen-1","health":{"state":"sweep_only",' +
  '"reason":{"reason":"unsupported","detail":"no inotify backend"}},' +
  '"changed":{"kind":"unknown"},"at":1001}'

/** A real SSE server: one open connection, held until the test closes it. */
function startFeedServer() {
  const sockets = new Set()
  const server = http.createServer((req, res) => {
    res.writeHead(200, {
      'Content-Type': 'text/event-stream',
      Connection: 'keep-alive',
      'Cache-Control': 'no-cache',
    })
    res.write(sseBlock(WATCHING_SNAPSHOT))
    sockets.add(res)
    res.on('close', () => sockets.delete(res))
  })
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      resolve({
        port: server.address().port,
        degrade() {
          for (const res of sockets) res.write(sseBlock(SWEEP_ONLY_SNAPSHOT))
        },
        async close() {
          for (const res of sockets) res.end()
          await new Promise((r) => server.close(r))
        },
      })
    })
  })
}

test.describe('#663 — the change feed health affordance', () => {
  test('present and quiet while watching, then visibly degrades', async ({ page }) => {
    test.setTimeout(30_000)

    const feed = await startFeedServer()
    try {
      await page.route('**/api/repository/events*', (route) => {
        const url = new URL(route.request().url())
        route.continue({ url: `http://127.0.0.1:${feed.port}${url.search}` })
      })

      await openApp(page)

      const badge = page.locator('.feed-health')
      await expect(badge, 'the affordance must be present on a healthy feed').toBeVisible()

      // The healthy state's own claim: quiet. No visible label competing
      // for attention, and the accessible announcement still names the
      // state explicitly (#663: "announced, not conveyed by colour alone"
      // — this is the half of that claim the resting state itself has to
      // keep).
      await expect(badge.locator('.feed-health-label')).toHaveCount(0)
      await expect(badge.locator('.feed-health-dot.degraded')).toHaveCount(0)
      await expect(badge.locator('.sr-only')).toHaveText('Change feed: watching for repository changes.')

      // Same connection, a real second event — the feed actually degrading,
      // not a fresh connection starting out that way.
      feed.degrade()

      await expect(badge.locator('.feed-health-label'), 'the affordance must change once the feed degrades')
        .toHaveText('Sweep only', { timeout: 10_000 })
      await expect(badge.locator('.feed-health-dot.degraded')).toBeVisible()
      await expect(badge.locator('.sr-only')).toContainText('no live watcher')
      await expect(badge.locator('.sr-only')).toContainText('not supported on this platform: no inotify backend')

      await page.unroute('**/api/repository/events*').catch(() => {})
    } finally {
      await feed.close()
    }
  })
})
