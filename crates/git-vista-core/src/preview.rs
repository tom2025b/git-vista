//! Laying out a history that does not exist yet (M10.08, #576).
//!
//! Pure, wasm-safe, and takes **no repository**. [`StreamLayout::push`](crate::layout::stream::StreamLayout::push) asks for
//! a commit and a membership predicate, never for an object database, so a
//! hypothetical commit lays out exactly like a real one — which is why this
//! feature costs a function rather than a renderer.
//!
//! This half is testable with no git at all: build two commit lists, lay them
//! out, diff them.
//!
//! # The three things that make a preview comparable to the real thing
//!
//! A preview is only useful if the picture it draws is the picture the user
//! would get by running the operation. Three separate mechanisms in the
//! existing layout key off the *identity* of a commit, and each of them will
//! silently disagree between a preview and a real run unless it is fed
//! correctly. All three are the caller's responsibility and all three are
//! stated on [`PreviewInput`]:
//!
//! 1. **Lane 0 is reserved from the refs.**
//!    [`trunk_reserve_tip`](crate::layout::trunk_reserve_tip) reads
//!    the ref slice it is handed. Refs that still name the old tip reserve
//!    lane 0 for the wrong commit — see [`PreviewInput::ref_moves`].
//! 2. **Colour slot 0 is claimed from the refs.**
//!    `layout::color::assign_branch_colors` falls back to a key of
//!    `~<the commit's own short hash>` for any commit no ref claims, so an
//!    unclaimed hypothetical commit takes a colour slot derived from an oid
//!    that by construction differs from the real one. Same fix, same field.
//! 3. **Row order is decided by commit *time*, not by list position.**
//!    `stable_topo_order` is a max-heap on `(time, Reverse(id))` under the
//!    topological constraint, so the hypothetical commit competes with every
//!    other branch tip in the window. See [`PreviewInput::added`]; this module
//!    documents that dependency and deliberately does not paper over it.

use std::collections::HashMap;

use crate::layout::layout_with_refs;
use crate::model::{CommitSummary, GitRef, Graph, Oid};

/// One commit that moved column between the before and after layouts.
///
/// Both lanes are carried, not just the id: a caller handed only "this commit
/// shifted" has no way to check the claim, and this is the field the
/// preview-versus-reality comparison test compares most directly.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaneShift {
    pub commit: Oid,
    pub from_lane: usize,
    pub to_lane: usize,
}

/// One change between the two halves, as the wire carries it.
///
/// Lives here rather than in `git-vista-protocol` because it names commit ids
/// and lane numbers — the repository domain. `PreviewOutcome<_, _, _, C>` is
/// generic over it exactly as `HistoryPage<R, E, S>` is generic over rows.
///
/// `Added` is the hypothetical commit. `RefMoved` is one ref the plan moves,
/// with both endpoints so a reviewer can check it. `LaneShifted` is a commit
/// that already existed and changed column — the only one of the three that
/// cannot be known without both layouts, and therefore the only one this
/// module derives ([`PreviewLayout::lane_shifts`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum PreviewChange {
    Added {
        commit: Oid,
    },
    RefMoved {
        ref_name: String,
        from: Oid,
        to: Oid,
    },
    LaneShifted {
        commit: Oid,
        from_lane: usize,
        to_lane: usize,
    },
}

impl From<LaneShift> for PreviewChange {
    /// The one mapping in this module that a transposition would corrupt
    /// invisibly: `from_lane` and `to_lane` are both `usize`, so swapping them
    /// still compiles and still round-trips. Pinned by
    /// `a_lane_shift_converts_to_a_change_without_transposing_the_lanes`.
    fn from(shift: LaneShift) -> Self {
        PreviewChange::LaneShifted {
            commit: shift.commit,
            from_lane: shift.from_lane,
            to_lane: shift.to_lane,
        }
    }
}

