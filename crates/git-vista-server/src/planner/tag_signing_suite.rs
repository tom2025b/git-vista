//! M2.21d (#238) / M2.21e (#239, ADR 0048): the annotated/signed `git tag`
//! argv contract, and the signed-tag execution path's failure
//! classification — gpg status-protocol parsing, the timed-out recovery
//! read using the bounded primitive, and the real sandboxed spawn with no
//! usable key.

use super::*;
use std::path::PathBuf;

fn run(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed in {repo:?}"
    );
}

/// `git rev-parse HEAD` in `repo`, trimmed — for tests that need a real
/// oid to build a compare-and-swap `GitOperation` against (#222).
async fn git_rev_parse_head(repo: &Path) -> String {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A fresh repository on branch `main` with one committed file and a
/// clean working tree.
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

/// M2.21d (#238) / M2.21e (#239, ADR 0048): the argv `git tag` is
/// actually handed, over every shape the type system admits — now
/// including the signed one.
///
/// The claim being pinned is a *property*, not a spelling: **whenever
/// `-a` or `-s` appears, `-m <message>` appears too, and `--edit` never
/// appears at all**. `git tag -a`/`-s` with neither is what launches
/// `core.editor`, and there is no `--no-edit` on `git tag` to undo that
/// later — so this is the only place the guarantee can be checked before
/// a process exists. The behavioural half (nothing ever writes
/// `.git/TAG_EDITMSG`, and a blocking editor really would hang) is
/// `contract_suite::annotated_tag_creation_never_opens_an_editor`.
///
/// Note the shapes iterated: an annotated (signed or not) tag *is* a
/// `TagAnnotation`, which cannot hold an empty `TagMessage`, so
/// "annotated with no message" is not among them — that is the
/// guarantee, expressed as a type rather than as a check.
#[test]
fn a_tag_argv_never_asks_for_an_editor() {
    let name = TagName::new("v1.0.0").unwrap();
    let target = CommitOid::new("a".repeat(40)).unwrap();
    let annotations = [
        None,
        Some(TagAnnotation {
            message: git_vista_protocol::TagMessage::new("notes").unwrap(),
            sign: false,
        }),
        // A message that looks like an option: it must ride as `-m`'s
        // value, never as a flag of its own.
        Some(TagAnnotation {
            message: git_vista_protocol::TagMessage::new("--edit").unwrap(),
            sign: false,
        }),
        // M2.21e (#239): the signed shape.
        Some(TagAnnotation {
            message: git_vista_protocol::TagMessage::new("signed release").unwrap(),
            sign: true,
        }),
    ];
    for annotation in &annotations {
        let argv = create_tag_argv(&name, &target, annotation.as_ref());
        assert_eq!(argv[0], "tag");
        let signed = annotation.as_ref().is_some_and(|a| a.sign);
        // Scan *flag* positions only. The entry after `-m` is that
        // option's value, and git's own parser consumes it as one no
        // matter what it spells — which is precisely why an
        // option-shaped message is safe here and why passing the message
        // as its own argv entry (rather than glued into one) is the
        // whole defence. Scanning every entry blindly would flag the
        // message `--edit` as if it were a request for an editor; it is
        // not, and `contract_suite::annotated_tag_creation_never_opens_
        // an_editor` drives that exact message through real git to prove
        // it lands as text.
        let value_of_m = argv.iter().position(|x| *x == "-m").map(|i| i + 1);
        for (i, arg) in argv.iter().enumerate() {
            if Some(i) == value_of_m {
                continue;
            }
            assert!(
                !matches!(*arg, "--edit" | "-e"),
                "{argv:?} asks git to open an editor"
            );
            assert!(
                !matches!(*arg, "-f" | "--force"),
                "{argv:?} would repoint an existing tag past the plan's \
                     RefAbsent precondition"
            );
            // A signed request is expected to carry `-s`; only an
            // *unsigned* argv must never carry a signing flag.
            if !signed {
                assert!(
                    !matches!(*arg, "-s" | "--sign" | "-u" | "--local-user"),
                    "{argv:?} asks for a signature this request never requested"
                );
            }
        }
        match annotation.as_ref() {
            None => assert_eq!(argv, vec!["tag", "v1.0.0", &"a".repeat(40)]),
            Some(a) if a.sign => {
                assert_eq!(
                    argv.get(1),
                    Some(&"-s"),
                    "a signed create must ask git to sign: {argv:?}"
                );
                assert!(
                    !argv.contains(&"-a"),
                    "-s already implies -a; this pins the argv this function \
                         actually emits, not merely a git behavior: {argv:?}"
                );
                let dash_m = argv
                        .iter()
                        .position(|x| *x == "-m")
                        .expect("a signed tag is annotated by definition — -s with no -m is exactly the editor case");
                assert_eq!(argv.get(dash_m + 1).copied(), Some(a.message.as_str()));
                assert_eq!(argv[argv.len() - 2..], ["v1.0.0", &"a".repeat(40)]);
            }
            Some(a) => {
                let dash_a = argv
                    .iter()
                    .position(|x| *x == "-a")
                    .expect("an annotated create must say -a");
                let dash_m = argv
                    .iter()
                    .position(|x| *x == "-m")
                    .expect("…and -a with no -m is exactly the editor case");
                assert!(dash_m > dash_a, "{argv:?}");
                assert_eq!(
                    argv.get(dash_m + 1).copied(),
                    Some(a.message.as_str()),
                    "the message must be -m's own argv entry, so an \
                         option-shaped message can never be read as a flag"
                );
                // The name and the target still follow, in that order.
                assert_eq!(argv[argv.len() - 2..], ["v1.0.0", &"a".repeat(40)]);
            }
        }
    }
}

// -------------------------------------------------------------------
// M2.21e (#239): signed tag execution
// -------------------------------------------------------------------

/// [`classify_sign_failure`] over fixtures captured from a real gpg 2.4.4
/// run (this host), plus synthetic status lines built from
/// libgpg-error's own numbering so the libassuan IPC range is covered
/// without needing to reproduce every specific failure by hand.
#[test]
fn classify_sign_failure_reads_the_gnupg_status_protocol_not_prose() {
    // Measured: `git tag -s` against a repo with an empty, keyless
    // GNUPGHOME prints `INV_SGNR` with no later `FAILURE` line on some
    // gpg versions.
    assert_eq!(
        classify_sign_failure(
            "[GNUPG:] INV_SGNR 9 nokey\ngpg: skipped \"nokey\": No secret key",
            true
        ),
        SignTagFailureKind::NoSecretKey
    );
    // The `FAILURE sign 17` shape (GPG_ERR_NO_SECKEY, masked from a
    // larger source-tagged code the same way `67108941 & 0xFFFF == 77`
    // below is).
    assert_eq!(
        classify_sign_failure("[GNUPG:] FAILURE sign 17", true),
        SignTagFailureKind::NoSecretKey
    );
    // 67108941 = (4 << 24) | 77 — an arbitrary non-zero source tag (real
    // gpg-error codes carry the erroring component in bits 24+) over
    // GPG_ERR_NO_AGENT, masked back down to 77 by `& 0xFFFF`.
    assert_eq!(
        classify_sign_failure("[GNUPG:] FAILURE sign 67108941", true),
        SignTagFailureKind::AgentUnreachable
    );
    // 67109123 = (4 << 24) | 259 (ASS_CONNECT_FAILED), inside the
    // 257..=281 libassuan range.
    assert_eq!(
        classify_sign_failure("[GNUPG:] FAILURE sign 67109123", true),
        SignTagFailureKind::AgentUnreachable
    );
    // An unmapped code must not be guessed into one of the named cases.
    assert_eq!(
        classify_sign_failure("[GNUPG:] FAILURE sign 99999999", true),
        SignTagFailureKind::Other
    );
    // No status line and no gpg on PATH: the one case this function
    // needs `gpg_on_path` to disambiguate.
    assert_eq!(
        classify_sign_failure("exec: gpg: command not found", false),
        SignTagFailureKind::GpgNotInstalled
    );
    // No status line but gpg IS on PATH, and stderr is genuinely
    // non-empty and unrecognised: real content this classifier could
    // not place, so it must stay Other rather than being guessed into
    // one of the named cases.
    assert_eq!(
        classify_sign_failure("fatal: some other git failure entirely", true),
        SignTagFailureKind::Other
    );
    // No status line, gpg on PATH, and stderr is EMPTY — the one case a
    // real invocation cannot produce on its own (measured: even a gpg
    // that fails on a missing/permission-denied GNUPGHOME emits status
    // lines before failing). Empty output means gpg was stopped before
    // it could run its protocol engine at all, which is the same shape
    // AgentUnreachable already names, so this must not fall to the
    // useless Other message on hosts where ~/.gnupg never existed.
    assert_eq!(
        classify_sign_failure("", true),
        SignTagFailureKind::AgentUnreachable
    );
    assert_eq!(
        classify_sign_failure("   \n  ", true),
        SignTagFailureKind::AgentUnreachable
    );
}

/// Mutation-shaped guard: swapping the `17` arm for `AgentUnreachable`
/// must be distinguishable from the real thing — the two closed-set
/// reasons must not collapse into each other from this fixture alone.
#[test]
fn classify_sign_failure_distinguishes_no_secret_key_from_agent_unreachable() {
    let no_key = classify_sign_failure("[GNUPG:] FAILURE sign 17", true);
    let no_agent = classify_sign_failure("[GNUPG:] FAILURE sign 67108941", true);
    assert_ne!(no_key, no_agent);
    assert_eq!(no_key, SignTagFailureKind::NoSecretKey);
    assert_eq!(no_agent, SignTagFailureKind::AgentUnreachable);
}

/// Byte-census guard on [`run_signed_tag`]'s `TimedOut` arm, in the
/// style `offline_guard_audit.rs` and `route_authz.rs` already use for a
/// fact a runtime test cannot cheaply reach: an integration test that
/// forces BOTH the signing spawn and the recovery read to hang would
/// need to arm a hang on the same repository's git config twice without
/// the first arming also blocking the earlier `git tag -s` invocation —
/// fragile to build reliably. What this pins instead: the recovery read
/// inside the `TimedOut` arm calls the bounded primitive, not the plain
/// one. [`crate::git_cmd::git_output_bounded`]'s own bound is already
/// proven against a real hung spawn by
/// `git_output_bounded_reports_timed_out_when_the_bound_is_too_tight`
/// in `git_cmd.rs`; this test's job is only to prove `run_signed_tag`
/// still calls that primitive rather than the unbounded
/// `rev_parse_ref_unpeeled` a future edit could revert to.
#[test]
fn the_timed_out_arms_recovery_read_uses_the_bounded_primitive() {
    const PLANNER_SRC: &str = include_str!("../planner.rs");
    let start = PLANNER_SRC
        .find("async fn run_signed_tag(")
        .expect("run_signed_tag must exist in planner.rs — this test's own anchor moved");
    let end = start
        + PLANNER_SRC[start..]
            .find("\n/// Map a failed signing spawn's stderr")
            .expect("run_signed_tag's end anchor (classify_sign_failure's doc) moved");
    let body = &PLANNER_SRC[start..end];

    assert!(
        body.contains("BoundedOutput::TimedOut) => {"),
        "run_signed_tag must still have a TimedOut arm — this test's premise moved"
    );
    let timed_out_arm_start = body
        .find("BoundedOutput::TimedOut) => {")
        .expect("checked above");
    let recovery_read_region = &body[timed_out_arm_start..];

    assert!(
        recovery_read_region.contains("git_output_bounded(")
            && recovery_read_region.matches("git_output_bounded(").count() >= 1,
        "the TimedOut arm's recovery read must call git_output_bounded — the plain \
             git_output/rev_parse_ref_unpeeled path has no kill_on_drop, so a hung repo turns \
             a bounded signing failure into an unbounded recovery read while the mutation \
             guard is still held. Body:\n{recovery_read_region}"
    );
    assert!(
        !recovery_read_region.contains("rev_parse_ref_unpeeled("),
        "the TimedOut arm must not call the unbounded rev_parse_ref_unpeeled directly — \
             that is precisely the regression this test exists to catch"
    );
}

/// The end-to-end path, against the **real sandboxed spawn** — not a
/// mock — with no usable key. `~/.gnupg` is Landlock-excluded under the
/// Strict tier regardless of what that directory holds
/// (`sandbox::DEFAULT_SECRET_EXCLUDES`), so this test needs no
/// throwaway `GNUPGHOME` at all: the sandbox itself is what makes the
/// key unreachable, on any host, always — which is precisely the
/// property `run_signed_tag`'s doc comment argues for.
///
/// The outer `tokio::time::timeout` is the test's own bound, strictly
/// larger than [`SIGN_TIMEOUT`]: if `exec_create_tag` ever regressed to
/// an actual hang, this test must still fail loudly in bounded time
/// rather than wedging the suite alongside the defect it exists to
/// catch.
#[tokio::test]
async fn a_signing_attempt_with_no_usable_key_fails_fast_with_a_typed_reason() {
    let (_dir, repo) = seeded_repo();
    let target = CommitOid::new(git_rev_parse_head(&repo).await).unwrap();
    let name = TagName::new("v-signed").unwrap();
    let annotation = TagAnnotation {
        message: git_vista_protocol::TagMessage::new("release").unwrap(),
        sign: true,
    };

    let started = std::time::Instant::now();
    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        exec_create_tag(&repo, NetworkNeed::Local, &name, &target, Some(&annotation)),
    )
    .await
    .expect(
        "exec_create_tag must return within 20s on its own — its own bound is 10s; a \
             hang here is exactly the defect this test exists to catch",
    );
    let elapsed = started.elapsed();

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a signing failure must be a refusal, not a 5xx or a raw pass-through: {body}"
    );
    let parsed: SignTagError = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "signing failure body must be the typed SignTagError, not raw text: {e}\nbody={body}"
        )
    });
    // Not TimedOut: that would mean it failed slow, not fast, which is
    // the one outcome this test exists to rule out. Beyond that, the
    // SPECIFIC bucket a keyless attempt lands in is genuinely
    // host-dependent — measured directly on three separate hosts.
    // Locally (gpg 2.4.4, `~/.gnupg` present but Landlock-withheld):
    // gpg's own lock/keydb lookups fail with permission errors and an
    // `INV_SGNR` status line — `NoSecretKey`. On this project's CI
    // runner: git's own wrapper reports "unable to sign the tag" with
    // no `[GNUPG:]` status line reaching stderr at all, which
    // `classify_sign_failure` correctly falls through to `Other` for —
    // correctly, because prose-matching "unable to sign the tag" would
    // repeat the exact anti-pattern (matching gettext-translated,
    // version-varying text) the whole status-fd approach exists to
    // avoid. An earlier version of this assertion excluded `Other`
    // entirely; CI proved that too strict, not the classifier wrong.
    //
    // So this test asserts what genuinely holds on every host instead:
    // never TimedOut (fails fast, not via the backstop), never a raw
    // gpg/git stderr dump reaching the client, and a message that names
    // an actual reason rather than being empty. The specific-bucket
    // assertion moved to unit tests with controlled input
    // (`classify_sign_failure_reads_the_gnupg_status_protocol_not_prose`),
    // where the classifier's mapping can be pinned exactly without
    // depending on what a given host's gpg happens to print.
    assert_ne!(
        parsed.kind,
        SignTagFailureKind::TimedOut,
        "a keyless signing attempt must fail fast, not via the timeout backstop: {}",
        parsed.message
    );
    assert!(
        !parsed.message.contains("[GNUPG:]") && !parsed.message.to_lowercase().contains("pinentry"),
        "the refusal message must never contain raw gpg status-fd or pinentry text — it \
             must be this server's own typed prose: {}",
        parsed.message
    );
    assert!(
        !parsed.message.is_empty(),
        "the refusal message must name an actual reason, not be empty"
    );
    assert!(
        elapsed < SIGN_TIMEOUT,
        "a keyless signing failure took {elapsed:?}, at or past the {SIGN_TIMEOUT:?} \
             bound — it should fail fast, not via the timeout backstop"
    );
    assert!(
        !parsed.message.contains("gpg:")
            && !parsed.message.contains("[GNUPG:]")
            && !parsed.message.contains("GNUPGHOME"),
        "the client-facing message must never carry raw gpg/git status output: {}",
        parsed.message
    );

    // The failed attempt must not have left a tag behind.
    let exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "refs/tags/v-signed"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success();
    assert!(
        !exists,
        "a failed signing attempt must not leave a tag behind"
    );
}
