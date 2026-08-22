//! Which stored comparison may be restored, and — the part that matters —
//! which may NOT (M4.27, #80).

use super::{restorable_for, StoredComparison};
use git_vista_protocol::diff::{ComparisonBasis, DiffSpec};
use git_vista_protocol::CommitOid;

fn oid(c: char) -> CommitOid {
    CommitOid::new(std::iter::repeat_n(c, 40).collect::<String>()).unwrap()
}

fn spec() -> DiffSpec {
    DiffSpec::CommitVsCommit {
        base: oid('a'),
        target: oid('b'),
        basis: ComparisonBasis::Direct,
    }
}

fn stored(repo: &str) -> StoredComparison {
    StoredComparison {
        repo_id: repo.to_string(),
        spec: spec(),
    }
}

#[test]
fn a_comparison_is_restored_into_the_repository_it_came_from() {
    assert_eq!(restorable_for(&stored("repo-1"), "repo-1"), Some(spec()));
}

#[test]
fn a_comparison_is_never_restored_into_a_different_repository() {
    // THE assertion this function exists for. Two commit oids carry no
    // repository with them: restored into the wrong repo, the same spec either
    // errors or — far worse — resolves against unrelated commits and renders a
    // diff that looks completely real. There is nothing on screen that would
    // tell the user it is nonsense.
    assert_eq!(restorable_for(&stored("repo-1"), "repo-2"), None);
}

#[test]
fn an_empty_repo_id_does_not_match_a_real_one() {
    // A degraded Frame reports no repo id, and `unwrap_or_default()` elsewhere
    // in this codebase turns that into "". If "" matched anything, the degraded
    // case would restore comparisons from every repository it ever saw.
    assert_eq!(restorable_for(&stored("repo-1"), ""), None);
    assert_eq!(restorable_for(&stored(""), "repo-1"), None);
}

#[test]
fn the_stored_form_survives_a_round_trip_through_json() {
    // It is persisted as JSON in localStorage; a shape that cannot come back
    // out is a comparison silently never restored.
    let before = stored("repo-1");
    let json = serde_json::to_string(&before).unwrap();
    let after: StoredComparison = serde_json::from_str(&json).unwrap();
    assert_eq!(before, after);
    assert_eq!(restorable_for(&after, "repo-1"), Some(spec()));
}
