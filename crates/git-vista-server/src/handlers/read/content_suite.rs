//! Diff and file content reads: the bounded diff/file caps (M1.10, #63),
//! `truncate_at_line`'s multi-byte safety (#69, M2.16), the four explicit
//! `POST /api/diff/spec` modes (M2.16, #69), and the malicious-path battery
//! against `GET /api/file/{id}/{*path}` (#67) — traversal, percent-encoding,
//! symlinks, tree/blob confusion, and the parent-fallback's identical
//! behaviour under all of it.

use super::*;
use axum::routing::get;
use axum::Router;
use git_vista_protocol::diff::DiffSpec;
use git_vista_protocol::RepositoryDescriptor;
use tower::ServiceExt;

// ---- bounded diff/file reads (M1.10, #63) --------------------------------
//
// These drive the `*_for_repo` seams directly. They cannot go through the
// axum handlers: those resolve the repository from the process-wide
// `CURRENT` selection, which panics when unset and has no test-time setter.

/// `git <args…>` in `repo`; asserts success. Same shape as the planner
/// suites' fixtures, duplicated because those helpers are private to their
/// own modules and unreachable from here.
fn run(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// `git <args…>` in `repo`, returning trimmed stdout; asserts success.
fn out(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {repo:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The exact byte length of `git -C <repo> <args…>`'s stdout. The metadata
/// tests size their injected cap off this, so the fixture never has to grow
/// to the real 8 MiB ceiling to exercise both cap branches.
fn stdout_len(repo: &Path, args: &[String]) -> usize {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {repo:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout.len()
}

/// A fresh repository on branch `main` with one committed file.
fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);
    (dir, repo)
}

/// A repository whose HEAD commit modifies several files — enough `-z`
/// metadata to cross a test-sized cap, nowhere near enough to need a real
/// 8 MiB fixture.
fn repo_with_multi_file_commit() -> (tempfile::TempDir, PathBuf, String) {
    let (dir, repo) = seeded_repo();
    for i in 0..4 {
        std::fs::write(repo.join(format!("file-{i}.txt")), "one\n").unwrap();
    }
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "add files"]);
    for i in 0..4 {
        std::fs::write(repo.join(format!("file-{i}.txt")), "two\n").unwrap();
    }
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "modify files"]);
    let id = out(&repo, &["rev-parse", "HEAD"]);
    (dir, repo, id)
}

/// Every diff read disables textconv (a configured textconv filter could
/// otherwise dump a binary blob into the patch), keeps options ahead of the
/// revisions, and the caps are the explicit, named ones — not whatever the
/// fail-safe wrapper happens to use.
#[test]
fn bounded_diff_argv_uses_explicit_caps_and_no_textconv() {
    let id = "a".repeat(40);
    let ordinary = diff_argv(&id, false);
    let merge = diff_argv(&id, true);

    for argv in ordinary.iter().chain(merge.iter()) {
        assert!(
            argv.contains(&"--no-textconv".to_string()),
            "every diff read must disable textconv: {argv:?}"
        );
        assert!(
            !argv.contains(&"--binary".to_string()),
            "binary content must never be inlined: {argv:?}"
        );
    }

    // Read order is [name-status, numstat, patch], each with its own shape.
    assert!(ordinary[0].contains(&"--name-status".to_string()));
    assert!(ordinary[0].contains(&"-z".to_string()));
    assert!(ordinary[1].contains(&"--numstat".to_string()));
    assert!(ordinary[1].contains(&"-z".to_string()));
    assert!(ordinary[2].contains(&"--patch".to_string()));
    assert!(ordinary[2].contains(&"--no-color".to_string()));

    // Ordinary commit: `show … --format= <id>`; the revision is last, so no
    // option can ever swallow it.
    for argv in ordinary.iter() {
        assert_eq!(argv[0], "show");
        assert_eq!(argv.last().unwrap(), &id);
        assert_eq!(argv[argv.len() - 2], "--format=");
    }
    // Merge: `diff … <id>^1 <id>`, again with the revisions trailing.
    for argv in merge.iter() {
        assert_eq!(argv[0], "diff");
        assert_eq!(argv[argv.len() - 2], format!("{id}^1"));
        assert_eq!(argv.last().unwrap(), &id);
    }

    // The caps the reads are handed are explicit and named.
    assert_eq!(patch_cap(false), DIFF_PATCH_CAP);
    assert_eq!(patch_cap(true), DIFF_PATCH_CAP_FULL);
    assert_eq!(DIFF_PATCH_CAP, 200_000);
    assert_eq!(DIFF_PATCH_CAP_FULL, 5_000_000);
    assert_eq!(DIFF_METADATA_CAP, 8 * 1024 * 1024);
    assert_eq!(FILE_CONTENT_CAP, 2_000_000);
}

// ---- truncate_at_line's multi-byte safety (#69, M2.16) --------------
//
// `truncate_at_line`'s own doc comment names the hazard: "The cap is first
// walked back to a char boundary so a multi-byte character straddling it
// can't panic the slice." That walk-back had no test until these — every
// truncation fixture in this file is pure ASCII, where the hazard cannot
// occur, so the guard was load-bearing and entirely unexercised. Its three
// call sites (`commit_diff_for_repo`, the file reader, `staging_diff_for_repo`)
// all feed it text decoded by `from_utf8_lossy`, which emits multi-byte
// U+FFFD for every invalid input byte — so non-ASCII at the cap boundary is
// not an exotic case, it is what malformed input decodes *to*.

