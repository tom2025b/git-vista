// Build the throwaway git repository the browser tests drive.
//
// Every shape here exists because a specific defect lived in it. Keep them:
//   * a multi-hunk file      -- #210's hunk-to-hunk keyboard navigation
//   * a very large file      -- #69c's virtualization (a window must stay bounded)
//   * staged + unstaged + untracked, simultaneously
//                            -- #68d's status cards and #348's chip/panel agreement
//   * a value unique to commit 1, commit 2, the index and the worktree
//                            -- #366's explicit diff-mode discrimination
//
// The repo is regenerated from scratch on every run, so the assertions can name
// exact counts instead of matching loosely. Nothing here touches the user's
// repositories; it lives entirely under the temp dir it is handed.

import { execFileSync } from 'node:child_process'
import { mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

// Commit identity is set per-invocation, never through repo or global config:
// this box has repositories whose local user.email is a personal gmail address,
// and a bare `git commit` would silently pick it up.
const IDENT = [
  '-c', 'user.name=Claude_Max',
  '-c', 'user.email=262510778+tom2025b@users.noreply.github.com',
  '-c', 'commit.gpgsign=false',
  '-c', 'tag.gpgsign=false',
]

/** How many `wip(#N): auto-checkpoint M` commits the fixture seeds between
 *  commit 1 and commit 2 (#374). 3, not 2, so the fold is unambiguously a
 *  "run" rather than the MIN_RUN boundary case. Asserted directly by the
 *  collapse spec, so it must stay in sync with the seeding below. */
export const WIP_RUN_COUNT = 3

/** Line count of the big file. Large enough that rendering every line would be
 *  obviously different from rendering a window, small enough to stay fast. */
export const BIG_FILE_LINES = 4000

/** How many hunks `multi-hunk.txt` carries after its edit. Asserted directly by
 *  the keyboard-navigation test, so it must stay in sync with the edit below. */
export const MULTI_HUNK_COUNT = 4

export function buildFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  git('init', '-q', '-b', 'main')

  // --- commit 1: a file with well-separated regions, so edits land as distinct
  // hunks rather than merging into one. 12 lines of context between regions is
  // comfortably more than git's default 3 on each side.
  const region = (n) =>
    [`region ${n} start`, ...Array.from({ length: 12 }, (_, i) => `  line ${n}.${i}`), `region ${n} end`]
  const multiHunk = Array.from({ length: MULTI_HUNK_COUNT }, (_, i) => region(i + 1)).flat()
  writeFileSync(join(root, 'multi-hunk.txt'), multiHunk.join('\n') + '\n')
  writeFileSync(join(root, 'compare-mode.txt'), 'one\n')
  git('add', 'multi-hunk.txt', 'compare-mode.txt')
  git('commit', '-q', '-m', 'seed: multi-hunk file')
  git('branch', 'base')

  // --- a run of WIP-checkpoint commits (#374), sitting between commit 1 and
  // commit 2 so it never shifts the newest-first indices the other specs
  // assert against (LONG_PATCH=1, openDiff's default nth(0)). Exact message
  // shape the real `~/.local/bin/autocheckpoint` script produces, so
  // `is_wip_checkpoint` matches it for real rather than by coincidence.
  for (let n = 1; n <= WIP_RUN_COUNT; n += 1) {
    writeFileSync(join(root, 'wip-marker.txt'), `checkpoint ${n}\n`)
    git('add', 'wip-marker.txt')
    git('commit', '-q', '-m', `wip(#374): auto-checkpoint ${n}`)
  }

  // --- commit 2: the big file, added whole so its diff is BIG_FILE_LINES of "+".
  writeFileSync(
    join(root, 'big.txt'),
    Array.from({ length: BIG_FILE_LINES }, (_, i) => `line ${i} of the large file`).join('\n') + '\n',
  )
  writeFileSync(join(root, 'compare-mode.txt'), 'two\n')
  git('add', 'big.txt', 'compare-mode.txt')
  git('commit', '-q', '-m', 'seed: large file for the virtualization budget')

  // --- commit 3: LONG *and* multi-hunk at once. This is the shape #210 breaks
  // on, and neither ingredient alone reproduces it: a long single-hunk patch
  // scrolls without ever losing a header, and a short multi-hunk patch fits
  // inside one window so nothing unmounts. Only a patch whose later hunks sit
  // thousands of lines below its first can scroll a FOCUSED header out of the
  // DOM. Achieved by adding a bulk file in the same commit that edits every
  // region, so the patch carries one huge hunk plus MULTI_HUNK_COUNT small ones.
  const edited = multiHunk.map((l) => (l.endsWith('.6') ? l + ' [edited]' : l))
  writeFileSync(join(root, 'multi-hunk.txt'), edited.join('\n') + '\n')
  writeFileSync(
    join(root, 'bulk.txt'),
    Array.from({ length: 2000 }, (_, i) => `bulk line ${i}`).join('\n') + '\n',
  )
  git('add', 'multi-hunk.txt', 'bulk.txt')
  git('commit', '-q', '-m', `seed: bulk file plus edits to all ${MULTI_HUNK_COUNT} regions`)

  // --- commit 4 (HEAD): short and multi-hunk — the POSITIVE control. Keyboard
  // navigation is expected to work here, which is what makes the failure on
  // commit 3 evidence about virtualization rather than about the focus model.
  const twoEdits = edited.map((l) => (l.endsWith('.2') ? l + ' [again]' : l))
  writeFileSync(join(root, 'multi-hunk.txt'), twoEdits.join('\n') + '\n')
  git('add', 'multi-hunk.txt')
  git('commit', '-q', '-m', 'seed: short multi-hunk edit')

  // --- working state: one staged, one unstaged, two untracked. The status
  // surfaces must agree on this exact shape (#348).
  // These sentinels are deliberately different from compare-mode.txt's two
  // committed values. A test that accidentally requests index or worktree
  // content cannot satisfy the ref-vs-ref assertions by returning any patch.
  writeFileSync(join(root, 'staged.txt'), 'three\n')
  git('add', 'staged.txt')

  writeFileSync(join(root, 'multi-hunk.txt'), edited.join('\n') + '\nunstaged tail\nfour\n')

  writeFileSync(join(root, 'untracked-a.txt'), 'a\n')
  writeFileSync(join(root, 'untracked-b.txt'), 'b\n')

  return {
    root,
    expected: { staged: 1, unstaged: 1, untracked: 2 },
    bigFileLines: BIG_FILE_LINES,
    multiHunkCount: MULTI_HUNK_COUNT,
  }
}

