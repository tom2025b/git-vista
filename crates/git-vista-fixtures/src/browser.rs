//! The repositories the browser suite drives.
//!
//! These five shapes were built in JavaScript, in `ci/browser/fixture.mjs`,
//! until #448 moved them here. The harness now invokes the `gv-fixture` binary
//! instead, so there is one implementation of "a repository broken in shape X"
//! rather than two — see `docs/adr/0076`.
//!
//! # Why these are separate repositories from each other
//!
//! Not tidiness: every one of them was split out because sharing broke
//! something, and the reasons are worth keeping because they are the same
//! reasons that will apply to the next fixture anyone is tempted to extend.
//!
//! A conflicted index puts a repository into MERGING state, which changes the
//! status headline, the section counts and the rebase-status surface — all of
//! which existing specs assert exact values for. So the conflicts cannot live
//! in the main fixture. Then `#430` needed conflicts that cannot be resolved by
//! picking lines, but the `#428`/`#429` specs assert an exact conflicted count
//! of two, so those could not go in the conflict fixture either. Then the
//! line-editor spec *resolves* what it opens, and running before
//! `conflict-panes.spec.mjs` alphabetically it emptied that spec's fixture and
//! failed all four of its tests — two specs cannot both run last.
//!
//! Each split is a spec that would otherwise fail for a reason having nothing
//! to do with what it tests.
//!
//! # Identity
//!
//! These builders author as `Claude_Max`, byte-identical to what the JavaScript
//! used. No spec asserts on it, but a fixture whose commits change author is a
//! fixture whose rendered history changed, and this migration is supposed to
//! change nothing a spec can see.

use crate::git::{self, BROWSER};
use std::path::Path;

/// How many `wip(#N): auto-checkpoint M` commits [`main_fixture`] seeds between
/// commit 1 and commit 2 (#374).
///
/// Three, not two, so the fold is unambiguously a "run" rather than the MIN_RUN
/// boundary case. Asserted directly by the collapse spec.
pub const WIP_RUN_COUNT: usize = 3;

/// Line count of the big file.
///
/// Large enough that rendering every line would be obviously different from
/// rendering a window, small enough to stay fast.
pub const BIG_FILE_LINES: usize = 4000;

/// How many hunks `multi-hunk.txt` carries after its edit.
///
/// Asserted directly by the keyboard-navigation test.
pub const MULTI_HUNK_COUNT: usize = 4;

/// Empty `root` and initialise a fresh repository in it under the browser
/// identity.
///
/// The repository is rebuilt from scratch on every run, which is what lets the
/// specs assert exact counts instead of matching loosely.
fn fresh(root: &Path) {
    clear_or_refuse(root, "fixture root");
    git::init_as(BROWSER, root);
    mark(root);
}

/// The marker every directory this module creates carries, so it can tell a
/// directory it built from one it is about to destroy.
pub const FIXTURE_MARKER: &str = ".gv-fixture";

/// Recursively clear `path` — but **only** if this module plausibly created it.
///
/// # Why this guard exists (#510)
///
/// `fresh` used to `remove_dir_all` whatever it was handed, and `gv-fixture`
/// takes that path straight from `argv` with no validation. So
/// `gv-fixture main ~/projects/Git-Vista` deleted the checkout, and
/// `gv-fixture interleaved-wip /tmp/work/repo` deleted an unrelated
/// `/tmp/work/twin-origin.git` the caller had never named.
///
/// # Why a marker rather than a list of forbidden paths
///
/// A blocklist of "dangerous" directories is unwinnable — it has to anticipate
/// every path worth protecting, and it is wrong the first time someone keeps a
/// repository somewhere it did not think of. A marker inverts the question
/// from *"is this one of the paths we must not delete"* to *"is this one of
/// the paths we made"*, which is knowable.
///
/// An **empty** directory is cleared without a marker: creating the target
/// yourself and handing it over is an ordinary thing to do, and deleting
/// nothing destroys nothing.
///
/// # Why the marker lives inside `.git/`
///
/// It started at the fixture root and immediately broke
/// `the_main_fixture_has_one_staged_one_unstaged_and_two_untracked`: a file at
/// the root is an **untracked file**, and these fixtures exist precisely so
/// specs can assert exact untracked counts. Inside `.git/` it is invisible to
/// `git status` and still an ordinary file this function can look for.
///
/// A bare repository has no `.git/` subdirectory — its root *is* the git
/// directory — so [`marker_path`] resolves to the root there, and the same
/// property holds: nothing lists it, because a bare repo has no working tree.
fn clear_or_refuse(path: &Path, what: &str) {
    if !path.exists() {
        return;
    }
    let empty = std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if !empty && !marker_path(path).exists() {
        panic!(
            "gv-fixture: refusing to delete {what} {} — it is not empty and carries no \
             {FIXTURE_MARKER} marker, so this crate did not create it.\n\
             If you really mean to destroy it, remove it by hand first.",
            path.display()
        );
    }
    std::fs::remove_dir_all(path)
        .unwrap_or_else(|e| panic!("clear {what} {}: {e}", path.display()));
}

/// Where the marker lives for `path`: inside `.git/` when there is one, and at
/// the root otherwise (a bare repository, whose root already IS the git dir).
///
/// Either way it is somewhere `git status` will never list, which is the
/// property that matters — see [`clear_or_refuse`].
fn marker_path(path: &Path) -> std::path::PathBuf {
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        dot_git.join(FIXTURE_MARKER)
    } else {
        path.join(FIXTURE_MARKER)
    }
}

/// Write the marker that lets a later [`clear_or_refuse`] recognise our work.
fn mark(path: &Path) {
    std::fs::write(marker_path(path), MARKER_BODY).expect("write fixture marker");
}

/// What the marker file says, so a human who finds one knows what it is.
const MARKER_BODY: &str = "\
Built by `gv-fixture` (crates/git-vista-fixtures). This directory is rebuilt
from scratch on every run and its contents are disposable.