/// The specific panic the walk-back exists to prevent: a cap landing
/// **inside** a multi-byte character. Without the boundary walk, `text[..end]`
/// slices mid-character and panics.
///
/// Byte layout of the fixture, counted by hand rather than derived:
/// `o`=0, `k`=1, `\n`=2, then `日`=3..6, `日`=6..9, `\n`=9. A cap of 5 lands
/// on the *third byte* of the first `日` — not a boundary.
#[test]
fn truncate_at_line_walks_back_off_a_multibyte_char_instead_of_panicking() {
    let mut text = String::from("ok\n日日\n");
    assert_eq!(
        text.len(),
        10,
        "fixture byte length changed; recount the cap"
    );
    assert!(
        !text.is_char_boundary(5),
        "cap 5 must land mid-character or this test proves nothing"
    );

    truncate_at_line(&mut text, 5);

    // Walk 5 → 4 → 3 (the start of the first `日`), then cut at the last
    // newline before it.
    assert_eq!(text, "ok");
}

/// The control: a cap that already sits on a char boundary must behave
/// identically, so the test above is measuring the walk-back rather than
/// truncation in general.
#[test]
fn truncate_at_line_on_an_exact_char_boundary_needs_no_walk_back() {
    let mut text = String::from("ok\n日日\n");
    assert!(text.is_char_boundary(3), "3 is the start of the first 日");

    truncate_at_line(&mut text, 3);

    assert_eq!(text, "ok");
}

/// With no newline before the cap, the function falls back to the
/// walked-back byte position — which must still be a char boundary, or the
/// `truncate` call panics. Keeps one whole character rather than a partial.
#[test]
fn truncate_at_line_with_no_newline_keeps_whole_characters() {
    let mut text = String::from("日日日");
    assert_eq!(text.len(), 9);
    assert!(!text.is_char_boundary(4));

    truncate_at_line(&mut text, 4);

    assert_eq!(text, "日", "cut mid-character instead of walking back");
}

/// The property the walk-back actually guarantees, stated directly: for
/// **every** cap position over multi-byte text, the call completes and
/// leaves valid UTF-8.
///
/// A cap is not a value this code chooses — it is `DIFF_PATCH_CAP` measured
/// against whatever bytes git emitted, so which byte it lands on is
/// effectively arbitrary. The three cases above pin specific known-bad
/// offsets; this one closes the gaps between them, and is what would catch a
/// future rewrite that handles some boundary cases but not all.
#[test]
fn truncate_at_line_never_panics_at_any_cap_over_multibyte_text() {
    // Deliberately mixed: ASCII, 2-byte (é), 3-byte (日), 4-byte (🦀), and
    // U+FFFD — the character `from_utf8_lossy` actually produces from
    // invalid input, which is how this text arises in production.
    let original = "a\né日\n🦀b\u{FFFD}\nzz";

    for cap in 0..=original.len() + 4 {
        let mut text = String::from(original);
        truncate_at_line(&mut text, cap);

        // `String` cannot hold invalid UTF-8, so surviving the call at all
        // is most of the proof; assert the result is a real prefix too, so
        // a "fix" that sanitised by rewriting bytes would not pass.
        assert!(
            original.starts_with(text.as_str()),
            "cap {cap} produced {text:?}, which is not a prefix of the input"
        );
    }
}

/// A `--name-status -z` read that hits the metadata cap is an explicit 413.
/// It must never come back as a *partial* file list: the `-z` parsers stop
/// cleanly on a short record, so a silently truncated read would render as a
/// plausible, wrong, shorter list of changed files.
#[tokio::test]
async fn bounded_diff_name_status_cap_returns_413() {
    let (_dir, repo, id) = repo_with_multi_file_commit();
    let [name_args, ..] = diff_argv(&id, false);
    let names_len = stdout_len(&repo, &name_args);
    assert!(names_len > 4, "fixture must exceed the injected cap");

    // Exactly what the guard exists to prevent: those same 4 bytes parse —
    // without complaint, because the `-z` parsers stop cleanly at a short
    // record — into a plausible file list that is simply wrong.
    let (partial, truncated) = git_stdout_capped(&repo, &name_args, "test", 4)
        .await
        .unwrap();
    assert!(truncated);
    let plausible = git_vista_core::diff::parse_name_status_z(&partial);
    assert!(
        plausible.len() < 4,
        "a short read parses to a shorter list: {plausible:?}"
    );

    let (status, msg) = commit_diff_for_repo(&repo, &id, false, 4)
        .await
        .expect_err("a truncated name-status read is an error, not a short list");

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(msg, "diff metadata exceeded 8 MiB");
}

/// The same for `--numstat -z`, reached with a cap sized to exactly the
/// name-status output: that read fills the cap without truncating (the
/// reader's probe byte tells "exactly cap" from "more"), so only the
/// strictly larger numstat read crosses it.
#[tokio::test]
async fn bounded_diff_numstat_cap_returns_413() {
    let (_dir, repo, id) = repo_with_multi_file_commit();
    let [name_args, numstat_args, _patch_args] = diff_argv(&id, false);
    let names_len = stdout_len(&repo, &name_args);
    let numstat_len = stdout_len(&repo, &numstat_args);
    assert!(
        numstat_len > names_len,
        "fixture invariant: numstat ({numstat_len}) must outgrow name-status ({names_len})"
    );

    let (status, msg) = commit_diff_for_repo(&repo, &id, false, names_len)
        .await
        .expect_err("a truncated numstat read is an error, not missing counts");
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(msg, "diff metadata exceeded 8 MiB");

    // Control: one byte of headroom past the larger read and the very same
    // commit succeeds — so the 413 above was the numstat cap, not a
    // name-status read that mis-reports an exactly-cap-sized output.
    let diff = commit_diff_for_repo(&repo, &id, false, numstat_len)
        .await
        .expect("both metadata reads fit at the larger cap");
    assert_eq!(diff.files.len(), 4);
    assert!(diff.files.iter().all(|f| f.additions == Some(1)));
}

