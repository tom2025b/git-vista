//! The Push / force-with-lease confirmation copy tests (#233, M2.20g):
//! `RemoteTipKnowledge` read off a plan response, and `push_confirm_copy`'s
//! danger/body/label composition. Extracted verbatim from `push_confirm_tests`
//! (a `#[cfg(test)]` child module inline in `core.rs`) so the parent file can
//! be read as production code. A child module of `core`, so it still reaches
//! `core.rs`'s private items (`short_oid`) through `super::`.

use super::*;
use git_vista_protocol::RefName;

fn commit_oid(c: char) -> CommitOid {
    CommitOid::new(c.to_string().repeat(40)).unwrap()
}

fn ref_change(before: RefState) -> RefChange {
    RefChange {
        ref_name: RefName::new("refs/remotes/origin/feature").unwrap(),
        before,
        after: RefState::At(commit_oid('b')),
    }
}

/// Mutation this catches: reading `changes.last()` instead of
/// `.first()`, or losing the oid on the way through `.clone()`.
#[test]
fn remote_tip_from_plan_reads_a_known_tip() {
    let changes = [ref_change(RefState::At(commit_oid('a')))];
    assert_eq!(
        remote_tip_from_plan(&changes),
        RemoteTipKnowledge::Known(commit_oid('a'))
    );
}

/// D5's distinction (`planner.rs` around line 1595): "never pushed"
/// must not collapse into the same answer as "couldn't read".
#[test]
fn remote_tip_from_plan_distinguishes_absent_from_unreadable() {
    let absent = [ref_change(RefState::Absent)];
    assert_eq!(
        remote_tip_from_plan(&absent),
        RemoteTipKnowledge::NotYetPushed
    );
    assert_eq!(remote_tip_from_plan(&[]), RemoteTipKnowledge::Unreadable);
}

/// A plain push never carries the danger tier, regardless of what
/// `set_upstream` says — mutation this catches: `danger` hardcoded
/// `true`, or the `None` arm falling through to the force-push wording.
#[test]
fn push_confirm_copy_plain_push_is_never_danger() {
    let copy = push_confirm_copy("feature", false, None, &[]);
    assert!(!copy.danger);
    assert_eq!(copy.title, "Push branch");
    assert_eq!(copy.confirm_label, "Push");
    assert!(copy.body.contains("feature"));
}

/// #233's explicit requirement: `danger` must come from `risk`, not
/// from `force.is_some()`. Constructing this with a `Remote` risk
/// (never actually returned by the server for a lease today, but not
/// impossible for this *function* to be called with) proves the
/// distinction is real rather than accidental — a hardcoded
/// `danger: force.is_some()` would fail this exact case.
#[test]
fn push_confirm_copy_danger_is_driven_by_risk_not_by_force_alone() {
    let oid = commit_oid('a');
    let destructive =
        push_confirm_copy("feature", false, Some((&oid, RiskLevel::Destructive)), &[]);
    assert!(destructive.danger);
    let non_destructive = push_confirm_copy("feature", false, Some((&oid, RiskLevel::Remote)), &[]);
    assert!(!non_destructive.danger);
}

/// The confirmation must name what would be overwritten — mutation this
/// catches: the oid never reaching the body at all.
#[test]
fn push_confirm_copy_names_the_oid_being_overwritten() {
    let oid = commit_oid('a');
    let copy = push_confirm_copy("feature", false, Some((&oid, RiskLevel::Destructive)), &[]);
    assert!(copy.body.contains(&oid.as_str()[..7]));
    assert_eq!(copy.title, "Force-push branch");
    assert_eq!(copy.confirm_label, "Force Push");
}

/// `set_upstream` adds a line without flipping which tier applies —
/// mutation this catches: the upstream line silently overwriting the
/// whole body, or `danger` accidentally keyed off `set_upstream`.
#[test]
fn push_confirm_copy_set_upstream_is_orthogonal_to_danger() {
    let plain = push_confirm_copy("feature", true, None, &[]);
    assert!(!plain.danger);
    assert!(plain.body.contains("--set-upstream"));

    let oid = commit_oid('a');
    let forced = push_confirm_copy("feature", true, Some((&oid, RiskLevel::Destructive)), &[]);
    assert!(forced.danger);
    assert!(forced.body.contains("--set-upstream"));
}

