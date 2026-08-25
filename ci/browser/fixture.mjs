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
import { dirname, join } from 'node:path'

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

/** #478: how many checkpoints the interleaved-twin fixture's branch carries in
 *  total, and how many of them are rewritten so the pushed twin diverges. Five
 *  and three, so BOTH chains clear MIN_RUN on their own (the local chain keeps
 *  the two shared checkpoints, the remote chain has three of its own) and the
 *  two runs come out different lengths — a fixture where both markers said the
 *  same number could not tell a correct grouping from a swapped one.
 *  Asserted directly by the collapse spec, so they must stay in sync. */
export const TWIN_CHECKPOINTS = 5
export const TWIN_REWRITTEN = 3

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

/**
 * A THIRD repository, for the conflicts that cannot be resolved by picking
 * lines (M4.31d, #430).
 *
 * Its own repository for exactly the reason `buildConflictFixture` is separate
 * from `buildFixture`: the #428/#429 specs assert an exact conflicted count
 * (`toHaveCount(2)`) and one of them mutates the fixture as it resolves. Adding
 * paths to that repo would fail those specs for a reason that has nothing to do
 * with what they test.
 *
 * Two shapes, chosen because they are the two #430 can actually build:
 *
 *   `logo.png` — binary/binary. Real NUL bytes in the first 8000, so git's own
 *   sniff calls it binary on both sides. Neither pane may render it as text,
 *   and the note must say why rather than only printing a byte count.
 *
 *   `doomed.txt` — delete/modify. `theirs` deletes it, `ours` edits it, so git
 *   reports `UD` (DeletedByThem). This is the case that exposed the defect the
 *   honesty review found: the index shows "no stage 3", which looks identical
 *   to an add-by-us, and only `kind` tells them apart.
 *
 * Deliberately NOT built here: a rename conflict. Git records no rename
 * information for conflicted paths, so there is nothing for a fixture to
 * produce and nothing for the UI to read — see #430's ADR.
 */
