// #374: runs of consecutive `wip(#N): auto-checkpoint` commits fold into one
// expandable marker in the graph view. `cargo test` never executes
// `crates/git-vista/src` (wasm-gated) — the host tests in `collapse.rs`
// prove the algorithm, but nothing about whether `canvas.rs`'s wiring,
// `GraphFocus`'s re-anchoring, or the tap handler actually work. With this
// feature defaulting ON, an unverified wiring bug ships to every session on
// first load, which is why this spec is required, not optional (see the
// feature's design doc, "Testing").

import { expect, test } from '@playwright/test'

import { TWIN_CHECKPOINTS, TWIN_REWRITTEN, WIP_RUN_COUNT } from '../fixture.mjs'
import { expectEachChainHasItsOwnMarker, forceOnline, openApp, runtime } from './helpers.mjs'

// The fixture seeds 4 real commits plus one run of WIP_RUN_COUNT checkpoint
// commits between commit 1 and commit 2 (fixture.mjs). Folded, the graph
// therefore renders: commit4(HEAD), commit3, commit2, one WipGroup, commit1
// = 4 real rows + 1 group row.
const EXPECTED_DISPLAY_ROWS = 5
// Expanded, the group's slot is replaced by its members: 4 real + WIP_RUN_COUNT.
// Asserted exactly, not as "more than before" — a loose assertion passes both
// when the run opens whole AND when only its head opens and the tail re-folds
// into a smaller group, which is precisely the defect this spec exists to
// catch. (It is also why WIP_RUN_COUNT must stay >= 3: at 2, an
// only-the-head-opened projection leaves a 1-commit tail, which is below
// MIN_RUN and so is indistinguishable from a correct expansion.)
const EXPECTED_EXPANDED_ROWS = EXPECTED_DISPLAY_ROWS - 1 + WIP_RUN_COUNT

test.describe('#374 WIP-checkpoint collapsing', () => {
  test('a run of checkpoints renders as one folded marker by default', async ({ page }) => {
    await openApp(page)
    // Default is ON, so the run is folded before any interaction.
    const marker = page.locator('.wip-group')
    await expect(marker).toHaveCount(1)
    await expect(marker.locator('.wip-group-label')).toContainText('WIP commits')
    await expect(marker.locator('.wip-group-label')).toContainText(String(WIP_RUN_COUNT))
    // The folded members are genuinely absent, not merely hidden.
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)
  })

  test('tapping the marker expands the run into individual commits', async ({ page }) => {
    await openApp(page)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)
    await page.locator('.wip-group .node-hit').click()
    await expect(page.locator('.wip-group')).toHaveCount(0)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_EXPANDED_ROWS)
  })

  test('the topbar toggle shows every checkpoint', async ({ page }) => {
    await openApp(page)
    await page.getByRole('button', { name: /WIP: folded/ }).click()
    await expect(page.locator('.wip-group')).toHaveCount(0)
    await expect(page.getByRole('button', { name: /WIP: shown/ })).toBeVisible()
  })

  test('a checkpoint commit can fold just its own run, not the whole graph', async ({
    page,
  }) => {
    // The topbar toggle is all-or-nothing. Once a run is open, the only way
    // back should not be un-folding every other run in the graph too, so each
    // member of an open run offers to fold that one section.
    await openApp(page)
    await page.locator('.wip-group .node-hit').click()
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_EXPANDED_ROWS)

    // Open the menu on the MIDDLE checkpoint of the run, not its first row:
    // the offer has to come from membership, not from being the run's head.
    //
    // Focus + Enter rather than a click, and not for convenience: the canvas
    // starts with its top rows underneath the fixed topbar, so a coordinate
    // click up there is intercepted by the header and silently lands
    // somewhere else. Enter on a focused row opens the identical menu through
    // the roving-tabindex path (#65), which is a real user route and needs no
    // hit-testing.
    await page.locator('.node-hit[data-row-index="4"]').focus()
    await page.keyboard.press('Enter')
    const fold = page.getByRole('button', {
      name: new RegExp(`Fold these ${WIP_RUN_COUNT} checkpoints`),
    })
    await expect(fold).toBeVisible()
    await fold.click()

    await expect(page.locator('.wip-group')).toHaveCount(1)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)
  })

  test('an ordinary commit is never offered the fold item', async ({ page }) => {
    await openApp(page)
    await page.locator('.node-hit[data-row-index="0"]').focus()
    await page.keyboard.press('Enter')

    // Assert the menu actually opened before asserting what is NOT in it —
    // without this the absence check passes just as happily when no menu
    // exists at all, which is exactly how it first passed.
    await expect(page.locator('.ctx-menu')).toBeVisible()
    await expect(page.getByRole('button', { name: /Fold these/ })).toHaveCount(0)
  })

  test('expanding a run leaves the rows above it in place', async ({ page }) => {
    // Rows are keyed by WHAT they show, not by when the projection last
    // changed. Keying on a global epoch instead rebuilds every visible row on
    // every expand/collapse/toggle, which is pure DOM churn for rows whose
    // content did not move — and it is what made a Playwright click go stale
    // mid-action and hit-test onto the header instead of the commit.
    await openApp(page)
    await page.evaluate(() => {
      document
        .querySelectorAll('.graph-row')
        .forEach((row, i) => {
          row.dataset.marked = String(i)
        })
    })

    await page.locator('.wip-group .node-hit').click()
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_EXPANDED_ROWS)

    // The three real commits above the run show the same commits at the same
    // display indices, so their DOM nodes must survive untouched.
    const survivors = await page.evaluate(
      () => [...document.querySelectorAll('.graph-row')].filter((r) => r.dataset.marked).length,
    )
    expect(survivors).toBe(3)
  })

  // #382: a graph whose runs sit below the viewport is indistinguishable from
  // a graph with none — the failure that got a working feature reported as
  // broken. The topbar must say how many exist, and say it CORRECTLY: the
  // fixture seeds exactly one run, so "1 run" is the only right answer and a
  // hardcoded string would fail the zero case below.
  test('the topbar reports how many WIP runs the graph holds', async ({ page }) => {
    await openApp(page)
    const toggle = page.getByRole('button', { name: /^WIP:/ })
    await expect(toggle).toContainText('folded')
    await expect(toggle).toContainText('1 run')
  })

  test('the count says nothing is hidden when the toggle is off', async ({ page }) => {
    await openApp(page)
    await page.getByRole('button', { name: /^WIP:/ }).click()
    const toggle = page.getByRole('button', { name: /^WIP:/ })
    await expect(toggle).toContainText('shown')
    // with collapsing off nothing is being hidden, so a count of hidden runs
    // would be a claim about nothing
    await expect(toggle).not.toContainText('run')
  })
})

