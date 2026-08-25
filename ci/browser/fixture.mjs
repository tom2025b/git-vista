// The throwaway git repositories the browser tests drive.
//
// THESE ARE NOT BUILT HERE ANY MORE. Since #448 every shape lives in the Rust
// fixture catalogue, `crates/git-vista-fixtures`, and this module invokes the
// `gv-fixture` binary rather than reimplementing the shapes in JavaScript.
//
// Why the catalogue and not a JavaScript twin (ADR 0076): two implementations
// of "a repository broken in shape X" is the drift problem one layer up, and
// this suite has already paid for it — `buildNonTextConflictFixture` exists as
// a THIRD conflict fixture (#432) only because extending the second would have
// broken specs asserting an exact conflicted count. Each Rust shape also
// carries the documentation explaining what is wrong with it and why it
// matters, which is the teaching material; a second builder here would let the
// lesson and the tested artifact drift apart again.
//
// What stayed in this file is what belongs to this suite rather than to the
// shapes: the constants the specs assert against, and the metadata each
// builder hands back to `global-setup.mjs`.
//
// Nothing here touches the user's repositories; every fixture lives entirely
// under the temp dir it is handed.

import { execFileSync } from 'node:child_process'
// `buildStashFixture` below is the one builder #448 did not move into the Rust
// catalogue: it landed in PR #490 while #448 was in flight, so the two merged
// cleanly as text and not as meaning -- #448 dropped these imports because
// nothing in this file needed them any more, and #490's builder needs three of
// them. Filed as a follow-up; until it moves, the imports stay.
import { existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'

// Commit identity is set per-invocation, never through repo or global config:
// this box has repositories whose local user.email is a personal gmail address,
// and a bare `git commit` would silently pick it up.
//
// Restored during the #448 merge for the same reason the `node:fs` imports
// above were: #448 moved every other builder into the Rust catalogue and
// dropped this with them, while #490's `buildStashFixture` -- which landed
// while #448 was in flight -- still calls git from JavaScript. The two merged
// as text and not as meaning. It goes away when that builder moves.
const IDENT = [
  '-c', 'user.name=Claude_Max',
  '-c', 'user.email=262510778+tom2025b@users.noreply.github.com',
  '-c', 'commit.gpgsign=false',
  '-c', 'tag.gpgsign=false',
]

/** Repo root, from this file's location: ci/browser -> ../.. */
const REPO = join(import.meta.dirname, '..', '..')

/** The catalogue binary. Built by `cargo build -p git-vista-fixtures`. */
const FIXTURE_BIN = join(REPO, 'target', 'debug', 'gv-fixture')

/**
 * Build one named shape into `root` by invoking the Rust catalogue.
 *
 * The failure is deliberately loud and says how to fix it: a missing binary
 * here otherwise surfaces much later as a spec failing against an empty
 * directory, which reads as a product defect rather than a missing build step.
 */
function build(shape, root) {
  if (!existsSync(FIXTURE_BIN)) {
    throw new Error(
      `browser fixtures: no catalogue binary at ${FIXTURE_BIN}\n` +
        `               build it first:  cargo build -p git-vista-fixtures`,
    )
  }
  execFileSync(FIXTURE_BIN, [shape, root], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] })
  return root
}

/** How many `wip(#N): auto-checkpoint M` commits the fixture seeds between
 *  commit 1 and commit 2 (#374). 3, not 2, so the fold is unambiguously a
 *  "run" rather than the MIN_RUN boundary case. Asserted directly by the
 *  collapse spec, so it must stay in sync with `browser::WIP_RUN_COUNT`. */
export const WIP_RUN_COUNT = 3

/** #478: how many checkpoints the interleaved-twin fixture's branch carries in
 *  total, and how many of them are rewritten so the pushed twin diverges. Five
 *  and three, so BOTH chains clear MIN_RUN on their own (the local chain keeps
 *  the two shared checkpoints, the remote chain has three of its own) and the
 *  two runs come out different lengths — a fixture where both markers said the
 *  same number could not tell a correct grouping from a swapped one.
 *  Asserted directly by the collapse spec. Mirrors `browser::TWIN_CHECKPOINTS`
 *  and `browser::TWIN_REWRITTEN`. */
export const TWIN_CHECKPOINTS = 5
export const TWIN_REWRITTEN = 3

/** Line count of the big file. Large enough that rendering every line would be
 *  obviously different from rendering a window, small enough to stay fast.
 *  Mirrors `browser::BIG_FILE_LINES`. */
export const BIG_FILE_LINES = 4000

/** How many hunks `multi-hunk.txt` carries after its edit. Asserted directly by
 *  the keyboard-navigation test. Mirrors `browser::MULTI_HUNK_COUNT`. */
export const MULTI_HUNK_COUNT = 4

/**
 * The repository every non-conflict spec drives.
 *
 * Shape and rationale: `git_vista_fixtures::browser::main_fixture`.
 */
export function buildFixture(root) {
  build('main', root)
  return {
    root,
    expected: { staged: 1, unstaged: 1, untracked: 2 },
    bigFileLines: BIG_FILE_LINES,
    multiHunkCount: MULTI_HUNK_COUNT,
  }
}

/**
 * A SECOND repository, left mid-merge with real unresolved conflicts (M4.31a,
 * #428): `both-modified.txt` (all three stages) and `added-by-both.txt` (no
 * stage 1, so the base pane is `Absent`).
 *
 * Shape and rationale: `git_vista_fixtures::browser::conflict_fixture`.
 */
