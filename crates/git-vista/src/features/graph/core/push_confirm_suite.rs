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
    let copy = push_confirm_copy("feature", false, None);
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
    let destructive = push_confirm_copy("feature", false, Some((&oid, RiskLevel::Destructive)));
    assert!(destructive.danger);
    let non_destructive = push_confirm_copy("feature", false, Some((&oid, RiskLevel::Remote)));
    assert!(!non_destructive.danger);
}

/// The confirmation must name what would be overwritten — mutation this
/// catches: the oid never reaching the body at all.
#[test]
fn push_confirm_copy_names_the_oid_being_overwritten() {
    let oid = commit_oid('a');
    let copy = push_confirm_copy("feature", false, Some((&oid, RiskLevel::Destructive)));
    assert!(copy.body.contains(&oid.as_str()[..7]));
    assert_eq!(copy.title, "Force-push branch");
    assert_eq!(copy.confirm_label, "Force Push");
}

/// `set_upstream` adds a line without flipping which tier applies —
/// mutation this catches: the upstream line silently overwriting the
/// whole body, or `danger` accidentally keyed off `set_upstream`.
#[test]
fn push_confirm_copy_set_upstream_is_orthogonal_to_danger() {
    let plain = push_confirm_copy("feature", true, None);
    assert!(!plain.danger);
    assert!(plain.body.contains("--set-upstream"));

    let oid = commit_oid('a');
    let forced = push_confirm_copy("feature", true, Some((&oid, RiskLevel::Destructive)));
    assert!(forced.danger);
    assert!(forced.body.contains("--set-upstream"));
}

#[test]
fn short_oid_truncates_to_seven_chars() {
    let oid = commit_oid('a');
    assert_eq!(short_oid(oid.as_str()).len(), 7);
    assert_eq!(short_oid("abc"), "abc");
}
