// Stand up a git-vista server the browser tests own outright.
//
// It never touches the operator's running instance: its own port, its own
// XDG_STATE_HOME (so its own bootstrap token and log), and a repository list
// naming only the throwaway fixture. Killing it affects nothing else.

import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'

/** The port the server binds. NOT configurable, and deliberately the same 8080
 *  the operator's server uses: `state.rs` compiles the address in and
 *  `parse_bind_addr` refuses anything else, because listening beyond loopback is
 *  a security decision rather than a setting. `run.sh` gives this process tree
 *  its own network namespace, so this 8080 and the host's are different
 *  interfaces that cannot see each other. */
export const TEST_PORT = 8080

export const SERVER_BIN = join(
  process.env.GV_REPO_ROOT || join(import.meta.dirname, '..', '..'),
  'target', 'debug', 'git-vista-server',
)

/**
 * Spawn the server and wait until it answers.
 *
 * Returns the child, the base URL, and the one-time bootstrap token read from
 * the state dir. The token is read from the file rather than scraped from
 * stdout because the file is the contract `gv` itself uses, and stdout parsing
 * breaks silently whenever the startup banner is reworded.
 */
export async function startServer({ repoPath, stateHome, extraRepos = [], port = TEST_PORT }) {
  if (!existsSync(SERVER_BIN)) {
    throw new Error(
      `server binary missing at ${SERVER_BIN}\n` +
        `build it first:  cargo build -p git-vista-server`,
    )
  }
  mkdirSync(stateHome, { recursive: true })

  // The repository to open is the first positional argument; without it the
  // server falls back to its working directory and serves whatever that happens
  // to be, in degraded mode if it is not a git repository.
  const child = spawn(SERVER_BIN, [repoPath], {
    cwd: repoPath,
    env: {
      ...process.env,
      XDG_STATE_HOME: stateHome,
      // `:`-separated, matching PATH — see state.rs's `repo_list`.
      GIT_VISTA_REPOS: [repoPath, ...extraRepos].join(':'),
      // Keep the fixture's own hooks out of it; the tests assert on git state,
      // not on hook behaviour.
      GIT_CONFIG_GLOBAL: '/dev/null',
      GIT_CONFIG_SYSTEM: '/dev/null',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })

  let log = ''
  child.stdout.on('data', (d) => (log += d))
  child.stderr.on('data', (d) => (log += d))
  child.on('exit', (code, signal) => {
    if (code !== 0 && code !== null) {
      console.error(`[gv-test-server] exited ${code}/${signal}\n${log}`)
    }
  })

  const base = `http://localhost:${port}`
  const deadline = Date.now() + 30_000
  for (;;) {
    if (child.exitCode !== null) {
      throw new Error(`server exited before it listened (code ${child.exitCode})\n${log}`)
    }
    try {
      const r = await fetch(`${base}/api/protocol`)
      if (r.ok) break
    } catch {
      // not listening yet
    }
    if (Date.now() > deadline) {
      child.kill('SIGKILL')
      throw new Error(`server did not listen on ${port} within 30s\n${log}`)
    }
    await sleep(200)
  }

  const tokenPath = join(stateHome, 'git-vista', 'bootstrap.token')
  const token = readFileSync(tokenPath, 'utf8').trim()
  if (!/^[0-9a-f]{64}$/.test(token)) {
    child.kill('SIGKILL')
    throw new Error(`bootstrap token at ${tokenPath} is not 64 hex chars`)
  }

  return { child, base, token, signInUrl: `${base}/#s=${token}`, log: () => log }
}