/// Write a file of about `len` bytes: `header` first, then deterministic
/// fixed-size rows. Streamed through a `BufWriter` rather than built in
/// memory so a 50 MiB fixture costs the test almost nothing, and generated
/// from the running offset so a longer file is a byte-identical *prefix*
/// extension of a shorter one (which is what makes an "append" diff cheap
/// for git to compute). No shell helper is involved — `yes`/`dd`/`head` are
/// banned by the argv boundary, and every child these tests spawn is
/// literally `git`.
fn write_rows(path: &Path, header: &str, len: usize, tag: &str) {
    use std::io::Write;
    let mut w = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    w.write_all(header.as_bytes()).unwrap();
    let mut written = header.len();
    while written < len {
        let row = format!("{written:012} {tag} bounded-read fixture row\n");
        let take = row.len().min(len - written);
        w.write_all(&row.as_bytes()[..take]).unwrap();
        written += take;
    }
    w.flush().unwrap();
}

/// A file read that hits the content cap is a *successful truncated file*,
/// not a missing object. It must therefore never fall through to the
/// `<id>^:<path>` fallback — that fallback exists for a file this commit
/// *deleted*, and silently answering a cap with the parent's older content
/// would be a wrong answer wearing a 200.
#[tokio::test]
async fn bounded_file_read_caps_without_parent_fallback() {
    let (_dir, repo) = seeded_repo();
    // The parent's version is small and unmistakable: if a cap ever fell
    // through to the fallback, this is what would come back.
    std::fs::write(repo.join("big.txt"), "PARENT-VERSION\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "parent"]);
    // The commit under test replaces it with a file past the 2 MB cap,
    // carrying its own marker on line one.
    write_rows(
        &repo.join("big.txt"),
        "CHILD-VERSION\n",
        FILE_CONTENT_CAP + 500_000,
        "child",
    );
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "child"]);
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let file = file_at_commit_for_repo(&repo, &id, "big.txt")
        .await
        .expect("a capped read of an existing file is a success, not an error");

    assert!(file.truncated, "the cap hit must be reported");
    assert!(!file.binary);
    assert!(
        file.content.len() <= FILE_CONTENT_CAP,
        "content kept {} bytes, cap is {FILE_CONTENT_CAP}",
        file.content.len()
    );
    assert!(
        file.content.starts_with("CHILD-VERSION\n"),
        "the cap must not fall back to the parent's version"
    );
    assert!(!file.content.contains("PARENT-VERSION"));
    assert_eq!(file.id, id);
    assert_eq!(file.path, "big.txt");

    // The fallback itself still works, for the case it was written for: a
    // file this commit deleted is served from the first parent.
    run(&repo, &["rm", "-q", "big.txt"]);
    run(&repo, &["commit", "-q", "-m", "delete"]);
    let deleted_at = out(&repo, &["rev-parse", "HEAD"]);
    let deleted = file_at_commit_for_repo(&repo, &deleted_at, "big.txt")
        .await
        .expect("a file deleted by this commit is served from its parent");
    assert!(deleted.content.starts_with("CHILD-VERSION\n"));
    assert!(deleted.truncated);
}

/// Roughly the size of the text fixture's first version.
const BIG_TEXT_BYTES: usize = 50 * 1024 * 1024;
/// How much the second version appends — comfortably past both patch caps.
const BIG_TEXT_APPEND: usize = 8 * 1024 * 1024;
/// A string that appears only inside the binary blob. If it ever shows up in
/// a patch, binary bytes reached the wire.
const BINARY_SENTINEL: &str = "GV-BINARY-SENTINEL-PAYLOAD";

fn on_disk_len(path: &Path) -> usize {
    std::fs::metadata(path).unwrap().len() as usize
}

/// `len` bytes of binary content: NUL-delimited sentinel runs. The leading
/// NUL is inside the first 8000 bytes, which is what makes both git and our
/// own sniff call this binary.
fn binary_blob(tag: &str, len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    bytes.push(0u8);
    bytes.extend_from_slice(tag.as_bytes());
    while bytes.len() < len {
        bytes.push(0u8);
        bytes.extend_from_slice(BINARY_SENTINEL.as_bytes());
    }
    bytes.truncate(len);
    bytes
}

// ---- POST /api/diff/spec: the four explicit modes (M2.16, #69) -------

