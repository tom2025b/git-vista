//! Repositories stopped mid-merge, with a real unresolved conflict on disk.
//!
//! # What a conflict actually is
//!
//! Almost everyone learns conflicts as "those `<<<<<<<` markers in my file".
//! That is the *symptom*, and it is the least important part. The real state
//! lives in the index.
//!
//! Normally git's index holds one entry per path, at **stage 0** — "this is the
//! agreed content". When a merge cannot decide, git does not write stage 0.
//! Instead it writes up to three entries for that one path:
//!
//! | stage | name   | what it holds                                  |
//! |-------|--------|------------------------------------------------|
//! | 1     | base   | the common ancestor's version                  |
//! | 2     | ours   | the version on the branch you were standing on |
//! | 3     | theirs | the version on the branch you were merging in  |
//!
//! `git ls-files -u` prints exactly these. **Which stages are present is the
//! conflict's shape**, and different shapes need genuinely different handling —
//! which is why this module has four of them rather than one. A tool that
//! assumes all three stages always exist will crash on the two shapes below
//! where they do not.
//!
//! The markers in the working-tree file are just git's rendering of stages 2
//! and 3 for a human to edit. For a binary file git does not even write them,
//! because there is no such thing as a line-wise merge of a PNG.
//!
//! # Why every builder ignores the merge's exit status
//!
//! `git merge` on a conflicted merge exits non-zero. That is not a failure of
//! the fixture — it *is* the fixture. Each builder uses [`git::try_run`] and
//! discards the result, then asserts the conflict really landed, so a merge
//! that unexpectedly *succeeded* is still caught.

use crate::git;
use crate::seeded::{empty, Fixture};

/// Stand up `main` with one commit holding `files`, the ancestor every shape
/// below diverges from.
///
/// Each shape seeds only the path it is about: a conflict fixture carrying
/// unrelated extra files is a fixture whose status output nobody predicted.
pub(crate) fn base_commit(repo: &std::path::Path, files: &[(&str, &[u8])]) {
    for (name, content) in files {
        git::write(repo, name, content);
    }
    git::run(repo, &["add", "-A"]);
    git::run(repo, &["commit", "-q", "-m", "base"]);
}

/// Assert `path` is genuinely conflicted, and return its stage numbers, sorted.
///
/// Reads `git ls-files -u`, which is git's own view — the fixture never asserts
/// its shape by calling the code that built it.
pub(crate) fn stages_of(repo: &std::path::Path, path: &str) -> Vec<u8> {
    let listing = git::out(repo, &["ls-files", "-u", "--", path]);
    let mut stages: Vec<u8> = listing
        .lines()
        .filter_map(|line| {
            // `<mode> <oid> <stage>\t<path>`
            let meta = line.split('\t').next()?;
            meta.split_whitespace().nth(2)?.parse().ok()
        })
        .collect();
    stages.sort_unstable();
    stages
}

