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
import { dirname, join } from 'node:path'

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
 *
 * Read off the spawn failure rather than an `existsSync` probe first (#496).
 * That is not only what lets this file drop `node:fs` entirely -- it also
 * covers the case the probe missed: a binary that exists but cannot be
 * executed raises EACCES here, and used to sail past the check and die with
 * git's own message instead of this one.
 */
function build(shape, root) {
  try {
    execFileSync(FIXTURE_BIN, [shape, root], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] })
  } catch (err) {
    if (err?.code === 'ENOENT' || err?.code === 'EACCES') {
      throw new Error(
        `browser fixtures: cannot run the catalogue binary at ${FIXTURE_BIN} (${err.code})\n` +
          `               build it first:  cargo build -p git-vista-fixtures`,
      )
    }
    throw err
  }
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

/** The subject of the entry that cannot be applied cleanly — `stash@{0}`, the
 *  newest, so a spec reaches it as "the first row". Mirrors
 *  `browser::STASH_CONFLICTING_SUBJECT`. */
export const STASH_CONFLICTING_SUBJECT = 'will not apply cleanly'

/** The path that entry collides on. Mirrors `browser::STASH_CONFLICTING_PATH`. */
export const STASH_CONFLICTING_PATH = 'collision.txt'

/** Left in the working tree and never stashed, so the push preview has
 *  something real to report as NOT captured (A2). Mirrors
 *  `browser::STASH_UNTRACKED_FILE`. */
export const STASH_UNTRACKED_FILE = 'untracked-note.txt'

/** How many entries the fixture leaves on the stash. Asserted directly by the
 *  drawer spec, so it must stay in sync with `browser::STASH_COUNT`. */
export const STASH_COUNT = 3

/**
 * A SEVENTH repository, holding real stash entries (M3.24, #77).
 *
 * Its own repository for the reason every fixture here is its own: the drawer
 * spec asserts an exact stash count, and `buildFixture`'s repo is left
 * deliberately dirty (staged + unstaged + untracked, simultaneously) for #68d
 * and #348. Stashing in that repo would empty the working tree those specs
 * assert on, and stashing anywhere else would change a count.
 *
 * Shape and rationale: `git_vista_fixtures::browser::stash_fixture` — which is
 * also where the reason the automatic entry's subject deliberately collides
 * with the seed commit's is written down, because that collision is what
 * `helpers.mjs`'s scoped `openDrawer` locators exist to survive.
 */
export function buildStashFixture(root) {
  build('stash', root)
  return {
    root,
    // Newest first, exactly as `GET /api/stashes` returns them.
    entries: [STASH_CONFLICTING_SUBJECT, 'WIP on main', 'half-finished refactor'],
    stashCount: STASH_COUNT,
    conflictingSelector: 'stash@{0}',
    conflictingPath: STASH_CONFLICTING_PATH,
    untracked: STASH_UNTRACKED_FILE,
  }
}

/** The branch the merge-preview repo offers to merge, and the branch it goes
 *  into. Mirror `browser::MERGE_PREVIEW_BRANCH` / `MERGE_PREVIEW_INTO`. */
export const MERGE_PREVIEW_BRANCH = 'feature'
export const MERGE_PREVIEW_INTO = 'main'

/** How many commits each side carries past the shared base. Two, so the graph
 *  has real width and a lane mistake is visible. Mirrors
 *  `browser::MERGE_PREVIEW_DEPTH`. */
export const MERGE_PREVIEW_DEPTH = 2

/**
 * An EIGHTH repository: two branches diverged from one base, each two commits
 * deep on disjoint files, so merging `feature` into `main` is clean and
 * produces a real two-parent commit (M10.08 A6, #594).
 *
 * Its own repository, and never merged by anything. `buildFixture`'s repo has
 * only `base`, a plain ancestor of `main`, so merging it is "already up to
 * date" — a preview with an empty change list, which is the one picture that
 * proves nothing. It is also left deliberately dirty for #348, and a merge
 * previewed against a dirty tree answers a different question.
 *
 * Shape and rationale: `git_vista_fixtures::browser::merge_preview_fixture`.
 */
export function buildMergePreviewFixture(root) {
  build('merge-preview', root)
  return { root, branch: MERGE_PREVIEW_BRANCH, into: MERGE_PREVIEW_INTO, depth: MERGE_PREVIEW_DEPTH }
}

/** Mirrors `browser::WORKTREE_*`. The four desks and what each one proves are
 *  documented on `git_vista_fixtures::browser::worktree_fixture`. */
export const WORKTREE_OPEN_DESK = 'desk-two'
export const WORKTREE_OPEN_BRANCH = 'feature/desk-two'
export const WORKTREE_LOCKED_DESK = 'locked-desk'
export const WORKTREE_OUTSIDE_DESK = 'worktree-outside-desk'
export const WORKTREE_GHOST_DESK = 'ghost-desk'
/** Clean and servable, and closed only by the removal spec (M11.05, #550). */
export const WORKTREE_REMOVABLE_DESK = 'removable-desk'
/** The main worktree plus its five linked desks. */
export const WORKTREE_ROW_COUNT = 6

/**
 * A NINTH repository, whose desks span every state the drawer must tell apart
 * (M11.03, #548).
 *
 * Its own repository for a sharper reason than most fixtures here: `git
 * worktree add` binds a branch to a desk, and git then refuses to check that
 * branch out anywhere else. Adding desks to a shared fixture would silently
 * make branches other specs check out unavailable — the very collision M11.02
 * is about, arriving as an unrelated spec's failure.
 *
 * Note the outside desk is created as a SIBLING of `root`, not inside it, so
 * it lands outside every allowed root the server registers. That placement is
 * the whole reason the fence sentence is reachable at all.
 */
export function buildWorktreeFixture(root) {
  build('worktree', root)
  return {
    root,
    openDesk: WORKTREE_OPEN_DESK,
    openBranch: WORKTREE_OPEN_BRANCH,
    lockedDesk: WORKTREE_LOCKED_DESK,
    outsideDesk: WORKTREE_OUTSIDE_DESK,
    ghostDesk: WORKTREE_GHOST_DESK,
    removableDesk: WORKTREE_REMOVABLE_DESK,
    rowCount: WORKTREE_ROW_COUNT,
  }
}
