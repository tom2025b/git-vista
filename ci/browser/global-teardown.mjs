// Kill the test server and remove the run's temp tree.
//
// Best-effort: a failure here must not turn a green run red. But it must be
// loud, and it must never signal a process it has not identified.

import { readFileSync, rmSync } from 'node:fs'

import { RUNTIME_FILE, STORAGE_FILE, looksLikeOurServer } from './global-setup.mjs'

export default async function globalTeardown() {
  let runtime
  try {
    runtime = JSON.parse(readFileSync(RUNTIME_FILE, 'utf8'))
  } catch {
    return // setup never got far enough to leave one
  }

  // Identity before signal. PIDs are recycled, and a stale record's number can
  // belong to anything by the time we read it -- including something of the
  // operator's. `looksLikeOurServer` reads /proc/<pid>/cmdline, so this is
  // "signal this program", not "signal whatever holds this number".
  if (runtime.pid) {
    if (looksLikeOurServer(runtime.pid)) {
      try {
        process.kill(runtime.pid, 'SIGTERM')
      } catch (e) {
        console.warn(`[teardown] could not signal server pid ${runtime.pid}: ${e.message}`)
      }
    } else {
      console.warn(
        `[teardown] pid ${runtime.pid} is not our server (exited, or the pid was ` +
          `recycled) — not signalling it`,
      )
    }
  }

  for (const p of [runtime.work, RUNTIME_FILE, STORAGE_FILE]) {
    if (p) rmSync(p, { recursive: true, force: true })
  }
}