/// A modify/modify conflict on `a.txt` — the ordinary case, all three stages.
///
/// ## What is wrong
///
/// Both branches edited the same lines of the same file, starting from the same
/// ancestor. Git can see all three versions and has no rule for choosing, so it
/// stops and asks.
///
/// ## What git put on disk
///
/// `a.txt` has index entries at **stages 1, 2 and 3** — ancestor `base\n`,
/// ours `ours\n`, theirs `theirs\n` — and the working-tree file holds conflict
/// markers. `.git/MERGE_HEAD` names the branch being merged, which is what
/// makes `git status` say "You have unmerged paths" and what a `git merge
/// --abort` reads to undo it.
///
/// ## Why it matters
///
/// This is the shape every tool gets right, so it is the baseline the others
/// are measured against: it is the only one of the four where "show me the
/// base, ours and theirs panes" can be answered literally, because all three
/// exist. Two suites had independently hand-built exactly this shape before
/// #448.
pub fn conflict_modify_modify() -> Fixture {
    let (dir, repo) = empty();
    base_commit(&repo, &[("a.txt", b"base\n")]);

    git::run(&repo, &["checkout", "-q", "-b", "theirs"]);
    git::write(&repo, "a.txt", b"theirs\n");
    git::run(&repo, &["commit", "-q", "-am", "theirs"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "a.txt", b"ours\n");
    git::run(&repo, &["commit", "-q", "-am", "ours"]);

    let _ = git::try_run(&repo, &["merge", "theirs"]);

    assert_eq!(
        stages_of(&repo, "a.txt"),
        vec![1, 2, 3],
        "modify/modify must leave all three stages"
    );
    (dir, repo)
}

/// An add/add conflict on `c.txt` — **no stage 1, because there is no ancestor**.
///
/// ## What is wrong
///
/// Two branches each independently created a file at the same path. Neither
/// created it "from" anything: before the branch point, that path did not
/// exist.
///
/// ## What git put on disk
///
/// `c.txt` has index entries at **stages 2 and 3 only**. There is no stage 1,
/// and asking for one is not "the base is empty" — it is "the base is absent".
///
/// ## Why it matters
///
/// This is the shape that breaks naive three-pane conflict UIs. Code written
/// against `conflict_modify_modify` reaches for the ancestor version, finds
/// nothing, and either panics or silently displays an empty pane labelled
/// "original" — telling the user the file used to be empty, which is a lie: it
/// did not exist. Git's own merge tooling reports this as `both added`, a
/// distinct status from `both modified`, precisely because the difference
/// matters.
pub fn conflict_add_add() -> Fixture {
    let (dir, repo) = empty();
    // `c.txt` must be absent from the ancestor — that absence IS the shape —
    // so the base commit carries an unrelated file to have something to hold.
    base_commit(&repo, &[("base.txt", b"base\n")]);

    git::run(&repo, &["checkout", "-q", "-b", "theirs"]);
    git::write(&repo, "c.txt", b"theirs made this\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "theirs adds c.txt"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "c.txt", b"ours made this\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "ours adds c.txt"]);

    let _ = git::try_run(&repo, &["merge", "theirs"]);

    assert_eq!(
        stages_of(&repo, "c.txt"),
        vec![2, 3],
        "add/add has no common ancestor, so there must be no stage 1"
    );
    (dir, repo)
}

