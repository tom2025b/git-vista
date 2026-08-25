//! Real repositories for the working-tree **status** vocabulary (#68, #365).
//!
//! ## What is wrong with them
//!
//! Nothing is corrupt. What is wrong is what these shapes are *for*: until
//! #365, every test of `git status --porcelain=v2 --branch -z` parsing in this
//! workspace fed the parser a **hand-written byte string**. Those tests are
//! good tests of the parser and they are not evidence about `git` at all,
//! because no git ran — the oids in them are literals like
//! `6666666666666666666666666666666666666666`.
//!
//! So `crates/git-vista-protocol/src/status.rs` could only say its record
//! shapes were *captured* from a real 2.43.0 once, by hand, and encoded. These
//! builders are the missing half: repositories whose status output is produced
//! by whichever `git` binary is pointed at them, so the same shapes can be
//! read by two versions and compared.
//!
//! ## Why a battery rather than one repository per record
//!
//! The vocabulary interacts. A conflicted index puts a repository into MERGING
//! state, which is why the conflict shapes below are separate; but *within*
//! the unconflicted set, a staged change and an unstaged change and a rename
//! must be able to coexist, because that is the state a real working tree is
//! in when a user looks at it, and a parser bug that only appears when two
//! record kinds are adjacent in one `-z` stream would survive a suite of
//! single-record repositories.
//!
//! ## Two records production never asks for
//!
//! Named here rather than left for a reader to discover:
//!
//! * **`!` (ignored)** requires `--ignored`. The `/api/status/v2` handler runs
//!   `status --porcelain=v2 --branch -z` and nothing in the workspace passes
//!   `--ignored`, so `StatusEntry::Ignored` is a variant the parser can produce
//!   and the product never sees.
//! * **`C` (copy)** requires `status.renames=copies` *and* a source that is
//!   itself part of the change set — git's copy detection will not consider an
//!   untouched file as a source. Under the production argv a copy is reported
//!   as an ordinary add.
//!
//! [`status_battery`] therefore produces both, and the floor test reads it
//! three ways — the production argv, plus `--ignored`, plus copy detection —
//! so the record shapes are exercised against every supported git even though
//! two of them cannot reach the product today.

use crate::git;
use crate::seeded::Fixture;
use std::path::Path;

