// Build the fixture, start a dedicated server, spend the one-time bootstrap
// token once, and save the resulting session for every spec to reuse.
//
// The token is single-use by design (it is exchanged for an HttpOnly session
// cookie), so signing in per-test is impossible without restarting the server
// per-test. Doing it once here and reusing `storageState` is the pattern that
// fits that constraint.

import { chromium } from '@playwright/test'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { buildFixture } from './fixture.mjs'
import { startServer } from './server.mjs'

export const RUNTIME_FILE = join(import.meta.dirname, '.runtime.json')
export const STORAGE_FILE = join(import.meta.dirname, '.storage.json')

export default async function globalSetup() {
  const work = mkdtempSync(join(tmpdir(), 'gv-browser-'))
  const fixture = buildFixture(join(work, 'fixture-repo'))
  const { child, base, signInUrl } = await startServer({
    repoPath: fixture.root,
    stateHome: join(work, 'state'),
  })

  const browser = await chromium.launch()
  const page = await browser.newPage()
  try {
    const failures = []
    page.on('response', (r) => {
      if (r.url().includes('/api/') && !r.ok()) failures.push(`${r.status()} ${r.url()}`)
    })
    await page.goto(signInUrl)
    // The app trades the fragment token for a cookie during startup. Waiting on
    // the topbar means the wasm bundle booted AND rendered; waiting on the
    // cookie alone would pass even if the app never mounted.
    await page.getByRole('heading', { name: 'git-vista' }).waitFor({ timeout: 30_000 })

    // The exchange is a POST that resolves after first paint, so poll rather
    // than sampling once.
    let cookies = []
    for (let i = 0; i < 60 && cookies.length === 0; i++) {
      cookies = await page.context().cookies()
      if (cookies.length === 0) await new Promise((r) => setTimeout(r, 250))
    }
    if (cookies.length === 0) {
      // Say WHY, with the evidence, rather than only that it happened.
      const body = (await page.evaluate(() => document.body.innerText)).slice(0, 600)
      throw new Error(
        'sign-in produced no cookie — the bootstrap exchange failed\n' +
          `failed /api/ responses: ${failures.length ? failures.join(', ') : '(none)'}\n` +
          `page text:\n${body}`,
      )
    }
    await page.context().storageState({ path: STORAGE_FILE })
  } finally {
    await browser.close()
  }

  writeFileSync(
    RUNTIME_FILE,
    JSON.stringify({ base, pid: child.pid, work, fixture }, null, 2),
  )
  // Let the server outlive this process; global teardown kills it by pid.
  child.unref()
}