/// Everything the pure half needs. No repository, no `Plan`, no transport type.
#[derive(Debug, Clone)]
pub struct PreviewInput {
    /// The repository's real history, newest-first, exactly as
    /// `walk_history` produces it. [`layout_with_refs`] re-sorts it through
    /// `stable_topo_order`, so the caller does not have to place `added`.
    pub before: Vec<CommitSummary>,
    /// The repository's real refs, display-shortened, exactly as `read_refs`
    /// produces them (HEAD first when it resolves).
    pub refs: Vec<GitRef>,
    /// The checked-out branch's short name; `None` when HEAD is detached.
    pub head_branch: Option<String>,
    /// The hypothetical commit, if the operation creates one. `None` for the
    /// fast-forward merge, which moves refs and adds nothing.
    ///
    /// # `added.time` is load-bearing, and `added.id` breaks its ties
    ///
    /// `stable_topo_order` emits ready commits from a max-heap on
    /// `(time, Reverse(id.0))`. The hypothetical commit is forced above its own
    /// parent by the topological constraint, but it competes for row 0 against
    /// every *other* branch tip in the window — any commit with no in-window
    /// child is ready from the start. Give it a `time` older than a sibling tip
    /// and the sibling takes row 0, exactly as it would for a real commit.
    ///
    /// A real run stamps the commit's committer time as "now", so a caller that
    /// wants the preview to agree with a real run must stamp a `time` that is
    /// `>=` every commit in [`before`](Self::before). On exact equality the
    /// tiebreak is the oid string — and the hypothetical oid differs from the
    /// real one by construction, so a tie is decided by the one value a preview
    /// may never be compared on.
    ///
    /// This is stated, not enforced: clamping the time here would hide a
    /// caller's mistake behind a layout that quietly disagreed with git.
    /// `the_added_commits_time_decides_its_row_not_its_list_position` pins the
    /// sensitivity so the dependency cannot be discovered the hard way.
    pub added: Option<CommitSummary>,
    /// Every ref the operation moves, as `(display ref name, new target)` —
    /// the branch AND `"HEAD"` when HEAD is attached to it.
    ///
    /// # This is a precondition, not a decoration
    ///
    /// [`layout_with_refs`] calls `trunk_reserve_tip(&refs, head_branch)`
    /// before laying anything out, and that reservation holds lane 0 for
    /// whichever commit the *passed* refs say `main` (then `master`, then the
    /// checked-out branch) points at. Hand it the old tip and the hypothetical
    /// commit at row 0 finds lane 0 taken and lands in lane 1 — while a real
    /// run of the operation, whose refs really did move, puts its new commit in
    /// lane 0. The colouring pass has the same dependency one step later: a
    /// commit no ref claims falls into `assign_branch_colors`'s synthetic
    /// fallback, whose key is `~<the commit's own short hash>` — so an
    /// unrewritten preview and the real commit get different colour slots for
    /// no reason but their different oids.
    ///
    /// So the rewrite happens *here*, from this list, in one place, and
    /// [`PreviewLayout::unmatched_ref_moves`] reports any entry that matched
    /// nothing rather than letting it pass silently.
    pub ref_moves: Vec<(String, Oid)>,
}

