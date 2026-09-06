//! #586 (M13.05): a tripwire that fails if a credential reaches a tracked
//! file. Same shape as `argv_boundary.rs`'s census tests — a test that reads
//! the repository and refuses.
//!
//! # The design question, and the answer this file commits to
//!
//! The obvious version — a list of forbidden literal strings, or a list of
//! files exempted from the scan — fails open exactly the way ADR 0119 found
//! for a different value: *"a list of known message sites is not a fix,
//! because that list was already incomplete twice — the safety has to live
//! in the value."* An exclusion list here has the identical failure mode:
//! the first documentation update that needs to *name* a prefix (`ghp_`,
//! `Authorization: Bearer`, …) either trips the guard or teaches someone to
//! add an exclusion — and the someone adding it is, structurally, the person
//! least likely to be thinking about what the guard protects.
//!
//! So this scanner matches on **shape**, not on a prefix alone and not on an
//! exclusion list of any kind. A real GitHub token is its prefix followed by
//! a long, unbroken run of base62 characters — 36 for a classic PAT
//! (`ghp_`/`gho_`/`ghs_`/`ghu_`), roughly 82 for a fine-grained
//! `github_pat_`. Documentation writes the prefix bare (`` `ghp_` ``), with
//! an ellipsis, or with a placeholder like `ghp_YOUR_TOKEN_HERE` — never a
//! real-length unbroken alphanumeric run immediately after it, because there
//! is no reason prose would ever need to. [`PrefixPattern::min_body_len`] is
//! chosen comfortably under the real minted length and comfortably over
//! anything a human writes by hand; the boundary itself is pinned by
//! [`body_shorter_than_the_shape_threshold_is_prose_not_a_credential`] and
//! [`body_at_the_shape_threshold_is_treated_as_a_real_credential`] below, and
//! [`prose_naming_every_prefix_without_a_real_body_never_trips_the_guard`]
//! proves the actual sentences this project's own docs already write pass
//! clean.
//!
//! `Authorization: Bearer` gets the same treatment for the same reason: the
//! header name and the literal word `Bearer` are exactly the kind of thing a
//! design document legitimately writes out (see `docs/SECURITY_MODEL.md`'s
//! own "Redact HTTP `Authorization` header text" row); only a header
//! followed by a real-length token-shaped value counts as a leak.
//!
//! **A note on `Authorization: Bearer`'s premise.** ADR 0122 (#582) argues at
//! length that no HTTP client exists anywhere in this codebase — every
//! remote operation is a spawned `git` process, so there is nowhere a
//! `Bearer` header could even be *constructed*. That makes a match on this
//! pattern in tracked source not merely a credential leak but evidence that
//! ADR 0122's premise has quietly stopped being true — worth noticing on its
//! own terms, not only as a leak.
//!
//! # "Through the same code path" — what that actually means here
//!
//! The issue's acceptance requires the red-fixture proof to run "through the
//! same code path" as the real guard, not merely assert the regex matches a
//! string in isolation. [`scan_tracked_files`] is the one function both the
//! real guard
//! ([`no_tracked_file_in_this_repository_contains_a_credential_shaped_string`])
//! and every fixture test below call — the fixture tests point it at a
//! throwaway `git init`-ed directory instead of this repository, but the
//! scanning, the file discovery (`git ls-files`, never a hand-rolled walk),
//! and the shape-matching are identical code. A regression that broke the
//! scan (the wrong git subcommand, a narrowed file set, a removed pattern)
//! would go undetected by the real guard as long as the repository stayed
//! clean — which it does today, and would keep doing even with a broken
//! scanner — so the fixture tests are what actually exercises the failure
//! path, and mutation-proving them is what proves the exercise means
//! anything (see the two mutation arms recorded in ADR 0123).
//!
//! # Fixture tokens are built at runtime, never as a literal in this file
//!
//! This file is itself a tracked file, scanned by the guard it defines. A
//! literal 20-character alphanumeric run sitting directly in this file's
//! source, right after one of these prefixes, would trip the guard the
//! moment this file lands — the same self-reference `argv_boundary.rs`
//! already solves for its own needle (*"the needles are assembled at
//! runtime so this file's own source never contains the bare pattern it
//! scans for"*). Every synthetic fixture token below is built with
//! `.repeat(...)`/`format!`, never spelled out as one string literal.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repo root resolves from CARGO_MANIFEST_DIR")
}