/**
 * A SECOND repository, left mid-merge with real unresolved conflicts (M4.31a,
 * #428).
 *
 * Deliberately its own repository rather than a conflict added to
 * `buildFixture`. A conflicted index puts a repo in MERGING state and changes
 * the status headline, the section counts and the rebase-status surface — all
 * of which existing specs assert exact values for (#348's staged/unstaged/
 * untracked shape especially). Conflicting the shared fixture would make those
 * specs fail for a reason that has nothing to do with what they test.
 *
 * Two conflicted paths, chosen so the panes differ:
 *
 *   `both-modified.txt` — modify/modify. All three stages PRESENT, so every
 *   pane has content and the base pane is real.
 *
 *   `added-by-both.txt` — add/add. **No stage 1**, so the base pane is
 *   `Absent` — the exact case ADR 0063 spends its longest section on, and the
 *   one a renderer is most likely to paint as an empty box.
 */
export function buildConflictFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  git('init', '-q', '-b', 'main')
  writeFileSync(join(root, 'both-modified.txt'), 'the common ancestor\n')
  git('add', 'both-modified.txt')
  git('commit', '-q', '-m', 'seed: the common ancestor')

  git('checkout', '-q', '-b', 'theirs')
  writeFileSync(join(root, 'both-modified.txt'), 'their version\n')
  writeFileSync(join(root, 'added-by-both.txt'), 'theirs created this\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'theirs: edit and add')

  git('checkout', '-q', 'main')
  writeFileSync(join(root, 'both-modified.txt'), 'our version\n')
  writeFileSync(join(root, 'added-by-both.txt'), 'ours created this\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'ours: edit and add')

  // This merge is SUPPOSED to fail — that is the whole fixture. Not asserted,
  // for the same reason `conflicts.rs`'s own test fixture does not assert it.
  try {
    git('merge', 'theirs')
  } catch {
    /* expected: leaves the index at stages 1/2/3 */
  }

  return { root, conflicted: ['added-by-both.txt', 'both-modified.txt'] }
}
