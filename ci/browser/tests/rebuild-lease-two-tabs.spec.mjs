// #664 review round 5 — the reproduction fable's report (§1d, third case)
// derived by reading and explicitly did not run.
//
// THE CLAIM UNDER TEST. Repository selection is per *session*, not per tab
// (`handlers/session.rs`), and `/api/plan` carries no repository selector —
// it resolves against whatever the session has selected when the request
// arrives. So a second tab selecting repository B moves the first tab's
// in-flight requests to B **with no epoch bump in tab A**. Tab A's Rebuild
// then builds a force-with-lease plan against B, both server gates pass it
// (plan and live selection agree — both B), and tab A re-opens a
// confirmation whose topbar still reads A.
//
// WHY IT IS WORTH A SPEC RATHER THAN AN ARGUMENT. Every client-side currency
// token — the one round 3 added, and the repository-bound variant round 4
// proposed — compares tab A's *belief* about the repository. In this case
// that belief is simply out of date: nothing in tab A is stale, the token is
// genuinely current, and the reopen is "correct" by every check the client
// can make on its own. If that is real, no counter fixes it and the fence has
// to be the plan's own `repository`/`worktree` tokens, which arrive on every
// plan and are discarded at `PlanOnScreen::of`.
//
// WHAT THIS ASSERTS. The safe behaviour: a Rebuild whose plan came back for a
// different repository must not re-open a confirmation. At `ab768f3d` this is
// expected to FAIL, and the failure is the reproduction. The assertions are
// written to say *which* half broke — a foreign plan arriving is one fact, a
// confirmation opening on it is the other, and only the second is the defect.
//
// BOTH FIXTURES ARE `git init -b main`, which is what makes this reachable at
// all: the two repositories share a branch name, so the plan for `main`
// builds successfully against the wrong one instead of failing for the
// uninteresting reason that the branch is missing. That is also the real
// shape of the danger — force-pushing `main` at the wrong remote.

import { execFileSync } from 'node:child_process'

import { expect, test } from '@playwright/test'

import { forceOnline, openBranchMenu, openMergePreviewRepo, runtime } from './helpers.mjs'

/** The branch both fixtures have. See the header. */
const SHARED_BRANCH = 'main'

function gitIn(root, args) {
  return execFileSync('git', ['-C', root, ...args], { encoding: 'utf8' }).trim()
}

function git(args) {
  return gitIn(runtime().mergePreviewFixture.root, args)
}

/** Give a repository an `origin` and a remote-tracking `main`, so a
 *  force-with-lease plan can actually be BUILT against it.
 *
 *  Both fixtures need this, and the first version of this probe only did the
 *  one tab A was showing. The consequence was quiet and would have been easy
 *  to misread as safety: tab A's rebuild resolved against tab B's repository,
 *  which had no `origin`, so `/api/plan` returned an error envelope, the
 *  continuation took its failure path, and no wrong-repository plan ever
 *  existed. "No foreign plan arrived" looked like the app refusing when it was
 *  really the fixture declining to pose the question. */
function giveOrigin(root) {
  try {
    gitIn(root, ['remote', 'remove', 'origin'])
  } catch {
    // no origin yet
  }
  gitIn(root, ['remote', 'add', 'origin', root])
  gitIn(root, ['update-ref', `refs/remotes/origin/${SHARED_BRANCH}`, SHARED_BRANCH])
}

/** Every `/api/plan` response body this page receives, parsed. The decisive
 *  observation: a plan carries `repository` and `worktree` tokens naming the
 *  desk it was built for, so "was this plan built against the repository the
 *  user is looking at" is answerable from the wire rather than inferred. */
function collectPlans(page) {
  const plans = []
  page.on('response', async (response) => {
    if (!response.url().endsWith('/api/plan')) return
    try {
      const body = await response.json()
      plans.push(body)
    } catch {
      // A non-JSON or aborted response is not a plan; nothing to record.
    }
  })
  return plans
}

/** Watch the frames this page's own app fetches, and report the desk the
 *  most recent one named.
 *
 *  Deliberately NOT a `fetch('/api/frame')` of its own: that has to
 *  reconstruct the protocol header and the session the app already holds, and
 *  getting either wrong yields `null` — which would have made the premise
 *  checks below compare two nulls and pass vacuously. Reading the app's own
 *  reply asks the question of the thing under test. */
function watchFrames(page) {
  const seen = []
  page.on('response', async (response) => {
    if (!response.url().includes('/api/frame')) return
    try {
      const f = await response.json()
      if (f && typeof f === 'object' && 'worktree_id' in f) seen.push(f.worktree_id)
    } catch {
      // not a frame body
    }
  })
  return () => seen[seen.length - 1] ?? null
}