This file is the marker that permits that deletion: without it, gv-fixture
refuses to clear a non-empty directory rather than destroying work it did not
create (#510). Deleting this file does not protect the directory — it makes
gv-fixture refuse to touch it at all.
";

fn run(root: &Path, args: &[&str]) {
    git::run_as(BROWSER, root, args);
}

/// One region of `multi-hunk.txt`: a header, twelve body lines, a footer.
///
/// Twelve lines of context between regions is comfortably more than git's
/// default three on each side, so edits land as distinct hunks rather than
/// merging into one.
fn region(n: usize) -> Vec<String> {
    let mut lines = vec![format!("region {n} start")];
    lines.extend((0..12).map(|i| format!("  line {n}.{i}")));
    lines.push(format!("region {n} end"));
    lines
}

fn multi_hunk_lines() -> Vec<String> {
    (1..=MULTI_HUNK_COUNT).flat_map(region).collect()
}

fn joined(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// The repository every non-conflict browser spec drives.
///
/// ## What is wrong with it
///
/// Nothing is *broken*, but every shape in it exists because a specific defect
/// lived in it:
///
/// * a multi-hunk file — #210's hunk-to-hunk keyboard navigation
/// * a very large file — #69c's virtualization; a window must stay bounded
/// * staged, unstaged and untracked changes simultaneously — #68d's status
///   cards and #348's chip/panel agreement
/// * a value unique to commit 1, commit 2, the index and the worktree —
///   #366's explicit diff-mode discrimination
///
/// ## What git put on disk
///
/// Five commits on `main`, a `base` branch left at the first, and a working
/// tree holding exactly one staged file, one unstaged edit and two untracked
/// files.
///
/// The third commit is the subtle one: it adds a bulk file *in the same commit*
/// that edits every region of `multi-hunk.txt`, so the patch carries one huge
/// hunk plus four small ones. Neither ingredient alone reproduces #210 — a long
/// single-hunk patch scrolls without ever losing a header, and a short
/// multi-hunk patch fits inside one window so nothing unmounts. Only a patch
/// whose later hunks sit thousands of lines below its first can scroll a
/// *focused* header out of the DOM. The fourth commit is short and multi-hunk:
/// the positive control that makes the third commit's failure evidence about
/// virtualization rather than about the focus model.
///
/// ## Why it matters
///
/// The sentinels in `compare-mode.txt` are deliberately different at every
/// layer — `one` in commit 1, `two` in commit 2, `three` staged, `four` in the
/// worktree — so a test that accidentally asks for index or worktree content
/// cannot satisfy a ref-versus-ref assertion by returning any patch at all.
pub fn main_fixture(root: &Path) {
    fresh(root);

    let multi = multi_hunk_lines();
    git::write(root, "multi-hunk.txt", joined(&multi).as_bytes());
    git::write(root, "compare-mode.txt", b"one\n");
    run(root, &["add", "multi-hunk.txt", "compare-mode.txt"]);
    run(root, &["commit", "-q", "-m", "seed: multi-hunk file"]);
    run(root, &["branch", "base"]);

    // A run of WIP-checkpoint commits (#374), sitting between commit 1 and
    // commit 2 so it never shifts the newest-first indices other specs assert
    // against. The exact message shape `~/.local/bin/autocheckpoint` produces,
    // so `is_wip_checkpoint` matches it for real rather than by coincidence.
    for n in 1..=WIP_RUN_COUNT {
        git::write(
            root,
            "wip-marker.txt",
            format!("checkpoint {n}\n").as_bytes(),
        );
        run(root, &["add", "wip-marker.txt"]);
        run(
            root,
            &[
                "commit",
                "-q",
                "-m",
                &format!("wip(#374): auto-checkpoint {n}"),
            ],
        );
    }

    // Commit 2: the big file, added whole so its diff is BIG_FILE_LINES of "+".
    let big: Vec<String> = (0..BIG_FILE_LINES)
        .map(|i| format!("line {i} of the large file"))
        .collect();
    git::write(root, "big.txt", joined(&big).as_bytes());
    git::write(root, "compare-mode.txt", b"two\n");
    run(root, &["add", "big.txt", "compare-mode.txt"]);
    run(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "seed: large file for the virtualization budget",
        ],
    );

    // Commit 3: long AND multi-hunk at once — the shape #210 breaks on.
    let edited: Vec<String> = multi
        .iter()
        .map(|l| {
            if l.ends_with(".6") {
                format!("{l} [edited]")
            } else {
                l.clone()
            }
        })
        .collect();
    git::write(root, "multi-hunk.txt", joined(&edited).as_bytes());
    let bulk: Vec<String> = (0..2000).map(|i| format!("bulk line {i}")).collect();
    git::write(root, "bulk.txt", joined(&bulk).as_bytes());
    run(root, &["add", "multi-hunk.txt", "bulk.txt"]);
    run(
        root,
        &[
            "commit",
            "-q",
            "-m",
            &format!("seed: bulk file plus edits to all {MULTI_HUNK_COUNT} regions"),
        ],
    );

    // Commit 4 (HEAD): short and multi-hunk — the positive control.
    let two_edits: Vec<String> = edited
        .iter()
        .map(|l| {
            if l.ends_with(".2") {
                format!("{l} [again]")
            } else {
                l.clone()
            }
        })
        .collect();
    git::write(root, "multi-hunk.txt", joined(&two_edits).as_bytes());
    run(root, &["add", "multi-hunk.txt"]);
    run(root, &["commit", "-q", "-m", "seed: short multi-hunk edit"]);

    // Working state: one staged, one unstaged, two untracked (#348).
    git::write(root, "staged.txt", b"three\n");
    run(root, &["add", "staged.txt"]);

    let mut unstaged = joined(&edited);
    unstaged.push_str("unstaged tail\nfour\n");
    git::write(root, "multi-hunk.txt", unstaged.as_bytes());

    git::write(root, "untracked-a.txt", b"a\n");
    git::write(root, "untracked-b.txt", b"b\n");
}

/// A repository left mid-merge with two unresolved conflicts (M4.31a, #428).
///
/// ## What is wrong
///
/// Two conflicted paths, chosen so the panes genuinely differ:
///
/// * `both-modified.txt` — modify/modify. All three stages **present**, so
///   every pane has content and the base pane is real.
/// * `added-by-both.txt` — add/add. **No stage 1**, so the base pane is
///   `Absent` — the case ADR 0063 spends its longest section on, and the one a
///   renderer is most likely to paint as an empty box, telling the user the
///   file used to be empty when in fact it did not exist.
///
/// ## Why it matters
///
/// One repository holding both shapes is what makes the difference visible
/// side by side: if a renderer treats an absent base as an empty one, exactly
/// one of these two rows is wrong, and the other proves it is not a general
/// failure to render.
pub fn conflict_fixture(root: &Path) {
    fresh(root);

    git::write(root, "both-modified.txt", b"the common ancestor\n");
    run(root, &["add", "both-modified.txt"]);
    run(root, &["commit", "-q", "-m", "seed: the common ancestor"]);

    run(root, &["checkout", "-q", "-b", "theirs"]);
    git::write(root, "both-modified.txt", b"their version\n");
    git::write(root, "added-by-both.txt", b"theirs created this\n");
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", "theirs: edit and add"]);

    run(root, &["checkout", "-q", "main"]);
    git::write(root, "both-modified.txt", b"our version\n");
    git::write(root, "added-by-both.txt", b"ours created this\n");
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", "ours: edit and add"]);

    // Supposed to fail — that is the fixture.
    let _ = git::try_run_as(BROWSER, root, &["merge", "theirs"]);
}

/// A repository whose conflicts cannot be resolved by picking lines
/// (M4.31d, #430).
///
/// ## What is wrong
///
/// The two shapes #430 can actually build:
///
/// * `logo.png` — binary/binary. Real NUL bytes inside the first 8000, so
///   git's own sniff calls it binary on both sides. Neither pane may render it
///   as text, and the note must say *why* rather than only printing a byte
///   count.
/// * `doomed.txt` — delete/modify. `theirs` deletes it, `ours` edits it, so
///   git reports `UD` (DeletedByThem). This is the case that exposed the defect
///   the honesty review found: the index shows "no stage 3", which looks
///   identical to an add-by-us, and only `kind` tells them apart.
///
/// ## Why it matters
///
/// Deliberately **not** built here: a rename conflict. Git records no rename
/// information for conflicted paths, so there is nothing for a fixture to
/// produce and nothing for the UI to read — see #430's ADR. A fixture claiming
/// to offer one would be teaching a capability git does not have.
pub fn non_text_conflict_fixture(root: &Path) {
    fresh(root);

    // A NUL in the first bytes is what git's binary sniff looks for; a .png
    // extension alone would not make it binary.
    let png = |marker: &str| {
        let mut bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0x00];
        bytes.extend_from_slice(marker.as_bytes());
        bytes
    };

    git::write(root, "logo.png", &png("ancestor"));
    git::write(root, "doomed.txt", b"the original line\n");
    run(root, &["add", "-A"]);
    run(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "seed: a binary file and a file one side will delete",
        ],
    );

    run(root, &["checkout", "-q", "-b", "theirs"]);
    git::write(root, "logo.png", &png("theirs-version"));
    run(root, &["rm", "-q", "doomed.txt"]);
    run(root, &["add", "-A"]);
    run(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "theirs: change the binary, delete the text file",
        ],
    );

    run(root, &["checkout", "-q", "main"]);
    git::write(root, "logo.png", &png("ours-version"));
    git::write(root, "doomed.txt", b"our edit to the doomed file\n");
    run(root, &["add", "-A"]);
    run(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "ours: change the binary, edit the text file",
        ],
    );

    let _ = git::try_run_as(BROWSER, root, &["merge", "theirs"]);
}