#[test]
fn short_oid_truncates_to_seven_chars() {
    let oid = commit_oid('a');
    assert_eq!(short_oid(oid.as_str()).len(), 7);
    assert_eq!(short_oid("abc"), "abc");
}

// M4.32 (#85): the planner computes advisories for a force-with-lease push and
// ships them in `Plan.advisories`. Before this, nothing client-side read the
// field — the warnings existed in the payload and never on the user's screen.

fn branch(name: &str) -> git_vista_protocol::BranchName {
    git_vista_protocol::BranchName::new(name).unwrap()
}

fn remote(name: &str) -> git_vista_protocol::RemoteName {
    git_vista_protocol::RemoteName::new(name).unwrap()
}

#[test]
fn a_default_branch_force_push_says_so_in_the_confirmation() {
    let oid = CommitOid::new("1234567890abcdef1234567890abcdef12345678").unwrap();
    let copy = push_confirm_copy(
        "main",
        false,
        Some((&oid, RiskLevel::Destructive)),
        &[Advisory::DefaultBranchPush {
            branch: branch("main"),
            remote: remote("origin"),
        }],
    );
    assert!(
        copy.body.contains("default branch"),
        "a push to the remote's default branch must say so: {}",
        copy.body
    );
    assert!(
        copy.body.contains("every clone"),
        "and must state the blast radius, not merely name the branch: {}",
        copy.body
    );
}

#[test]
fn an_unknown_default_branch_never_reads_as_an_all_clear() {
    // THE assertion this whole change exists for. The server carries a separate
    // `DefaultBranchUnknown` variant precisely so "the check could not run" is
    // distinguishable from "the check ran and this is not the default branch"
    // — the latter emits NO advisory. If this rendered as reassurance, or were
    // dropped silently, the user would be told a dangerous push is ordinary.
    let oid = CommitOid::new("1234567890abcdef1234567890abcdef12345678").unwrap();
    let copy = push_confirm_copy(
        "feature",
        false,
        Some((&oid, RiskLevel::Destructive)),
        &[Advisory::DefaultBranchUnknown {
            reason: "origin does not record a default branch".into(),
        }],
    );
    assert!(
        copy.body.contains("could not tell"),
        "an unreadable default branch must be reported as unknown: {}",
        copy.body
    );
    assert!(
        copy.body
            .contains("origin does not record a default branch"),
        "and must carry the server's own reason verbatim: {}",
        copy.body
    );
    assert!(
        copy.body.contains("not as safe"),
        "and must explicitly refuse to read as an all-clear: {}",
        copy.body
    );
}

#[test]
fn remote_history_replaced_is_not_printed_twice() {
    // The force body already says the push overwrites origin/<branch>, makes
    // other people's commits unreachable, and cannot be undone. Rendering the
    // advisory too would state the same fact twice in one dialog, which is how
    // a reader learns to skim the warnings that are NOT duplicated.
    let oid = CommitOid::new("1234567890abcdef1234567890abcdef12345678").unwrap();
    let with = push_confirm_copy(
        "feature",
        false,
        Some((&oid, RiskLevel::Destructive)),
        &[Advisory::RemoteHistoryReplaced {
            branch: branch("feature"),
            remote: remote("origin"),
        }],
    );
    let without = push_confirm_copy("feature", false, Some((&oid, RiskLevel::Destructive)), &[]);
    assert_eq!(
        with.body, without.body,
        "RemoteHistoryReplaced is already in the body; it must add nothing"
    );
    // And the fact itself must actually be in there — otherwise this test
    // passes by both sides being equally silent about it.
    assert!(
        without.body.contains("can't be undone"),
        "the body must state the irreversibility it is being credited with: {}",
        without.body
    );
}

#[test]
fn a_plain_push_renders_no_advisories_even_if_handed_some() {
    // `advisories_for` only ever returns advisories for a force-with-lease
    // push, so this is a defence against a future caller passing them anyway:
    // an ordinary push cannot replace remote history and must not be dressed
    // up as though it could.
    let copy = push_confirm_copy(
        "main",
        false,
        None,
        &[Advisory::DefaultBranchPush {
            branch: branch("main"),
            remote: remote("origin"),
        }],
    );
    assert!(!copy.body.contains("default branch"), "{}", copy.body);
    assert!(!copy.danger);
}