/** Open the OTHER fixture repository in **Active** mode.
 *
 *  Not `openApp`, and the difference is load-bearing. `openApp` answers the
 *  mode dialog with "look only", and **mode rides the selection** (ADR 0007) —
 *  so a second tab selecting in Visualize mode puts the whole *session* in
 *  look-only, and tab A's write-shaped `POST /api/plan` comes back
 *  `403 forbidden: "This repository is open in Visualize mode"`. That is a
 *  real protection, but it is a MODE protection, and it would have made this
 *  probe report "the app refused" while leaving the repository question
 *  entirely unasked. Measured, not reasoned: the first version of this probe
 *  used `openApp` and got exactly that refusal. */
async function openOtherRepoInActiveMode(page) {
  await forceOnline(page)
  await page.goto(runtime().base)
  await expect(page.getByRole('heading', { name: 'git-vista' })).toBeVisible()
  const entry = page.getByRole('button', { name: /fixture-repo/i }).first()
  await expect(entry).toBeVisible({ timeout: 20_000 })
  await entry.click()
  const active = page.getByRole('button', { name: /full git operations/ })
  await expect(active, 'the mode dialog follows opening a repository').toBeVisible({
    timeout: 20_000,
  })
  await active.click()
  await expect(page.getByRole('region', { name: 'Commit history graph' })).toBeVisible()
  await expect(page.locator('p.status.repo')).toContainText('fixture-repo', { timeout: 20_000 })
}

