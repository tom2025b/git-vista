//! M1.13b (#66) Task 8: tests for the tier-dispatch classifier.
//!
//! The dispatch is the "must not be wrong" decision — which operation runs with
//! no sandbox — so it is tested for the failure directions, not just the happy
//! path.

use super::*;

#[test]
fn remote_subcommands_need_the_network() {
    for sub in [
        "push",
        "fetch",
        "clone",
        "ls-remote",
        "pull",
        // plumbing / helpers added after the C10 audit
        "fetch-pack",
        "send-pack",
        "http-fetch",
        "http-push",
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
        assert_eq!(
            need,
            NetworkNeed::Local,
            "documented fail-closed gap: {args:?}"
        );
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
        "status",
        "commit",
        "add",
        "reset",
        "checkout",
        "merge",
        "branch",
        "rev-parse",
        "merge-base",
        "diff",
        "log",
        "cat-file",
        "config",
        "update-ref",
        "commit-tree",
        "bundle",
        "stash",
        "reflog",
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
    assert_eq!(
        network_need(&["remote", "get-url", "origin"]),
        NetworkNeed::Local
    );
    assert_eq!(
        network_need(&["remote", "add", "origin", "url"]),
        NetworkNeed::Local
    );
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
    assert_eq!(
        need,
        NetworkNeed::Local,
        "an unknown alias name classifies Local"
    );
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
/// never `Unsandboxed`.
///
/// Task 8 wired the Strict/Network split in; the additional coverage that asked
/// for lives below (`a_local_operation_gets_the_strict_tier_with_no_ports`,
/// `a_remote_operation_gets_the_network_tier_with_the_git_ports`,
/// `an_untrusted_repository_can_never_be_unsandboxed`). This test is kept as
/// written because it now pins a second thing: `policy_for_repo` is the entry
/// point `escape_contract::policy_for_case` calls, and the ten Network-tier
/// battery cases depend on it staying non-`Unsandboxed`.
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

// ---------------------------------------------------------------------------
// Task 8 / D3: the declared-intent dispatch, wired to production
// ---------------------------------------------------------------------------
//
// Everything above this line tests the two pure classifiers in isolation.
// Everything below tests the thing Task 8 actually changed: that the classifier
// answers now *reach* `policy_for`, that trust is consulted, and that the
// failure directions fail closed. The negative cases are the point — a green
// "local gets Strict" proves very little on its own, since a `policy_for` that
// returned Strict unconditionally would also pass it.

use git_vista_protocol::{BranchName, CommitMessage, CommitOid, GitOperation, RefName, RemoteName};

fn branch(s: &str) -> BranchName {
    BranchName::new(s).expect("valid branch name")
}
fn oid(s: &str) -> CommitOid {
    CommitOid::new(s).expect("valid oid")
}

/// One value of every `GitOperation` variant, so the classifier test below is
/// exhaustive in fact and not only in intent.
///
/// This deliberately does **not** use a wildcard or a macro: adding a variant
/// makes `network_need_for_operation` fail to compile, and this list is what
/// then makes the *test* fail to be exhaustive in a way a reviewer notices —
/// the count assertion below is what turns "somebody added a variant and only
/// fixed the match" into a red test.
fn every_operation() -> Vec<GitOperation> {
    let tip = "1111111111111111111111111111111111111111";
    vec![
        GitOperation::CreateBranch {
            name: branch("feature"),
            at: oid(tip),
        },
        GitOperation::CommitOnHead {
            message: CommitMessage::new("msg").expect("valid message"),
            allow_empty: false,
        },
        GitOperation::EmptyCommitOnBranch {
            branch: branch("feature"),
            message: CommitMessage::new("msg").expect("valid message"),
            expected_tip: oid(tip),
        },
        GitOperation::StageAll,
        GitOperation::UnstageAll,
        GitOperation::CheckoutBranch {
            branch: branch("feature"),
        },
        GitOperation::MergeBranch {
            branch: branch("feature"),
        },
        GitOperation::PushBranch {
            branch: branch("feature"),
            remote: RemoteName::new("origin").expect("valid remote"),
        },
        GitOperation::DeleteBranch {
            branch: branch("feature"),
        },
        GitOperation::ForceDeleteBranch {
            branch: branch("feature"),
        },
        GitOperation::RebaseOntoBase {
            base: RefName::new("refs/heads/main").expect("valid ref"),
        },
        GitOperation::RestoreBranch {
            name: branch("feature"),
            tip: oid(tip),
        },
        GitOperation::ResetBranch {
            branch: branch("feature"),
            to: oid(tip),
            expected_tip: oid(tip),
        },
        GitOperation::RevertCommit { commit: oid(tip) },
        GitOperation::ResetTestRepo,
    ]
}

/// Exactly one operation in the enum reaches a remote, and it is `PushBranch`.
///
/// The negative half is what matters: fourteen operations must be `Local`, so a
/// future edit that classified, say, `MergeBranch` as `Remote` to "be safe"
/// would be caught here. Widening is not safe — it moves an operation from the
/// no-network Strict tier into a tier with outbound TCP on four ports.
#[test]
fn exactly_one_operation_declares_a_network_need() {
    let ops = every_operation();
    assert_eq!(
        ops.len(),
        15,
        "every_operation() must list every GitOperation variant; the enum has 15"
    );
    let mut remote = Vec::new();
    for op in &ops {
        match network_need_for_operation(op) {
            NetworkNeed::Remote => remote.push(format!("{op:?}")),
            NetworkNeed::Local => {}
        }
    }
    assert_eq!(
        remote.len(),
        1,
        "exactly one operation may declare Remote; declared: {remote:?}"
    );
    assert!(
        remote[0].starts_with("PushBranch"),
        "the one Remote operation must be PushBranch, not {}",
        remote[0]
    );
}

/// The declaration is what picks the tier, and the *stated* argv of each
/// operation must agree with it — this is the cross-check's own premise, tested
/// on the real argv the planner builds for the one remote operation.
#[test]
fn the_push_declaration_and_the_push_argv_agree() {
    assert_eq!(
        network_need(&["push", "origin", "feature"]),
        NetworkNeed::Remote,
        "the argv classifier must agree with the PushBranch declaration, or \
         every push would trip the D3 cross-check"
    );
}

// --- the cross-check (D3) --------------------------------------------------

/// The tolerated direction: an argv the incomplete `REMOTE_SUBCOMMANDS` list
/// does not recognise must never pull a `Remote` declaration down to `Local`.
/// Narrowing here would take the network away from an operation that declared
/// it needs the network, on the word of a list documented as incomplete.
#[test]
fn the_cross_check_never_narrows_a_remote_declaration() {
    for args in [
        vec!["remote", "update"],
        vec!["submodule", "update", "--remote"],
        vec!["status", "--porcelain"],
        vec![],
    ] {
        assert_eq!(
            reconcile_need(NetworkNeed::Remote, &args),
            NetworkNeed::Remote,
            "a declared Remote must survive argv {args:?}"
        );
    }
}

/// The empty-argv hazard D3 named explicitly: `network_need(&[])` is `Local`,
/// which routes to Strict. Before Task 8 nothing in production called it, so
/// that was latent; now that the tier is live, the guarantee is that an empty
/// argv cannot *decide* anything — the declaration does.
#[test]
fn an_empty_argv_cannot_move_the_tier() {
    assert_eq!(
        network_need(&[]),
        NetworkNeed::Local,
        "documented behaviour"
    );
    assert_eq!(
        reconcile_need(NetworkNeed::Remote, &[]),
        NetworkNeed::Remote
    );
    assert_eq!(reconcile_need(NetworkNeed::Local, &[]), NetworkNeed::Local);
}

/// Agreement is a no-op in both directions.
#[test]
fn the_cross_check_passes_agreeing_pairs_through() {
    assert_eq!(
        reconcile_need(NetworkNeed::Local, &["status", "--porcelain"]),
        NetworkNeed::Local
    );
    assert_eq!(
        reconcile_need(NetworkNeed::Remote, &["push", "origin"]),
        NetworkNeed::Remote
    );
}

/// The disagreement that is a server bug: declared `Local`, argv starts with a
/// known remote subcommand. In a debug build this must be *loud*, because a
/// developer meeting it has written a mismatch between
/// `network_need_for_operation` and the argv their `exec_*` builds.
///
/// `debug_assert!` compiles away in release, where the documented behaviour is
/// "log and keep the stricter tier". That half is asserted structurally by
/// `the_cross_check_keeps_the_stricter_tier_on_mismatch` below rather than by
/// running a release build from a debug test.
#[test]
#[should_panic(expected = "cross-check")]
fn a_local_declaration_with_a_remote_argv_panics_in_debug() {
    let _ = reconcile_need(NetworkNeed::Local, &["push", "origin", "main"]);
}

/// The release behaviour, stated as the property that makes it safe: the value
/// `reconcile_need` would return on a mismatch is the declared `Local`, and
/// `tier_for` maps that to `Strict` — the tier with **no** network at all,
/// which is stricter than the `Network` tier the argv argued for. So the
/// mismatch fails closed: a genuinely-remote command mislabelled `Local` gets
/// `EACCES` on `connect()` and says so, rather than silently gaining a socket.
#[test]
fn the_cross_check_keeps_the_stricter_tier_on_mismatch() {
    assert_eq!(
        tier_for(NetworkNeed::Local, false),
        Tier::Strict,
        "the value kept on a mismatch must route to the stricter tier"
    );
    assert_ne!(
        tier_for(NetworkNeed::Local, false),
        tier_for(NetworkNeed::Remote, false),
        "if these were the same tier the cross-check would be decorative"
    );
}

// --- INV-13 / ADR 0029: Strict is refused, never downgraded -----------------

fn caps(landlock_abi: i32, bwrap_present: bool, userns: bool) -> capabilities::Capabilities {
    capabilities::Capabilities {
        landlock_abi,
        bwrap_present,
        userns,
        seccomp_available: true,
    }
}

/// Every single missing capability refuses, and names itself. The assertion
/// that matters is `is_err()`: the alternatives ADR 0029 rejects — returning a
/// `Network` policy, or a `Strict` policy with hooks blocked — are both `Ok`,
/// so a regression to either fails here rather than shipping a quietly weaker
/// sandbox.
#[test]
fn strict_refuses_and_names_the_capability_when_the_host_cannot_supply_it() {
    let launcher = Some(PathBuf::from("/usr/bin/bwrap"));
    for (label, c, expect) in [
        ("no landlock", caps(-1, true, true), "landlock_abi>=6"),
        (
            "landlock below floor",
            caps(LANDLOCK_ABI_FLOOR as i32 - 1, true, true),
            "landlock_abi>=6",
        ),
        ("no userns", caps(8, true, false), "user_namespaces"),
        ("no bwrap", caps(8, false, true), "bwrap"),
    ] {
        let got = strict_launcher(&c, launcher.clone());
        match got {
            Err(shim::ShimError::StrictUnavailable { missing }) => {
                assert!(
                    missing.contains(&expect),
                    "{label}: the refusal must name `{expect}`, got {missing:?}"
                );
            }
            other => {
                panic!("{label}: INV-13 requires a named refusal, never a degrade — got {other:?}")
            }
        }
    }
}

/// A host with every capability but no launcher at a reviewed absolute path
/// still cannot run the tier, and the refusal must not be empty-handed.
#[test]
fn strict_refuses_with_a_named_reason_when_only_the_launcher_is_absent() {
    match strict_launcher(&caps(8, true, true), None) {
        Err(shim::ShimError::StrictUnavailable { missing }) => {
            assert!(
                !missing.is_empty(),
                "a refusal that names nothing tells the operator nothing"
            );
        }
        other => panic!("expected a named refusal, got {other:?}"),
    }
}

/// The refusal's own text must point at the decision, not just at the symptom —
/// an operator who sees it needs to know this is deliberate.
#[test]
fn the_strict_refusal_explains_itself() {
    let e = shim::ShimError::StrictUnavailable {
        missing: vec!["bwrap"],
    };
    let text = e.to_string();
    assert!(text.contains("bwrap"), "names what is missing: {text}");
    assert!(text.contains("ADR 0029"), "cites the decision: {text}");
    assert!(
        text.contains("refused"),
        "says the operation is refused, not degraded: {text}"
    );
}

/// A host that *can* supply the tier gets the launcher back unchanged.
/// Without this leg the refusal tests above would pass on a `strict_launcher`
/// that refused unconditionally.
#[test]
fn strict_is_granted_on_a_capable_host() {
    let launcher = PathBuf::from("/usr/bin/bwrap");
    assert_eq!(
        strict_launcher(&caps(8, true, true), Some(launcher.clone())),
        Ok(launcher)
    );
}

// --- the production policy, end to end -------------------------------------

/// A local operation on an untrusted repository is `Strict`, with the tier's
/// whole shape: a resolved bwrap launcher (there is no strict tier without the
/// namespaces) and **no** network ports (F3 — `--net-deny`).
#[test]
fn a_local_operation_gets_the_strict_tier_with_no_ports() {
    let repo = tempfile::tempdir().expect("tempdir");
    let policy = policy_for(repo.path(), false, NetworkNeed::Local)
        .expect("policy builds (shim present via tests/forces_shim_build.rs)");
    assert_eq!(policy.tier, Tier::Strict);
    assert!(
        policy.bwrap.is_some(),
        "a Strict policy without a launcher would panic in `shim_argv`"
    );
    assert!(
        policy.net_ports.is_empty(),
        "the strict tier denies the network outright; ports there would be an \
         argv that contradicts itself"
    );
}

/// A remote operation is `Network`: no bwrap (its namespace breaks push, F3),
/// and the git ports present.
#[test]
fn a_remote_operation_gets_the_network_tier_with_the_git_ports() {
    let repo = tempfile::tempdir().expect("tempdir");
    let policy = policy_for(repo.path(), false, NetworkNeed::Remote)
        .expect("policy builds (shim present via tests/forces_shim_build.rs)");
    assert_eq!(policy.tier, Tier::Network);
    assert_eq!(policy.bwrap, None);
    assert_eq!(policy.net_ports, DEFAULT_GIT_PORTS.to_vec());
}

/// The property the whole dispatch rests on, asserted against the *production*
/// constructor rather than against `tier_for` with a local `let trusted =
/// false` (the vacuity the C10 audit flagged): an untrusted repository cannot
/// reach `Unsandboxed` for any need, and the secret set is never empty in
/// whichever tier it does reach.
#[test]
fn an_untrusted_repository_can_never_be_unsandboxed() {
    let repo = tempfile::tempdir().expect("tempdir");
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        let policy = policy_for(repo.path(), false, need).expect("policy builds");
        assert_ne!(
            policy.tier,
            Tier::Unsandboxed,
            "an untrusted repository must never be unsandboxed (need={need:?})"
        );
        assert!(
            !policy.secret_excludes.is_empty(),
            "the secret set must never be silently empty (need={need:?})"
        );
    }
}

/// Revokes on drop, so a panicking assertion cannot leave a real trust marker
/// behind in `~/.local/state/git-vista/trusted-repos`.
struct TrustGuard(PathBuf);

impl Drop for TrustGuard {
    fn drop(&mut self) {
        let _ = trust::revoke(&self.0);
    }
}

/// `sandbox::trust`'s first production consumer, tested through the production
/// constructor: a granted repository reaches `Unsandboxed` for *every* need
/// (trust is a property of the repository, not the operation), and revoking
/// takes it straight back.
///
/// The before/after legs are both required. Without the "before" leg this would
/// pass on a `policy_for` that always returned `Unsandboxed`; without the
/// "after" leg it would pass on one that never consulted `revoke`.
#[test]
fn an_operator_granted_repository_is_unsandboxed_and_revoke_takes_it_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let canonical = dir.path().canonicalize().expect("canonicalize");

    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_ne!(
            policy_for(dir.path(), false, need)
                .expect("policy builds")
                .tier,
            Tier::Unsandboxed,
            "before the grant, nothing may be unsandboxed"
        );
    }

    let guard = TrustGuard(canonical.clone());
    trust::grant(&canonical).expect("grant writes a marker");
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_eq!(
            policy_for(dir.path(), false, need)
                .expect("policy builds")
                .tier,
            Tier::Unsandboxed,
            "an operator-trusted repository runs unsandboxed for every need"
        );
    }

    drop(guard);
    for need in [NetworkNeed::Local, NetworkNeed::Remote] {
        assert_ne!(
            policy_for(dir.path(), false, need)
                .expect("policy builds")
                .tier,
            Tier::Unsandboxed,
            "revoking trust must take the sandbox back immediately"
        );
    }
}

/// Trust is keyed by canonical path, and this is the failure direction that
/// matters: a marker granted for one repository must not trust a *different*
/// one. A hash-collision or a prefix-match implementation would fail here.
#[test]
fn a_grant_for_one_repository_does_not_trust_its_neighbour() {
    let granted = tempfile::tempdir().expect("tempdir");
    let other = tempfile::tempdir().expect("tempdir");
    let canonical = granted.path().canonicalize().expect("canonicalize");
    let guard = TrustGuard(canonical.clone());
    trust::grant(&canonical).expect("grant");

    assert_ne!(
        policy_for(other.path(), false, NetworkNeed::Local)
            .expect("policy builds")
            .tier,
        Tier::Unsandboxed,
        "a grant must not leak to a repository the operator never named"
    );
    drop(guard);
}

/// A path that does not exist cannot be canonicalised, and every uncertainty in
/// the trust chain means untrusted. Fail-closed, asserted rather than assumed.
#[test]
fn an_unresolvable_path_is_never_trusted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-repo");
    assert!(!repo_is_trusted(&missing));
}
