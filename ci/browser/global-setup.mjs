// Build the fixture, start a dedicated server, spend the one-time bootstrap
// token once, and save the resulting session for every spec to reuse.
//
// The token is single-use by design (it is exchanged for an HttpOnly session
// cookie), so signing in per-test is impossible without restarting the server
// per-test. Doing it once here and reusing `storageState` is the pattern that
// fits that constraint.

import { chromium } from '@playwright/test'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  buildBrokenHeadFixture,
  buildConflictFixture,
  buildEditorFixture,
  buildFixture,
  buildInterleavedWipFixture,
  buildMergePreviewFixture,
  buildNonTextConflictFixture,
  buildStashFixture,
} from './fixture.mjs'
import { startServer } from './server.mjs'

export const RUNTIME_FILE = join(import.meta.dirname, '.runtime.json')
export const STORAGE_FILE = join(import.meta.dirname, '.storage.json')

/** The command a process must be running for us to accept it as our server. */
export const SERVER_PROC_MARKER = 'git-vista-server'

/**
 * Is `pid` still the process we started?
 *
 * A bare `process.kill(pid)` on a stale record is a live hazard: PIDs are
 * recycled, and the number that identified our server last run can identify
 * anything at all this run -- including something of the operator's. Reading
 * `/proc/<pid>/cmdline` costs nothing and turns "signal whatever holds this
 * number" into "signal this program".
 */
export function looksLikeOurServer(pid) {
  try {
    const cmdline = readFileSync(`/proc/${pid}/cmdline`, 'utf8')
    return cmdline.includes(SERVER_PROC_MARKER)
  } catch {
    return false // already gone, or not ours to read
  }
}

/** Remove a previous run's leftovers before starting, so nothing is inherited. */
function clearStaleState() {
  if (existsSync(RUNTIME_FILE)) {
    try {
      const stale = JSON.parse(readFileSync(RUNTIME_FILE, 'utf8'))
      if (stale.pid && looksLikeOurServer(stale.pid)) {
        console.warn(`[setup] killing a leaked server from a previous run (pid ${stale.pid})`)
        process.kill(stale.pid, 'SIGTERM')
      }
      if (stale.work) rmSync(stale.work, { recursive: true, force: true })
    } catch {
      // An unreadable record is itself stale; drop it.
    }
  }
  rmSync(RUNTIME_FILE, { force: true })
  rmSync(STORAGE_FILE, { force: true })
}

export default async function globalSetup() {
  clearStaleState()

  const work = mkdtempSync(join(tmpdir(), 'gv-browser-'))
  const fixture = buildFixture(join(work, 'fixture-repo'))
  // #428: a second, separately-built repository left mid-merge. Served
  // alongside the main fixture so the conflict specs have real stage entries
  // to inspect without putting the shared fixture into MERGING state.
  const conflictFixture = buildConflictFixture(join(work, 'conflict-repo'))
  // #430: a third repository holding the conflicts that cannot be resolved
  // as text (binary/binary and delete/modify). Separate again, because the
  // #428/#429 specs assert an exact conflicted count on conflict-repo.
  const nonTextFixture = buildNonTextConflictFixture(join(work, 'nontext-repo'))
  // #432: a fourth repo for the line-by-line editor, which RESOLVES what it
  // opens — sharing conflict-repo emptied conflict-panes' fixture and failed
  // all four of its tests.
  const editorFixture = buildEditorFixture(join(work, 'editor-repo'))
  // #473: a fifth repo whose HEAD resolves to nothing. Separate because it is
  // deliberately broken — no other spec's repo may be left in this state.
  const brokenHeadFixture = buildBrokenHeadFixture(join(work, 'broken-head-repo'))
  // #478: a sixth repo holding a branch and its DIVERGED remote-tracking twin,
  // whose two checkpoint chains interleave row for row. Separate because it
  // needs a remote — `fixture-repo` has none — and because six more commits in
  // the shared fixture would move the newest-first indices half this suite
  // asserts against. Its bare origin lives beside it under `work` and is not
  // offered to the picker.
  const interleavedFixture = buildInterleavedWipFixture(join(work, 'interleaved-repo'))
  // #77: a seventh repo with three real stash entries, one of which cannot be
  // applied cleanly. Separate because it is the only repo here whose stash list
  // has an asserted count, and because applying that entry leaves collision.txt
  // conflicted — a state no other spec's repo may inherit.
  const stashFixture = buildStashFixture(join(work, 'stash-repo'))
  // #594: an eighth repo, two branches diverged from one base. Separate
  // because every other fixture here is either already up to date with its
  // other branch (so a merge preview would have nothing to draw) or
  // deliberately dirty/conflicted (so it would answer a different question).
  // Nothing ever merges it — the spec opens a confirmation and cancels.
  const mergePreviewFixture = buildMergePreviewFixture(join(work, 'merge-preview-repo'))
  const { child, base, signInUrl } = await startServer({
    repoPath: fixture.root,
    extraRepos: [
      conflictFixture.root,
      nonTextFixture.root,
      editorFixture.root,
      brokenHeadFixture.root,
      interleavedFixture.root,
      stashFixture.root,
      mergePreviewFixture.root,
    ],
    stateHome: join(work, 'state'),
  })

  // Record ownership IMMEDIATELY, before anything that can throw. Writing this
  // only after a successful sign-in (as an earlier version did) means any
  // failure between here and there leaves teardown with no record, and the
  // server and temp tree survive the run -- holding the port and silently
  // breaking the next one.
  writeFileSync(
    RUNTIME_FILE,
    JSON.stringify(
      {
        base,
        pid: child.pid,
        work,
        fixture,
        conflictFixture,
        nonTextFixture,
        editorFixture,
        brokenHeadFixture,
        stashFixture,
        interleavedFixture,
        mergePreviewFixture,
      },
      null,
      2,
    ),
  )

  try {
    const failures = []
    const browser = await chromium.launch()
    try {
      const page = await browser.newPage()
      page.on('response', (r) => {
        if (r.url().includes('/api/') && !r.ok()) failures.push(`${r.status()} ${r.url()}`)
      })
      await page.goto(signInUrl)
      // Waiting on the topbar means the wasm bundle booted AND rendered;
      // waiting on the cookie alone would pass even if the app never mounted.
      await page.getByRole('heading', { name: 'git-vista' }).waitFor({ timeout: 30_000 })

      // The exchange is a POST that resolves after first paint, so poll.
      let cookies = []
      for (let i = 0; i < 60 && cookies.length === 0; i++) {
        cookies = await page.context().cookies()
        if (cookies.length === 0) await new Promise((r) => setTimeout(r, 250))
      }
      if (cookies.length === 0) {
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
  } catch (e) {
    // Clean up everything we created, then re-throw. Without this the run fails
    // AND leaks, and the leak is the more expensive half.
    try {
      child.kill('SIGKILL')
    } catch {
      /* already dead */
    }
    rmSync(work, { recursive: true, force: true })
    rmSync(RUNTIME_FILE, { force: true })
    throw e
  }

  // Let the server outlive this process; global teardown kills it by pid.
  child.unref()
}