/// A repository where **each of the four `DiffSpec` modes sees a different
/// change**, so a test can prove a mode diffed what it claims rather than
/// merely returning some non-empty patch.
///
/// `v.txt` moves through four values, each parked in a different place:
///
/// ```text
///   one    commit 1 (branch `base`)
///   two    commit 2 (branch `main`, HEAD)
///   three  staged in the index, not committed
///   four   in the working tree, not staged
/// ```
///
/// So `WorktreeVsIndex` must see three→four, `IndexVsCommit(HEAD)` two→three,
/// and `CommitVsCommit`/`RefVsRef` one→two. Four modes, four distinguishable
/// answers — a mode that silently ran the wrong argv shows up as the wrong
/// pair, not as a pass.
fn four_mode_repo() -> (tempfile::TempDir, PathBuf, String, String) {
    let (dir, repo) = seeded_repo();

    std::fs::write(repo.join("v.txt"), "one\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "v = one"]);
    let c1 = out(&repo, &["rev-parse", "HEAD"]);
    run(&repo, &["branch", "base"]);

    std::fs::write(repo.join("v.txt"), "two\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "v = two"]);
    let c2 = out(&repo, &["rev-parse", "HEAD"]);

    // Staged but uncommitted.
    std::fs::write(repo.join("v.txt"), "three\n").unwrap();
    run(&repo, &["add", "-A"]);

    // Working tree, on top of the staged value and not added.
    std::fs::write(repo.join("v.txt"), "four\n").unwrap();

    (dir, repo, c1, c2)
}

/// Assert a patch changes exactly `from` → `to`, and **not** any of the
/// other values in play. The negative half is the point: without it, a
/// patch containing every value (what `git diff` against the wrong base
/// would produce) satisfies the positive assertions too.
fn assert_changes(patch: &str, from: &str, to: &str, mode: &str) {
    assert!(
        patch.contains(&format!("-{from}")),
        "{mode}: expected removal of {from:?}; patch was:\n{patch}"
    );
    assert!(
        patch.contains(&format!("+{to}")),
        "{mode}: expected addition of {to:?}; patch was:\n{patch}"
    );
    for other in ["one", "two", "three", "four"] {
        if other == from || other == to {
            continue;
        }
        assert!(
            !patch.contains(&format!("-{other}\n")) && !patch.contains(&format!("+{other}\n")),
            "{mode}: patch mentions {other:?}, so it diffed the wrong pair;\
                 \npatch was:\n{patch}"
        );
    }
}

#[tokio::test]
async fn spec_diff_worktree_vs_index_sees_the_unstaged_edit_only() {
    let (_dir, repo, _c1, _c2) = four_mode_repo();
    let out = spec_diff_for_repo(&repo, DiffSpec::WorktreeVsIndex)
        .await
        .expect("worktree-vs-index answers");
    assert_changes(&out.patch, "three", "four", "WorktreeVsIndex");
    assert!(!out.truncated);
    assert_eq!(out.spec, DiffSpec::WorktreeVsIndex, "spec must echo back");
}

#[tokio::test]
async fn spec_diff_index_vs_commit_sees_the_staged_edit_only() {
    let (_dir, repo, _c1, c2) = four_mode_repo();
    let spec = DiffSpec::IndexVsCommit {
        commit: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
    };
    let out = spec_diff_for_repo(&repo, spec.clone())
        .await
        .expect("index-vs-commit answers");
    assert_changes(&out.patch, "two", "three", "IndexVsCommit");
    assert_eq!(out.spec, spec);
}

#[tokio::test]
async fn spec_diff_commit_vs_commit_sees_only_what_is_committed() {
    let (_dir, repo, c1, c2) = four_mode_repo();
    let spec = DiffSpec::CommitVsCommit {
        base: git_vista_protocol::plan::CommitOid::new(&c1).unwrap(),
        target: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
    };
    let out = spec_diff_for_repo(&repo, spec.clone())
        .await
        .expect("commit-vs-commit answers");
    // Neither the staged nor the worktree value may appear: this mode
    // reads committed history only.
    assert_changes(&out.patch, "one", "two", "CommitVsCommit");
    assert_eq!(out.spec, spec);
}

#[tokio::test]
async fn spec_diff_ref_vs_ref_resolves_names_to_the_same_answer() {
    let (_dir, repo, _c1, _c2) = four_mode_repo();
    let spec = DiffSpec::RefVsRef {
        base: git_vista_protocol::plan::RefName::new("base").unwrap(),
        target: git_vista_protocol::plan::RefName::new("main").unwrap(),
    };
    let out = spec_diff_for_repo(&repo, spec.clone())
        .await
        .expect("ref-vs-ref answers");
    assert_changes(&out.patch, "one", "two", "RefVsRef");
    assert_eq!(out.spec, spec);
}

/// The four modes must not collapse into each other. Stated as a direct
/// comparison because every per-mode test above could pass while two modes
/// quietly ran identical argv — `CommitVsCommit` and `RefVsRef` genuinely
/// *do* produce identical argv shapes by design, so "they differ" cannot be
/// assumed from the type alone.
#[tokio::test]
async fn the_worktree_index_and_commit_modes_return_genuinely_different_patches() {
    let (_dir, repo, c1, c2) = four_mode_repo();

    let worktree = spec_diff_for_repo(&repo, DiffSpec::WorktreeVsIndex)
        .await
        .unwrap();
    let index = spec_diff_for_repo(
        &repo,
        DiffSpec::IndexVsCommit {
            commit: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
        },
    )
    .await
    .unwrap();
    let committed = spec_diff_for_repo(
        &repo,
        DiffSpec::CommitVsCommit {
            base: git_vista_protocol::plan::CommitOid::new(&c1).unwrap(),
            target: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
        },
    )
    .await
    .unwrap();

    assert_ne!(worktree.patch, index.patch);
    assert_ne!(index.patch, committed.patch);
    assert_ne!(worktree.patch, committed.patch);
}

/// `--no-textconv` is a security property, not a formatting preference: a
/// repository's own `.gitattributes` can bind a `diff=<driver>` textconv
/// filter, and git *executes* that configured program to render file
/// contents. This proves the flag actually reaches git.
///
/// # What removing the flag actually does here — measured, not assumed
///
/// Mutation-checked by deleting `--no-textconv` from `spec_diff_for_repo`'s
/// argv and re-running: baseline 5 pass, mutated 4 pass and **this test
/// alone** fails. So it does guard the flag specifically rather than
/// tripping on any change.
///
/// But it fails by a different route than the assertion below suggests, and
/// that is worth stating rather than leaving for someone to rediscover.
/// Without the flag, git tries to run the filter, needs a temp file to do
/// it, and **the sandbox refuses**:
///
/// ```text
/// (500, "fatal: unable to create temp-file: Permission denied")
/// ```
///
/// The call errors before any patch exists, so `unwrap()` panics and the
/// marker assertion never evaluates. That is a genuinely good finding — the
/// sandbox blocks textconv execution independently of this flag, so the two
/// are defence in depth rather than one guard. It also means **this test
/// would still go red if the marker assertion were deleted**, which is
/// exactly the kind of overlap that makes a test look stronger than it is.
///
/// The assertion is kept because it is the one that stays meaningful if the
/// sandbox is ever loosened, or on a filter that needs no temp file — but
/// on this box, today, the sandbox fires first.
#[tokio::test]
async fn spec_diff_never_runs_a_repository_configured_textconv_filter() {
    let (_dir, repo, _c1, c2) = four_mode_repo();

    // A textconv driver that replaces any file's rendered content. If it
    // runs, the marker appears instead of the real diff text.
    std::fs::write(repo.join(".gitattributes"), "v.txt diff=pwned\n").unwrap();
    run(
        &repo,
        &["config", "diff.pwned.textconv", "echo TEXTCONV_RAN"],
    );

    for (label, spec) in [
        ("WorktreeVsIndex", DiffSpec::WorktreeVsIndex),
        (
            "IndexVsCommit",
            DiffSpec::IndexVsCommit {
                commit: git_vista_protocol::plan::CommitOid::new(&c2).unwrap(),
            },
        ),
    ] {
        // `expect`, not `unwrap`: with the flag removed this is where the
        // failure actually lands (sandbox-denied temp file), so the message
        // should name the cause rather than printing a bare Err.
        let out = spec_diff_for_repo(&repo, spec).await.unwrap_or_else(|e| {
            panic!(
                "{label}: the diff read failed instead of answering: {e:?}. \
                     If this is a temp-file permission error, git attempted a \
                     textconv filter — meaning --no-textconv is missing from \
                     this mode's argv and the sandbox caught what the flag \
                     should have prevented."
            )
        });
        assert!(
            !out.patch.contains("TEXTCONV_RAN"),
            "{label}: a repository-configured textconv filter executed — \
                 --no-textconv is missing from this mode's argv"
        );
    }
}

/// A repository whose HEAD commit modifies both a ~50 MiB text file and a
/// NUL-bearing binary blob.
///
/// Two deliberate choices. `bin.dat` sorts before `zbig.txt`, so git's patch
/// leads with the binary section — otherwise the 200 KB panel cap would cut
/// away the very "Binary files … differ" line the test is about. And the
/// text change is an *append*: git trims the identical 50 MiB prefix in one
/// pass, so the fixture stays a fixture instead of a minutes-long diff,
/// while still producing a patch far past both patch caps.
fn pathological_repo() -> (tempfile::TempDir, PathBuf, String) {
    let (dir, repo) = seeded_repo();
    write_rows(&repo.join("zbig.txt"), "ZBIG\n", BIG_TEXT_BYTES, "alpha");
    std::fs::write(repo.join("bin.dat"), binary_blob("one", 64 * 1024)).unwrap();
    assert_eq!(on_disk_len(&repo.join("zbig.txt")), BIG_TEXT_BYTES);
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "add pathological content"]);

    write_rows(
        &repo.join("zbig.txt"),
        "ZBIG\n",
        BIG_TEXT_BYTES + BIG_TEXT_APPEND,
        "alpha",
    );
    assert_eq!(
        on_disk_len(&repo.join("zbig.txt")),
        BIG_TEXT_BYTES + BIG_TEXT_APPEND,
        "the fixture must really be ~50 MiB, not a silently short write"
    );
    std::fs::write(repo.join("bin.dat"), binary_blob("two", 96 * 1024)).unwrap();
    run(&repo, &["add", "-A"]);
    run(
        &repo,
        &["commit", "-q", "-m", "modify pathological content"],
    );

    let id = out(&repo, &["rev-parse", "HEAD"]);
    (dir, repo, id)
}

