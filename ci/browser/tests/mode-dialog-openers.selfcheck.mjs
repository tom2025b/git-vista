// Source-structure guard for the shared FULL-mode openers.
//
// This deliberately does not pretend to replace the browser specs: those prove
// the locators describe the rendered app and that FULL-only operations work.
// This guard has the narrower job of keeping the proven wait -> click ->
// dismissal sequence from regressing to an instant visibility sample again.

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'

const helpers = readFileSync(new URL('./helpers.mjs', import.meta.url), 'utf8')

function openerSource(name) {
  const start = helpers.indexOf(`export async function ${name}(page) {`)
  assert.notEqual(start, -1, `${name} must still exist`)
  const next = helpers.indexOf('\nexport async function ', start + 1)
  return helpers.slice(start, next === -1 ? helpers.length : next)
}

for (const name of ['openStashRepo', 'openWorktreeRepo']) {
  test(`${name} waits for FULL mode and proves both overlays dismissed`, () => {
    const source = openerSource(name)

    assert.doesNotMatch(
      source,
      /full\.isVisible\(\)\.catch/,
      'a guaranteed mode dialog must never be reduced to an instant sample',
    )

    const wait = source.indexOf(
      "await expect(full, 'the mode dialog follows opening a repository').toBeVisible()",
    )
    const click = source.indexOf('await full.click()')
    const pickerGone = source.indexOf(
      "await expect(entry, 'the picker must be dismissed before graph interactions').toHaveCount(0)",
    )
    const modeGone = source.indexOf(
      "await expect(full, 'the mode dialog must be dismissed before graph interactions').toHaveCount(0)",
    )

    assert.ok(wait >= 0, 'the guaranteed dialog must be awaited')
    assert.ok(click > wait, 'FULL mode must be clicked only after it is visible')
    assert.ok(pickerGone > click, 'the picker dismissal must be checked after the click')
    assert.ok(modeGone > pickerGone, 'the mode-dialog dismissal must also be checked')
  })
}