/// The two layouts and what differs between them.
///
/// # The two report fields are read together, not either/or
///
/// [`unmatched_ref_moves`](Self::unmatched_ref_moves) and
/// [`added_without_ref_moves`](Self::added_without_ref_moves) each name a
/// different way the caller can have failed to point a ref at the hypothetical
/// commit, and neither implies the other. A caller that supplies an `added`
/// commit and a `ref_moves` list whose every entry matched nothing gets
/// `added_without_ref_moves == false` — the list was not empty — and takes the
/// full lane-1-plus-synthetic-colour damage anyway, reported only by
/// `unmatched_ref_moves`. A correct preview has **both** empty/false.
///
/// Derives `Debug` so a comparison test that disagrees can print what it got.
#[derive(Debug, Clone)]
pub struct PreviewLayout {
    /// The repository as it is.
    pub before: Graph,
    /// The repository as it would be: `added` prepended, `ref_moves` applied.
    pub after: Graph,
    /// Every commit present in both halves whose lane differs, in `after` row
    /// order. Derived here because it is the one change that needs both
    /// layouts; the caller already knows what it added and which refs it moved.
    pub lane_shifts: Vec<LaneShift>,
    /// Entries of `ref_moves` that named no ref in `refs`. Always empty for a
    /// correct caller. Returned rather than logged so a test can prove the
    /// rewrite fired — a rewrite that silently matched nothing is precisely the
    /// wrong-reason failure this field exists to catch.
    ///
    /// Holds **only** unmatched ref names. The other way a caller can get the
    /// rewrite wrong — supplying an `added` commit and no `ref_moves` at all —
    /// has no name to report and gets its own field,
    /// [`added_without_ref_moves`](Self::added_without_ref_moves), rather than
    /// a sentinel string in here. One field, one meaning: a `Vec<String>`
    /// documented as "ref names that did not match" must not sometimes contain
    /// something that is not a ref name.
    pub unmatched_ref_moves: Vec<String>,
    /// `true` when `added` is `Some` and `ref_moves` is empty — a caller bug,
    /// because all three supported operations move at least one ref, and
    /// because the hypothetical commit then lands in the synthetic colour
    /// fallback and (usually) lane 1. Reported rather than corrected: this
    /// module does not know which ref the caller meant to move.
    ///
    /// `false` for a fast-forward (`added: None`, refs move) and `false` for
    /// the degenerate no-op (`added: None`, `ref_moves` empty) — neither is a
    /// bug.
    pub added_without_ref_moves: bool,
}

