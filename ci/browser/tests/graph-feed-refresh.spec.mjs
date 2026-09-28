// #852: real external writes through the real feed, observed in the DOM.
// last_edited_by: codex
// **Signed:** codex · 2026-09-28T10:52:54-04:00
import { execFileSync } from 'node:child_process'
import { writeFileSync, unlinkSync } from 'node:fs'
import { join } from 'node:path'
import { expect, test } from '@playwright/test'
import { openApp, runtime } from './helpers.mjs'

const nodeSelector = 'circle.node-hit'
const mark = page => page.evaluate(selector => {
  window.__gv852Node = document.querySelector(selector)
}, nodeSelector)
const preserved = page => page.evaluate(selector =>
  window.__gv852Node === document.querySelector(selector), nodeSelector)

test('external worktree edits refresh status without remount; external ref changes remount', async ({ page }) => {
  test.setTimeout(60_000)
  const root = runtime().fixture.root
  const file = join(root, 'gv852-external-editor-save.txt')
  const git = (...args) => execFileSync('git', ['-C', root, ...args], { encoding: 'utf8' })
  const tag = 'gv852-external-ref'
  let probes = 0
  let wroteFile = false
  let wroteTag = false
  page.on('response', response => {
    const url = new URL(response.url())
    if (url.pathname === '/api/frame' && url.searchParams.has('repo')) probes++
  })
  try {
    await openApp(page)
    await expect.poll(() => probes).toBeGreaterThan(0)
    await mark(page)
    const before = probes
    writeFileSync(file, 'an external editor save\n')
    wroteFile = true
    await expect.poll(() => probes, { timeout: 20_000 }).toBeGreaterThan(before)
    // The chip's own read must now carry the changed status. Checking only
    // that a request happened would accept a discarded or mis-scoped reply.
    await page.getByRole('button', { name: /^Repository status:/ }).click()
    const detail = page.getByRole('dialog', { name: 'Repository status details' })
    await expect(detail).toContainText(`${runtime().fixture.expected.untracked + 1} files are new and not tracked by Git`)
    expect(await preserved(page)).toBe(true)
    await detail.getByRole('button', { name: 'Close', exact: true }).click()
    git('tag', tag, 'HEAD')
    wroteTag = true
    await expect.poll(() => preserved(page), { timeout: 20_000 }).toBe(false)
    await expect(page.locator(nodeSelector).first()).toBeAttached()
    await expect(page.getByRole('region', { name: 'Commit history graph' })).toContainText(tag)
  } finally {
    if (wroteFile) unlinkSync(file)
    if (wroteTag) git('tag', '-d', tag)
  }
})
