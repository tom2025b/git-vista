//! Activity-panel state and the pure decisions the feed rows make (M1.11, #64).
//!
//! Framework-free (M1.11 D1). This core is deliberately **small**: the Activity panel is
//! mostly view, and inventing state for it to own would be worse than admitting that. What
//! is here is the part that is genuinely a decision rather than a rendering — which commit
//! a feed row is "about" — plus the two total mappings from [`ActivityKind`], which belong
//! together so that adding a kind fails in one place instead of rendering a blank row.
//!
//! The panel's *visibility* lives here as a flag rather than a bare `RwSignal<bool>` so
//! there is a named owner for it. The right-edge exclusivity rule that couples it to the
//! detail panel is **not** here: that is a property of the overlay stack, not of Activity,
//! and it lands with `features/shell` in Task 8.

use git_vista_core::activity::{ActivityEvent, ActivityKind};

use crate::icons::GitIcons;

/// Whether the Activity panel is showing.
///
/// Thin on purpose. It exists so `activity_open` has an owner with a name, and so the
/// panel's open/close reads as intent (`core.open()`) rather than as a boolean poke that
/// any module can perform.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ActivityCore {
    open: bool,
}

impl ActivityCore {
    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// The topbar button's behaviour: same control opens and closes.
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// The commit a feed row's context menu should point at: where the ref ended up, or —
/// for a deletion, which has no new state — the tip that was deleted.
///
/// The all-zero oid is git's "no such object" sentinel, and it reaches the frontend on the
/// old side of a creation and the new side of a deletion. Reading it as a real hash would
/// open a context menu on a commit that does not exist. This convention is shared with the
/// backend's activity encoding (`git_vista_core::activity`), which is exactly why it is
/// worth a test rather than an inline `.filter()` in a view closure.
pub fn event_commit(event: &ActivityEvent) -> Option<String> {
    fn usable(oid: &Option<String>) -> Option<String> {
        oid.as_ref().filter(|o| is_usable_oid(o)).cloned()
    }
    usable(&event.new_oid).or_else(|| usable(&event.old_oid))
}

/// Whether `oid` names something a menu can be opened on.
///
/// Excludes git's null oid — all ASCII zeros — and the empty string. Length is not
/// checked: sha-1 and sha-256 repos give 40 and 64 characters respectively.
///
/// The empty case is load-bearing and easy to lose. The inline version this replaces was
/// `!oid.bytes().all(|b| b == b'0')`, and `all` on an empty iterator is `true`, so an
/// empty oid fell out as "null" by accident. Writing the check the obvious way round —
/// "is it all zeros?" — silently reverses that and lets `""` through as a commit hash.
fn is_usable_oid(oid: &str) -> bool {
    !oid.is_empty() && !oid.bytes().all(|b| b == b'0')
}

/// Short human name for one event kind — the row's leading word.
pub fn kind_label(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Commit => "Commit",
        ActivityKind::Amend => "Amend",
        ActivityKind::Merge => "Merge",
        ActivityKind::Rebase => "Rebase",
        ActivityKind::Checkout => "Switch",
        ActivityKind::Reset => "Reset",
        ActivityKind::CherryPick => "Cherry-pick",
        ActivityKind::Revert => "Revert",
        ActivityKind::BranchCreated => "Branch created",
        ActivityKind::BranchDeleted => "Branch deleted",
        ActivityKind::Push => "Push",
        ActivityKind::Fetch => "Fetch",
        ActivityKind::Pull => "Pull",
        ActivityKind::Clone => "Clone",
        ActivityKind::Other => "Event",
    }
}