/// Lay out `before`, then lay out the same history with `added` and
/// `ref_moves` applied, and report what differs.
///
/// Order of operations, which is the whole content of this function:
///
/// 1. `before = layout_with_refs(input.before, input.refs, head_branch)`.
/// 2. Build `after_refs` by rewriting every [`GitRef`] whose `name` matches an
///    entry of `ref_moves` to that entry's new target, collecting unmatched
///    names. **Before any layout call** — see [`PreviewInput::ref_moves`].
/// 3. `after = layout_with_refs(added.into_iter().chain(before_commits),
///    after_refs, head_branch)`. `head_branch` is unchanged: none of
///    revert/cherry-pick/merge changes which branch is checked out.
/// 4. `lane_shifts` = for each `after` row whose commit id appears in `before`,
///    emit a [`LaneShift`] when the lanes differ.
///
/// Every row in both halves carries `on_remote: false`, because
/// `StreamLayout::push` emits it and this pipeline is [`layout_with_refs`] and
/// nothing else — the server's remote-membership stamping pass is deliberately
/// not part of it. Stamping would make the preview-versus-reality comparison
/// red on its own: the throwaway clone the real half is laid out from has
/// `origin/*` refs the source repository does not.
pub fn lay_out_preview(input: PreviewInput) -> PreviewLayout {
    let PreviewInput {
        before,
        refs,
        head_branch,
        added,
        ref_moves,
    } = input;

    let before_graph = layout_with_refs(before.clone(), refs.clone(), head_branch.as_deref());

    // Step 2, and it must happen before step 3: `layout_with_refs` reserves
    // lane 0 and seeds colour slot 0 from the ref slice it is handed.
    let mut after_refs = refs;
    let mut unmatched_ref_moves = Vec::new();
    for (name, new_target) in &ref_moves {
        let mut matched = false;
        for r in after_refs.iter_mut() {
            if &r.name == name {
                r.target = new_target.clone();
                matched = true;
            }
        }
        if !matched {
            unmatched_ref_moves.push(name.clone());
        }
    }

    let added_without_ref_moves = added.is_some() && ref_moves.is_empty();

    let after_commits: Vec<CommitSummary> = added.into_iter().chain(before).collect();
    let after_graph = layout_with_refs(after_commits, after_refs, head_branch.as_deref());

    let lane_shifts = {
        let before_lanes: HashMap<&Oid, usize> = before_graph
            .rows
            .iter()
            .map(|r| (&r.commit.id, r.lane))
            .collect();
        after_graph
            .rows
            .iter()
            .filter_map(|row| {
                let from_lane = *before_lanes.get(&row.commit.id)?;
                if from_lane == row.lane {
                    return None;
                }
                Some(LaneShift {
                    commit: row.commit.id.clone(),
                    from_lane,
                    to_lane: row.lane,
                })
            })
            .collect()
    };

    PreviewLayout {
        before: before_graph,
        after: after_graph,
        lane_shifts,
        unmatched_ref_moves,
        added_without_ref_moves,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RefKind;

    /// A 40-hex-character oid built from one repeated digit, so `Oid::short`
    /// (the key `assign_branch_colors`'s synthetic fallback hashes) is
    /// distinct per commit and readable in a failure message.
    fn oid(digit: char) -> Oid {
        Oid((0..40).map(|_| digit).collect())
    }

    fn commit(digit: char, time: i64, parents: &[char]) -> CommitSummary {
        CommitSummary {
            id: oid(digit),
            parents: parents.iter().copied().map(oid).collect(),
            summary: format!("commit {digit}"),
            author: "Test".into(),
            time,
        }
    }

    fn git_ref(name: &str, kind: RefKind, target: char) -> GitRef {
        GitRef {
            name: name.into(),
            kind,
            target: oid(target),
        }
    }

    fn badge_names(rows: &[crate::model::GraphRow], row: usize) -> Vec<&str> {
        rows[row].refs.iter().map(|r| r.name.as_str()).collect()
    }

    /// A linear trunk: `3` (tip) -> `2` -> `1`, with `HEAD` and `main` on the
    /// tip. `read_refs` emits HEAD first, and `attach_ref_badges` preserves
    /// that order, so badge assertions can be exact.
    fn linear_trunk() -> (Vec<CommitSummary>, Vec<GitRef>) {
        let commits = vec![
            commit('3', 300, &['2']),
            commit('2', 200, &['1']),
            commit('1', 100, &[]),
        ];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '3'),
            git_ref("main", RefKind::Branch, '3'),
        ];
        (commits, refs)
    }

    /// The invariant: rewriting the moved refs *before* laying `after` out is
    /// what puts the hypothetical commit where a real run would put it —
    /// lane 0, colour slot 0 (the trunk's), badges attached to it and not to
    /// the old tip.
    ///
    /// Every expected value below is a literal, never a re-derivation: `0` for
    /// the trunk colour is `assign_branch_colors`'s documented trunk slot, and
    /// asserting `after.color == before.color` would have passed with both
    /// sides wrong.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M1a — REMOVES the mechanism.** Delete the rewrite loop and pass
    ///   `refs` straight through as `after_refs`. `trunk_reserve_tip` then
    ///   reserves lane 0 for the *old* tip `3`, the hypothetical commit takes
    ///   `leftmost_free` = lane 1, and no ref claims it so
    ///   `assign_branch_colors` gives it the `~9999999` synthetic slot. Red on
    ///   `lane`, on `color`, and on both badge assertions.
    /// * **M1b — WEAKENS the mechanism.** Rewrite only refs where
    ///   `r.is_branch()`, skipping `HEAD`. `trunk_reserve_tip` and the colour
    ///   seeds read branches only, so lane 0 and colour 0 stay **green** — and
    ///   the `HEAD` badge stays stuck on the old tip. Red on the badge
    ///   assertions alone. A different failure, and the one a lane/colour-only
    ///   test would never see.
    #[test]
    fn the_ref_rewrite_lands_the_hypothetical_commit_in_the_trunk_lane_and_colour() {
        let (before, refs) = linear_trunk();
        let out = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });

        assert_eq!(out.after.rows[0].commit.id, oid('9'));
        assert_eq!(out.after.rows[0].row, 0);
        assert_eq!(
            out.after.rows[0].lane, 0,
            "the hypothetical commit must take the trunk lane, as a real run \
             would; lane 1 means the ref rewrite did not reach trunk_reserve_tip"
        );
        assert_eq!(
            out.after.rows[0].color, 0,
            "slot 0 is the trunk colour; anything else is the ~<short-hash> \
             synthetic fallback, which hashes the one value a preview may not \
             be compared on"
        );

        assert_eq!(badge_names(&out.after.rows, 0), vec!["HEAD", "main"]);
        assert_eq!(
            badge_names(&out.after.rows, 1),
            Vec::<&str>::new(),
            "the old tip keeps no badge: both refs moved off it"
        );

        // The `before` half is laid out from the *original* refs.
        assert_eq!(badge_names(&out.before.rows, 0), vec!["HEAD", "main"]);
        assert_eq!(out.before.rows[0].commit.id, oid('3'));

        assert_eq!(
            out.lane_shifts,
            Vec::<LaneShift>::new(),
            "committing on the tip of a linear trunk moves no existing commit"
        );
        assert_eq!(out.unmatched_ref_moves, Vec::<String>::new());
        assert!(!out.added_without_ref_moves);
    }

    /// The invariant: a `ref_moves` entry that matched no ref is *named*, and
    /// one that matched is not. A rewrite that silently hit nothing is the
    /// failure this field exists to catch, so "it matched something" is not
    /// good enough — the mixed case is the one that discriminates.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M2a — REMOVES the mechanism.** Always `unmatched_ref_moves:
    ///   Vec::new()`. Red on the mixed case *and* on the nothing-matched case.
    /// * **M2b — WEAKENS the mechanism.** Track one crate-wide `matched` flag
    ///   instead of one per entry, reporting nothing when any entry matched.
    ///   Green on the nothing-matched case (nothing matched, so everything is
    ///   still reported) and red on the mixed case only.
    #[test]
    fn a_ref_move_that_named_no_ref_is_reported_even_when_another_one_matched() {
        let (before, refs) = linear_trunk();

        let mixed = lay_out_preview(PreviewInput {
            before: before.clone(),
            refs: refs.clone(),
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![
                ("HEAD".into(), oid('9')),
                ("main".into(), oid('9')),
                ("origin/main".into(), oid('9')),
            ],
        });
        assert_eq!(
            mixed.unmatched_ref_moves,
            vec!["origin/main".to_string()],
            "two entries matched and one did not; only the one that did not is \
             reported"
        );

        let nothing_matched = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("origin/main".into(), oid('9')), ("v1.0".into(), oid('9'))],
        });
        assert_eq!(
            nothing_matched.unmatched_ref_moves,
            vec!["origin/main".to_string(), "v1.0".to_string()],
            "reported in the caller's own order, so the caller can find them"
        );
    }

    /// The invariant: an `added` commit with no `ref_moves` is reported as the
    /// caller bug it is — and the *absence* of an added commit is not, however
    /// empty `ref_moves` is.
    ///
    /// The second half of the test also records what the bug actually costs:
    /// lane 1 instead of lane 0. That is the damage `ref_moves` exists to
    /// prevent, asserted here rather than described.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M3a — REMOVES the mechanism.** Hard-code `added_without_ref_moves:
    ///   false`. Red on the first assertion; the no-op case still passes.
    /// * **M3b — WEAKENS the mechanism.** Compute it as `ref_moves.is_empty()`
    ///   alone, dropping the `added.is_some()` conjunct. Green on the first
    ///   assertion and red on the no-op case, which is not a bug and must not
    ///   be reported as one.
    #[test]
    fn an_added_commit_with_no_ref_moves_is_reported_but_an_empty_no_op_is_not() {
        let (before, refs) = linear_trunk();

        let caller_bug = lay_out_preview(PreviewInput {
            before: before.clone(),
            refs: refs.clone(),
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: Vec::new(),
        });
        assert!(
            caller_bug.added_without_ref_moves,
            "a hypothetical commit that no ref points at is a caller bug"
        );
        assert_eq!(
            caller_bug.after.rows[0].commit.id,
            oid('9'),
            "it is still laid out — the report is a diagnosis, not a refusal"
        );
        assert_eq!(
            caller_bug.after.rows[0].lane, 1,
            "and this is what the bug costs: lane 0 is still reserved for the \
             old tip, so the hypothetical commit forks off it"
        );

        let no_op = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: None,
            ref_moves: Vec::new(),
        });
        assert!(
            !no_op.added_without_ref_moves,
            "nothing was added, so there is no commit missing a ref"
        );
    }

    /// The invariant: `lane_shifts` names the commits that changed column, and
    /// only those.
    ///
    /// A fast-forward is the sharp case, and the brief's expectation that it
    /// shifts nothing is wrong: `feature`'s tip sits in lane 1 before (lane 0
    /// is reserved for `main`'s older tip) and in lane 0 after (`main` moved
    /// onto it). That collapse *is* what a fast-forward looks like. Asserting
    /// an empty vector here would have been vacuous — a `lane_shifts` that
    /// always returned `vec![]` would pass it.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M4a — REMOVES the mechanism.** Return `Vec::new()` for
    ///   `lane_shifts`. Red with an empty vector where one shift is expected.
    /// * **M4b — WEAKENS the mechanism.** Drop the `from_lane == row.lane`
    ///   guard and emit a shift for every commit present in both halves. Red
    ///   with *two* entries — the extra one being `1 -> 1` for the root, a
    ///   commit that did not move. Over-reporting, not under-reporting: a
    ///   different failure and a different symptom.
    #[test]
    fn a_fast_forward_collapses_the_branch_tip_into_the_trunk_lane() {
        // `4` is the feature tip, `2` the trunk tip it is ahead of.
        let before = vec![commit('4', 400, &['2']), commit('2', 200, &[])];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '2'),
            git_ref("main", RefKind::Branch, '2'),
            git_ref("feature", RefKind::Branch, '4'),
        ];

        let out = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: None,
            ref_moves: vec![("HEAD".into(), oid('4')), ("main".into(), oid('4'))],
        });

        assert_eq!(
            out.before.rows[0].lane, 1,
            "before: lane 0 is held for main's own tip, so the branch forks right"
        );
        assert_eq!(out.after.rows[0].lane, 0, "after: main is that commit");
        assert_eq!(
            out.lane_shifts,
            vec![LaneShift {
                commit: oid('4'),
                from_lane: 1,
                to_lane: 0,
            }],
            "exactly one commit moved column, and the root that did not move \
             must not be listed"
        );
        assert!(!out.added_without_ref_moves);
    }

    /// The invariant: the hypothetical commit's **time**, not its position in
    /// the list, decides its row — because `stable_topo_order` is a max-heap on
    /// `(time, Reverse(id))` and every childless commit in the window is ready
    /// at once.
    ///
    /// This pins a dependency the design did not have, and it is the reason
    /// `PreviewInput::added` documents a `time` precondition. A caller that
    /// stamps the reverted commit's time (or `0`) onto the hypothetical commit
    /// gets a graph that disagrees with a real run for a reason that has
    /// nothing to do with the layout.
    ///
    /// Both halves are asserted so the test cannot be satisfied by a layout
    /// that ignores time altogether.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M5a — REMOVES the ordering.** In `layout::topology`, replace
    ///   `stable_topo_order(commits)` with `commits` (trust the caller's
    ///   order). The stale-time case then lands the hypothetical at row 0
    ///   because it is first in the vector — red on the second half, green on
    ///   the first.
    /// * **M5b — WEAKENS the ordering.** Change `stable_topo_order`'s heap key
    ///   from `(c.time, Reverse(id))` to `(Reverse(id),)` — topology preserved,
    ///   time ignored. Row 0 is then decided by oid, so `'9'` beats `'4'` in
    ///   both cases: red on the second half *and* on the first, with the
    ///   opposite symptom to M5a.
    #[test]
    fn the_added_commits_time_decides_its_row_not_its_list_position() {
        // A sibling branch tip `4` at t=400 sits beside the trunk tip `3`.
        let before = vec![
            commit('4', 400, &['2']),
            commit('3', 300, &['2']),
            commit('2', 200, &[]),
        ];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '3'),
            git_ref("main", RefKind::Branch, '3'),
            git_ref("feature", RefKind::Branch, '4'),
        ];
        let ref_moves = vec![
            ("HEAD".to_string(), oid('9')),
            ("main".to_string(), oid('9')),
        ];

        let newest = lay_out_preview(PreviewInput {
            before: before.clone(),
            refs: refs.clone(),
            head_branch: Some("main".into()),
            added: Some(commit('9', 500, &['3'])),
            ref_moves: ref_moves.clone(),
        });
        assert_eq!(
            newest.after.rows[0].commit.id,
            oid('9'),
            "stamped newer than every commit in `before`, it takes row 0 — \
             which is what a real run, stamping committer time = now, produces"
        );

        let stale = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 350, &['3'])),
            ref_moves,
        });
        assert_eq!(
            stale.after.rows[0].commit.id,
            oid('4'),
            "stamped older than the sibling tip, the hypothetical commit loses \
             row 0 to it — the caller's `time` is load-bearing and this module \
             deliberately does not clamp it"
        );
        assert_eq!(stale.after.rows[1].commit.id, oid('9'));
    }

    /// The invariant: a hypothetical **merge** lays out over both its parents,
    /// with the second parent fanning strictly rightward and an edge wired to
    /// it.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M6a — REMOVES the mechanism.** In `lay_out_preview`, build
    ///   `after_commits` from `before` alone, dropping `added`. Red on the very
    ///   first assertion: row 0 is the old tip and there is no merge at all.
    /// * **M6b — WEAKENS the mechanism.** In `StreamLayout::push`, `break`
    ///   after the first parent in the extra-parents loop, so a merge's second
    ///   parent reserves no lane. Row 0 is still the merge and its parents are
    ///   still recorded — red only on the second-parent edge and on the
    ///   sibling's lane. A different failure, and one the row assertion alone
    ///   would miss.
    #[test]
    fn a_hypothetical_merge_wires_an_edge_to_its_second_parent() {
        // `3` is the trunk tip, `4` a sibling; both descend from `2`.
        let before = vec![
            commit('3', 300, &['2']),
            commit('4', 250, &['2']),
            commit('2', 200, &['1']),
            commit('1', 100, &[]),
        ];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '3'),
            git_ref("main", RefKind::Branch, '3'),
            git_ref("feature", RefKind::Branch, '4'),
        ];

        let out = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 500, &['3', '4'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });

        assert_eq!(out.after.rows[0].commit.id, oid('9'));
        assert_eq!(out.after.rows[0].lane, 0);
        assert_eq!(out.after.rows[0].commit.parents, vec![oid('3'), oid('4')]);

        // row 1 = `3` (first parent, continues lane 0), row 2 = `4`.
        assert_eq!(out.after.rows[1].commit.id, oid('3'));
        assert_eq!(out.after.rows[1].lane, 0);
        assert_eq!(out.after.rows[2].commit.id, oid('4'));
        assert_eq!(
            out.after.rows[2].lane, 1,
            "the merge's second parent fans strictly rightward"
        );

        assert!(
            out.after.edges.contains(&crate::model::Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 1,
            }),
            "the merge must be wired to its second parent; edges were {:?}",
            out.after.edges
        );
    }

    /// The invariant: **every** row of **both** halves carries
    /// `on_remote: false`.
    ///
    /// Not a field default — a property of the pipeline. The preview is
    /// `layout_with_refs` and nothing else, and the server's remote-membership
    /// stamping pass is deliberately not part of it. Stamping would make the
    /// preview-versus-reality comparison red on its own, because the throwaway
    /// clone the real half is laid out from carries `origin/*` refs the source
    /// repository does not have.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// Both live in `layout/stream.rs`, which this lane does not own — named,
    /// not applied.
    ///
    /// * **M7a — REMOVES the property.** `StreamLayout::push` emits
    ///   `on_remote: true`. Red on every row of both halves.
    /// * **M7b — WEAKENS the property.** `StreamLayout::push` emits
    ///   `on_remote: row == 0`. Red on exactly one row per half — the shape a
    ///   test that only checked `rows[0]`, or only checked `after`, would miss.
    #[test]
    fn no_row_of_either_half_claims_to_be_on_a_remote() {
        let (before, refs) = linear_trunk();
        let out = lay_out_preview(PreviewInput {
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });

        assert_eq!(out.before.rows.len(), 3);
        assert_eq!(out.after.rows.len(), 4);
        for (half, graph) in [("before", &out.before), ("after", &out.after)] {
            for row in &graph.rows {
                assert!(
                    !row.on_remote,
                    "{half} row {} ({}) claims to be on a remote; the preview \
                     pipeline runs no stamping pass",
                    row.row,
                    row.commit.id.short()
                );
            }
        }
    }

    /// The invariant: converting a [`LaneShift`] into a
    /// [`PreviewChange::LaneShifted`] does not transpose the lanes.
    ///
    /// Both fields are `usize`, so a swap compiles, round-trips, and would only
    /// ever show up as an arrow drawn the wrong way. The two lane values here
    /// are deliberately different, and the expected values are literals.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M8a — REMOVES the mapping.** Emit `PreviewChange::Added { commit }`
    ///   from the `From` impl. Red on the variant itself: the match arm does
    ///   not bind.
    /// * **M8b — WEAKENS the mapping.** Swap `from_lane` and `to_lane` in the
    ///   `From` impl. The variant still matches and the commit is still right;
    ///   red on the two lane assertions only.
    #[test]
    fn a_lane_shift_converts_to_a_change_without_transposing_the_lanes() {
        let change: PreviewChange = LaneShift {
            commit: oid('4'),
            from_lane: 3,
            to_lane: 1,
        }
        .into();

        match change {
            PreviewChange::LaneShifted {
                commit,
                from_lane,
                to_lane,
            } => {
                assert_eq!(commit, oid('4'));
                assert_eq!(from_lane, 3);
                assert_eq!(to_lane, 1);
            }
            other => panic!("expected LaneShifted, got {other:?}"),
        }
    }

    /// The three change variants are told apart on the wire by their own tag,
    /// in `snake_case`, with their fields beside it. Literals, one per case.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M9a — REMOVES the tagging.** Drop `#[serde(tag = "change")]`.
    ///   Externally tagged JSON has no `change` key at all, so every `get`
    ///   returns `None`: red on all three.
    /// * **M9b — WEAKENS the tagging.** Drop `rename_all = "snake_case"`. The
    ///   objects keep their shape and their `change` key; only `lane_shifted`
    ///   becomes `LaneShifted` and `ref_moved` becomes `RefMoved`. Red on two
    ///   of the three literals and green on `Added`, which is already its own
    ///   spelling — the near-miss a shape-only assertion would wave through.
    #[test]
    fn each_change_variant_carries_its_own_snake_case_tag() {
        let cases = [
            (PreviewChange::Added { commit: oid('9') }, "added"),
            (
                PreviewChange::RefMoved {
                    ref_name: "main".into(),
                    from: oid('3'),
                    to: oid('9'),
                },
                "ref_moved",
            ),
            (
                PreviewChange::LaneShifted {
                    commit: oid('4'),
                    from_lane: 1,
                    to_lane: 0,
                },
                "lane_shifted",
            ),
        ];

        for (change, expected_tag) in cases {
            let json: serde_json::Value = serde_json::to_value(&change).unwrap();
            assert_eq!(
                json.get("change").and_then(|v| v.as_str()),
                Some(expected_tag),
                "wrong tag for {change:?} — json was {json}"
            );
            let back: PreviewChange = serde_json::from_value(json).unwrap();
            assert_eq!(back, change);
        }
    }
}