/// A repository with two text conflicts, for the line-by-line editor
/// (M4.31c, #432).
///
/// ## What is wrong
///
/// `first.txt` and `second.txt` are both modify/modify, both sides text on
/// both paths — so `text_resolvable` is true and the editor is actually
/// offered. A binary or delete/modify path would be correctly refused it, and
/// the spec would have nothing to drive.
///
/// ## Why there are two
///
/// Each test resolves one. A test that had to share would be racing its
/// sibling: resolution is destructive, and the second test would open a file
/// the first had already finished with.
pub fn editor_fixture(root: &Path) {
    fresh(root);

    git::write(root, "first.txt", b"the common ancestor\n");
    git::write(root, "second.txt", b"the common ancestor\n");
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", "seed both files"]);

    run(root, &["checkout", "-q", "-b", "theirs"]);
    git::write(root, "first.txt", b"their version\n");
    git::write(root, "second.txt", b"their version\n");
    run(root, &["commit", "-q", "-am", "theirs edits both"]);

    run(root, &["checkout", "-q", "main"]);
    git::write(root, "first.txt", b"our version\n");
    git::write(root, "second.txt", b"our version\n");
    run(root, &["commit", "-q", "-am", "ours edits both"]);

    let _ = git::try_run_as(BROWSER, root, &["merge", "theirs"]);
}

/// A repository whose `HEAD` holds an object id nothing resolves (#473).
///
/// ## What is wrong
///
/// `.git/HEAD` holds forty zeroes — a well-formed object id with no object
/// behind it. `main` still points at a real commit, so the readable half of the
/// repository survives, which is the state the notice has to be legible
/// against. Nothing resolves HEAD here, so the graph has no current commit: do
/// not add assertions about rows to specs that open it.
///
/// ## Why it is a browser fixture at all
///
/// `head_notice` is host-tested, and that test proves the *decision*. It cannot
/// prove the decision is *reached*. The consumer is `app/mod.rs`, which is
/// `#[cfg(target_arch = "wasm32")]` and which `cargo test` never compiles —
/// exactly the shape #473 itself was.
///
/// See [`crate::broken_head`] for the same shape as the Rust suites use it,
/// including the trap that `git rev-parse --verify HEAD` *succeeds* here.
pub fn broken_head_fixture(root: &Path) {
    fresh(root);

    git::write(root, "a.txt", b"a\n");
    run(root, &["add", "-A"]);
    run(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "seed: one real commit, so the branch still reads",
        ],
    );

    std::fs::write(root.join(".git/HEAD"), format!("{}\n", "0".repeat(40)))
        .expect("overwrite .git/HEAD");
}

/// A builder: empties the directory it is given and writes one shape into it.
pub type Builder = fn(&Path);

/// How many checkpoints [`interleaved_wip_fixture`]'s branch carries in total,
/// and how many are rewritten so the pushed twin diverges (#478).
///
/// Five and three, so **both** chains clear MIN_RUN on their own — the local
/// chain keeps the two shared checkpoints, the remote chain has three of its
/// own — and the two runs come out different lengths. A fixture where both
/// markers said the same number could not tell a correct grouping from a
/// swapped one. Asserted directly by the collapse spec.
pub const TWIN_CHECKPOINTS: usize = 5;

/// How many of [`TWIN_CHECKPOINTS`] are rewritten after the push.
pub const TWIN_REWRITTEN: usize = 3;