/// The whole point of the milestone, driven through the real handler helper:
/// a commit no iPad could ever render still comes back bounded, honestly
/// flagged, and with the binary blob's bytes nowhere near the wire.
#[tokio::test]
async fn bounded_diff_handles_large_text_and_binary_without_blob_leak() {
    let (_dir, repo, id) = pathological_repo();

    let panel = commit_diff_for_repo(&repo, &id, false, DIFF_METADATA_CAP)
        .await
        .expect("a pathological commit still answers, bounded");

    assert!(panel.truncated, "a 50 MiB change must report truncation");
    assert!(
        panel.patch.len() <= DIFF_PATCH_CAP,
        "panel patch kept {} bytes, cap is {DIFF_PATCH_CAP}",
        panel.patch.len()
    );
    // git *names* the binary file rather than printing it — with neither
    // `--binary` nor textconv, its bytes have no way onto the wire.
    assert!(
        panel
            .patch
            .contains("Binary files a/bin.dat and b/bin.dat differ"),
        "git's binary line must survive the cap; patch starts: {:?}",
        &panel.patch[..panel.patch.len().min(300)]
    );
    assert!(
        !panel.patch.contains(BINARY_SENTINEL),
        "the blob's bytes leaked into the patch"
    );
    assert!(
        !panel.patch.contains('\0'),
        "NUL bytes leaked into the patch"
    );

    // The metadata is complete even though the patch was cut: the binary
    // file carries git's `-`/`-` counts (i.e. `None`), the text file real
    // ones. The text file is the positive control — without it, `None`
    // could equally mean the numstat fold matched nothing at all.
    let bin = panel
        .files
        .iter()
        .find(|f| f.path == "bin.dat")
        .expect("bin.dat is in the file list");
    assert_eq!(bin.additions, None);
    assert_eq!(bin.deletions, None);
    let text = panel
        .files
        .iter()
        .find(|f| f.path == "zbig.txt")
        .expect("zbig.txt is in the file list");
    assert!(
        text.additions.unwrap_or(0) > 0,
        "the numstat fold must have matched the text file: {text:?}"
    );

    // `?full=1` lifts the panel cap to the viewer's, and no further.
    let full = commit_diff_for_repo(&repo, &id, true, DIFF_METADATA_CAP)
        .await
        .expect("the full-screen read is bounded too");
    assert!(full.truncated);
    assert!(
        full.patch.len() <= DIFF_PATCH_CAP_FULL,
        "full patch kept {} bytes, cap is {DIFF_PATCH_CAP_FULL}",
        full.patch.len()
    );
    assert!(
        full.patch.len() > DIFF_PATCH_CAP,
        "?full=1 must actually lift the panel cap"
    );
}

