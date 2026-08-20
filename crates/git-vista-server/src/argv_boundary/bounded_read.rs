//! Layer 1b (M1.10, #63; collapsed to one file spawn in #221): the streaming
//! source boundary for the two bounded read handlers in `handlers/read.rs`.
//! Split out of `argv_boundary.rs` as its own seam — a proof about how a
//! *read* stays bounded, distinct from the allowlist's proof about *who* may
//! spawn at all.
//!
//! **This file is scanned too, and is not exempt.** The parent's spawn-site
//! scan (`every_process_spawn_site_is_allowlisted_and_spawns_only_git`) walks
//! every `.rs` file under `src/`, including this one, and its by-name
//! exemption from the literal-`git` check names only `src/argv_boundary.rs` —
//! not this path. This file already assembles the needle it looks for at
//! runtime (`Command` + `::new(`) rather than spelling it out, the same
//! discipline `argv_boundary.rs` and `sandbox/compat.rs` apply to their own
//! source; keep doing that in any comment added here too, or a prose mention
//! reads as a new, unreviewed spawn site.

use std::path::Path;

use super::code_only;

/// The body of the one **production** `fn <name>` in `code` (already passed
/// through [`code_only`]), matched brace-for-brace.
///
/// Deliberately strict: exactly one definition must exist, and it must sit
/// ahead of `mod tests`, so a same-named test helper can neither be picked up
/// instead of the real thing nor make the scan ambiguous.
fn production_body<'a>(code: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let defs = code.matches(&marker).count();
    assert_eq!(
        defs, 1,
        "expected exactly one `{marker}` definition in handlers/read.rs, found {defs}"
    );
    let at = code.find(&marker).expect("counted above");
    let tests_at = code
        .find("mod tests")
        .expect("handlers/read.rs has a test module");
    assert!(
        at < tests_at,
        "`{marker}` was found inside `mod tests`, not in production code"
    );

    let open = at
        + code[at..]
            .find('{')
            .expect("a function signature is followed by its body brace");
    let mut depth = 0usize;
    for (offset, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let body = &code[open + 1..open + offset];
                    assert!(
                        body.len() > 200,
                        "extracted body for `{marker}` is implausibly small ({} bytes)",
                        body.len()
                    );
                    return body;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces while extracting `{marker}`");
}