/// A credential-prefix pattern: the literal prefix, and the minimum length
/// of unbroken alphanumeric body that must immediately follow it for a match
/// to count as a real credential shape rather than prose. See the module
/// doc for the argument behind the threshold.
struct PrefixPattern {
    name: &'static str,
    prefix: &'static str,
    min_body_len: usize,
}

/// Real minted lengths, for the record: classic PATs and App tokens
/// (`ghp_`/`gho_`/`ghs_`/`ghu_`) carry a 36-character body; fine-grained
/// `github_pat_` tokens carry roughly 82. `20` sits comfortably under both
/// and comfortably over `ghp_YOUR_TOKEN_HERE`-shaped prose — see the
/// boundary tests below for the assertion, not just this comment.
const MIN_BODY_LEN: usize = 20;

const PREFIX_PATTERNS: &[PrefixPattern] = &[
    PrefixPattern {
        name: "ghp_ (classic personal access token)",
        prefix: "ghp_",
        min_body_len: MIN_BODY_LEN,
    },
    PrefixPattern {
        name: "github_pat_ (fine-grained personal access token)",
        prefix: "github_pat_",
        min_body_len: MIN_BODY_LEN,
    },
    PrefixPattern {
        name: "gho_ (OAuth token)",
        prefix: "gho_",
        min_body_len: MIN_BODY_LEN,
    },
    PrefixPattern {
        name: "ghs_ (GitHub App server-to-server token)",
        prefix: "ghs_",
        min_body_len: MIN_BODY_LEN,
    },
    PrefixPattern {
        name: "ghu_ (GitHub App user-to-server token)",
        prefix: "ghu_",
        min_body_len: MIN_BODY_LEN,
    },
];

const BEARER_PATTERN_NAME: &str = "Authorization: Bearer";

/// A single line's worst prefix match, or `None`. Loops every occurrence of
/// every prefix on the line (not just the first), so a prefix appearing
/// twice — once as bare prose, once with a real body — is still caught.
fn find_prefix_match(line: &str) -> Option<&'static str> {
    for pattern in PREFIX_PATTERNS {
        for (idx, _) in line.match_indices(pattern.prefix) {
            let body = &line[idx + pattern.prefix.len()..];
            let run = body
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            if run >= pattern.min_body_len {
                return Some(pattern.name);
            }
        }
    }
    None
}