/// The file viewer against the same fixture: a 58 MiB blob comes back at the
/// 2 MB cap, and the binary file keeps its existing "flagged, empty" shape.
#[tokio::test]
async fn bounded_file_handler_caps_large_existing_file() {
    let (_dir, repo, id) = pathological_repo();

    let big = file_at_commit_for_repo(&repo, &id, "zbig.txt")
        .await
        .expect("a huge existing file is a truncated success");
    assert!(!big.binary);
    assert!(big.truncated, "a 58 MiB file must report the 2 MB cap");
    assert!(
        big.content.len() <= FILE_CONTENT_CAP,
        "kept {} bytes, cap is {FILE_CONTENT_CAP}",
        big.content.len()
    );
    assert!(
        big.content.starts_with("ZBIG\n"),
        "the retained prefix is the file's own beginning"
    );

    // Bounding the read left the binary representation exactly as it was.
    let bin = file_at_commit_for_repo(&repo, &id, "bin.dat")
        .await
        .expect("the binary blob still resolves");
    assert!(bin.binary);
    assert!(bin.content.is_empty());
    assert!(!bin.truncated);
}

// ---- malicious `{*path}` against GET /api/file/{id}/{*path} (#67) --------
//
// `file_at_commit_for_repo` turns `path` into a `<rev>:<path>` git revision
// spec and shells out to `git -C <repo> show <spec>`. `-C <repo>` is
// equivalent to `cd <repo> && git show <spec>` — the process's effective cwd
// for git's own `<rev>:./path` / `<rev>:../path` resolution (documented in
// gitrevisions(7)) is therefore always `repo`, which every real caller
// (`resolve_repo`/`resolve_worktree`/`current()`) sets to a registered
// worktree's own root, never a subdirectory of one. So the cwd-relative
// resolution these tests probe is always rooted at the tree root in
// production, and — as the tests below establish — git itself refuses a
// `../` that would walk above that cwd ("outside repository"), independent
// of anything this server does. That is the fact this whole battery exists
// to pin down instead of assume.

/// A repository shaped to exercise the malicious-path battery: a root file,
/// a subdirectory (so a tree-vs-blob path exists), and a **committed
/// symlink** whose target must come back as blob content, never followed.
fn path_battery_repo() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    std::fs::write(repo.join("secret.txt"), "root-secret\n").unwrap();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/file.txt"), "sub-file\n").unwrap();
    std::os::unix::fs::symlink("file.txt", repo.join("sub/link.txt")).unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "path battery fixture"]);
    (dir, repo)
}

/// `../../../etc/passwd`, and a same-depth `../` from the tree root: git's
/// own boundary check refuses to resolve a `<rev>:../path` that would walk
/// above the cwd it resolved `-C repo` to (the worktree root), independent
/// of the tree object. This is the uncertain case the task exists to
/// establish, and it comes back a hard refusal, not a path.
#[tokio::test]
async fn file_read_relative_traversal_cannot_walk_above_repo_root() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    for path in ["../../../etc/passwd", "../secret.txt", "../../secret.txt"] {
        let err = file_at_commit_for_repo(&repo, &id, path)
            .await
            .expect_err(&format!("{path} must not resolve"));
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            err.1.contains("outside repository"),
            "path {path:?} produced unexpected message: {}",
            err.1
        );
    }
}

/// `./secret.txt` resolves from the same cwd (the repo root) precisely as
/// the bare tree-relative path does — the positive control for the
/// traversal test above: `./` and root-relative agree because cwd == tree
/// root in production.
#[tokio::test]
async fn file_read_dot_slash_prefix_matches_tree_relative_path() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let dotted = file_at_commit_for_repo(&repo, &id, "./secret.txt")
        .await
        .expect("./secret.txt must resolve, cwd is the tree root");
    let bare = file_at_commit_for_repo(&repo, &id, "secret.txt")
        .await
        .expect("control read");
    assert_eq!(dotted.content, bare.content);
    assert_eq!(dotted.content, "root-secret\n");
}

/// A leading `/` is not tree-root shorthand — git treats it as a literal
/// path component and reports the object missing, the same shape as any
/// other not-found path.
#[tokio::test]
async fn file_read_leading_slash_is_not_found_not_root_shorthand() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let err = file_at_commit_for_repo(&repo, &id, "/secret.txt")
        .await
        .expect_err("a leading slash must not silently mean the tree root");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
}

