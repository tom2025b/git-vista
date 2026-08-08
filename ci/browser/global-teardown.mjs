// Kill the test server and remove the run's temp tree. Best-effort: a failure
// here must not turn a green run red, but it must be loud enough to notice.

import { readFileSync, rmSync } from 'node:fs'

import { RUNTIME_FILE, STORAGE_FILE } from './global-setup.mjs'

export default async function globalTeardown() {
  let runtime
  try {
    runtime = JSON.parse(readFileSync(RUNTIME_FILE, 'utf8'))
  } catch {
    return // setup never got far enough to leave one
  }
  try {
    process.kill(runtime.pid, 'SIGTERM')
  } catch (e) {
    console.warn(`[teardown] could not signal server pid ${runtime.pid}: ${e.message}`)
  }
  for (const p of [runtime.work, RUNTIME_FILE, STORAGE_FILE]) {
    rmSync(p, { recursive: true, force: true })
  }
}