/// Layer 1b (M1.10, #63; collapsed to one file spawn in #221): the
/// *streaming* source boundary. Every git read the two bounded read handlers
/// perform must go through a primitive that owns its child process end to
/// end and bounds what it reads — proved structurally, on the source, not
/// inferred from the size of a returned buffer.
///
/// Exactly one production body is extracted for each of `commit_diff_for_repo`
/// and `file_at_commit_for_repo`; across only those two bodies there must be
/// exactly four such calls: three `git_stdout_capped(` (the diff's
/// `--name-status`, `--numstat` and `--patch` reads) plus exactly one
/// `git_cat_file_batch(` (#221: the file read's single `cat-file --batch`
/// spawn, which does the #168 type check and, when it resolves to a blob,
/// the content read, on the one still-open process — including through the
/// `<id>^:<path>` parent-fallback). And no escape hatch — no uncapped
/// `git_stdout(`, no `.output()`, no `.wait_with_output()`, no direct
/// `Command` construction, each of which would buffer whatever git chose to
/// print.
///
/// `file_at_commit_for_repo` went from one call site to two in #168 (a
/// `git cat-file -t <spec>` type check, through the same capped primitive,
/// ran before the `git show` content read) and from two back down to *one*
/// in #221: the type check and the content read are now two possible facts
/// read off one `cat-file --batch` response stream, so a tree or submodule
/// entry is still rejected without ever reading (or serving) content bytes —
/// enforced by the wire's own field order rather than by two separate
/// spawns. See that function's doc comment, and `git_cat_file_batch`'s in
/// `git_cmd.rs`.
///
/// The scope is deliberately narrow. The unrelated `worktree_status` read in
/// the very same file legitimately buffers a whole (tiny, static-arg) git
/// output — since Task 6 through the sealed `git_cmd::git_output` helper rather
/// than a raw `Command` — and the assertion below that the *file* still
/// contains that call while the two extracted *bodies* do not is what proves
/// the extractor cut where it claims to, instead of quietly matching nothing.
#[test]
fn bounded_read_source_boundary_is_streaming_and_exactly_four() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handlers/read.rs");
    let src = std::fs::read_to_string(&path).expect("readable handlers/read.rs");
    let code = code_only(&src);

    let capped = ["git_stdout", "_capped("].concat();
    let batched = ["git_cat_file", "_batch("].concat();
    let uncapped = ["git_stdout", "("].concat();
    // Assembled at runtime, like the spawn scan above, so this file's own source
    // never contains the bare patterns it forbids.
    let banned: [(String, &str); 4] = [
        ([".output", "()"].concat(), "buffers all of git's stdout"),
        (
            [".wait_with", "_output()"].concat(),
            "buffers all of git's stdout",
        ),
        (
            ["Command", "::new"].concat(),
            "spawns git outside the capped primitive",
        ),
        (
            ["git_out", "put("].concat(),
            "buffers all of git's stdout (the sealed helper is for small, \
             fixed-size reads, never for these streams)",
        ),
    ];

    let diff_body = production_body(&code, "commit_diff_for_repo");
    let file_body = production_body(&code, "file_at_commit_for_repo");

    // The two bodies are distinct regions of the same file.
    assert_ne!(
        diff_body.as_ptr(),
        file_body.as_ptr(),
        "the extractor returned the same body twice"
    );

    let diff_calls = diff_body.matches(&capped).count();
    assert_eq!(
        diff_calls, 3,
        "commit_diff_for_repo must perform exactly three bounded reads \
         (--name-status -z, --numstat -z, --patch), found {diff_calls}"
    );

    let file_capped_calls = file_body.matches(&capped).count();
    assert_eq!(
        file_capped_calls, 0,
        "file_at_commit_for_repo must no longer call the two-spawn \
         git_stdout_capped primitive at all — #221 folded its reads into the \
         single-spawn batch primitive below, found {file_capped_calls}"
    );
    let file_batch_calls = file_body.matches(&batched).count();
    assert_eq!(
        file_batch_calls, 1,
        "file_at_commit_for_repo must perform exactly one batched read (the \
         #168 type check and, when applicable, the content read, both off \
         one still-open `cat-file --batch` process, including through the \
         parent-fallback), found {file_batch_calls}"
    );

    assert_eq!(
        diff_calls + file_batch_calls,
        4,
        "exactly four target callers cross the capped/batched boundary"
    );

    for (what, body) in [
        ("commit_diff_for_repo", diff_body),
        ("file_at_commit_for_repo", file_body),
    ] {
        assert_eq!(
            body.matches(&uncapped).count(),
            0,
            "{what}: an uncapped `{uncapped}` read survives — every read here \
             must name its own cap"
        );
        for (needle, why) in banned.iter() {
            assert_eq!(
                body.matches(needle.as_str()).count(),
                0,
                "{what}: `{needle}` {why}; the bounded primitive owns the child"
            );
        }
    }

    // Narrowness, both directions. The file as a whole still contains the
    // unrelated buffering invocation — `worktree_status` runs
    // `git status --porcelain=v2` and buffers its (tiny, static-arg) output,
    // since Task 6 through the sealed `git_cmd::git_output` helper — so the two
    // extractions above cut where they claim to rather than swallowing the
    // whole file and asserting over nothing. Before Task 6 the witness here was
    // a raw `.output()` call; that migrated away, and a witness that quietly
    // degrades to `#[cfg(test)]` fixtures is this guard passing vacuously — so
    // the witness now names the production helper itself. (`porcelain=v2` is
    // checked against the raw source: `code_only` blanks string contents.)
    let sealed_buffered = ["git_out", "put("].concat();
    assert!(
        code.matches(sealed_buffered.as_str()).count() > 0,
        "file-wide `{sealed_buffered}` vanished: either worktree_status changed, \
         or this guard is now passing vacuously"
    );
    assert!(
        src.contains("porcelain=v2"),
        "the unrelated worktree-status read is expected to remain in this file"
    );
    // Each extracted body really is the one under test, not a stray region that
    // happens to be brace-balanced.
    assert!(
        diff_body.contains("patch_cap(full)"),
        "the extracted diff body does not select a patch cap"
    );
    assert!(
        file_body.contains("FILE_CONTENT_CAP"),
        "the extracted file body does not name the file content cap"
    );
}
