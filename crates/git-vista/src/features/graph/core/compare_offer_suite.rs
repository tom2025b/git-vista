//! Which comparison a commit's menu offers, and — the part that matters —
//! which endpoint becomes `base` (M4.27, #80).

use super::{offer_for, CompareOffer};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn with_no_anchor_a_commit_offers_to_become_one() {
    assert_eq!(offer_for(None, A), CompareOffer::SetAnchor);
}

#[test]
fn the_anchor_itself_offers_only_to_be_cleared() {
    // Comparing a commit with itself is an empty diff. Offering it would be a
    // dead end dressed as an action.
    assert_eq!(offer_for(Some(A), A), CompareOffer::ClearAnchor);
}

#[test]
fn the_anchor_is_the_base_and_the_menus_own_commit_is_the_target() {
    // THE assertion this module exists for. Swap these two and every diff the
    // feature produces is inverted — additions render as deletions — while
    // still looking entirely plausible on screen. Nothing else in the app
    // would catch it.
    //
    // Direction matches "Compare with HEAD" on a branch stub, which puts the
    // thing you tapped FIRST as `base`: you anchored A, then asked B's menu to
    // compare, so the question is "A → B".
    assert_eq!(
        offer_for(Some(A), B),
        CompareOffer::Compare {
            base: A.to_string(),
            target: B.to_string(),
        }
    );
}

#[test]
fn the_offer_is_not_symmetric() {
    // Guards the same defect from the other side: if `offer_for` ignored which
    // argument was which, these two would be equal.
    let one = offer_for(Some(A), B);
    let other = offer_for(Some(B), A);
    assert_ne!(one, other, "A→B and B→A are different comparisons");
}