/// A branch and its **diverged remote-tracking twin**, whose checkpoint chains
/// interleave in display order (#478).
///
/// ## What is wrong
///
/// Every checkpoint number appears **twice, on different commits** — which is
/// exactly what the issue reporter saw scrolling real history. The branch was
/// pushed, then rewritten, so `origin/feature/wip-twin` still points at commits
/// the branch no longer contains.
///
/// ## What git put on disk
///
/// A real bare repository beside this one (`twin-origin.git`), pushed to before
/// the rewrite, then fetched. Nothing here fakes a ref: the remote-tracking ref
/// is genuinely what a push whose branch then moved leaves behind. Newest
/// first, the history reads:
///
/// ```text
///   checkpoint 5   (local)     <- feature/wip-twin
///   checkpoint 5   (remote)    <- origin/feature/wip-twin
///   checkpoint 4   (local)
///   checkpoint 4   (remote)
///   checkpoint 3   (local)
///   checkpoint 3   (remote)
///   checkpoint 2               <- shared: the fork point both chains descend from
///   checkpoint 1               <- shared
///   seed                       <- main
/// ```
///
/// Commit times are pinned rather than taken from the clock, and the rewritten
/// half is offset thirty seconds later than the pushed half. The walk is
/// `DateOrder`, so a fixture whose two chains shared a timestamp would order
/// them arbitrarily and the spec would flake.
///
/// ## Why it matters
///
/// The two chains **alternate**, so every display-adjacent pair is a
/// cross-chain pair — the condition under which the pre-#478 scan found no run
/// longer than one and folded nothing. A fixture with two separated chains
/// would not reproduce it.
///
/// The bare repository is deliberately *not* named after this one: the picker
/// matches entries by name, and an origin whose name contained the repo's would
/// make the match ambiguous the day someone hands the bare repo to the server
/// too.
pub fn interleaved_wip_fixture(root: &Path) {
    fresh(root);

    let origin = root
        .parent()
        .expect("fixture root must have a parent")
        .join("twin-origin.git");
    // The one place this module writes OUTSIDE the root it was given, which is
    // why it goes through the same guard (#510). The CLI's doc discloses it.
    clear_or_refuse(&origin, "twin origin");
    std::fs::create_dir_all(&origin).expect("create twin origin");
    git::run_as(BROWSER, &origin, &["init", "-q", "--bare"]);
    mark(&origin);

    // A fixed base time, so the row order is a property of the fixture rather
    // than of the minute it was built in. 2026-01-02T10:<n>:<offset>Z.
    let at = |n: usize, offset: usize| -> String {
        let base = 1_767_349_200_i64; // 2026-01-02T10:00:00Z
        format!("{} +0000", base + (n as i64) * 60 + offset as i64)
    };
    let checkpoint = |n: usize, body: &str, offset: usize| {
        git::write(
            root,
            "wip-marker.txt",
            format!(
                "{body}
"
            )
            .as_bytes(),
        );
        run(root, &["add", "wip-marker.txt"]);
        git::run_dated_as(
            BROWSER,
            root,
            &[
                "commit",
                "-q",
                "-m",
                &format!("wip(#478): auto-checkpoint {n}"),
            ],
            &at(n, offset),
        );
    };

    run(
        root,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    git::write(root, "seed.txt", b"a commit that is not a checkpoint\n");
    run(root, &["add", "seed.txt"]);
    run(root, &["commit", "-q", "-m", "seed: the branch point"]);

    run(root, &["checkout", "-q", "-b", "feature/wip-twin"]);
    for n in 1..=TWIN_CHECKPOINTS {
        checkpoint(n, &format!("checkpoint {n}"), 0);
    }

    // Push BEFORE rewriting: this is what leaves a remote-tracking ref pointing
    // at commits the branch no longer contains.
    run(root, &["push", "-q", "origin", "feature/wip-twin"]);

    // The rewrite. Same messages, different commits, thirty seconds later each,
    // so every rewritten checkpoint sorts immediately above the one it replaced.
    run(
        root,
        &["reset", "-q", "--hard", &format!("HEAD~{TWIN_REWRITTEN}")],
    );
    for n in (TWIN_CHECKPOINTS - TWIN_REWRITTEN + 1)..=TWIN_CHECKPOINTS {
        checkpoint(n, &format!("checkpoint {n} (rewritten)"), 30);
    }

    // Make the twin visible as a remote-tracking ref in this repository.
    run(root, &["fetch", "-q", "origin"]);
}

/// The seed commit's subject — and, because of how `git stash` builds its
/// automatic message, half of `stash@{1}`'s subject too.
///
/// This collision is deliberate. See [`stash_fixture`]'s "Why it matters".
pub const STASH_SEED_SUBJECT: &str = "seed: two tracked files";

/// `stash@{2}`'s subject: the `git stash push -m <message>` form, written by
/// the user and stored verbatim.
pub const STASH_NAMED_SUBJECT: &str = "half-finished refactor";

/// The prefix git itself writes for a bare `git stash` — `stash@{1}` here.
///
/// The full message is `WIP on <branch>: <sha> <subject>`; only this much is
/// git's own vocabulary, and it is what the drawer keys "automatic" off.
pub const STASH_AUTO_PREFIX: &str = "WIP on main";

/// `stash@{0}`'s subject: the entry that cannot be applied cleanly.
pub const STASH_CONFLICTING_SUBJECT: &str = "will not apply cleanly";

/// The path `stash@{0}` collides on.
pub const STASH_CONFLICTING_PATH: &str = "collision.txt";

/// Left in the working tree by [`stash_fixture`] and never stashed by it, so
/// the push preview has something to report as NOT captured.
pub const STASH_UNTRACKED_FILE: &str = "untracked-note.txt";

/// How many entries [`stash_fixture`] leaves on the stash. Asserted directly by
/// the drawer spec, so it is contract rather than incidental.
pub const STASH_COUNT: usize = 3;

/// A repository with **three real stash entries, one of which cannot be
/// applied** (M3.24, #77).
///
/// ## What is wrong with it
///
/// Nothing is corrupt. What is *wrong* is the thing the stash drawer exists to
/// tell the truth about: **a pop is two operations, and the first can fail.**
/// `git stash pop` is an apply followed by a drop, and when the apply
/// conflicts, git applies what it can, writes conflict markers, leaves the
/// entry on the list and exits non-zero. A drawer that reports "popped" there
/// has lied about the user's data twice over — the entry was not dropped, and
/// the working tree *was* changed (ADR 0077).
///
/// So `stash@{0}` is built to conflict, on purpose, and it is deliberately the
/// **newest** entry so a spec can reach it as "the first row" without depending
/// on row ordering beyond that.
///
/// ## What git put on disk
///
/// One commit, then three stashes, then a fourth commit that is what makes the
/// newest stash unappliable. Newest first, `git stash list` reads:
///
/// ```text
///   stash@{0}  On main: will not apply cleanly          <- conflicts on apply
///   stash@{1}  WIP on main: <sha> seed: two tracked files
///   stash@{2}  On main: half-finished refactor
/// ```
///
/// The two message forms are both present because git writes them
/// differently, and the drawer has to tell them apart:
///
/// * `git stash push -m <message>` stores the message as given. The drawer
///   shows it as written, marked as the user's — no "auto" pill.
/// * a bare `git stash` makes git compose `WIP on <branch>: <sha> <subject>`
///   from the branch and the base commit. The drawer parses that: the branch
///   becomes a pill, the sha is dropped, and the entry IS marked automatic.
///
/// `stash@{0}` is made unappliable the only way that is honest — by moving the
/// ground under it. An edit to one line of `collision.txt` is stashed; then a
/// *different* edit to that same line is committed. The stash's base no longer
/// matches, so `git stash apply` cannot merge it and leaves the path
/// conflicted.
///
/// `untracked-note.txt` is left untracked and never stashed, so the push
/// preview has something real to report as NOT captured (A2). A preview that
/// always warns would satisfy the letter of that criterion and miss its
/// failure — a user believing they stashed a new file that git left on disk.
///
/// ## Why it matters
///
/// Beyond the pop: **the automatic entry's subject is also the seed commit's
/// subject**, because git copies the base commit's subject verbatim into `WIP
/// on <branch>: <sha> <subject>`. That is not an accident of this fixture that
/// could be tidied away — it is what git does, so any fixture built this way
/// has it.
///
/// The consequence is that `"seed: two tracked files"` resolves in the graph's
/// SVG `<title>`, in the activity feed, and in the drawer at once, and a
/// page-wide `getByText` dies of a strict-mode violation. That is a real
/// failure this suite hit on its first run against a browser, and it is why
/// `helpers.mjs`'s `openDrawer` returns the drawer's own region and every
/// caller queries inside it. Keeping the collision is the point: it is the
/// cheapest standing proof that the drawer's locators are scoped.
///
/// ## Why it is its own repository
///
/// Two reasons, both the same shape as the other splits above. It is the only
/// repository here whose stash list has an asserted count, so anything else
/// stashing into it would break that count. And applying `stash@{0}` leaves
/// `collision.txt` conflicted — a state no other spec's repository may
/// inherit.
pub fn stash_fixture(root: &Path) {
    fresh(root);

    git::write(root, "tracked.txt", b"the committed line\n");
    git::write(root, STASH_CONFLICTING_PATH, b"original\n");
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", STASH_SEED_SUBJECT]);

    // Ends up as stash@{2} once the two below land: the `-m` message form.
    git::write(root, "tracked.txt", b"a named change\n");
    run(root, &["stash", "push", "-m", STASH_NAMED_SUBJECT]);

    // stash@{1}: the automatic `WIP on main: <sha> <subject>` form. No -m, so
    // git composes the message — and copies the seed commit's subject into it.
    git::write(root, "tracked.txt", b"an unnamed change\n");
    run(root, &["stash"]);

    // stash@{0}: the one that conflicts. Stash an edit to collision.txt, then
    // commit a DIFFERENT edit to the same line.
    git::write(root, STASH_CONFLICTING_PATH, b"the stashed edit\n");
    run(root, &["stash", "push", "-m", STASH_CONFLICTING_SUBJECT]);
    git::write(
        root,
        STASH_CONFLICTING_PATH,
        b"a conflicting committed edit\n",
    );
    run(root, &["add", STASH_CONFLICTING_PATH]);
    run(
        root,
        &["commit", "-q", "-m", "move the line the stash also touches"],
    );

    // Never stashed by this fixture — see the doc comment.
    git::write(root, STASH_UNTRACKED_FILE, b"not in the index\n");
}

/// The branch `merge_preview_fixture` offers to merge, and the branch it is
/// merged into. Named constants because the browser spec matches on the menu
/// item's text, which interpolates both.
pub const MERGE_PREVIEW_BRANCH: &str = "feature";
pub const MERGE_PREVIEW_INTO: &str = "main";

/// How many commits each side carries past the shared base.
///
/// Two, not one. A branch a single commit from its base gives the preview a
/// graph with no width: every lane assignment is trivially right, and a
/// renderer that put both chains in one lane would look correct. Mirrors the
/// same reasoning `divergent::merge_clean_two_branch` states at length.
pub const MERGE_PREVIEW_DEPTH: usize = 2;

/// Two branches diverged from one base, each two commits deep, touching
/// disjoint files — so merging `feature` into `main` produces a real
/// two-parent commit and no conflict (M10.08 A6, #594).
///
/// ## Why a browser fixture and not `divergent::merge_clean_two_branch`
///
/// Same topology, deliberately, and the reasoning there applies here
/// unchanged. What differs is the delivery: that one hands back a
/// `tempfile::TempDir` for a Rust test to open, and this suite needs a
/// repository at a path the *server* is told to serve, built by the
/// `gv-fixture` binary. Sharing would mean widening that function's return
/// shape for one caller; the topology is eight git commands and stating them
/// twice is cheaper than a seam.
///
/// ## Why it is its own repository
///
/// `main_fixture` has a `base` branch, but `base` is a plain ancestor of
/// `main`, so merging it is "already up to date" — a preview with an empty
/// change list, which is exactly the picture that proves nothing. And
/// `main_fixture` is left deliberately dirty (one staged, one unstaged, two
/// untracked, asserted by #348's spec); a merge previewed against a dirty tree
/// is a different question than the one A6 asks.
///
/// ## Why nothing here is ever merged
///
/// The fixture is handed to the app *pre*-merge, and the spec only ever opens
/// a confirmation and cancels it. That is the point: a preview must draw the
/// merge without performing it, so a fixture that arrived already merged could
/// not tell a working preview from a picture of the past.
pub fn merge_preview_fixture(root: &Path) {
    fresh(root);

    git::write(root, "shared.txt", b"shared\n");
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", "seed: the shared base"]);

    run(root, &["checkout", "-q", "-b", MERGE_PREVIEW_BRANCH]);
    for n in 1..=MERGE_PREVIEW_DEPTH {
        git::write(
            root,
            &format!("feature-{n}.txt"),
            format!("feature work {n}\n").as_bytes(),
        );
        run(root, &["add", "-A"]);
        run(
            root,
            &["commit", "-q", "-m", &format!("feature: add {n}.txt")],
        );
    }

    run(root, &["checkout", "-q", MERGE_PREVIEW_INTO]);
    for n in 1..=MERGE_PREVIEW_DEPTH {
        git::write(
            root,
            &format!("main-{n}.txt"),
            format!("main work {n}\n").as_bytes(),
        );
        run(root, &["add", "-A"]);
        run(root, &["commit", "-q", "-m", &format!("main: add {n}.txt")]);
    }
}

/// Every browser shape, by the name the `gv-fixture` binary accepts.
pub const SHAPES: &[(&str, Builder)] = &[
    ("main", main_fixture),
    ("conflict", conflict_fixture),
    ("non-text-conflict", non_text_conflict_fixture),
    ("editor", editor_fixture),
    ("broken-head", broken_head_fixture),
    ("interleaved-wip", interleaved_wip_fixture),
    ("stash", stash_fixture),
    ("merge-preview", merge_preview_fixture),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn build(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let f = SHAPES.iter().find(|(n, _)| *n == name).unwrap().1;
        f(&root);
        (dir, root)
    }

    /// #348's exact working-tree shape. The specs assert these counts
    /// literally, so they are contract, not incidental.
    #[test]
    fn the_main_fixture_has_one_staged_one_unstaged_and_two_untracked() {
        let (_d, root) = build("main");
        // Untrimmed: the first column is a space for "changed but not
        // staged", and trimming would shift the first line's columns left.
        let status = git::out_exact_as(BROWSER, &root, &["status", "--porcelain"]);

        let mut staged = 0;
        let mut unstaged = 0;
        let mut untracked = 0;
        for line in status.lines().filter(|l| l.len() >= 2) {
            let (x, y) = (line.as_bytes()[0], line.as_bytes()[1]);
            if x == b'?' {
                untracked += 1;
                continue;
            }
            if x != b' ' {
                staged += 1;
            }
            if y != b' ' {
                unstaged += 1;
            }
        }
        assert_eq!((staged, unstaged, untracked), (1, 1, 2), "{status}");
    }

    /// The WIP run must be a run — the collapse spec asserts on its length.
    #[test]
    fn the_main_fixture_seeds_exactly_the_declared_wip_run() {
        let (_d, root) = build("main");
        let log = git::out_as(BROWSER, &root, &["log", "--format=%s"]);
        let wip = log
            .lines()
            .filter(|l| l.starts_with("wip(#374): auto-checkpoint"))
            .count();
        assert_eq!(wip, WIP_RUN_COUNT);
    }

    /// `compare-mode.txt` must differ at all four layers, or a test asking for
    /// the wrong one could still be satisfied.
    #[test]
    fn compare_mode_differs_at_commit_one_commit_two_index_and_worktree() {
        let (_d, root) = build("main");
        assert_eq!(
            git::out_as(BROWSER, &root, &["show", "base:compare-mode.txt"]),
            "one"
        );
        assert_eq!(
            git::out_as(BROWSER, &root, &["show", "HEAD:compare-mode.txt"]),
            "two"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("staged.txt")).unwrap(),
            "three\n"
        );
        let worktree = std::fs::read_to_string(root.join("multi-hunk.txt")).unwrap();
        assert!(worktree.ends_with("unstaged tail\nfour\n"), "{worktree:?}");
    }

    /// The conflict fixture's two rows must be the two DIFFERENT shapes it
    /// claims — one with a base and one without. If they were the same shape,
    /// the spec comparing the panes would be comparing a thing with itself.
    #[test]
    fn the_conflict_fixture_pairs_a_present_base_with_an_absent_one() {
        let (_d, root) = build("conflict");
        let unmerged = git::out_as(BROWSER, &root, &["ls-files", "-u"]);

        let stages = |path: &str| {
            let mut v: Vec<u8> = unmerged
                .lines()
                .filter(|l| l.ends_with(path))
                .filter_map(|l| {
                    l.split('\t')
                        .next()?
                        .split_whitespace()
                        .nth(2)?
                        .parse()
                        .ok()
                })
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(stages("both-modified.txt"), vec![1, 2, 3]);
        assert_eq!(stages("added-by-both.txt"), vec![2, 3]);
    }

    /// #430's premise: git itself must call `logo.png` binary, and `doomed.txt`
    /// must be `UD` — deleted by them — not merely "missing a stage".
    #[test]
    fn the_non_text_fixture_is_binary_on_one_path_and_deleted_by_them_on_the_other() {
        let (_d, root) = build("non-text-conflict");

        let patch = git::out_as(
            BROWSER,
            &root,
            &["diff", "main", "theirs", "--", "logo.png"],
        );
        assert!(patch.contains("Binary files"), "{patch:?}");

        let status = git::out_as(BROWSER, &root, &["status", "--porcelain"]);
        assert!(
            status
                .lines()
                .any(|l| l.starts_with("UD ") && l.ends_with("doomed.txt")),
            "expected a UD row for doomed.txt: {status}"
        );
    }

    /// Both editor paths must be ordinary text conflicts, or the editor is
    /// never offered and the spec has nothing to drive.
    #[test]
    fn the_editor_fixture_offers_two_plain_text_conflicts() {
        let (_d, root) = build("editor");
        let status = git::out_as(BROWSER, &root, &["status", "--porcelain"]);
        for path in ["first.txt", "second.txt"] {
            assert!(
                status
                    .lines()
                    .any(|l| l.starts_with("UU ") && l.ends_with(path)),
                "expected UU for {path}: {status}"
            );
            let text = std::fs::read_to_string(root.join(path)).unwrap();
            assert!(text.contains("<<<<<<<"), "{path} should carry markers");
        }
    }

    /// The interleave is the whole shape: every display-adjacent pair must be
    /// a cross-chain pair, or the condition #478 fixed is not reproduced.
    #[test]
    fn the_two_checkpoint_chains_alternate_in_display_order() {
        let (_d, root) = build("interleaved-wip");
        let log = git::out_as(
            BROWSER,
            &root,
            &[
                "log",
                "--date-order",
                "--format=%s|%d",
                "feature/wip-twin",
                "origin/feature/wip-twin",
            ],
        );
        let subjects: Vec<&str> = log
            .lines()
            .map(|l| l.split('|').next().unwrap())
            .filter(|s| s.starts_with("wip(#478)"))
            .collect();
        // Every checkpoint number in the rewritten range appears twice.
        for n in (TWIN_CHECKPOINTS - TWIN_REWRITTEN + 1)..=TWIN_CHECKPOINTS {
            let needle = format!("wip(#478): auto-checkpoint {n}");
            assert_eq!(
                subjects.iter().filter(|s| **s == needle).count(),
                2,
                "checkpoint {n} must appear on two different commits: {subjects:?}"
            );
        }
    }

    /// The remote-tracking ref must be real, and must point at commits the
    /// branch no longer contains — a faked ref would prove nothing.
    #[test]
    fn the_twin_is_a_real_remote_tracking_ref_the_branch_has_left_behind() {
        let (_d, root) = build("interleaved-wip");
        let local = git::out_as(BROWSER, &root, &["rev-parse", "feature/wip-twin"]);
        let remote = git::out_as(BROWSER, &root, &["rev-parse", "origin/feature/wip-twin"]);
        assert_ne!(local, remote, "the twin must have diverged");
        assert!(
            !git::try_run_as(
                BROWSER,
                &root,
                &["merge-base", "--is-ancestor", &remote, &local]
            ),
            "the remote tip must NOT still be on the branch"
        );
    }

    #[test]
    fn the_broken_head_fixture_has_an_unresolvable_head_and_a_readable_branch() {
        let (_d, root) = build("broken-head");
        assert!(!git::try_run_as(
            BROWSER,
            &root,
            &["rev-parse", "--verify", "HEAD^{commit}"]
        ));
        assert_eq!(
            git::out_as(
                BROWSER,
                &root,
                &["rev-parse", "--verify", "refs/heads/main"]
            )
            .len(),
            40
        );
    }

    /// Rebuilding over an existing directory must produce the fixture, not a
    /// merge of two runs — the harness rebuilds from scratch on every run and
    /// the specs' exact counts depend on it.
    #[test]
    fn building_twice_into_one_root_yields_the_fixture_not_the_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        main_fixture(&root);
        std::fs::write(root.join("stray.txt"), "left over\n").unwrap();
        main_fixture(&root);
        assert!(!root.join("stray.txt").exists(), "root must be cleared");
    }

    /// Three entries, in the two message forms git actually writes.
    ///
    /// Read off `git stash list`, which is what the server's own stash read
    /// parses — not off the constants, which are this builder's inputs and
    /// would agree with themselves.
    #[test]
    fn the_stash_fixture_leaves_three_entries_in_both_message_forms() {
        let (_d, root) = build("stash");
        let list = git::out_as(BROWSER, &root, &["stash", "list"]);
        let lines: Vec<&str> = list.lines().collect();

        assert_eq!(lines.len(), STASH_COUNT, "{list}");

        // Newest first, exactly as `GET /api/stashes` returns them.
        assert!(
            lines[0].starts_with("stash@{0}: ") && lines[0].ends_with(STASH_CONFLICTING_SUBJECT),
            "newest entry is not the conflicting one: {list}"
        );
        // The bare-`git stash` form. git composes this; the fixture never
        // writes the string, so an assertion on it is an assertion about git.
        assert!(
            lines[1].contains(STASH_AUTO_PREFIX),
            "the middle entry is not git's automatic form: {list}"
        );
        assert!(
            lines[2].ends_with(STASH_NAMED_SUBJECT),
            "the oldest entry is not the -m form: {list}"
        );
        // The two forms must be distinguishable, which is the whole point of
        // having both: exactly one of them is git's `WIP on` vocabulary.
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains(STASH_AUTO_PREFIX))
                .count(),
            1,
            "the drawer tells the forms apart by this prefix: {list}"
        );
    }

    /// The A4 property: `stash@{0}` really cannot be applied.
    ///
    /// Proved by applying it, not by trusting the shape. A pop is an apply plus
    /// a drop, so a fixture whose apply quietly succeeded would let the drawer
    /// report "Popped" and still pass every spec built on it.
    #[test]
    fn the_newest_stash_entry_conflicts_on_apply_and_survives_it() {
        let (_d, root) = build("stash");

        let applied = git::try_run_as(BROWSER, &root, &["stash", "apply", "stash@{0}"]);
        assert!(
            !applied,
            "stash@{{0}} applied cleanly; it is supposed to conflict"
        );

        // Unmerged, not merely modified: `UU` is what routes the path into the
        // shared conflict view (A3).
        let status = git::out_exact_as(BROWSER, &root, &["status", "--porcelain"]);
        assert!(
            status
                .lines()
                .any(|l| l.starts_with("UU ") && l.ends_with(STASH_CONFLICTING_PATH)),
            "expected an unmerged {STASH_CONFLICTING_PATH}: {status}"
        );

        // A refused apply drops nothing. This is the half a drawer is most
        // likely to get wrong, so the fixture pins it.
        let after = git::out_as(BROWSER, &root, &["stash", "list"]);
        assert_eq!(after.lines().count(), STASH_COUNT, "{after}");
    }

    /// The collision `openDrawer`'s scoped locators exist to survive.
    ///
    /// git copies the base commit's subject verbatim into `WIP on <branch>:
    /// <sha> <subject>`, so the automatic entry's subject is also a commit
    /// subject and the same string resolves in the graph's SVG `<title>`, in
    /// the activity feed AND in the drawer at once. A page-wide `getByText`
    /// therefore dies of a strict-mode violation — a real failure this suite
    /// hit on its first run against a browser, and the reason `helpers.mjs`'s
    /// `openDrawer` returns the drawer's own region.
    ///
    /// Pinned as *the oldest commit's* subject, not merely "some commit's":
    /// with a bare `git stash` the collision is unavoidable, so a test that
    /// asked only for a collision could not go red while a fixture still had
    /// one. Which commit the automatic entry hangs off is a real choice — take
    /// it later and `stash@{1}` holds a different base — and it is losable, so
    /// it is what this pins.
    ///
    /// Both sides are read back from git: the stash message from `stash list`,
    /// the subject from `log`. Neither is compared against the constant that
    /// produced it.
    #[test]
    fn the_automatic_stash_entry_repeats_the_oldest_commits_subject() {
        let (_d, root) = build("stash");

        let auto = git::out_as(BROWSER, &root, &["stash", "list"])
            .lines()
            .find(|l| l.contains(STASH_AUTO_PREFIX))
            .expect("the automatic entry")
            .to_string();

        let subjects = git::out_as(BROWSER, &root, &["log", "--format=%s"]);
        let mut subjects = subjects.lines();
        let newest = subjects.next().expect("a newest commit").to_string();
        let oldest = subjects.next_back().expect("an oldest commit").to_string();

        assert!(
            !oldest.is_empty() && oldest != newest,
            "this fixture needs at least two distinctly-named commits for the \
             assertion below to be able to fail: {subjects:?}"
        );
        assert!(
            auto.ends_with(&oldest),
            "the automatic entry should repeat the OLDEST commit's subject \
             ({oldest:?}), so `stash@{{1}}` hangs off the seed commit: {auto}"
        );
    }

    /// The merge-preview fixture must be genuinely divergent, genuinely
    /// clean, and genuinely **un**merged.
    ///
    /// All three matter and each fails differently. Not divergent and the
    /// merge is a fast-forward, so the preview has no merge commit to draw.
    /// Not clean and the preview answers `Conflict` — a correct answer to a
    /// different question. Already merged and the spec cannot tell a working
    /// preview from a picture of what already happened.
    ///
    /// MUTATION 1: drop the `main:` commits, leaving `main` an ancestor of
    ///   `feature` — RED on the merge-base assertion, because the base is then
    ///   `main`'s own tip.
    /// MUTATION 2: have both branches write the same path — RED on the
    ///   clean-merge assertion, and green on every other one, which is what
    ///   tells the two apart.
    #[test]
    fn the_merge_preview_fixture_is_divergent_clean_and_not_yet_merged() {
        let (_d, root) = build("merge-preview");

        assert_eq!(
            git::out_as(BROWSER, &root, &["rev-list", "--count", MERGE_PREVIEW_INTO]),
            (MERGE_PREVIEW_DEPTH + 1).to_string(),
            "main must be the base plus its own commits"
        );
        assert_eq!(
            git::out_as(
                BROWSER,
                &root,
                &["rev-list", "--count", MERGE_PREVIEW_BRANCH]
            ),
            (MERGE_PREVIEW_DEPTH + 1).to_string(),
            "feature must be the base plus its own commits, so the graph has width"
        );

        // Neither branch is an ancestor of the other: the shared base is
        // strictly behind both tips.
        let base = git::out_as(
            BROWSER,
            &root,
            &["merge-base", MERGE_PREVIEW_INTO, MERGE_PREVIEW_BRANCH],
        );
        for branch in [MERGE_PREVIEW_INTO, MERGE_PREVIEW_BRANCH] {
            assert_ne!(
                base,
                git::out_as(BROWSER, &root, &["rev-parse", branch]),
                "{branch} must not be the merge base, or this is a fast-forward"
            );
        }

        // Merging is clean — proven on a THROWAWAY CLONE, never here: a
        // fixture that verified itself by merging would hand the suite an
        // already-merged repository, which is the one state this shape must
        // not be in.
        let clone_dir = tempfile::tempdir().unwrap();
        let clone = clone_dir.path().join("clone");
        git::run_as(
            BROWSER,
            std::path::Path::new("."),
            &[
                "clone",
                "-q",
                root.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git::run_as(BROWSER, &clone, &["checkout", "-q", MERGE_PREVIEW_INTO]);
        git::run_as(
            BROWSER,
            &clone,
            &[
                "merge",
                "-q",
                "--no-edit",
                &format!("origin/{MERGE_PREVIEW_BRANCH}"),
            ],
        );
        assert!(
            !clone.join(".git/MERGE_HEAD").exists(),
            "the two branches touch disjoint files and must merge without a conflict"
        );

        assert!(
            !root.join(".git/MERGE_HEAD").exists(),
            "the fixture handed to the suite must stay PRE-merge"
        );
        let tip_parents = git::out_as(BROWSER, &root, &["cat-file", "-p", MERGE_PREVIEW_INTO])
            .lines()
            .filter(|line| line.starts_with("parent "))
            .count();
        assert_eq!(
            tip_parents, 1,
            "main's tip must be an ordinary commit — a merge already performed \
             would let the spec pass against a picture of the past"
        );
    }
}

#[cfg(test)]
mod deletion_guard_tests {
    use super::{clear_or_refuse, mark};

    /// The case that cost this guard its issue: a real directory holding real
    /// work, named on the command line by mistake.
    #[test]
    fn a_non_empty_unmarked_directory_is_refused() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let victim = tmp.path().join("someones-checkout");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("important.txt"), "work").unwrap();

        let refused = std::panic::catch_unwind(|| clear_or_refuse(&victim, "fixture root"));

        assert!(
            refused.is_err(),
            "a non-empty unmarked directory must be refused"
        );
        assert!(
            victim.join("important.txt").exists(),
            "the refusal must leave the contents alone — a guard that panics AFTER \
             deleting is not a guard"
        );
    }

    /// The permission is the marker, and nothing else.
    #[test]
    fn a_marked_directory_is_cleared() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let ours = tmp.path().join("prior-fixture");
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("leftover.txt"), "from the last run").unwrap();
        mark(&ours);

        clear_or_refuse(&ours, "fixture root");

        assert!(!ours.exists(), "a marked directory is ours to rebuild");
    }

    /// Handing over a directory you created yourself is ordinary, and deleting
    /// nothing destroys nothing.
    #[test]
    fn an_empty_directory_needs_no_marker() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let empty = tmp.path().join("staging");
        std::fs::create_dir_all(&empty).unwrap();

        clear_or_refuse(&empty, "fixture root");

        assert!(!empty.exists());
    }

    /// A path that does not exist is not an error — most runs take this arm.
    #[test]
    fn a_missing_path_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("temp dir");
        clear_or_refuse(&tmp.path().join("never-existed"), "fixture root");
    }

    /// The marker must SAY what it is. Someone will find one of these in a
    /// stray directory months from now with no idea why it is there.
    #[test]
    fn the_marker_explains_itself_and_says_deleting_it_does_not_protect_you() {
        let tmp = tempfile::tempdir().expect("temp dir");
        mark(tmp.path());
        let body = std::fs::read_to_string(super::marker_path(tmp.path())).unwrap();
        assert!(
            body.contains("gv-fixture"),
            "the marker must name the tool that wrote it"
        );
        assert!(
            body.contains("disposable"),
            "it must say the contents are disposable, which is the whole claim"
        );
        assert!(
            body.contains("refuse to touch it at all"),
            "and it must correct the natural wrong guess — that deleting the marker \
             protects the directory, when it does the opposite"
        );
    }
}