export function buildNonTextConflictFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  // A NUL in the first bytes is what git's own binary sniff looks for; a .png
  // extension alone would not make it binary.
  const png = (marker) => Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x00, 0x00]), Buffer.from(marker)])

  git('init', '-q', '-b', 'main')
  writeFileSync(join(root, 'logo.png'), png('ancestor'))
  writeFileSync(join(root, 'doomed.txt'), 'the original line\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'seed: a binary file and a file one side will delete')

  git('checkout', '-q', '-b', 'theirs')
  writeFileSync(join(root, 'logo.png'), png('theirs-version'))
  git('rm', '-q', 'doomed.txt')
  git('add', '-A')
  git('commit', '-q', '-m', 'theirs: change the binary, delete the text file')

  git('checkout', '-q', 'main')
  writeFileSync(join(root, 'logo.png'), png('ours-version'))
  writeFileSync(join(root, 'doomed.txt'), 'our edit to the doomed file\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'ours: change the binary, edit the text file')

  // Expected to fail — that is the fixture.
  try {
    git('merge', 'theirs')
  } catch {
    /* expected: leaves logo.png at stages 1/2/3 and doomed.txt as UD */
  }

  return { root, conflicted: ['doomed.txt', 'logo.png'] }
}

/**
 * A FOURTH repository, for the line-by-line editor (M4.31c, #432).
 *
 * Its own repository for the reason the two above already are, and this time
 * it was learned the hard way: the editor spec RESOLVES what it opens, and
 * running before `conflict-panes.spec.mjs` alphabetically it emptied that
 * spec's fixture and failed all four of its tests. `conflict-panes` says in
 * its own comment that it mutates the shared fixture and must run last — two
 * specs cannot both be last.
 *
 * Two text conflicts, because each test resolves one and a test that had to
 * share would be racing its sibling. Both sides text on both paths, so
 * `text_resolvable` is true and the editor is actually offered — a binary or
 * delete/modify path would be correctly refused it.
 */
export function buildEditorFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  git('init', '-q', '-b', 'main')
  writeFileSync(join(root, 'first.txt'), 'the common ancestor\n')
  writeFileSync(join(root, 'second.txt'), 'the common ancestor\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'seed both files')

  git('checkout', '-q', '-b', 'theirs')
  writeFileSync(join(root, 'first.txt'), 'their version\n')
  writeFileSync(join(root, 'second.txt'), 'their version\n')
  git('commit', '-q', '-am', 'theirs edits both')

  git('checkout', '-q', 'main')
  writeFileSync(join(root, 'first.txt'), 'our version\n')
  writeFileSync(join(root, 'second.txt'), 'our version\n')
  git('commit', '-q', '-am', 'ours edits both')

  // Expected to fail — that is the fixture.
  try {
    git('merge', 'theirs')
  } catch {
    /* expected: both paths left at stages 1/2/3 */
  }

  return { root, conflicted: ['first.txt', 'second.txt'] }
}

/**
 * A FIFTH repository, whose HEAD holds an object id nothing resolves (#473).
 *
 * Its own repository for the same reason every other fixture here is separate:
 * this one is deliberately BROKEN, and every other spec's repo must stay
 * usable. Nothing resolves HEAD here, so the graph has no current commit — do
 * not add assertions about rows to specs that open it.
 *
 * Why a browser fixture at all, when `head_notice` is host-tested: that test
 * proves the decision, and cannot prove it is REACHED. The consumer is
 * `app/mod.rs`, which is `#[cfg(target_arch = "wasm32")]` and which
 * `cargo test` never compiles — the exact shape this suite's README table
 * catalogues, and the shape #473 itself was.
 *
 * The HEAD is written by hand rather than produced by a git command, because
 * no porcelain command will put a repository into this state: it is what a
 * repository looks like after a bad manual ref write, or after the object a
 * detached HEAD pointed at is garbage-collected.
 */
export function buildBrokenHeadFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  git('init', '-q', '-b', 'main')
  writeFileSync(join(root, 'a.txt'), 'a\n')
  git('add', '-A')
  git('commit', '-q', '-m', 'seed: one real commit, so the branch still reads')

  // A well-formed object id with no object behind it. `main` still points at a
  // real commit, so the readable half of the repository survives — which is
  // the state the notice has to be legible against.
  writeFileSync(join(root, '.git/HEAD'), '0'.repeat(40) + '\n')

  return { root }
}

/**
 * A SIXTH repository: a branch and its DIVERGED remote-tracking twin, both
 * carrying checkpoint chains that interleave in display order (#478).
 *
 * Its own repository for the usual reason, and one more. The usual one: the
 * shared `fixture-repo` seeds a contiguous checkpoint run between commit 1 and
 * commit 2 precisely so it never shifts the newest-first indices half the suite
 * asserts against, and adding six commits and a remote to it would break those
 * for reasons that have nothing to do with folding. The extra one: this repo
 * needs a *remote*, and `buildFixture` has none — the whole defect is what a
 * branch looks like next to the twin a fetch brings back.
 *
 * **Why a browser fixture at all**, when `collapse.rs` is host-tested and
 * mutation-proven: the host tests prove `project` is CORRECT. They cannot prove
 * it is REACHED. Its consumers — `app/canvas.rs`, `render/nodes.rs`,
 * `render/edges.rs` — are `#[cfg(target_arch = "wasm32")]` and `cargo test`
 * never compiles them. That is the #68d/#69c/#350 shape this whole suite exists
 * for: pure logic, fully tested, with no live consumer.
 *
 * The shape, newest first, once the fetch has happened:
 *
 * ```
 *   checkpoint 5   (local)     <- feature/wip-twin
 *   checkpoint 5   (remote)    <- origin/feature/wip-twin
 *   checkpoint 4   (local)
 *   checkpoint 4   (remote)
 *   checkpoint 3   (local)
 *   checkpoint 3   (remote)
 *   checkpoint 2               <- shared: the fork point both chains descend from
 *   checkpoint 1               <- shared
 *   seed                       <- main
 * ```
 *
 * Every checkpoint number appears twice on different commits, which is exactly
 * what the issue reporter saw scrolling real history. The two chains alternate,
 * so EVERY display-adjacent pair is a cross-chain pair — the condition under
 * which the pre-#478 scan found no run longer than one and folded nothing.
 *
 * Commit times are pinned rather than taken from the clock, and the rewritten
 * half is offset thirty seconds later than the pushed half, so the interleave is
 * deterministic: the walk is `DateOrder`, so a fixture whose two chains shared a
 * timestamp would order them arbitrarily and the spec would flake.
 *
 * The remote is a real bare repository beside this one (`twin-origin.git`),
 * pushed to before the rewrite. Nothing here fakes a ref: `origin/feature/wip-twin` is a genuine
 * remote-tracking ref left behind by a push whose branch then moved, which is
 * what any pushed-then-rewritten branch looks like.
 */
