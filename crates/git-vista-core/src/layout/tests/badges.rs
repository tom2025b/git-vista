//! Badge tests: attaching refs to the commits they point at, and dropping refs
//! whose target is outside the walked window.

use super::*;
use crate::layout::layout_with_refs;

#[test]
fn refs_are_badged_on_their_commits_and_off_window_refs_dropped() {
    let g = layout_with_refs(
        vec![commit("b", &["a"]), commit("a", &[])],
        vec![
            gitref("HEAD", RefKind::Head, "b"),
            gitref("main", RefKind::Branch, "b"),
            gitref("v1", RefKind::Tag, "a"),
            // Points outside the walked window — must be dropped, not panic.
            gitref("old", RefKind::Branch, "zzz"),
        ],
        Some("main"),
    );
    assert_eq!(ref_names(&g, "b"), vec!["HEAD", "main"]);
    assert_eq!(ref_names(&g, "a"), vec!["v1"]);
    assert!(
        g.rows
            .iter()
            .all(|r| r.refs.iter().all(|x| x.name != "old")),
        "off-window ref isn't attached anywhere"
    );
}
