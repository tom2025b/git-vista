// Browser tests for git-vista's rendered output (#355).
//
// These exist because `cargo test` never compiles code behind
// `#[cfg(target_arch = "wasm32")]`, which is where every UI defect found in
// August 2026 lived. The Rust suite proves the pure core is correct; this suite
// proves the core is REACHED.

import { defineConfig, devices } from '@playwright/test'

import { STORAGE_FILE } from './global-setup.mjs'

export default defineConfig({
  testDir: './tests',
  // The box is 2 physical cores with a spinning disk; parallel workers thrash it
  // and the specs share one server anyway.
  workers: 1,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : [['list']],
  timeout: 30_000,
  expect: { timeout: 10_000 },
  globalSetup: './global-setup.mjs',
  globalTeardown: './global-teardown.mjs',
  use: {
    ...devices['Desktop Chrome'],
    storageState: STORAGE_FILE,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
})