/// A delete/modify conflict on `d.txt` — **one side has no content at all**.
///
/// ## What is wrong
///
/// One branch deleted the file; the other edited it. There is no content-level
/// merge to attempt, because the disagreement is not about the contents — it is
/// about whether the file should exist. Git cannot answer that; only a person
/// who knows why it was deleted can.
///
/// ## What git put on disk
///
/// `d.txt` has index entries at **stages 1 and 2** (ancestor and ours) — the
/// deleting side contributes no blob, so its stage is simply absent. The
/// working-tree file contains *no conflict markers*: git leaves the surviving
/// version there verbatim, because there is nothing to interleave it with.
///
/// ## Why it matters
///
/// This is the conflict users most often resolve wrongly, and the reason is a
/// UI failure rather than a git failure. A file with no markers *looks*
/// resolved. `git status` says `deleted by them`, but the file on disk reads
/// perfectly normally — so people `git add` it without thinking and silently
/// revert someone's deliberate deletion. Any tool that decides "is this
/// resolved?" by scanning for `<<<<<<<` gets this shape wrong every time.
pub fn conflict_delete_modify() -> Fixture {
    let (dir, repo) = empty();
    base_commit(&repo, &[("d.txt", b"line one\nline two\n")]);

    git::run(&repo, &["checkout", "-q", "-b", "theirs"]);
    git::run(&repo, &["rm", "-q", "d.txt"]);
    git::run(&repo, &["commit", "-q", "-m", "theirs deletes d.txt"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "d.txt", b"line one\nline two edited\n");
    git::run(&repo, &["commit", "-q", "-am", "ours edits d.txt"]);

    let _ = git::try_run(&repo, &["merge", "theirs"]);

    assert_eq!(
        stages_of(&repo, "d.txt"),
        vec![1, 2],
        "the deleting side contributes no blob, so stage 3 must be absent"
    );
    (dir, repo)
}

/// A binary conflict on `b.bin` — **no line merge exists, and none is offered**.
///
/// ## What is wrong
///
/// Both sides changed a file containing NUL bytes. Git detects it is not text
/// and refuses to attempt a line-wise merge, because interleaving two versions
/// of a binary format produces a file that is neither version and opens in
/// nothing.
///
/// ## What git put on disk
///
/// All three stages are present, exactly as in modify/modify — but the
/// **working-tree file is left as ours, byte for byte, with no markers
/// inserted**. Git reports `warning: Cannot merge binary files`. So the index
/// says "conflicted" while the file on disk is a perfectly valid, complete
/// version of the binary.
///
/// ## Why it matters
///
/// The only resolution available is *choose one whole side* — `--ours` or
/// `--theirs`. A conflict editor that offers line-by-line hunk picking here is
/// offering an operation that cannot produce a valid file, and one that renders
/// the "conflicted" file as text will spray NUL bytes and mojibake at the user.
/// The browser harness needed a third, separately-written conflict fixture
/// (#432) purely because this shape did not exist in the shared catalogue.
pub fn conflict_binary() -> Fixture {
    let (dir, repo) = empty();
    // NUL in the first bytes is what makes git call it binary — the check is a
    // scan of the leading block, not a file extension.
    base_commit(&repo, &[("b.bin", b"\x00\x01base payload\x00\xff")]);

    git::run(&repo, &["checkout", "-q", "-b", "theirs"]);
    git::write(&repo, "b.bin", b"\x00\x01theirs payload\x00\xfe");
    git::run(&repo, &["commit", "-q", "-am", "theirs edits b.bin"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "b.bin", b"\x00\x01ours payload\x00\xfd");
    git::run(&repo, &["commit", "-q", "-am", "ours edits b.bin"]);

    let _ = git::try_run(&repo, &["merge", "theirs"]);

    assert_eq!(
        stages_of(&repo, "b.bin"),
        vec![1, 2, 3],
        "a binary modify/modify still records all three stages"
    );
    (dir, repo)
}

/// A repository stopped part-way through a `git revert`, with the revert
/// conflicted.
///
/// ## What is wrong
///
/// Reverting a commit is applying its diff backwards. If later commits touched
/// the same lines, that backwards patch does not apply, and git stops in the
/// middle of a *sequencer operation* rather than a merge.
///
/// ## What git put on disk
///
/// A conflicted index, as with any merge — **plus `.git/REVERT_HEAD`**, naming
/// the commit being reverted. This is the part that catches tools out: the
/// escape hatch is `git revert --abort`, not `git merge --abort`, and
/// `.git/MERGE_HEAD` does *not* exist. Cherry-pick and rebase have their own
/// equivalents (`CHERRY_PICK_HEAD`, `.git/rebase-merge/`), so "am I mid-merge?"
/// is really four separate questions.
///
/// ## Why it matters
///
/// A tool that decides "is an operation in progress?" by checking for
/// `MERGE_HEAD` alone reports this repository as idle — while git will refuse
/// every command with "you are in the middle of a revert". The user is then
/// stuck in a state their tool insists they are not in.
pub fn sequence_mid_revert() -> Fixture {
    let (dir, repo) = empty();

    git::write(&repo, "f.txt", b"line1\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "line1"]);

    git::write(&repo, "f.txt", b"line1\nline2\n");
    git::run(&repo, &["commit", "-q", "-am", "line2"]);
    let target = git::out(&repo, &["rev-parse", "HEAD"]);

    // Later work rewrites the very line the revert would remove, so the
    // backwards patch cannot apply cleanly.
    git::write(&repo, "f.txt", b"line1\nline2 rewritten\n");
    git::run(&repo, &["commit", "-q", "-am", "rewrite line2"]);

    let _ = git::try_run(&repo, &["revert", "--no-edit", &target]);

    assert!(
        repo.join(".git/REVERT_HEAD").exists(),
        "the sequencer state must be on disk, or this shape proves nothing"
    );
    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "a revert is not a merge — MERGE_HEAD must be absent"
    );
    (dir, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four shapes must differ in the way the docs claim they differ. If
    /// this ever goes red, the teaching text above has stopped being true.
    #[test]
    fn each_shape_records_the_stages_its_documentation_claims() {
        let (_d, r) = conflict_modify_modify();
        assert_eq!(stages_of(&r, "a.txt"), vec![1, 2, 3]);

        let (_d, r) = conflict_add_add();
        assert_eq!(stages_of(&r, "c.txt"), vec![2, 3]);

        let (_d, r) = conflict_delete_modify();
        assert_eq!(stages_of(&r, "d.txt"), vec![1, 2]);

        let (_d, r) = conflict_binary();
        assert_eq!(stages_of(&r, "b.bin"), vec![1, 2, 3]);
    }

    /// The claim that makes `conflict_delete_modify` worth having: the working
    /// tree carries no markers, so "does it contain `<<<<<<<`?" reports this
    /// conflicted file as clean.
    #[test]
    fn a_delete_modify_leaves_no_conflict_markers_in_the_worktree() {
        let (_d, repo) = conflict_delete_modify();
        let text = std::fs::read_to_string(repo.join("d.txt")).unwrap();
        assert!(
            !text.contains("<<<<<<<"),
            "delete/modify must not carry markers: {text:?}"
        );
        assert!(!git::out(&repo, &["status", "--porcelain"]).is_empty());
    }

    /// ...whereas the ordinary text conflict does carry them, which is what
    /// makes the contrast above a real distinction rather than a quirk of how
    /// this fixture happens to be built.
    #[test]
    fn a_modify_modify_does_leave_conflict_markers() {
        let (_d, repo) = conflict_modify_modify();
        let text = std::fs::read_to_string(repo.join("a.txt")).unwrap();
        assert!(text.contains("<<<<<<<"), "expected markers, got {text:?}");
    }

    /// The binary claim: all three stages, yet git inserted nothing — the file
    /// on disk is still exactly our side's bytes.
    #[test]
    fn a_binary_conflict_leaves_our_bytes_untouched_and_unmarked() {
        let (_d, repo) = conflict_binary();
        let bytes = std::fs::read(repo.join("b.bin")).unwrap();
        assert_eq!(bytes, b"\x00\x01ours payload\x00\xfd");
        assert!(!bytes.windows(7).any(|w| w == b"<<<<<<<"));
    }

    /// git must agree the file is binary, or the shape is just a text conflict
    /// with awkward bytes and the whole lesson is wrong. Asked across the two
    /// branches — rather than of the conflicted index, which answers only
    /// "Unmerged path" — git says it will not diff these as text.
    #[test]
    fn git_itself_classifies_the_binary_fixture_as_binary() {
        let (_d, repo) = conflict_binary();
        let patch = git::out(&repo, &["diff", "main", "theirs", "--", "b.bin"]);
        assert!(
            patch.contains("Binary files"),
            "git should refuse a textual diff of b.bin, got {patch:?}"
        );
    }

    /// A merge conflict writes MERGE_HEAD; the revert shape must not, or the
    /// distinction `sequence_mid_revert` exists to teach is not real.
    #[test]
    fn a_merge_conflict_writes_merge_head_and_a_revert_writes_revert_head() {
        let (_d, merged) = conflict_modify_modify();
        assert!(merged.join(".git/MERGE_HEAD").exists());
        assert!(!merged.join(".git/REVERT_HEAD").exists());

        let (_d, reverted) = sequence_mid_revert();
        assert!(reverted.join(".git/REVERT_HEAD").exists());
        assert!(!reverted.join(".git/MERGE_HEAD").exists());
    }
}