// ── #478: two diverged chains, interleaved in display order ──────────────
//
// Everything above drives `fixture-repo`, whose checkpoint run is CONTIGUOUS.
// Those specs are the regression half: they prove the rewrite of `project` did
// not break the #374 behaviour. They cannot prove the #478 path is reached,
// because a contiguous run folds under the old scan and the new one alike.
//
// This block drives `interleaved-repo`, where a branch and its diverged
// remote-tracking twin put two checkpoint chains in the graph at once. Every
// display-adjacent pair is a cross-chain pair, so the pre-#478 scan found no
// run longer than one and folded nothing.
//
// The numbers below are DERIVED, not guessed: the fixture repository was built
// and run through the real layout engine (`layout_with_refs`) and `project`,
// and these are what came out. Rows as the layout engine places them:
//
//   row 0 lane 1  checkpoint 5 (local)     row 5 lane 2  checkpoint 3 (remote)
//   row 1 lane 2  checkpoint 5 (remote)    row 6 lane 1  checkpoint 2 (shared)
//   row 2 lane 1  checkpoint 4 (local)     row 7 lane 1  checkpoint 1 (shared)
//   row 3 lane 2  checkpoint 4 (remote)    row 8 lane 0  seed
//   row 4 lane 1  checkpoint 3 (local)
//
// The shared tail sits in the local chain's lane, so it folds into the local
// group: members [0, 2, 4, 6, 7] and [1, 3, 5] — five and three, scattered
// through display order, with the other chain's rows between them.

/** The local chain: the rewritten checkpoints, plus the shared ones below the
 *  divergence, which sit in its lane and so chain onto it. */
const LOCAL_RUN = TWIN_REWRITTEN + (TWIN_CHECKPOINTS - TWIN_REWRITTEN)
/** The remote-tracking twin has only its own rewritten-away half: the shared
 *  tail is in the other lane, so its chain stops at the fork point. */
const REMOTE_RUN = TWIN_REWRITTEN