/// The glyph for one event kind. Rebase deliberately shares the merge glyph — the existing
/// "Rebase onto main" menu item already reads that way — and pull shares it too (a pull
/// *is* fetch + merge).
pub fn kind_glyph(ic: &GitIcons, kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Commit | ActivityKind::CherryPick | ActivityKind::Other => ic.commit,
        ActivityKind::Amend => ic.modified,
        ActivityKind::Merge | ActivityKind::Rebase | ActivityKind::Pull => ic.merge,
        ActivityKind::Checkout => ic.checkout,
        ActivityKind::Reset | ActivityKind::Revert => ic.undo,
        ActivityKind::BranchCreated => ic.branch,
        ActivityKind::BranchDeleted => ic.deleted,
        ActivityKind::Push => ic.push,
        ActivityKind::Fetch => ic.branch_alt,
        ActivityKind::Clone => ic.repository,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use git_vista_core::activity::ActivitySource;

    const NULL_SHA1: &str = "0000000000000000000000000000000000000000";
    const TIP: &str = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b";

    fn event(old: Option<&str>, new: Option<&str>) -> ActivityEvent {
        ActivityEvent {
            time: 0,
            kind: ActivityKind::Other,
            ref_name: None,
            summary: String::new(),
            old_oid: old.map(str::to_owned),
            new_oid: new.map(str::to_owned),
            source: ActivitySource::External,
            undo: None,
            refs: None,
        }
    }

    #[test]
    fn the_new_tip_wins_when_both_sides_are_real() {
        let e = event(Some(TIP), Some("ffffffffffffffffffffffffffffffffffffffff"));
        assert_eq!(
            event_commit(&e).as_deref(),
            Some("ffffffffffffffffffffffffffffffffffffffff"),
            "a moved ref is 'about' where it landed"
        );
    }

    #[test]
    fn a_deletion_falls_back_to_the_tip_that_died() {
        // No new state at all: the row still has to open a menu on something, and the
        // dead tip is what makes "Create branch from this commit" a manual restore.
        let e = event(Some(TIP), None);
        assert_eq!(event_commit(&e).as_deref(), Some(TIP));
    }

    #[test]
    fn a_null_new_oid_is_not_treated_as_a_commit() {
        // The bug this guards: a deletion encoded with an all-zero new_oid rather than
        // `None` would otherwise open a context menu on a commit that does not exist.
        let e = event(Some(TIP), Some(NULL_SHA1));
        assert_eq!(event_commit(&e).as_deref(), Some(TIP));
    }

    #[test]
    fn a_null_old_oid_on_a_creation_yields_no_commit() {
        let e = event(Some(NULL_SHA1), None);
        assert_eq!(
            event_commit(&e),
            None,
            "a creation's old side is not a commit"
        );
    }

    #[test]
    fn an_event_referencing_nothing_yields_no_commit() {
        assert_eq!(event_commit(&event(None, None)), None);
    }

    #[test]
    fn an_empty_oid_is_not_a_commit() {
        // Pins the vacuous-truth trap documented on `is_usable_oid`: the inline check this
        // came from dropped `""` only because `all` is `true` on an empty iterator, so the
        // extraction is one `!` away from opening a context menu on an empty hash.
        let e = event(Some(TIP), Some(""));
        assert_eq!(event_commit(&e).as_deref(), Some(TIP));
        assert_eq!(event_commit(&event(Some(""), Some(""))), None);
    }

    #[test]
    fn a_sha256_null_oid_is_recognised_too() {
        let null256 = "0".repeat(64);
        let e = event(None, Some(&null256));
        assert_eq!(event_commit(&e), None);
    }

    #[test]
    fn the_panel_opens_closes_and_toggles() {
        let mut a = ActivityCore::default();
        assert!(!a.is_open(), "the panel starts closed");
        a.toggle();
        assert!(a.is_open());
        a.toggle();
        assert!(!a.is_open());
        a.open();
        a.open();
        assert!(a.is_open(), "opening twice is not a toggle");
        a.close();
        assert!(!a.is_open());
    }

    #[test]
    fn every_kind_has_a_label_and_a_glyph() {
        // Both mappings are total by `match`, so this is really a guard on emptiness:
        // a new kind added with a `""` placeholder in either table renders a blank row.
        use crate::icons::icon_set;
        let ic = icon_set(false);
        for kind in [
            ActivityKind::Commit,
            ActivityKind::Amend,
            ActivityKind::Merge,
            ActivityKind::Rebase,
            ActivityKind::Checkout,
            ActivityKind::Reset,
            ActivityKind::CherryPick,
            ActivityKind::Revert,
            ActivityKind::BranchCreated,
            ActivityKind::BranchDeleted,
            ActivityKind::Push,
            ActivityKind::Fetch,
            ActivityKind::Pull,
            ActivityKind::Clone,
            ActivityKind::Other,
        ] {
            assert!(!kind_label(kind).is_empty(), "{kind:?} has no label");
            assert!(!kind_glyph(ic, kind).is_empty(), "{kind:?} has no glyph");
        }
    }
}