/// Whether the line carries an `Authorization: Bearer <real-length-value>`
/// header. Same shape argument as [`find_prefix_match`]: the header name and
/// the word `Bearer` are legitimate to write in prose; only a following
/// token-shaped run of real length counts.
fn find_bearer_match(line: &str) -> bool {
    for (idx, _) in line.match_indices("Authorization:") {
        let rest = line[idx + "Authorization:".len()..].trim_start();
        let Some(after_bearer) = rest.strip_prefix("Bearer") else {
            continue;
        };
        let after_bearer = after_bearer.trim_start();
        let run = after_bearer
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .count();
        if run >= MIN_BODY_LEN {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone)]
struct Violation {
    file: PathBuf,
    line: usize,
    pattern: &'static str,
}

/// `git ls-files`, not a hand-rolled directory walk — the same authority
/// every other census in this crate defers to for "what does this
/// repository actually track" (`.gitignore`, submodules, and anything
/// `git rm --cached`ed all resolve correctly for free). `-z` so a path
/// containing a newline cannot desynchronise the list.
fn tracked_files(repo_root: &Path) -> Vec<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("ls-files")
        .arg("-z")
        .output()
        .unwrap_or_else(|e| panic!("running git ls-files under {repo_root:?}: {e}"));
    assert!(
        out.status.success(),
        "git ls-files failed under {repo_root:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| repo_root.join(std::str::from_utf8(s).expect("git ls-files paths are utf-8")))
        .collect()
}

/// The scan every test in this file ultimately calls — the real guard
/// against this repository, and every fixture proof against a throwaway
/// one. See the module doc's "through the same code path" section for why
/// that sharing is the point, not an implementation convenience.
fn scan_tracked_files(repo_root: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    for file in tracked_files(repo_root) {
        // A path git ls-files reports can be gone by the time this reads it
        // (a fixture cleaned up mid-scan, in principle) — skip rather than
        // panic; a missing file cannot itself carry a credential.
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let rel = file.strip_prefix(repo_root).unwrap_or(&file).to_path_buf();
        for (i, line) in text.lines().enumerate() {
            if let Some(pattern) = find_prefix_match(line) {
                violations.push(Violation {
                    file: rel.clone(),
                    line: i + 1,
                    pattern,
                });
            } else if find_bearer_match(line) {
                violations.push(Violation {
                    file: rel.clone(),
                    line: i + 1,
                    pattern: BEARER_PATTERN_NAME,
                });
            }
        }
    }
    violations
}

/// **The guard.** Runs under `cargo test --workspace`, so `./dev gate`'s
/// existing `test` step is what makes this "run in the gate" — no separate
/// wiring needed. The panic message never prints the matched text itself
/// (only the file, line, and which pattern) — a failing security guard
/// echoing the very secret it caught would be a second leak riding the
/// first one's coattails.
#[test]
fn no_tracked_file_in_this_repository_contains_a_credential_shaped_string() {
    let violations = scan_tracked_files(&repo_root());
    assert!(
        violations.is_empty(),
        "credential-shaped string(s) found in tracked files — value withheld \
         on purpose (this guard exists so a secret is never echoed, including \
         in its own failure message):\n{}",
        violations
            .iter()
            .map(|v| format!(
                "  {}:{} — looks like a {} credential",
                v.file.display(),
                v.line,
                v.pattern
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A throwaway repository: `git init`, write the given files, `git add -A`.
/// No commit — `git ls-files` reads the index, which `add` alone populates.
fn throwaway_tracked_repo(files: &[(&str, String)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a tempdir");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(dir.path())
        .status()
        .expect("running git init");
    assert!(status.success(), "git init failed");
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating fixture subdirectory");
        }
        std::fs::write(&path, content).expect("writing fixture file");
    }
    let status = Command::new("git")
        .arg("add")
        .arg("-A")
        .current_dir(dir.path())
        .status()
        .expect("running git add");
    assert!(status.success(), "git add failed");
    dir
}

// -- "prove it can go red": one fixture per pattern, so removing any single
// pattern from PREFIX_PATTERNS breaks exactly its own test, never another
// one's. This is the mutation-check target for "remove a pattern". --

#[test]
fn ghp_classic_pat_shaped_content_is_caught_through_the_real_scan() {
    let token = format!("ghp_{}", "A".repeat(24));
    let repo = throwaway_tracked_repo(&[("leaked.md", format!("token: {token}\n"))]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected one violation, got {violations:?}"
    );
    assert!(violations[0].pattern.starts_with("ghp_"));
}

#[test]
fn github_pat_fine_grained_shaped_content_is_caught_through_the_real_scan() {
    let token = format!("github_pat_{}", "B".repeat(40));
    let repo = throwaway_tracked_repo(&[("leaked.md", format!("token: {token}\n"))]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected one violation, got {violations:?}"
    );
    assert!(violations[0].pattern.starts_with("github_pat_"));
}

#[test]
fn gho_oauth_token_shaped_content_is_caught_through_the_real_scan() {
    let token = format!("gho_{}", "C".repeat(24));
    let repo = throwaway_tracked_repo(&[("leaked.md", format!("token: {token}\n"))]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected one violation, got {violations:?}"
    );
    assert!(violations[0].pattern.starts_with("gho_"));
}

#[test]
fn ghs_app_server_token_shaped_content_is_caught_through_the_real_scan() {
    let token = format!("ghs_{}", "D".repeat(24));
    let repo = throwaway_tracked_repo(&[("leaked.md", format!("token: {token}\n"))]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected one violation, got {violations:?}"
    );
    assert!(violations[0].pattern.starts_with("ghs_"));
}

#[test]
fn ghu_app_user_token_shaped_content_is_caught_through_the_real_scan() {
    let token = format!("ghu_{}", "E".repeat(24));
    let repo = throwaway_tracked_repo(&[("leaked.md", format!("token: {token}\n"))]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected one violation, got {violations:?}"
    );
    assert!(violations[0].pattern.starts_with("ghu_"));
}

#[test]
fn authorization_bearer_header_with_a_real_looking_value_is_caught_through_the_real_scan() {
    let value = "F".repeat(24);
    let repo = throwaway_tracked_repo(&[("notes.md", format!("Authorization: Bearer {value}\n"))]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected one violation, got {violations:?}"
    );
    assert_eq!(violations[0].pattern, BEARER_PATTERN_NAME);
}

// -- "weaken the scan's file set": one fixture proving the scan is not
// scoped to source files, or to any single directory. This is the
// mutation-check target for that arm. --

#[test]
fn the_scan_reaches_every_tracked_file_not_only_rust_source() {
    let token = format!("ghp_{}", "G".repeat(30));
    let repo = throwaway_tracked_repo(&[
        ("README.md", format!("{token}\n")),
        ("notes.txt", "nothing interesting here\n".to_string()),
        ("src/lib.rs", "// nothing here either\n".to_string()),
        ("docs/guide/setup.md", "unrelated setup notes\n".to_string()),
    ]);
    let violations = scan_tracked_files(repo.path());
    assert_eq!(
        violations.len(),
        1,
        "expected exactly the README.md violation, got {violations:?}"
    );
    assert_eq!(violations[0].file, PathBuf::from("README.md"));
}

// -- the shape boundary, pinned directly --

#[test]
fn body_shorter_than_the_shape_threshold_is_prose_not_a_credential() {
    let short = "A".repeat(MIN_BODY_LEN - 1);
    let line = format!("ghp_{short}");
    assert_eq!(find_prefix_match(&line), None);
}

#[test]
fn body_at_the_shape_threshold_is_treated_as_a_real_credential() {
    let exact = "A".repeat(MIN_BODY_LEN);
    let line = format!("ghp_{exact}");
    assert!(find_prefix_match(&line).is_some());
}

// -- the prose exemption, pinned with the actual sentences docs write --

#[test]
fn prose_naming_every_prefix_without_a_real_body_never_trips_the_guard() {
    let doc = "\
Never commit a real `ghp_` token, `github_pat_` token, `gho_` token, \
`ghs_` token, or `ghu_` token — paste it into the settings field, never \
into git.

Example header shape: `Authorization: Bearer <token>` — the value shown \
here is a placeholder, not a real credential.

A short example some docs still write out: ghp_YOUR_TOKEN_HERE. The \
underscores break any long alphanumeric run, so this never reaches the \
shape threshold regardless of how the sentence around it is worded.
";
    let repo = throwaway_tracked_repo(&[("docs/credentials.md", doc.to_string())]);
    let violations = scan_tracked_files(repo.path());
    assert!(
        violations.is_empty(),
        "prose naming prefixes must not trip the guard, got {violations:?}"
    );
}
