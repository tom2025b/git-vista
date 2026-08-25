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
import { existsSync } from 'node:fs'
import { join } from 'node:path'

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