/** Folded: two markers plus the one ordinary commit (`seed`). */
const TWIN_FOLDED_ROWS = 3
/** Toggle off: every commit, nine of them. */
const TWIN_ALL_ROWS = TWIN_CHECKPOINTS + TWIN_REWRITTEN + 1
/** The local marker opened: its five members, the twin's marker, and seed. */
const TWIN_LOCAL_OPEN_ROWS = LOCAL_RUN + 1 + 1

/**
 * Open the interleaved repository.
 *
 * Its own opener rather than `openApp`, which hardcodes `fixture-repo` — the
 * same shape `broken-head.spec.mjs` needed for the same reason.
 */
async function openTwinRepo(page) {
  await forceOnline(page)
  const { base } = runtime()
  await page.goto(base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()

  const entry = page.getByRole('button', { name: /interleaved-repo/i }).first()
  await expect(entry).toBeVisible()
  await entry.click()

  // The mode dialog only appears when the repository is not already open.
  const visualize = page.getByRole('button', { name: /look only/ })
  if (await visualize.isVisible().catch(() => false)) {
    await visualize.click()
  }

  // Prove we are looking at the right repository before asserting on its rows.
  await expect(page.locator('p.status.repo')).toContainText(/interleaved-repo/i)
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  await expect(page.locator('circle.node-hit').first()).toBeAttached()
}

/**
 * The vertical span of every edge path drawn in the graph, as `[startY, endY]`.
 *
 * Read out of the rendered `d` rather than from any app state: the question is
 * whether the path was DRAWN, which is what `visible_edges` decides. `section
 * .graph` holds edge paths and stub paths; this fixture has no branch stubs
 * (verified against the layout engine), so every path here is an edge.
 */
function edgeSpans(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('section.graph svg path')]
      .map((p) => (p.getAttribute('d') ?? '').match(/-?\d+(\.\d+)?/g))
      .filter((n) => n && n.length >= 4)
      // `d` is "M x1 y1 L x2 y2" or "M x1 y1 C ...  x2 y2": the second number
      // is always the start y and the last is always the end y.
      .map((n) => [Number(n[1]), Number(n[n.length - 1])]),
  )
}

