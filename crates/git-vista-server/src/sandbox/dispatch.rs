//! M1.13b (#66) Task 8: tests for the tier-dispatch classifier.
//!
//! The dispatch is the "must not be wrong" decision — which operation runs with
//! no sandbox — so it is tested for the failure directions, not just the happy
//! path.

use super::*;

#[test]
fn remote_subcommands_need_the_network() {
    for sub in [
        "push", "fetch", "clone", "ls-remote", "pull",
        // plumbing / helpers added after the C10 audit
        "fetch-pack", "send-pack", "http-fetch", "http-push",
    ] {
        assert_eq!(
            network_need(&[sub, "origin"]),
            NetworkNeed::Remote,
            "`git {sub}` reaches a remote"
        );
    }
}

/// The C10 audit's list of network-capable commands the argv classifier still
/// misses. These fail closed to `Local`/Strict, which *breaks* the network
/// attempt rather than granting it — the safe direction. This test documents
/// the known gap so the day someone moves classification to the typed operation
/// model, these are the cases to cover. It asserts the *current* fail-closed
/// behaviour, not that the gap is fixed.
#[test]
fn known_network_gaps_fail_closed_to_local_not_unsandboxed() {
    for args in [
        vec!["remote", "update"],
        vec!["submodule", "update", "--remote"],
        vec!["maintenance", "run", "--task=prefetch"],
        vec!["credential", "fill"],
    ] {
        let need = network_need(&args);
        assert_eq!(need, NetworkNeed::Local, "documented fail-closed gap: {args:?}");
        assert_ne!(
            tier_for(need, false),
            Tier::Unsandboxed,
            "even a misclassified network command must never be unsandboxed: {args:?}"
        );
    }
}

#[test]
fn local_subcommands_do_not_need_the_network() {
    for sub in [
        "status", "commit", "add", "reset", "checkout", "merge", "branch", "rev-parse",
        "merge-base", "diff", "log", "cat-file", "config", "update-ref", "commit-tree",
        "bundle", "stash", "reflog",
    ] {
        assert_eq!(
            network_need(&[sub, "--whatever"]),
            NetworkNeed::Local,
            "`git {sub}` is local"
        );
    }
}

/// `git remote get-url` looks network-adjacent but only reads `.git/config`.
/// Misclassifying it as `Remote` would be harmless, but proving it is `Local`
/// documents the distinction the code comment claims.
#[test]
fn remote_config_subcommands_are_local_not_networked() {
    assert_eq!(network_need(&["remote", "get-url", "origin"]), NetworkNeed::Local);
    assert_eq!(network_need(&["remote", "add", "origin", "url"]), NetworkNeed::Local);
    assert_eq!(network_need(&["remote", "-v"]), NetworkNeed::Local);
}

/// Leading global flags (`-C <path>`, `-c k=v`) must not be mistaken for the
/// subcommand. A hostile *repository* cannot inject these onto the server's
/// argv, but the classifier is robust to them regardless.
#[test]
fn leading_global_flags_are_skipped_to_find_the_subcommand() {
    assert_eq!(
        network_need(&["-C", "/srv/repo", "push", "origin"]),
        NetworkNeed::Remote
    );
    assert_eq!(
        network_need(&["-c", "http.proxy=x", "-C", "/srv/repo", "status"]),
        NetworkNeed::Local
    );
    // A `-c` that tries to look like `push` as its *value* must not be read as
    // the subcommand.
    assert_eq!(
        network_need(&["-c", "alias.x=push", "status"]),
        NetworkNeed::Local
    );
}

#[test]
fn an_empty_or_flags_only_argv_is_local() {
    assert_eq!(network_need(&[]), NetworkNeed::Local);
    assert_eq!(network_need(&["--version"]), NetworkNeed::Local);
    assert_eq!(network_need(&["--help"]), NetworkNeed::Local);
}

/// An unknown subcommand fails **closed** to `Local`/`Strict`. A network op
/// wrongly given Strict breaks loudly; a local op wrongly given Network merely
/// over-permits. The dangerous direction — silently gaining access — is the one
/// this default forecloses.
#[test]
fn an_unknown_subcommand_fails_closed_to_local() {
    assert_eq!(network_need(&["some-new-porcelain"]), NetworkNeed::Local);
}

/// C10's strongest (failed) escalation attempt, kept as a regression guard.
/// `git -c alias.x=push x origin` expands `x` to `push` and runs it, so the
/// classifier's name-based view (`x` is unknown → `Local`) disagrees with what
/// git executes. The security property that matters survives regardless: with
/// `trusted=false` this can never reach `Unsandboxed`. The *availability*
/// consequence — the hidden push runs under Strict and fails — is the
/// intended fail-closed direction, not a hole.
#[test]
fn an_injected_alias_can_never_reach_unsandboxed() {
    let args = ["-c", "alias.x=push", "x", "origin"];
    let need = network_need(&args);
    // The name-based classifier sees `x`, an unknown subcommand → Local. That is
    // the documented fail-closed behaviour, asserted so a future change to it is
    // deliberate.
    assert_eq!(need, NetworkNeed::Local, "an unknown alias name classifies Local");
    // The property that must hold no matter how classification lands:
    assert_ne!(
        tier_for(need, false),
        Tier::Unsandboxed,
        "an injected alias must never escalate an untrusted repo to no-sandbox"
    );
}

// -------------------------------------------------------------------------
// tier_for — the accidental-Unsandboxed guard
// -------------------------------------------------------------------------

/// The property the whole design rests on: an **untrusted** repository can
/// never reach `Unsandboxed`, for any operation. If this ever fails, a hostile
/// repository is one classification bug away from running with no sandbox.
#[test]
fn an_untrusted_repo_is_never_unsandboxed_for_any_operation() {
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_ne!(
            tier_for(need, false),
            Tier::Unsandboxed,
            "untrusted repos must never be unsandboxed (need={need:?})"
        );
    }
}

/// Unsandboxed is reachable *only* through the trust flag — and then for every
/// operation, because trust is a property of the repository, not the operation.
#[test]
fn unsandboxed_is_reachable_only_through_the_trust_flag() {
    assert_eq!(tier_for(NetworkNeed::Local, true), Tier::Unsandboxed);
    assert_eq!(tier_for(NetworkNeed::Remote, true), Tier::Unsandboxed);
}

#[test]
fn untrusted_dispatch_is_strict_for_local_and_network_for_remote() {
    assert_eq!(tier_for(NetworkNeed::Local, false), Tier::Strict);
    assert_eq!(tier_for(NetworkNeed::Remote, false), Tier::Network);
}

/// Pin the actual production tier, not a local `let trusted = false` (which
/// would pass even if the real caller used `true` — the C10 audit flagged the
/// earlier version of this test as vacuous for exactly that reason). This
/// exercises the real `policy_for_repo` and asserts the tier it hands out is
/// never `Unsandboxed`. When Task 8's dispatch is wired in, this test must be
/// updated to also cover the Strict/Network split — but the "never Unsandboxed
/// without an explicit trust flag" property must survive that change.
#[test]
fn the_production_policy_is_never_unsandboxed_today() {
    let repo = tempfile::tempdir().expect("tempdir");
    let policy = super::policy_for_repo(repo.path())
        .expect("policy builds (shim present via tests/forces_shim_build.rs)");
    assert_ne!(
        policy.tier,
        Tier::Unsandboxed,
        "no repository may be unsandboxed until an explicit persisted trust flag exists"
    );
}