/// axum's `{*path}` wildcard percent-decodes the captured string before the
/// handler ever sees it (verified here against the real extractor, not
/// assumed), so `%2e%2e%2f` arrives at `file_at_commit_for_repo` already
/// turned into a literal `../` — no double-decoding boundary for an
/// attacker to exploit, and the traversal refusal above still applies to
/// whatever comes out the other side.
#[tokio::test]
async fn axum_wildcard_decodes_percent_encoding_before_the_handler() {
    async fn echo(AxumPath(path): AxumPath<String>) -> String {
        path
    }
    let app = Router::new().route("/f/{*path}", get(echo));
    let req = axum::http::Request::get("/f/%2e%2e%2fsecret.txt%2e%2e")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let decoded = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(decoded, "../secret.txt..");

    // Double-encoded (`%252e` -> literal `%2e`, not `.`) must NOT decode a
    // second time anywhere in the pipeline — it should reach the handler
    // still percent-escaped text and fail as a not-found path, not as a
    // second-order traversal.
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);
    let err = file_at_commit_for_repo(&repo, &id, "%252e%252e%252fsecret.txt")
        .await
        .expect_err("double-encoded traversal must not resolve");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
}

/// A path that names a **tree**, not a blob, is now a `404` (#168) — not
/// the `200` this test used to pin. `git show <rev>:<dir>` happily prints
/// a directory listing, and until this change the handler forwarded it
/// verbatim as if it were file content (no NUL, so it wasn't even flagged
/// binary). A tree is a different resource from a file, not another
/// representation of the same one, so the fix is a clean rejection
/// rather than a discriminator bolted onto `FileContent` — see the
/// doc comment on `file_at_commit_for_repo`. This test previously pinned
/// the listing-as-200 behaviour under a name saying so; it is now the
/// regression test for the rejection instead, renamed to match.
#[tokio::test]
async fn file_read_of_a_tree_path_is_rejected_not_returned_as_content() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let err = file_at_commit_for_repo(&repo, &id, "sub")
        .await
        .expect_err("a tree path must not answer as file content");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert!(
        err.1.contains("tree"),
        "reason should name the object kind: {}",
        err.1
    );
}

/// An empty path segment (`<id>:`) means the root tree in git, and is
/// rejected for exactly the same reason as the named-tree case above, one
/// level up — deliberately, not by accident: nothing distinguishes "no
/// path given" from "path names the root tree" once the type check is in
/// place, and the root tree is exactly as much "not a file" as `sub` is.
#[tokio::test]
async fn file_read_of_empty_path_is_rejected_as_the_root_tree() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let err = file_at_commit_for_repo(&repo, &id, "")
        .await
        .expect_err("an empty path names the root tree, not a file");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert!(
        err.1.contains("tree"),
        "reason should name the object kind: {}",
        err.1
    );
}

/// The trap this task exists to close: a path that is a regular **file**
/// in the parent commit and becomes a **directory** in the child commit.
/// A naive fix that made the tree case "fail" the existing `<id>:<path>`
/// vs `<id>^:<path>` content-read ladder would fall through to the
/// parent on the child's tree and hand back the parent's *file* bytes
/// with a 200 — silently answering a request for commit `X` with content
/// from `X^`. The type check must resolve against `X` first and reject
/// immediately on a tree, never reaching the parent at all.
#[tokio::test]
async fn a_file_that_becomes_a_directory_is_rejected_not_served_from_the_parent() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("was-a-file"), "PARENT-FILE-CONTENT\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(
        &repo,
        &["commit", "-q", "-m", "parent: was-a-file is a file"],
    );
    let parent_id = out(&repo, &["rev-parse", "HEAD"]);

    // The child replaces the file with a directory of the same name.
    run(&repo, &["rm", "-q", "was-a-file"]);
    std::fs::create_dir_all(repo.join("was-a-file")).unwrap();
    std::fs::write(repo.join("was-a-file/inner.txt"), "inner\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(
        &repo,
        &["commit", "-q", "-m", "child: was-a-file is now a directory"],
    );
    let child_id = out(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(child_id, parent_id);

    let err = file_at_commit_for_repo(&repo, &child_id, "was-a-file")
            .await
            .expect_err("a directory in the requested commit must be rejected, not silently answered from the parent's file");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert!(!err.1.contains("PARENT-FILE-CONTENT"));

    // Control: the parent's own read of the same path still works and
    // still returns the file it always did — the fix changed only the
    // child's answer, not the parent's.
    let parent_file = file_at_commit_for_repo(&repo, &parent_id, "was-a-file")
        .await
        .expect("the parent's own read of the file is unaffected");
    assert_eq!(parent_file.content, "PARENT-FILE-CONTENT\n");
}

/// The mirror of the trap test above, for the case #167's original
/// fallback exists to serve: a path this commit **deleted** (so the
/// first type resolution genuinely finds nothing) whose *parent* version
/// was a **tree**, not a file. Before #168 this returned the parent's
/// directory listing as a 200 — the same wart as the direct case, one
/// commit removed, reached only through the fallback ladder. The type
/// check must apply to the fallback's resolution too, not just the first
/// attempt.
#[tokio::test]
async fn a_deleted_path_whose_parent_was_a_tree_is_rejected_through_the_fallback() {
    let (_dir, repo) = path_battery_repo();
    let parent_id = out(&repo, &["rev-parse", "HEAD"]);

    // The child deletes `sub` entirely, so `<child>:sub` resolves to
    // nothing and the fallback is what actually answers.
    run(&repo, &["rm", "-q", "-r", "sub"]);
    run(&repo, &["commit", "-q", "-m", "child: delete sub"]);
    let child_id = out(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(child_id, parent_id);

    let err = file_at_commit_for_repo(&repo, &child_id, "sub")
        .await
        .expect_err("the parent's tree must not leak through the fallback as a 200");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert!(
        !err.1.contains("file.txt"),
        "no listing content should appear in the error"
    );
}

/// A submodule (a `commit`-typed tree entry) is exactly as much "not a
/// file" as a directory — `git show <rev>:<submodule-path>` prints the
/// referenced commit's own log/diff, not the submodule's own bytes, which
/// is an even more misleading 200 than a directory listing would be.
#[tokio::test]
async fn a_submodule_entry_is_rejected_not_shown_as_the_referenced_commits_log() {
    let (_dir, repo) = seeded_repo();
    let inner_commit = out(&repo, &["rev-parse", "HEAD"]);
    // A gitlink tree entry (mode 160000) pointing at some commit — enough
    // to make git treat the path as type `commit`, with no real
    // submodule checkout required for this handler-level test.
    run(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &inner_commit,
            "vendor/lib",
        ],
    );
    run(&repo, &["commit", "-q", "-m", "add a submodule gitlink"]);
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let err = file_at_commit_for_repo(&repo, &id, "vendor/lib")
        .await
        .expect_err("a submodule gitlink must not answer as file content");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert!(
        err.1.contains("commit"),
        "reason should name the object kind: {}",
        err.1
    );
}