export function buildConflictFixture(root) {
  build('conflict', root)
  return { root, conflicted: ['added-by-both.txt', 'both-modified.txt'] }
}

/**
 * A THIRD repository, for the conflicts that cannot be resolved by picking
 * lines (M4.31d, #430): `logo.png` (binary/binary) and `doomed.txt` (`UD`,
 * deleted by them).
 *
 * Shape and rationale:
 * `git_vista_fixtures::browser::non_text_conflict_fixture`.
 */
export function buildNonTextConflictFixture(root) {
  build('non-text-conflict', root)
  return { root, conflicted: ['doomed.txt', 'logo.png'] }
}

/**
 * A FOURTH repository, for the line-by-line editor (M4.31c, #432). Two text
 * conflicts, because the editor spec RESOLVES what it opens and a test that
 * had to share would be racing its sibling.
 *
 * Shape and rationale: `git_vista_fixtures::browser::editor_fixture`.
 */
export function buildEditorFixture(root) {
  build('editor', root)
  return { root, conflicted: ['first.txt', 'second.txt'] }
}

/**
 * A FIFTH repository, whose HEAD holds an object id nothing resolves (#473).
 * Nothing resolves HEAD here, so the graph has no current commit — do not add
 * assertions about rows to specs that open it.
 *
 * Shape and rationale: `git_vista_fixtures::browser::broken_head_fixture`.
 */
export function buildBrokenHeadFixture(root) {
  build('broken-head', root)
  return { root }
}

/**
 * A SIXTH repository: a branch and its DIVERGED remote-tracking twin, both
 * carrying checkpoint chains that interleave in display order (#478). The two
 * chains alternate, so EVERY display-adjacent pair is a cross-chain pair — the
 * condition under which the pre-#478 scan found no run longer than one and
 * folded nothing.
 *
 * Shape and rationale:
 * `git_vista_fixtures::browser::interleaved_wip_fixture`.
 */
export function buildInterleavedWipFixture(root) {
  build('interleaved-wip', root)
  return {
    root,
    originPath: join(dirname(root), 'twin-origin.git'),
    checkpoints: TWIN_CHECKPOINTS,
    rewritten: TWIN_REWRITTEN,
  }
}

/**
 * A SEVENTH repository, holding real stash entries (M3.24, #77).
 *
 * Its own repository for the reason every fixture here is its own: the drawer
 * spec asserts an exact stash count, and `buildFixture`'s repo is left
 * deliberately dirty (staged + unstaged + untracked, simultaneously) for #68d
 * and #348. Stashing in that repo would empty the working tree those specs
 * assert on, and stashing anywhere else would change a count.
 *
 * Three entries, because each one exists for a different assertion:
 *
 *   `stash@{2}` — the OLDEST, made with `git stash push -m`, so its reflog
 *   message is the `On <branch>: <text>` form. Pins that a user's own words are
 *   shown as written and marked as theirs (no "auto" pill).
 *
 *   `stash@{1}` — made with a bare `git stash`, so git writes
 *   `WIP on main: <sha> <subject>`. Pins the other parse: the branch comes out
 *   as a pill, the base commit's sha is dropped from the subject, and the entry
 *   IS marked automatic.
 *
 *   `stash@{0}` — the NEWEST, and the one that CONFLICTS on apply. Built by
 *   stashing an edit to a line and then committing a different edit to the same
 *   line, so `git stash apply` cannot merge it. This is the A4 fixture: a pop
 *   here applies something and drops nothing, and the drawer must not say
 *   "popped".
 *
 * The conflicting entry is deliberately `stash@{0}` so a spec can reach it
 * without depending on row ordering beyond "first".
 */
export function buildStashFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  git('init', '-q', '-b', 'main')
  writeFileSync(join(root, 'tracked.txt'), 'the committed line\n')
  writeFileSync(join(root, 'collision.txt'), 'original\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'seed: two tracked files')

  // --- stash@{2} after the two below land: the `-m` message form.
  writeFileSync(join(root, 'tracked.txt'), 'a named change\n')
  git('stash', 'push', '-m', 'half-finished refactor')

  // --- stash@{1}: the automatic `WIP on main: <sha> <subject>` form.
  writeFileSync(join(root, 'tracked.txt'), 'an unnamed change\n')
  git('stash')

  // --- stash@{0}: the one that conflicts.
  //
  // Stash an edit to `collision.txt`, then commit a DIFFERENT edit to the same
  // line. The stash's base no longer matches the working tree, so applying it
  // leaves the path conflicted. This is what A4 is about, and it is why this
  // fixture cannot be shared with any spec that wants a clean tree.
  writeFileSync(join(root, 'collision.txt'), 'the stashed edit\n')
  git('stash', 'push', '-m', 'will not apply cleanly')
  writeFileSync(join(root, 'collision.txt'), 'a conflicting committed edit\n')
  git('add', 'collision.txt')
  git('commit', '-q', '-m', 'move the line the stash also touches')

  // An untracked file left in place, so the push preview has something to
  // report as NOT stashed (A2). It is never stashed by this fixture.
  writeFileSync(join(root, 'untracked-note.txt'), 'not in the index\n')

  return {
    root,
    // Newest first, exactly as `GET /api/stashes` returns them.
    entries: ['will not apply cleanly', 'WIP on main', 'half-finished refactor'],
    stashCount: 3,
    conflictingSelector: 'stash@{0}',
    conflictingPath: 'collision.txt',
    untracked: 'untracked-note.txt',
  }
}