/// The ordinary status vocabulary, in one repository.
///
/// ## What git put on disk
///
/// One seed commit, two submodules, then a working tree holding one of each
/// unconflicted record shape at once:
///
/// ```text
///   1 MM  both.txt          staged AND unstaged edits to one path
///   1 M.  staged.txt        staged only
///   1 .M  unstaged.txt      worktree only
///   2 R.  rename-dst.txt    a staged rename, two -z tokens
///   2 C.  copy-dst.txt      a staged copy   (only with status.renames=copies)
///   1 M.  copy-src.txt      the copy's source, edited — see below
///   ?     untracked.txt     never added
///   !     build.log         ignored by .gitignore  (only with --ignored)
///   1 .M SCMU vendor/moved  submodule: pointer moved, and dirty both ways
///   1 .M S.MU vendor/dirty  submodule: dirty both ways, pointer unmoved
/// ```
///
/// The copy's source is edited on purpose. Git's copy detection only considers
/// files that are themselves part of the change set as copy sources, so a
/// pristine `copy-src.txt` yields an ordinary `1 A.` add for the destination
/// and no `2 C.` record exists to parse. That is the whole reason `C` is hard
/// to produce and the reason it is worth pinning here.
///
/// The two submodules differ in exactly one axis — whether the recorded commit
/// moved — because the `<sub>` field encodes that separately from dirt
/// (`S<c><m><u>`), and a fixture with only one of them could not tell a parser
/// that read the wrong column from one that read the right one.
///
/// ## Why it matters
///
/// This is the repository the #365 floor comparison reads with two git
/// binaries. Every record kind the parser knows how to build is present, so
/// "2.32 and the current git agree" is a claim about the whole vocabulary
/// rather than about whichever record happened to be in a scratch repo.
pub fn status_battery() -> Fixture {
    let dir = tempfile::tempdir().expect("create fixture tempdir");
    let repo = dir.path().join("repo");
    let sub = dir.path().join("sub");

    // The repository both submodules are added from. Two commits, so one of
    // them can be moved forward without fetching anything.
    git::init(&sub);
    git::write(&sub, "a.txt", b"a\n");
    git::run(&sub, &["add", "-A"]);
    git::run(&sub, &["commit", "-q", "-m", "sub: first"]);

    git::init(&repo);
    git::write(&repo, "staged.txt", b"staged only\n");
    git::write(&repo, "unstaged.txt", b"unstaged only\n");
    git::write(&repo, "both.txt", b"both sides\n");
    git::write(&repo, "rename-src.txt", &body("rename"));
    git::write(&repo, "copy-src.txt", &body("copy"));
    git::write(&repo, ".gitignore", b"build.log\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "seed: the status battery"]);

    // `protocol.file.allow` must arrive as `-c`; see `git::run_configured`.
    let sub_url = sub.to_str().expect("fixture paths are utf-8");
    for path in ["vendor/dirty", "vendor/moved"] {
        git::run_configured(
            &repo,
            &["protocol.file.allow=always"],
            &["submodule", "add", "-q", sub_url, path],
        );
    }
    git::run(&repo, &["commit", "-q", "-m", "add two submodules"]);

    // --- the working tree the battery is actually about --------------------
    git::write(&repo, "staged.txt", b"staged only, changed\n");
    git::run(&repo, &["add", "staged.txt"]);

    git::write(&repo, "unstaged.txt", b"unstaged only, changed\n");

    git::write(&repo, "both.txt", b"both sides, staged\n");
    git::run(&repo, &["add", "both.txt"]);
    git::write(&repo, "both.txt", b"both sides, in the worktree\n");

    git::run(&repo, &["mv", "rename-src.txt", "rename-dst.txt"]);

    // The copy, and the edit to its source that makes git willing to call it
    // one. Order matters: the destination must hold the source's ORIGINAL
    // bytes, so the copy scores 100 against the committed blob.
    let original = std::fs::read(repo.join("copy-src.txt")).expect("read the copy source");
    git::write(&repo, "copy-dst.txt", &original);
    let mut edited = original.clone();
    edited.extend_from_slice(b"a line only the source has\n");
    git::write(&repo, "copy-src.txt", &edited);
    git::run(&repo, &["add", "copy-src.txt", "copy-dst.txt"]);

    git::write(&repo, "untracked.txt", b"not in the index\n");
    git::write(&repo, "build.log", b"ignored by .gitignore\n");

    // Submodule dirt: a tracked edit AND an untracked file, so both of the
    // `<sub>` field's dirt columns are exercised rather than one.
    for path in ["vendor/dirty", "vendor/moved"] {
        let inner = repo.join(path);
        git::write(&inner, "a.txt", b"a, edited inside the submodule\n");
        git::write(
            &inner,
            "untracked-inside.txt",
            b"not in the submodule index\n",
        );
    }
    // ...and one of them also moves its recorded commit. A commit made inside
    // the submodule moves its HEAD, which is what the parent records as a
    // pointer change — no fetch and no second remote involved.
    git::run(
        &repo.join("vendor/moved"),
        &["commit", "-q", "--allow-empty", "-m", "sub: second"],
    );

    (dir, repo)
}

/// The three conflict kinds a **rename/rename** merge produces at once (#68).
///
/// ## What git put on disk
///
/// `f.txt` is renamed to a different name on each side, then merged:
///
/// ```text
///   u DD  f.txt            both deleted
///   u AU  main-name.txt    added by us
///   u UA  side-name.txt    added by them
/// ```
///
/// ## Why it matters
///
/// `DD` is the combination that looks impossible to build and is the one most
/// likely to be quietly dropped from a battery. It does **not** require both
/// sides to plainly delete the path — a plain delete on both sides is not a
/// conflict at all, and git resolves it silently. It falls out of a
/// rename/rename(1to2): each side moved the file away, so the original path is
/// deleted on both sides *and* the merge is unresolved, which is exactly what
/// `DD` means.
pub fn status_conflicts_rename() -> Fixture {
    let (dir, repo) = crate::seeded::seeded_files(&[("f.txt", "base\n")], "seed");

    git::run(&repo, &["checkout", "-q", "-b", "side"]);
    git::run(&repo, &["mv", "f.txt", "side-name.txt"]);
    git::run(&repo, &["commit", "-q", "-m", "side renames it"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::run(&repo, &["mv", "f.txt", "main-name.txt"]);
    git::run(&repo, &["commit", "-q", "-m", "main renames it"]);

    // Expected to fail: that is the shape.
    let merged = git::try_run(&repo, &["merge", "side", "-m", "the rename/rename merge"]);
    assert!(!merged, "the rename/rename merge was supposed to conflict");

    (dir, repo)
}

/// The other four conflict kinds, from one ordinary merge (#68).
///
/// ## What git put on disk
///
/// ```text
///   u AA  both-add.txt     both added, no common ancestor for the path
///   u UU  both-mod.txt     both modified
///   u UD  they-del.txt     we modified, they deleted
///   u DU  we-del.txt       we deleted, they modified
/// ```
///
/// ## Why it matters
///
/// Together with [`status_conflicts_rename`] this covers all seven `u`-record
/// `XY` combinations git can emit, which is the set
/// `crates/git-vista-protocol`'s `every_conflict_xy_combination` enumerates
/// from `git-status(1)`'s own table. Two merges, seven kinds, nothing invented.
pub fn status_conflicts_merge() -> Fixture {
    let (dir, repo) = crate::seeded::seeded_files(
        &[
            ("both-mod.txt", "base\n"),
            ("we-del.txt", "base\n"),
            ("they-del.txt", "base\n"),
        ],
        "seed",
    );

    git::run(&repo, &["checkout", "-q", "-b", "side"]);
    git::write(&repo, "both-mod.txt", b"side\n");
    git::write(&repo, "we-del.txt", b"side\n");
    git::write(&repo, "both-add.txt", b"side's version\n");
    git::run(&repo, &["rm", "-q", "they-del.txt"]);
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "side"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "both-mod.txt", b"main\n");
    git::write(&repo, "they-del.txt", b"main\n");
    git::write(&repo, "both-add.txt", b"main's version\n");
    git::run(&repo, &["rm", "-q", "we-del.txt"]);
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main"]);

    let merged = git::try_run(&repo, &["merge", "side", "-m", "the conflicting merge"]);
    assert!(!merged, "the merge was supposed to conflict");

    (dir, repo)
}

/// Twelve numbered lines under `tag`.
///
/// Long enough that git's rename and copy detection scores it at 100% against
/// an identical blob rather than declining to pair two short files.
fn body(tag: &str) -> Vec<u8> {
    (1..=12)
        .map(|i| format!("{tag} body line {i}\n"))
        .collect::<String>()
        .into_bytes()
}

/// A battery builder: takes nothing, returns a repository and its tempdir.
///
/// Named rather than written inline in [`BATTERIES`] because
/// `&[(&str, fn() -> (TempDir, PathBuf))]` is over clippy's complexity
/// threshold, and the alias reads better anyway. Mirrors
/// [`crate::browser::Builder`], which exists for the same reason.
pub type BatteryBuilder = fn() -> Fixture;

/// Every battery shape, by a stable name.
///
/// A table rather than three call sites so the floor test cannot silently read
/// fewer repositories than exist: adding a shape here adds it to the run, and
/// the test asserts the table is non-empty.
pub const BATTERIES: &[(&str, BatteryBuilder)] = &[
    ("battery", status_battery as BatteryBuilder),
    ("conflicts-rename", status_conflicts_rename),
    ("conflicts-merge", status_conflicts_merge),
];

/// The path a battery repository lives at, for callers that only have the
/// fixture.
pub fn repo_of(f: &Fixture) -> &Path {
    &f.1
}
