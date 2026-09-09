// Shared by the interaction spec and its harness self-checks. Refs #357.
// Only the HTTP diff/preview boundary is stubbed; the shipped wasm renders
// the patch, handles input, maintains selection, and serializes the plan.
import { expect } from '@playwright/test'
import { forceOnline, runtime } from './helpers.mjs'

const GENERATION = 'diff-v1:357-browser-selection'
const PATCH = `diff --git a/alpha.txt b/alpha.txt
--- a/alpha.txt
+++ b/alpha.txt
@@ -10,4 +10,4 @@
 lead
-old red
+new red
 middle
-old blue
+new blue
@@ -30,3 +30,3 @@
 before second
-old second
+new second
 after second
diff --git a/beta.txt b/beta.txt
--- a/beta.txt
+++ b/beta.txt
@@ -1,3 +1,3 @@
 before beta
-old beta
+new beta
 after beta
`

export const FIRST_HUNK = { index: 0, old_start: 10, new_start: 10 }
export const SECOND_HUNK = { index: 1, old_start: 30, new_start: 30 }
export const BETA_HUNK = { index: 0, old_start: 1, new_start: 1 }
export const FIRST_LINES = [1, 2, 4, 5] // context counts toward local indices

export function lineFile(path, hunks) {
  return { path, selection: { select: 'lines', hunks } }
}

export async function openStagingSelection(page) {
  await page.route('**/api/staging/diff?**', route => {
    expect(new URL(route.request().url()).searchParams.get('direction')).toBe('stage')
    return route.fulfill({ json: { generation: GENERATION, patch: PATCH, truncated: false } })
  })
  await page.route('**/api/staging/preview', route => route.fulfill({
    json: { generation: GENERATION, patch: '', whole_files: [] },
  }))
  await forceOnline(page)
  await page.goto(runtime().base)
  const entry = page.getByRole('button', { name: /fixture-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()
  const active = page.getByRole('button', { name: /full git operations/ })
  await expect(active).toBeVisible()
  await active.click()
  await expect(entry).toHaveCount(0)
  await expect(active).toHaveCount(0)
  await expect(page.locator('p.status.repo')).toContainText('fixture-repo')
  await page.locator('circle.node-hit').first().click()
  await page.getByRole('button', { name: /Select Changes to Stage…/ }).click()
  const viewer = page.locator('.viewer-modal')
  await expect(viewer).toBeVisible()
  await expect(viewer.locator('.stage-hunk-text')).toHaveCount(3)
  await expect(viewer.locator('.stage-line-check')).toHaveCount(8)
  return viewer
}

/** Assert every changed line, including siblings which must stay unselected.
 * Both the accessibility state and the visible glyph read is_line_selected. */
export async function expectLineSelection(viewer, selected, timeout = 10_000) {
  const wanted = Array.from({ length: 8 }, (_, i) => ({
    pressed: String(selected.includes(i)), glyph: selected.includes(i) ? '✓' : '',
  }))
  await expect.poll(() => viewer.locator('.stage-line-check').evaluateAll(nodes =>
    nodes.map(node => ({ pressed: node.getAttribute('aria-pressed'), glyph: node.textContent.trim() })),
  ), { message: '#357 exact line selection and glyphs', timeout }).toEqual(wanted)
  const preview = viewer.getByRole('button', { name: 'Preview', exact: true })
  const clear = viewer.getByRole('button', { name: 'Clear selection', exact: true })
  if (selected.length) {
    await expect(preview).toBeEnabled()
    await expect(clear).toBeEnabled()
  } else {
    await expect(preview).toBeDisabled()
    await expect(clear).toBeDisabled()
  }
}

/** Observe the request emitted by a real Preview click, not an injected plan.
 * Exact files pin line-vs-hunk shape, local indices, anchors and scope. */
export async function expectSelectionPlan(page, viewer, files) {
  const request = page.waitForRequest(req =>
    new URL(req.url()).pathname === '/api/staging/preview' && req.method() === 'POST',
  )
  await viewer.getByRole('button', { name: 'Preview', exact: true }).click()
  const plan = (await request).postDataJSON()
  expect(plan.files, '#357 exact selection plan').toEqual(files)
  expect(plan.generation).toBe(GENERATION)
  expect(plan.direction).toBe('stage')
  expect(plan.repository).toBeTruthy()
  expect(plan.worktree).toBeTruthy()
  await expect(viewer.getByRole('button', { name: 'Preview', exact: true })).toBeEnabled()
}