/// A path with an embedded newline can never name a real git object, so it
/// must fail as a clean not-found — not panic, and not somehow be
/// interpreted as two arguments (it travels as a single argv element, same
/// belt-and-braces as the id check above it).
#[tokio::test]
async fn file_read_embedded_newline_is_a_clean_not_found() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let err = file_at_commit_for_repo(&repo, &id, "secret.txt\nsub/file.txt")
        .await
        .expect_err("a newline-bearing path cannot name a real object");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
}

/// A several-KB path is refused cleanly by git (no such blob) rather than
/// causing unbounded allocation or a hang on this server's side — the read
/// is still going through `git_stdout_capped`, the same bounded reader as
/// every other file/diff read.
#[tokio::test]
async fn file_read_very_long_path_is_refused_cleanly() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);
    let long_path = "a".repeat(8_000);

    let err = file_at_commit_for_repo(&repo, &id, &long_path)
        .await
        .expect_err("no several-KB path exists in the fixture");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
}

/// A committed symlink's blob **is** its target string — `git show` must
/// report that literal text, not follow the link and return the linked
/// file's content. Confirms path resolution never leaves git's own object
/// model to touch the filesystem's symlink semantics.
#[tokio::test]
async fn file_read_of_a_committed_symlink_returns_target_text_not_dereferenced() {
    let (_dir, repo) = path_battery_repo();
    let id = out(&repo, &["rev-parse", "HEAD"]);

    let link = file_at_commit_for_repo(&repo, &id, "sub/link.txt")
        .await
        .expect("the symlink blob itself resolves");
    assert_eq!(link.content, "file.txt");
    assert!(!link.content.contains("sub-file"));
}

/// Every case above, again against the `<id>^:path>` fallback: build a
/// commit whose tree lacks all of the fixture's paths (so the first `show`
/// attempt always misses and the retry against the parent is what actually
/// answers), then repeat the security-relevant assertions. A malicious path
/// that only got exercised on the happy path would miss this second attempt
/// entirely.
#[tokio::test]
async fn malicious_paths_behave_identically_through_the_parent_fallback() {
    let (_dir, repo) = path_battery_repo();
    let parent_id = out(&repo, &["rev-parse", "HEAD"]);

    // A child commit that deletes everything path_battery_repo added, so
    // `<child>:<path>` always misses and every read below is answered by
    // the `<child>^:<path>` retry against `parent_id`'s tree.
    run(&repo, &["rm", "-q", "-r", "secret.txt", "sub"]);
    run(&repo, &["commit", "-q", "-m", "delete everything"]);
    let child_id = out(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(
        child_id, parent_id,
        "the fallback must actually cross a commit"
    );

    // Traversal is still refused.
    let err = file_at_commit_for_repo(&repo, &child_id, "../secret.txt")
        .await
        .expect_err("traversal must be refused through the fallback too");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(err.1.contains("outside repository"));

    // The symlink still comes back as its target text, not dereferenced.
    // `FileContent.id` echoes back the *requested* commit id even when the
    // content was actually read from its parent's tree — that's the
    // existing contract (see `bounded_file_read_caps_without_parent_fallback`
    // above), not something this test introduces.
    let link = file_at_commit_for_repo(&repo, &child_id, "sub/link.txt")
        .await
        .expect("the fallback must reach the parent's symlink blob");
    assert_eq!(link.content, "file.txt");
    assert_eq!(link.id, child_id);

    // A tree path is rejected through the fallback too (#168) — covered
    // in full, including the "no listing leaks into the error" check, by
    // `a_deleted_path_whose_parent_was_a_tree_is_rejected_through_the_fallback`.
    let tree_err = file_at_commit_for_repo(&repo, &child_id, "sub")
        .await
        .expect_err("a tree must not answer as content, fallback or not");
    assert_eq!(tree_err.0, StatusCode::NOT_FOUND);

    // `truncated` must never be true for this tiny fixture — a sign the
    // cap logic didn't misfire on the retry path.
    assert!(!link.truncated);
}

#[tokio::test]
async fn catalog_endpoint_lists_entries_without_leaking_paths() {
    // The capability report is valid JSON and, by default, carries no paths.
    let app = Router::new().route("/api/catalog", get(crate::handlers::catalog::catalog_list));
    let req = axum::http::Request::get("/api/catalog")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    // Deserialises as the descriptor list, and no descriptor carries a path.
    let list: Vec<RepositoryDescriptor> = serde_json::from_slice(&bytes).unwrap();
    assert!(list.iter().all(|d| d.path.is_none()));
}
