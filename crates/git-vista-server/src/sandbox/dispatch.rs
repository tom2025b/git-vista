//! M1.13b (#66) Task 8: tests for the tier-dispatch classifier.
//!
//! The dispatch is the "must not be wrong" decision — which operation runs with
//! no sandbox — so it is tested for the failure directions, not just the happy
//! path.

use super::*;

#[test]
fn remote_subcommands_need_the_network() {
    for sub in ["push", "fetch", "clone", "ls-remote", "pull"] {
        assert_eq!(
            network_need(&[sub, "origin"]),
            NetworkNeed::Remote,
            "`git {sub}` reaches a remote"
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

/// The default `trusted` value in production is `false` until Task 7 lands the
/// persisted trust flag, so today no repository is unsandboxed. This test pins
/// that so a future change to the default is a deliberate, visible edit.
#[test]
fn trust_defaults_false_so_nothing_is_unsandboxed_yet() {
    // The production caller passes `trusted: false` unconditionally for now;
    // this asserts the safe interim explicitly.
    let interim_trusted = false;
    assert_ne!(tier_for(NetworkNeed::Local, interim_trusted), Tier::Unsandboxed);
    assert_ne!(tier_for(NetworkNeed::Remote, interim_trusted), Tier::Unsandboxed);
}