test.describe('#664 — a second tab moving the session selection', () => {
  test('a rebuild whose plan came back for another repository must not re-open a confirmation', async ({
    page,
    context,
  }) => {
    test.setTimeout(120_000)
    const tag = 'review-r5-two-tabs'

    // ── Tab A: a force-with-lease confirmation for `main`, stale enough to
    //    offer Rebuild. Same setup `rebuild-lease-cancel.spec.mjs` uses, on
    //    the shared branch instead of `feature`.
    await page.setViewportSize({ width: 1280, height: 2400 })
    // Installed BEFORE the navigation: the frame this reads is fetched during
    // `openMergePreviewRepo`, so a watcher attached afterwards sees nothing.
    const tabAFrame = watchFrames(page)
    await openMergePreviewRepo(page)
    // BOTH repositories, so the plan builds successfully against whichever one
    // the session happens to be pointing at when the request lands.
    giveOrigin(runtime().mergePreviewFixture.root)
    giveOrigin(runtime().fixture.root)

    const tabAWorktree = tabAFrame()
    expect(tabAWorktree, 'tab A must know which desk it is showing').toBeTruthy()

    try {
      await openBranchMenu(page, SHARED_BRANCH)
      await page
        .getByRole('button', { name: `Force Push ‘${SHARED_BRANCH}’…`, exact: false })
        .click()
      await expect(page.getByText('What this plan says')).toBeVisible({ timeout: 20_000 })
      git(['tag', tag])
      await expect(page.getByRole('button', { name: 'Rebuild', exact: true })).toBeVisible({
        timeout: 20_000,
      })

      // ── Tab B: same browser context, so the same session cookie, so the
      //    same server-side selection cell. Selecting the other repository
      //    here moves tab A's future requests without tab A learning anything.
      const other = await context.newPage()
      const tabBFrame = watchFrames(other)
      await openOtherRepoInActiveMode(other)
      const tabBWorktree = tabBFrame()
      expect(
        tabBWorktree,
        'the premise: tab B must have selected a DIFFERENT desk',
      ).not.toBe(tabAWorktree)

      // ── Tab A, unaware, rebuilds.
      const plans = collectPlans(page)
      await page.bringToFront()
      // The dialog is ALREADY open here, so "a confirmation is on screen"
      // afterwards proves nothing on its own — it would be true of an app that
      // did nothing at all. What distinguishes acting on the foreign plan is
      // that `open_confirm` REPLACES the dialog's contents with it, and the two
      // repositories' plans differ (different lease oid, different expected
      // ref change). So the text is captured before and compared after.
      // `body`, not a dialog selector: the confirmation "reuses the commit
      // modal's iPad-proven inline-styled overlay" (its own doc), so it has no
      // stable class or `role="dialog"` to hang a locator on — a guessed
      // selector here just times out and says nothing about the app.
      const before = await page.locator('body').innerText()
      await page.getByRole('button', { name: 'Rebuild', exact: true }).click()
      // No UI signal means "nothing happened", so settle explicitly — the
      // same explicit wait the sibling cancel spec documents.
      await page.waitForTimeout(3_000)

      // Fact 1: which desk did the plans tab A received belong to? This is
      // the wire evidence, independent of anything the UI chose to do.
      const desks = plans.map(
        (p) => p?.worktree ?? `(no worktree field; error=${JSON.stringify(p?.error ?? null)})`,
      )
      const shapes = plans.map((p) => (p && typeof p === 'object' ? Object.keys(p).join(',') : typeof p))
      const foreign = plans.filter((p) => p?.worktree && p.worktree !== tabAWorktree)
      // Fact 2: did a confirmation open on one?
      const reopened = await page.getByText('What this plan says').count()

      const after = await page.locator('body').innerText()
      const replaced = after !== before

      // The sharpest single fact available: an object id that exists only in
      // the OTHER repository. If tab A's screen is showing it, the foreign
      // plan is not merely in memory — it is what the user is being asked to
      // approve. Short form, because that is how the UI renders an oid.
      const foreignOids = [
        ...new Set(
          foreign
            .flatMap((p) => JSON.stringify(p.expected_ref_changes ?? []).match(/[0-9a-f]{40}/g) ?? [])
            .map((oid) => oid.slice(0, 7)),
        ),
      ]
      const shownForeignOids = foreignOids.filter((short) => after.includes(short))

      const evidence =
        `tab A desk ${tabAWorktree}; tab B moved the session to ${tabBWorktree}. ` +
        `plans seen by tab A: ${plans.length}, their desks: [${desks.join(' | ')}], ` +
        `body keys: [${shapes.join(' || ')}], confirmation open: ${reopened}, ` +
        `screen text changed: ${replaced}, foreign oids on screen: [${shownForeignOids.join(',')}] ` +
        `out of [${foreignOids.join(',')}].`

      expect(
        plans.length,
        `the premise: tab A must actually have received plan replies to judge. ${evidence}`,
      ).toBeGreaterThan(0)

      // THE ORDER MATTERS, and an earlier draft of this file got it wrong.
      //
      // "A confirmation re-opened" is NOT by itself the defect — a rebuild
      // that succeeds against the RIGHT repository re-opens the confirmation,
      // and that is the button working. The defect is re-opening on a plan
      // built for a DIFFERENT desk. So the foreign plan has to be established
      // first; if none arrived, this probe has not reproduced the mechanism
      // and must say exactly that rather than fail on the reopen and be read
      // as a confirmed defect.
      expect(
        foreign.length,
        'NOT REPRODUCED: every plan tab A received names tab A\'s own desk, so the ' +
          'session move did not reach these requests. Either the mechanism differs from ' +
          `the traced one, or this probe is measuring the wrong field. ${evidence}`,
      ).toBeGreaterThan(0)

      // The defect, stated as the thing that must not happen: the client had
      // a plan for another desk in hand and rebuilt the live confirmation
      // around it. `replaced` is what separates that from "the dialog was
      // already open and nothing touched it".
      expect(
        shownForeignOids,
        `a plan built for another desk must never be swapped into a live confirmation — ` +
          `the topbar would still name this repository while the button force-pushes the ` +
          `other one. ${evidence}`,
      ).toEqual([])

      await other.close()
    } finally {
      // `git()` is synchronous and throws; each cleanup is independent, so a
      // failure in one must not skip the rest and leave the shared
      // merge-preview fixture dirty for every other spec that uses it.
      // `gitIn` is synchronous and throws; each cleanup is independent, so a
      // failure in one must not skip the rest and leave a shared fixture
      // dirty for every other spec that uses it. BOTH repositories are
      // restored — this probe modifies the main fixture too, which no other
      // spec in this suite does.
      await page.unroute('**/api/plan').catch(() => {})
      const { mergePreviewFixture, fixture } = runtime()
      const cleanups = [
        [mergePreviewFixture.root, ['tag', '-d', tag]],
        [mergePreviewFixture.root, ['remote', 'remove', 'origin']],
        [mergePreviewFixture.root, ['update-ref', '-d', `refs/remotes/origin/${SHARED_BRANCH}`]],
        [fixture.root, ['remote', 'remove', 'origin']],
        [fixture.root, ['update-ref', '-d', `refs/remotes/origin/${SHARED_BRANCH}`]],
      ]
      for (const [root, args] of cleanups) {
        try {
          gitIn(root, args)
        } catch {
          // already gone
        }
      }
      await page.keyboard.press('Escape').catch(() => {})
    }
  })
})