export function buildInterleavedWipFixture(root) {
  rmSync(root, { recursive: true, force: true })
  mkdirSync(root, { recursive: true })
  // A sibling, deliberately NOT named after this repository: the picker matches
  // entries by name, and an origin whose name contained the repo's would make
  // `/interleaved-repo/` ambiguous the day someone hands the bare repo to the
  // server too.
  const originPath = join(dirname(root), 'twin-origin.git')
  rmSync(originPath, { recursive: true, force: true })
  mkdirSync(originPath, { recursive: true })

  const git = (...args) =>
    execFileSync('git', [...IDENT, '-C', root, ...args], {
      encoding: 'utf8',
      env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
    })

  // A fixed base time, so the row order below is a property of the fixture and
  // not of the minute it was built in.
  const at = (n, offsetSeconds) => {
    const t = Date.UTC(2026, 0, 2, 10, n, offsetSeconds) / 1000
    return `${t} +0000`
  }
  const checkpoint = (n, body, offsetSeconds) => {
    writeFileSync(join(root, 'wip-marker.txt'), `${body}\n`)
    git('add', 'wip-marker.txt')
    execFileSync('git', [...IDENT, '-C', root, 'commit', '-q', '-m', `wip(#478): auto-checkpoint ${n}`], {
      encoding: 'utf8',
      env: {
        ...process.env,
        GIT_CONFIG_GLOBAL: '/dev/null',
        GIT_CONFIG_SYSTEM: '/dev/null',
        GIT_AUTHOR_DATE: at(n, offsetSeconds),
        GIT_COMMITTER_DATE: at(n, offsetSeconds),
      },
    })
  }

  execFileSync('git', ['init', '-q', '--bare', originPath], {
    encoding: 'utf8',
    env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
  })
  git('init', '-q', '-b', 'main')
  git('remote', 'add', 'origin', originPath)
  writeFileSync(join(root, 'seed.txt'), 'a commit that is not a checkpoint\n')
  git('add', 'seed.txt')
  git('commit', '-q', '-m', 'seed: the branch point')

  git('checkout', '-q', '-b', 'feature/wip-twin')
  for (let n = 1; n <= TWIN_CHECKPOINTS; n += 1) checkpoint(n, `checkpoint ${n}`, 0)

  // Push BEFORE rewriting: this is what leaves a remote-tracking ref pointing
  // at commits the branch no longer contains.
  git('push', '-q', 'origin', 'feature/wip-twin')

  // The rewrite. Same messages, different commits, thirty seconds later each,
  // so every rewritten checkpoint sorts immediately above the one it replaced.
  git('reset', '-q', '--hard', `HEAD~${TWIN_REWRITTEN}`)
  for (let n = TWIN_CHECKPOINTS - TWIN_REWRITTEN + 1; n <= TWIN_CHECKPOINTS; n += 1) {
    checkpoint(n, `checkpoint ${n} (rewritten)`, 30)
  }

  // Make the twin visible as a remote-tracking ref in this repository.
  git('fetch', '-q', 'origin')

  return {
    root,
    originPath,
    checkpoints: TWIN_CHECKPOINTS,
    rewritten: TWIN_REWRITTEN,
  }
}