test.describe('#478 two diverged chains whose checkpoints interleave', () => {
  test('each chain folds into its own marker', async ({ page }) => {
    await openTwinRepo(page)

    // Two markers, not one and not none. None is the defect (#478); one is the
    // wrong fix the issue names — folding two branches' checkpoints together
    // and claiming a chain that does not exist.
    await expect(page.locator('.wip-group')).toHaveCount(2)
    await expect(page.locator('.graph-row')).toHaveCount(TWIN_FOLDED_ROWS)

    // The two markers carry DIFFERENT counts, and those counts are the two
    // chains' real lengths. Asserting "two markers" alone would pass against a
    // grouping that split the same chain in half, or that mixed the chains and
    // happened to land on two groups.
    //
    // Shared with `harness-selfcheck.spec.mjs` rather than written out here,
    // so the self-check that proves this can go red runs THIS assertion and
    // not a copy of it. See `helpers.mjs` for what the two copies cost.
    await expectEachChainHasItsOwnMarker(page, { local: LOCAL_RUN, remote: REMOTE_RUN })
  })

  test('the topbar counts both runs', async ({ page }) => {
    await openTwinRepo(page)
    await expect(page.getByRole('button', { name: /^WIP:/ })).toContainText('2 runs')
  })

  test('the toggle shows every checkpoint on both chains', async ({ page }) => {
    // The paired positive for the count above: the marker really is standing in
    // for eight commits, so "3 rows" is folding and not a graph that only ever
    // had three commits in it.
    await openTwinRepo(page)
    await page.getByRole('button', { name: /WIP: folded/ }).click()

    await expect(page.locator('.wip-group')).toHaveCount(0)
    await expect(page.locator('.graph-row')).toHaveCount(TWIN_ALL_ROWS)
  })

  test('opening one chain leaves the other folded', async ({ page }) => {
    await openTwinRepo(page)
    // The first marker in DOM order is the first display slot: the local chain.
    await page.locator('.wip-group .node-hit').first().click()

    // Its five members are back, scattered through display order with the
    // twin's marker still among them — this is the case a contiguous span
    // cannot express, and the reason the group had to stop being a range.
    await expect(page.locator('.graph-row')).toHaveCount(TWIN_LOCAL_OPEN_ROWS)
    await expect(page.locator('.wip-group')).toHaveCount(1)
    await expect(page.locator('.wip-group-label')).toContainText(String(REMOTE_RUN))
  })

  test('a member of the opened chain offers to fold that chain, not the other', async ({
    page,
  }) => {
    await openTwinRepo(page)
    await page.locator('.wip-group .node-hit').first().click()
    await expect(page.locator('.graph-row')).toHaveCount(TWIN_LOCAL_OPEN_ROWS)

    // Display row 3 is the opened chain's third member (checkpoint 3), which is
    // NOT its head — the offer has to come from membership. Focus + Enter, not
    // a click, for the reason the contiguous case documents above.
    await page.locator('.node-hit[data-row-index="3"]').focus()
    await page.keyboard.press('Enter')

    // The menu names this chain's length, not the twin's and not the sum.
    const fold = page.getByRole('button', {
      name: new RegExp(`Fold these ${LOCAL_RUN} checkpoints`),
    })
    await expect(fold).toBeVisible()
    await fold.click()

    await expect(page.locator('.wip-group')).toHaveCount(2)
    await expect(page.locator('.graph-row')).toHaveCount(TWIN_FOLDED_ROWS)
  })

  test('the edge into the folded fork point is still drawn, running upward', async ({ page }) => {
    // The culler's half of #478, and the only assertion in this suite that
    // reaches it. `visible_edges` used to compare `from_display < end &&
    // to_display >= start`, which assumes a display edge runs downward. Folding
    // non-adjacent rows breaks that: the twin's oldest commit descends from a
    // fork point that folded into the OTHER chain's marker, and that marker is
    // above — so the edge points back up the screen and the old filter dropped
    // it wherever the viewport sat. `edge_path` always drew it correctly; only
    // the culling was wrong, which is why the visible symptom is a missing line
    // rather than a misdrawn one.
    await openTwinRepo(page)
    await expect(page.locator('.wip-group')).toHaveCount(2)

    const spans = await edgeSpans(page)
    expect(spans.length, 'the folded graph must still draw its edges').toBeGreaterThan(0)
    const upward = spans.filter(([from, to]) => to < from)
    expect(upward, `exactly one edge must run upward, got ${JSON.stringify(spans)}`).toHaveLength(1)
  })

  test('with the toggle off no edge runs upward', async ({ page }) => {
    // The paired positive. Without it the assertion above passes just as
    // happily against a graph that draws every edge upward, or against a
    // parser that mixed up the coordinates — and an upward edge is only
    // meaningful because the unfolded graph has none.
    await openTwinRepo(page)
    await page.getByRole('button', { name: /WIP: folded/ }).click()
    await expect(page.locator('.wip-group')).toHaveCount(0)

    const spans = await edgeSpans(page)
    expect(spans.length, 'the unfolded graph must draw its edges').toBeGreaterThan(0)
    expect(spans.filter(([from, to]) => to < from)).toHaveLength(0)
  })
})

