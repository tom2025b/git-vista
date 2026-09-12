// Run via ci/browser/run.sh --config ../../crates/git-vista/tests/as_of.playwright.config.mjs
// Uses the existing real server/fixture harness; kept in this issue's allowed test scope.
import { defineConfig, devices } from '../../../ci/browser/node_modules/@playwright/test/index.mjs'
import { STORAGE_FILE } from '../../../ci/browser/global-setup.mjs'

export default defineConfig({
  testDir: '.', testMatch: 'as_of.spec.mjs', workers: 1, retries: 0,
  timeout: 45_000, expect: { timeout: 15_000 }, reporter: 'list',
  outputDir: '/tmp/gv136-browser-results',
  globalSetup: '../../../ci/browser/global-setup.mjs',
  globalTeardown: '../../../ci/browser/global-teardown.mjs',
  use: { ...devices['Desktop Chrome'], storageState: STORAGE_FILE, trace: 'retain-on-failure' },
})