// #571: the stub layer is placed by DISPLAY slot, like every other layer.
//
// `resolved_stubs` answers in raw row indices, because that is the space the
// aggregate keeps. Folding separates raw from display space, and a stub drawn at
// its raw anchor row sits one row lower for every checkpoint folded above it. A
// stub connector rises only half a row while crossing from the anchor's lane to
// the stub's own column — and stub columns start past the commit lane
// high-water — so the mis-placed line is near-horizontal and lands across an
// unrelated commit's subject text. Measured on git-vista's own history: 0 such
// crossings unfolded, 59 folded.
//
// `cargo test` cannot reach any of this: `render/stubs.rs` is wasm-gated, and the
// host test in `collapse.rs` proves only that `place_stubs` computes the right
// slot, never that the layer asks it.
test.describe('#571 branch stubs are placed in display space', () => {
  // `stub_node_cy(row, 0)` is half a row above the anchor dot, and the name
  // label sits 4px below the ring's centre (`render/stubs.rs`).
  const RING_RISE = 28
  const LABEL_BASELINE_DROP = 4

  /** The y the `base` stub's label is actually drawn at. */
  async function stubLabelY(page) {
    const label = page.locator('.stub-label', { hasText: 'base' })
    await expect(label, 'the fixture leaves `base` on the oldest commit, which renders as a stub')
      .toHaveCount(1)
    return Number(await label.getAttribute('y'))
  }

  /** The y of the dot at display slot `i` — read from the DOM, not computed. */
  async function dotCy(page, i) {
    return Number(await page.locator(`.node-hit[data-row-index="${i}"]`).getAttribute('cy'))
  }

  test('folded, the stub hangs over its anchor commit and not over its raw row', async ({
    page,
  }) => {
    await openApp(page)
    await expect(page.locator('.wip-group')).toHaveCount(1)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_DISPLAY_ROWS)

    // `base` is on the fixture's oldest commit, which sits BELOW the folded run
    // — so its raw row and its display slot differ by exactly the checkpoints
    // folded above it.
    const anchor = await dotCy(page, EXPECTED_DISPLAY_ROWS - 1)
    expect(await stubLabelY(page)).toBe(anchor - RING_RISE + LABEL_BASELINE_DROP)

    // The same statement in the terms of the defect: drawn at its raw row the
    // ring would be WIP_RUN_COUNT - 1 rows further down, off the bottom of a
    // graph that no longer has rows there.
    const rawRowY = anchor + (WIP_RUN_COUNT - 1) * 56
    expect(await stubLabelY(page)).not.toBe(rawRowY - RING_RISE + LABEL_BASELINE_DROP)
  })

  test('unfolding moves the stub down onto the row its anchor now occupies', async ({ page }) => {
    // The paired positive, and the one that pins the repaint: the stub layers
    // are eager, so they only move when something tells them to. Reading only
    // `stub_epoch` (a paging signal) leaves them frozen at the folded geometry
    // after the toggle, which is a stub drawn against a row that has moved.
    await openApp(page)
    await page.getByRole('button', { name: /WIP: folded/ }).click()
    await expect(page.locator('.wip-group')).toHaveCount(0)
    await expect(page.locator('.graph-row')).toHaveCount(EXPECTED_EXPANDED_ROWS)

    const anchor = await dotCy(page, EXPECTED_EXPANDED_ROWS - 1)
    expect(await stubLabelY(page)).toBe(anchor - RING_RISE + LABEL_BASELINE_DROP)
  })
})

// #573: a fold marker's label starts clear of what crosses its own slot.
//
// `build_wip_group` used to draw `⋯ N WIP commits ⋯` at `node_cx(lane) +
// NODE_RADIUS + 8` — hugging its dot, consulting no occupancy — so anything
// passing through the marker's display row was drawn across its label. Measured
// on git-vista's own history, folded: 113 display edges and 4 stub connectors
// reached into a marker label, and none reached into a commit's.
//
// The fixture reproduces the stub half exactly: `base` sits on the oldest
// commit, one slot below the marker, and a stub ring hangs half a row above its
// anchor — so it covers the marker's row. Its column is allocated past the
// commit lane high-water, which is why the label has to move at all.
test.describe('#573 a fold marker label clears its own slot', () => {
  // `marker_label_x`: past the occupied lane by LABEL_GAP, the same column a
  // commit row would get for the same occupancy.
  const LABEL_GAP = 18

  test('the marker label starts past the stub column hanging over it', async ({ page }) => {
    await openApp(page)
    await expect(page.locator('.wip-group')).toHaveCount(1)

    const stub = page.locator('circle[aria-label="base — new branch, no commits yet"]')
    await expect(stub, 'the fixture leaves `base` on the oldest commit').toHaveCount(1)
    const stubCx = Number(await stub.getAttribute('cx'))

    const label = page.locator('.wip-group-label')
    await expect(label).toHaveCount(1)
    const labelX = Number(await label.getAttribute('x'))

    expect(labelX).toBe(stubCx + LABEL_GAP)
    // Stated again as the property, not the coordinate: the label may not begin
    // to the left of the column the stub occupies.
    expect(labelX).toBeGreaterThan(stubCx)
  })

  test('the marker dot itself does not move', async ({ page }) => {
    // The paired negative. Pushing the LABEL clear is the fix; pushing the
    // marker's dot would move the graph column and break every row around it,
    // and an assertion on the label alone cannot tell the two apart.
    await openApp(page)
    const dot = page.locator('.wip-group .node-hit')
    await expect(dot).toHaveCount(1)
    const firstRow = page.locator('.node-hit[data-row-index="0"]')
    expect(Number(await dot.getAttribute('cx'))).toBe(Number(await firstRow.getAttribute('cx')))
  })
})
