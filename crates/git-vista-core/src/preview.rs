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
//! 2. **Colour is claimed from the refs — and only from *branch* refs.**
//!    `layout::color::assign_branch_colors` seeds from
//!    `refs.iter().filter(|r| r.is_branch())`, and
//!    [`GitRef::is_branch`](crate::model::GitRef::is_branch) is
//!    `RefKind::Branch | RefKind::RemoteBranch` — **never** `RefKind::Head`.
//!    Any commit no *branch* ref claims falls back to a key of
//!    `~<the commit's own short hash>`, so an unclaimed hypothetical commit
//!    takes a colour slot derived from an oid that by construction differs
//!    from the real one.
//!
//!    Moving a **branch** onto it fixes that ([`PreviewInput::ref_moves`],
//!    same field as item 1). Moving only `HEAD` does **not** — and on a
//!    detached HEAD there is no branch to move, so no `ref_moves` list can
//!    fix it. That case is unfixable here rather than merely unfixed, and is
//!    reported: [`PreviewLayout::added_claimed_by_no_branch`]. The server's
//!    `preview::lay_out` reads that report and answers `Unavailable
//!    { CheckFailed }` — a detached HEAD gets no preview of any operation that
//!    writes a commit, which is the price of not drawing a colour a real run
//!    will not use.
//! 3. **Row order is decided by commit *time*, not by list position.**
//!    `stable_topo_order` is a max-heap on `(time, Reverse(id))` under the
//!    topological constraint, so the hypothetical commit competes with every
//!    other branch tip in the window. See [`PreviewInput::added`].
//!
//!    When the tie actually happens — the hypothetical commit sharing its
//!    committer second with an in-window commit that is not one of its own
//!    ancestors — the heap decides row order by comparing object ids, and this
//!    commit's id is one a real run will not write. That is the same shape as
//!    item 2 and gets the same treatment: it is reported,
//!    [`PreviewLayout::added_time_tied`], and the server's
//!    `preview::refusal_for` answers `Unavailable { CheckFailed }`. Unlike item
//!    2 it resolves itself a second later, and the refusal's sentence says so.
//!
//!    Ancestors of the hypothetical commit are excluded from that scan on
//!    purpose. An in-window ancestor keeps an unemitted child for as long as
//!    `added` is in the heap, so `stable_topo_order` never reaches the oid
//!    comparison for it — and without the exclusion the ordinary
//!    commit-then-preview-a-revert path would refuse, since `added`'s own
//!    parent shares its second constantly.

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
    /// lane 0. The colouring pass has a similar dependency one step later: a
    /// commit no **branch** ref claims falls into `assign_branch_colors`'s
    /// synthetic fallback, whose key is `~<the commit's own short hash>` — so
    /// an unrewritten preview and the real commit get different colour slots
    /// for no reason but their different oids.
    ///
    /// So the rewrite happens *here*, from this list, in one place, and
    /// [`PreviewLayout::unmatched_ref_moves`] reports any entry that matched
    /// nothing rather than letting it pass silently.
    ///
    /// # Necessary, and **not** sufficient
    ///
    /// The two passes do not read the same refs. `trunk_reserve_tip` reads
    /// local `main`/`master`/the checked-out branch; `assign_branch_colors`
    /// seeds from every ref where `is_branch()` holds. Neither reads
    /// `RefKind::Head`. So a `ref_moves` list that moves `"HEAD"` and no
    /// branch is applied faithfully, reports nothing here, and still leaves
    /// the hypothetical commit in the synthetic colour fallback.
    ///
    /// On a detached HEAD that is not a caller mistake — the operation really
    /// does move `HEAD` alone, and there is no branch for any list to name.
    /// [`PreviewLayout::added_claimed_by_no_branch`] is the field that reports
    /// it, and it is the general condition of which
    /// [`PreviewLayout::added_without_ref_moves`] is one special case.
    pub ref_moves: Vec<(String, Oid)>,
    /// The most rows the **`after`** graph may hold.
    ///
    /// The caller has already chosen a window when it built
    /// [`before`](Self::before) — the server reads exactly
    /// `PREVIEW_HISTORY_LIMIT` commits — and prepending the hypothetical commit
    /// without truncating pushes `after` one row past it. The real
    /// post-operation view is walked through the same cap, so it holds that
    /// many rows and no more; an untruncated `after` predicts a floor row the
    /// user's own next page load will not draw, and boundary edges and stubs
    /// are computed from the window, so the extra row can move those too.
    ///
    /// Bounds `after` **only**. `before` is the repository as it is right now
    /// and must keep every commit the caller read, so that half still matches a
    /// plain graph view taken at the same instant.
    ///
    /// The truncation drops from the **end** of `added` + `before`, and
    /// `before` arrives newest-first, so the row that falls out is the oldest —
    /// which is the row a real walk of the same width loses once a newer commit
    /// exists. That equivalence assumes the caller honoured
    /// [`added`](Self::added)'s stated `time` precondition; a hypothetical
    /// commit stamped older than the window's floor is already predicting a
    /// different picture for a different reason.
    ///
    /// `usize::MAX` means "do not bound it", which is what a caller laying out
    /// a hand-built window smaller than any cap wants.
    pub history_limit: usize,
}

/// The two layouts and what differs between them.
///
/// # The four report fields are read together, and they are not independent
///
/// **The claim this doc used to make — "a correct preview has *both*
/// empty/false" — was false, and false in the direction that hid a real
/// defect.** A preview taken on a **detached HEAD** has
/// [`unmatched_ref_moves`](Self::unmatched_ref_moves) empty and
/// [`added_without_ref_moves`](Self::added_without_ref_moves) false, and its
/// hypothetical row is still coloured by a hash of an object id that does not
/// exist yet. Two clear fields were read as an exhaustiveness guarantee they
/// never were.
///
/// What is actually true, field by field:
///
/// * `unmatched_ref_moves` non-empty: some `ref_moves` entry named no ref at
///   all, so both the lane-0 reservation and the colour seeding still read the
///   **old** targets. Always a caller mistake.
/// * `added_without_ref_moves` **implies**
///   [`added_claimed_by_no_branch`](Self::added_claimed_by_no_branch): an empty
///   `ref_moves` list moves no branch onto the hypothetical commit, so no
///   branch ref can claim it. The converse does not hold, which is why both
///   fields exist — the implication runs one way only.
/// * `added_claimed_by_no_branch` is the **general** colour condition, and the
///   only one of the three that can be true while the caller did everything
///   right. It is also the only one this module cannot tell the caller how to
///   fix.
///
/// `unmatched_ref_moves` is independent of the other two in both directions: a
/// caller that supplies an `added` commit and a `ref_moves` list whose every
/// entry matched nothing gets `added_without_ref_moves == false` — the list was
/// not empty — and takes the full lane-1-plus-synthetic-colour damage anyway,
/// while a detached-HEAD preview sets `added_claimed_by_no_branch` with
/// `unmatched_ref_moves` empty.
///
/// A preview whose `after` graph a real run would reproduce has all **four**
/// clear.
///
/// The fourth, [`added_time_tied`](Self::added_time_tied), was for one round a
/// precondition with no field at all — `added.time` was documented on
/// [`PreviewInput::added`] and deliberately not enforced. It has a field now,
/// and `preview::refusal_for` refuses on it like the other three. This
/// paragraph said otherwise for two commits after that stopped being true,
/// which is the drift these docs keep shipping: an outside auditor found it,
/// not the round that created it.
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
    /// Holds **only** unmatched ref names. The *other* ways the rewrite can
    /// come out wrong — there are two more, not one — have no ref name to
    /// report and get their own fields,
    /// [`added_without_ref_moves`](Self::added_without_ref_moves) and
    /// [`added_claimed_by_no_branch`](Self::added_claimed_by_no_branch), rather
    /// than a sentinel string in here. One field, one meaning: a `Vec<String>`
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
    ///
    /// Implies [`added_claimed_by_no_branch`](Self::added_claimed_by_no_branch)
    /// and is strictly narrower than it; see that field.
    pub added_without_ref_moves: bool,
    /// `true` when `added` is `Some` and, **after the rewrite**, no ref with
    /// [`is_branch()`](crate::model::GitRef::is_branch) targets it.
    ///
    /// The hypothetical row's colour is then
    /// `stable_color_slot("~<its own short oid>")` — a hash of the one value a
    /// preview may never be compared on, because the preview's oid and the real
    /// operation's oid differ by construction (committer date, default message).
    ///
    /// # Why this is a separate field and not folded into the one above
    ///
    /// `assign_branch_colors` seeds only from `RefKind::Branch` and
    /// `RefKind::RemoteBranch`, never `RefKind::Head`. So a non-empty
    /// `ref_moves` list that names `"HEAD"` alone clears
    /// [`added_without_ref_moves`](Self::added_without_ref_moves), matches a
    /// real ref (so clears [`unmatched_ref_moves`](Self::unmatched_ref_moves)),
    /// and leaves the colour damage in place. On a **detached HEAD** that is
    /// exactly the shape a correct caller produces: `read_refs` emits `"HEAD"`
    /// as its own entry whether or not HEAD is on a branch, and a detached HEAD
    /// moves that one ref and nothing else.
    ///
    /// # This one is reported because it cannot be repaired, not because the
    /// caller erred
    ///
    /// The other two fields name mistakes with fixes: pass the display ref
    /// names `read_refs` emitted, pass the branch the operation moves. This one
    /// has no fix available to either side. A **real** run of the same
    /// operation on a detached HEAD also lands in the synthetic fallback, keyed
    /// on the *real* commit's short oid — an object id that does not exist
    /// until the commit is made. There is therefore no colour the preview could
    /// choose that would be the colour a real run draws, including a "defined"
    /// one: picking any fixed slot would make the preview differ from reality
    /// deliberately rather than accidentally. So the pure half reports, and the
    /// caller decides whether to refuse.
    ///
    /// **The caller in this repository refuses.** The server's
    /// `preview::lay_out` turns this field into
    /// `PreviewUnavailable::CheckFailed`, with a sentence bound to which of the
    /// two causes it found — "HEAD is detached …" only when `head_branch`
    /// really was `None`. That is the consumer this field waited a round for;
    /// a preview of a revert, cherry-pick or merge-commit on a detached HEAD is
    /// therefore *unavailable* rather than mis-coloured. Fixing it so a
    /// detached HEAD previews **correctly** means giving `assign_branch_colors`
    /// a seed for the detached `HEAD` ref, which changes what a *real* run is
    /// painted as well, and so belongs in the colour pass rather than in either
    /// half of this module.
    ///
    /// # Colour only — the lane still agrees, in the detached-HEAD case
    ///
    /// `trunk_reserve_tip` reads local `main`/`master`/the checked-out branch.
    /// On a detached HEAD the preview's rewritten ref slice and the real
    /// repository's refs hold the *same* branch targets (only `HEAD` moved and
    /// `HEAD` is not read), so both sides reserve the same lane 0 and place the
    /// hypothetical commit in the same lane. Colour is the whole of the
    /// divergence — and a colour slot is a visible line colour, not an internal
    /// number.
    ///
    /// That scoping is specific to the detached-HEAD shape. The general
    /// condition can also be met by a caller that moves a branch onto some
    /// *other* commit while adding one here, and lanes may then diverge too.
    pub added_claimed_by_no_branch: bool,
    /// `true` when `added` is `Some` and some commit already in the window
    /// shares its committer second **and could be ready beside it** — the one
    /// state in which `stable_topo_order` decides row order by comparing oid
    /// strings, and the preview's oid is not the oid a real run will write.
    ///
    /// # Why this cannot be computed correctly instead of reported
    ///
    /// `stable_topo_order`'s heap key is `(time, Reverse(id))`, so on an exact
    /// time tie the row order is decided by the hypothetical commit's oid. That
    /// oid differs from the real run's **by construction** — the server's
    /// `commit_tree` writes under a fixed `preview@git-vista.invalid` identity
    /// and cannot pin `GIT_COMMITTER_DATE` — and one hash's lexicographic
    /// relation to a third string carries no information about a *different*
    /// hash's relation to it. So there is no value this module could substitute
    /// that would be the order git draws; a fixed rule ("the new commit always
    /// sorts first") would be wrong exactly as often as a coin flip while
    /// looking deterministic.
    ///
    /// The preview and a real run commonly share a one-second git timestamp,
    /// so this is an ordinary path rather than an exotic one.
    ///
    /// # It is measured, not modelled — and this paragraph used to say otherwise
    ///
    /// The condition is exact: [`crate::layout::topo_order_with_id_ties`] runs
    /// the same walk `layout_with_refs` runs, over the same list, and reports
    /// which ids it decided by comparing id strings. An id comparison happens
    /// in exactly one circumstance — two entries in the heap at the same
    /// instant carrying the same second — and that is a fact about the walk's
    /// state, not about the input. Nothing outside the walk can know it.
    ///
    /// The first version approximated it with "is this same-second commit an
    /// in-window ancestor of `added`?", which is sound and strictly too narrow:
    /// a commit blocked behind any *other* unemitted child also never reaches
    /// the heap beside `added`, and was refused anyway. This paragraph argued
    /// that measuring instead would mean "either teaching `stable_topo_order`
    /// to report its own comparisons or writing its tie logic out a second time
    /// here, where the two copies could drift". The second objection is right.
    /// The first is not, and it is what shipped: one walk, one heap, one key, a
    /// flag deciding only whether the walk records what it observed. There is
    /// no second copy to drift because there is no second copy.
    ///
    /// Pinned by `a_same_second_commit_the_heap_never_reaches_is_not_refused`,
    /// whose two halves differ in one number — the blocking child's committer
    /// time — and go red on opposite sides. **Restoring the ancestor predicate
    /// to make this comment true again reddens that test**, which is the
    /// intended relationship between the two.
    pub added_time_tied: bool,
}

/// Lay out `before`, then lay out the same history with `added` and
/// `ref_moves` applied, and report what differs.
///
/// Order of operations, which is the whole content of this function:
///
/// 1. `before = layout_with_refs(input.before, input.refs, head_branch)`.
/// 2. Build `after_refs` by rewriting every [`GitRef`] whose `name` matches an
///    entry of `ref_moves` **and whose kind can be one** — `is_ref_moves_target`,
///    so a tag or a remote-tracking ref sharing a branch's display name is left
///    where it is. **Before any layout call** — see [`PreviewInput::ref_moves`].
/// 3. `after = layout_with_refs(added.into_iter().chain(before)
///    .take(history_limit), after_refs, head_branch)`. `head_branch` is
///    unchanged: none of revert/cherry-pick/merge changes which branch is
///    checked out. The `take` is not decoration: without it, prepending the
///    hypothetical row to a `before` list the caller already capped returns
///    `history_limit + 1` rows out of a `history_limit`-wide window.
/// 4. `lane_shifts` = for each `after` row whose commit id appears in `before`,
///    emit a [`LaneShift`] when the lanes differ.
/// 5. Report. [`PreviewLayout::added_claimed_by_no_branch`] is read off
///    `after_refs` — the **rewritten** slice, the one `layout_with_refs`
///    actually saw — so it describes the graph that was returned rather than
///    the inputs it was built from. Reading the pre-rewrite `refs` here would
///    report every correct attached-HEAD preview as damaged.
///
/// None of the four report fields is a refusal: this function always returns
/// both graphs. Whether a damaged `after` graph may be shown is the caller's
/// decision. Making a report *fire* as a refusal therefore takes a consumer,
/// and a field no consumer reads is a diagnosis nobody hears —
/// `added_claimed_by_no_branch` was exactly that for one round: computed here,
/// read by nothing but its own test, while a detached-HEAD preview shipped the
/// graph it describes. All four now have the same consumer, the server's
/// `preview::refusal_for`, which refuses on any of them.
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
        history_limit,
    } = input;

    let before_graph = layout_with_refs(before.clone(), refs.clone(), head_branch.as_deref());

    // Step 2, and it must happen before step 3: `layout_with_refs` reserves
    // lane 0 and seeds colour slot 0 from the ref slice it is handed.
    let mut after_refs = refs;
    let mut unmatched_ref_moves = Vec::new();
    for (name, new_target) in &ref_moves {
        let mut matched = false;
        for r in after_refs.iter_mut() {
            // Name **and** kind: `read_refs` flattens branches, remote branches
            // and tags into one display namespace, so a legal repository with
            // `refs/heads/main` and `refs/tags/main` offers two entries called
            // "main" and only one of them is the ref this operation moves.
            if &r.name == name && r.is_ref_moves_target() {
                r.target = new_target.clone();
                matched = true;
            }
        }
        if !matched {
            unmatched_ref_moves.push(name.clone());
        }
    }

    let added_without_ref_moves = added.is_some() && ref_moves.is_empty();

    // Read off `after_refs`, not `refs`: the question is whether the slice
    // `layout_with_refs` is about to be handed contains a branch that claims the
    // hypothetical commit, and only `is_branch()` refs seed `assign_branch_colors`
    // (a `RefKind::Head` entry named "HEAD" seeds nothing, which is the whole of
    // the detached-HEAD case).
    let added_claimed_by_no_branch = match added.as_ref() {
        Some(c) => !after_refs.iter().any(|r| r.is_branch() && r.target == c.id),
        None => false,
    };

    let added_id_for_tie: Option<Oid> = added.as_ref().map(|c| c.id.clone());
    let after_commits: Vec<CommitSummary> =
        // `.take` bounds `after` only — see `PreviewInput::history_limit`. A
        // no-op whenever the caller's window is not full, and exactly the
        // oldest row when it is.
        added.into_iter().chain(before).take(history_limit).collect();
    // The fourth report, and the only one about row *order* rather than about
    // which ref claims the new commit. Measured, not modelled: the same walk
    // `layout_with_refs` runs is asked which ids it decided by comparing id
    // strings, over the exact list it is about to lay out. See
    // `PreviewLayout::added_time_tied`.
    let added_time_tied = match added_id_for_tie.as_ref() {
        Some(id) => crate::layout::topo_order_with_id_ties(after_commits.clone())
            .1
            .contains(id),
        None => false,
    };
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
        added_claimed_by_no_branch,
        added_time_tied,
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

    /// The same trunk, but **HEAD is detached** on its tip: `read_refs` still
    /// emits a `RefKind::Head` entry named `"HEAD"` (it emits one whenever HEAD
    /// resolves, branch or not), and `main` is a separate `RefKind::Branch`
    /// entry on the same commit. The caller's `head_branch` is `None`, which is
    /// what `read_head_branch` returns here.
    ///
    /// The server's `ref_moves_to` builds its list from `read_head_branch`, so
    /// a detached HEAD moves exactly one ref: `"HEAD"`.
    fn detached_at_trunk_tip() -> (Vec<CommitSummary>, Vec<GitRef>) {
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
            history_limit: usize::MAX,
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
        assert!(
            !out.added_claimed_by_no_branch,
            "`main` is a branch ref and the rewrite put it on the hypothetical \
             commit — all three reports are clear, which is what makes this \
             preview reproducible by a real run"
        );
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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

    /// The invariant: `added_claimed_by_no_branch` is true exactly when no
    /// **branch** ref in the rewritten slice targets the hypothetical commit —
    /// which on a detached HEAD is what a *correct* caller produces, and which
    /// the other two report fields do not notice.
    ///
    /// The two arms are the discrimination. A field that was simply always
    /// `true` would pass the detached arm and fail the attached one; a field
    /// that was always `false` fails the detached arm alone.
    ///
    /// # Why each arm is run twice, with two different hypothetical oids
    ///
    /// The property is not "the colour is wrong" — it is **oid-dependence**.
    /// Asserting a single slot number proves nothing about dependence, so each
    /// arm runs the identical input twice changing only the hypothetical
    /// commit's oid, and the arms assert opposite answers: detached differs,
    /// attached does not.
    ///
    /// `stable_color_slot` is `1 + fnv1a(name) % 6`, so an arbitrary pair of
    /// oids can collide onto one slot and make the detached arm green while the
    /// mechanism is broken. `'9'` and `'8'` are chosen because their synthetic
    /// keys land apart and were checked: `stable_color_slot("~9999999") == 3`
    /// and `stable_color_slot("~8888888") == 2`. Both literals are asserted
    /// below, so swapping the digits for a colliding pair makes this test go
    /// red rather than quietly vacuous.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M10a — REMOVES the mechanism.** Hard-code
    ///   `added_claimed_by_no_branch: false` in `lay_out_preview`. Red on the
    ///   detached arm — the case is silently drawn again — and **green** on the
    ///   attached arm. Under-reporting.
    /// * **M10b — WEAKENS the mechanism.** Compute the field from the
    ///   pre-rewrite `refs` instead of `after_refs`. The detached arm stays
    ///   green (no branch claims the commit either way) and the attached arm
    ///   goes red: every correct attached-HEAD preview is now reported as
    ///   damaged, because before the rewrite `main` still points at the old
    ///   tip. Over-reporting — the opposite symptom, and the one that would
    ///   turn a refusal into a refusal of everything.
    ///
    /// The two colour assertions in the detached arm witness the *defect* the
    /// field reports; their mechanism lives in `layout/color.rs`, which this
    /// lane does not own. Mutations there, named and not applied: seeding
    /// `assign_branch_colors` from all refs rather than `is_branch()` ones
    /// would make both runs slot 0 (red on both literals); keying the synthetic
    /// fallback on the row index rather than the short oid would make both runs
    /// one shared slot (red on the literals and on the inequality).
    #[test]
    fn a_detached_head_leaves_the_hypothetical_commit_claimed_by_no_branch() {
        let detached = |digit: char| {
            let (before, refs) = detached_at_trunk_tip();
            lay_out_preview(PreviewInput {
                history_limit: usize::MAX,
                before,
                refs,
                head_branch: None,
                added: Some(commit(digit, 400, &['3'])),
                // A detached HEAD moves exactly one ref, and it is not a branch.
                ref_moves: vec![("HEAD".into(), oid(digit))],
            })
        };
        let attached = |digit: char| {
            let (before, refs) = detached_at_trunk_tip();
            lay_out_preview(PreviewInput {
                history_limit: usize::MAX,
                before,
                refs,
                head_branch: Some("main".into()),
                added: Some(commit(digit, 400, &['3'])),
                ref_moves: vec![("HEAD".into(), oid(digit)), ("main".into(), oid(digit))],
            })
        };

        // --- detached: reported, and the two older fields stay clear ---
        let nine = detached('9');
        let eight = detached('8');

        assert_eq!(nine.after.rows[0].commit.id, oid('9'));
        assert_eq!(eight.after.rows[0].commit.id, oid('8'));

        assert!(
            nine.added_claimed_by_no_branch,
            "HEAD moved onto the hypothetical commit but HEAD is RefKind::Head, \
             which assign_branch_colors does not seed from"
        );
        assert!(eight.added_claimed_by_no_branch);

        assert_eq!(
            nine.unmatched_ref_moves,
            Vec::<String>::new(),
            "the \"HEAD\" entry matched a real ref, so the caller made no \
             naming mistake — this is the field that used to be read as an \
             all-clear"
        );
        assert!(
            !nine.added_without_ref_moves,
            "ref_moves was not empty, so this field is false too — both of the \
             older fields are clear for a preview that is nonetheless wrong"
        );

        // The defect the field reports: same input, different oid, different
        // colour. Literals, not a re-derivation.
        assert_eq!(
            nine.after.rows[0].color, 3,
            "the synthetic fallback keyed on ~9999999"
        );
        assert_eq!(
            eight.after.rows[0].color, 2,
            "the synthetic fallback keyed on ~8888888"
        );
        assert_ne!(
            nine.after.rows[0].color, eight.after.rows[0].color,
            "the hypothetical row's colour moves with its oid — and the real \
             run's oid is not this one"
        );

        // --- attached: not reported, and the colour does not move ---
        let nine_attached = attached('9');
        let eight_attached = attached('8');

        assert!(
            !nine_attached.added_claimed_by_no_branch,
            "`main` is a RefKind::Branch and the rewrite put it on the \
             hypothetical commit, so the trunk colour claims it"
        );
        assert!(!eight_attached.added_claimed_by_no_branch);

        assert_eq!(
            nine_attached.after.rows[0].color, 0,
            "slot 0 is the trunk colour, and it is a pure function of the name \
             `main` — not of any oid"
        );
        assert_eq!(eight_attached.after.rows[0].color, 0);

        // --- the one-way implication the field docs claim ---
        let (before, refs) = detached_at_trunk_tip();
        let no_ref_moves = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: Vec::new(),
        });
        assert!(no_ref_moves.added_without_ref_moves);
        assert!(
            no_ref_moves.added_claimed_by_no_branch,
            "`added_without_ref_moves` implies this field — an empty ref_moves \
             list moves no branch onto the commit, so no branch can claim it. \
             A field that did not hold this would make the two reports \
             contradict each other on one input"
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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
            history_limit: usize::MAX,
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

    /// **A tag that shares a branch's display name must not be moved.**
    ///
    /// `read_refs` shortens local branches, remote branches and tags into ONE
    /// display namespace (`git-vista-git/src/refs.rs`, the
    /// `category_and_short_name` match): `refs/heads/main` and `refs/tags/main`
    /// both arrive as `name: "main"`, told apart only by `kind`. The rewrite
    /// loop matched on `name` alone and had no `break`, so a `ref_moves` entry
    /// for the branch `main` rewrote the tag as well — and the after graph then
    /// drew the tag badge on a commit real `git revert` would never move it to,
    /// with every diagnostic field clear.
    ///
    /// `ref_moves` only ever names the checked-out local branch and `"HEAD"`
    /// (the server's `ref_moves_to` is its sole production constructor), so
    /// restricting the rewrite to `RefKind::Head`/`RefKind::Branch` changes
    /// nothing for any list production actually builds.
    ///
    /// The assertion is on the **badges**, not on the predicate: a badge is
    /// attached by target oid, so "the tag is still on commit `2`" is the
    /// observable fact a user would see, and it is checked against literals.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M10a — REMOVES the mechanism.** Drop the kind filter from the
    ///   rewrite loop. The tag moves onto `9`, so row 0 badges read
    ///   `["HEAD", "main", "main"]` and row 2 loses its badge: red on both
    ///   halves.
    /// * **M10b — WEAKENS the mechanism.** Filter on `is_branch()` instead,
    ///   which admits `RefKind::RemoteBranch` and — more to the point here —
    ///   excludes `RefKind::Head`. The tag is left alone so the row-2
    ///   assertion stays green, and the `HEAD` badge never leaves the old tip:
    ///   red on row 0's badge list alone.
    #[test]
    fn the_ref_rewrite_leaves_a_tag_that_shares_a_branchs_display_name_alone() {
        let before = vec![
            commit('3', 300, &['2']),
            commit('2', 200, &['1']),
            commit('1', 100, &[]),
        ];
        // The legal collision: branch `main` on the tip, tag `main` two
        // commits back. One display namespace, two different refs.
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '3'),
            git_ref("main", RefKind::Branch, '3'),
            git_ref("main", RefKind::Tag, '2'),
        ];

        let out = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });

        assert_eq!(out.after.rows[0].commit.id, oid('9'));
        assert_eq!(
            badge_names(&out.after.rows, 0),
            vec!["HEAD", "main"],
            "HEAD and the BRANCH main moved onto the hypothetical commit — the \
             tag did not, so exactly two badges belong here"
        );
        assert_eq!(
            out.after.rows[0]
                .refs
                .iter()
                .map(|r| r.kind.clone())
                .collect::<Vec<_>>(),
            vec![RefKind::Head, RefKind::Branch],
            "and neither of them is the tag"
        );

        assert_eq!(out.after.rows[2].commit.id, oid('2'));
        assert_eq!(
            badge_names(&out.after.rows, 2),
            vec!["main"],
            "`git revert` moves the branch and HEAD and nothing else, so the \
             tag must still be drawn on commit 2"
        );
        assert_eq!(
            out.after.rows[2].refs[0].kind,
            RefKind::Tag,
            "and the badge left on commit 2 is the tag, not some other ref"
        );

        assert_eq!(
            out.unmatched_ref_moves,
            Vec::<String>::new(),
            "both entries matched a ref of a movable kind"
        );
    }

    /// A trunk with an **independent competitor tip** stamped at the same
    /// second the hypothetical commit will carry: `4` (tip of `side`, t=400)
    /// and `3` (tip of `main`, t=300) -> `2`. `4` has no in-window child, so it
    /// is ready from the start and competes with the hypothetical commit for
    /// row 0 on `(time, Reverse(id))` alone.
    fn trunk_with_a_competitor_tip_at_400() -> (Vec<CommitSummary>, Vec<GitRef>) {
        let commits = vec![
            commit('4', 400, &['2']),
            commit('3', 300, &['2']),
            commit('2', 200, &[]),
        ];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '3'),
            git_ref("main", RefKind::Branch, '3'),
            git_ref("side", RefKind::Branch, '4'),
        ];
        (commits, refs)
    }

    /// A same-second competitor that is **blocked behind its own child**.
    ///
    /// `4` shares the hypothetical commit's second and is not one of its
    /// ancestors — the two facts the old ancestor-based predicate looked at,
    /// and on those alone it refused. But `5` is `4`'s child and has not been
    /// emitted, so `4` is not in the heap when the hypothetical commit is
    /// popped and the two oids are never compared. `blocker_time` is the whole
    /// experiment: below the hypothetical's second the block holds, above it
    /// the block clears and the tie is real.
    fn competitor_blocked_behind_its_child(blocker_time: i64) -> (Vec<CommitSummary>, Vec<GitRef>) {
        let commits = vec![
            commit('5', blocker_time, &['4']),
            commit('4', 400, &['2']),
            commit('3', 300, &['2']),
            commit('2', 200, &[]),
        ];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '3'),
            git_ref("main", RefKind::Branch, '3'),
            git_ref("side", RefKind::Branch, '5'),
        ];
        (commits, refs)
    }

    /// **A commit that shares the second but can never be compared is not
    /// refused.** #576 finding 6's first fix asked "is this same-second commit
    /// an in-window ancestor of the new one?" That is sound and strictly too
    /// narrow: a commit blocked behind any *other* unemitted child also never
    /// reaches the heap beside the hypothetical commit, and was refused anyway.
    /// An outside auditor found the needless refusal; this is the case it
    /// named.
    ///
    /// # The two halves differ in ONE number, and it is the mechanism's number
    ///
    /// Both halves use the same four commits, the same refs and the same
    /// hypothetical commit. The only difference is the blocking child's
    /// committer time, and it decides whether the block is still standing when
    /// the hypothetical commit is popped:
    ///
    /// * **350 — below.** The hypothetical commit is the newest ready entry and
    ///   is emitted first; `4` only becomes ready afterwards. No comparison
    ///   happens, so there is nothing to refuse.
    /// * **450 — above.** `5` outranks the hypothetical commit and is emitted
    ///   first, which frees `4` into the heap while the hypothetical commit is
    ///   still in it. Now both carry second 400 and the oid decides the row.
    ///
    /// A predicate that cannot see the heap cannot tell those two apart — the
    /// commits, the refs, the ancestry and the shared second are identical in
    /// both. That is why the report is measured off the walk rather than
    /// modelled from the input, and it is why this test needs both halves: the
    /// first alone would pass for a report that never fires, the second alone
    /// for one that always does.
    #[test]
    fn a_same_second_commit_the_heap_never_reaches_is_not_refused() {
        let (blocked, refs) = competitor_blocked_behind_its_child(350);
        let clear = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before: blocked,
            refs: refs.clone(),
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });
        assert!(
            !clear.added_time_tied,
            "`4` shares the second and is no ancestor, but its child `5` is \
             still unemitted, so `4` is not in the heap beside the new commit \
             and no oid is compared. Refusing here would turn the feature off \
             on a preview that is perfectly determinate"
        );

        let (unblocked, refs) = competitor_blocked_behind_its_child(450);
        let tied = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before: unblocked,
            refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });
        assert!(
            tied.added_time_tied,
            "moving the blocking child ABOVE the new commit's second frees `4` \
             into the heap while the new commit is still there — the same four \
             commits, and now the oid really does decide row order"
        );
    }

    /// **The defect, measured before the refusal that answers it.** Two
    /// previews of the same operation on the same history, differing in
    /// **nothing but the hypothetical commit's oid**, put a different commit in
    /// row 0.
    ///
    /// `stable_topo_order`'s heap key is `(time, Reverse(id))`, so once the new
    /// commit and an independent tip share a committer second the oid decides
    /// the order — and the preview's oid is not the one a real run writes
    /// (`commit_tree` uses a fixed `preview@git-vista.invalid` identity and
    /// cannot pin `GIT_COMMITTER_DATE`). Rows, lanes and edge coordinates all
    /// hang off that order.
    ///
    /// This test asserts the disagreement, not a preferred answer: there is no
    /// correct row order to assert, which is the whole finding.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M12a — REMOVES the tie.** Stamp the competitor tip `4` at 401 or
    ///   399 instead of 400. Both previews then agree and `assert_ne!` fires:
    ///   the test proves it is the *tie* doing this, not the oid alone.
    /// * **M12b — REMOVES the competitor.** Drop `4` from the fixture. Row 0 is
    ///   forced by topology, both previews agree, and `assert_ne!` fires again
    ///   — the other half of the same claim.
    #[test]
    fn a_same_second_tie_lets_the_hypothetical_oid_decide_row_zero() {
        let (before, refs) = trunk_with_a_competitor_tip_at_400();
        let preview_of = |digit: char| {
            lay_out_preview(PreviewInput {
                history_limit: usize::MAX,
                before: before.clone(),
                refs: refs.clone(),
                head_branch: Some("main".into()),
                added: Some(commit(digit, 400, &['3'])),
                ref_moves: vec![("HEAD".into(), oid(digit)), ("main".into(), oid(digit))],
            })
        };

        // "111…" sorts below "444…", "999…" above it. Nothing else differs.
        let low = preview_of('1');
        let high = preview_of('9');

        assert_eq!(
            low.after.rows[0].commit.id,
            oid('1'),
            "the smaller oid wins the tie, so the hypothetical commit takes row 0"
        );
        assert_eq!(
            high.after.rows[0].commit.id,
            oid('4'),
            "the larger oid loses it, and the competitor tip takes row 0 instead"
        );
        assert_ne!(
            low.after.rows[0].commit.id, high.after.rows[0].commit.id,
            "row 0 changed hands on nothing but the hypothetical oid — the one \
             value a preview may never be compared on"
        );
    }

    /// **The report that answers it**, in all three directions.
    ///
    /// The tie flag must fire when an independent commit shares the second, and
    /// must NOT fire when nothing shares it, and must NOT fire when the only
    /// same-second commit is an in-window **ancestor** of the new one. That
    /// third case is the ordinary path — commit, then immediately preview a
    /// revert of it — and a flag that fired there would refuse nearly every
    /// real preview.
    ///
    /// The exclusion is sound, not a guess: a commit reachable from `added`
    /// through in-window parents has an unemitted child (`added` itself, or
    /// something between) for as long as `added` is in the heap, so
    /// `stable_topo_order` never compares their oids.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M13a — REMOVES the mechanism.** Return `false` unconditionally. Red
    ///   on the first case alone.
    /// * **M13b — WEAKENS the mechanism.** Drop the ancestor exclusion and scan
    ///   the whole of `before`. Red on the third case alone — the one that
    ///   decides whether this refusal is usable at all.
    #[test]
    fn the_tie_report_fires_on_an_independent_commit_and_not_on_an_ancestor() {
        let (before, refs) = trunk_with_a_competitor_tip_at_400();

        let tied = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before: before.clone(),
            refs: refs.clone(),
            head_branch: Some("main".into()),
            added: Some(commit('9', 400, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });
        assert!(
            tied.added_time_tied,
            "`4` is an independent tip at the same second: the heap compares \
             the two oids and the preview's is not the real one"
        );

        let clear = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before: before.clone(),
            refs: refs.clone(),
            head_branch: Some("main".into()),
            added: Some(commit('9', 450, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });
        assert!(
            !clear.added_time_tied,
            "at 450 nothing shares the second, so no oid is ever compared and \
             there is nothing to refuse"
        );

        // The ordinary path: the only commit at the same second is the new
        // commit's own parent, which cannot be ready beside it.
        let (linear, linear_refs) = linear_trunk();
        let ancestor_only = lay_out_preview(PreviewInput {
            history_limit: usize::MAX,
            before: linear,
            refs: linear_refs,
            head_branch: Some("main".into()),
            added: Some(commit('9', 300, &['3'])),
            ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
        });
        assert!(
            !ancestor_only.added_time_tied,
            "commit 3 is the new commit's parent, stamped in the same second — \
             it is blocked behind it in the heap and their oids are never \
             compared. Refusing here would refuse nearly every real preview"
        );
    }

    /// **The `after` graph may not be one row taller than the window (#576
    /// finding 7).**
    ///
    /// The caller reads a fixed number of commits — the server reads exactly
    /// `PREVIEW_HISTORY_LIMIT` — and the real post-operation view is walked
    /// through that same cap, so it holds that many rows. Prepending the
    /// hypothetical commit without truncating predicts one row more than the
    /// user's own next page load will ever draw, and the extra floor row also
    /// changes which parents fall outside the window, so boundary edges and
    /// stubs move with it.
    ///
    /// Both directions are asserted. At the cap the oldest row must be *gone*
    /// and the count must not grow; one row under it the count must grow by
    /// one, because the truncation is a bound and not a fixed size.
    ///
    /// # Two mutations that make this red, failing differently
    ///
    /// * **M14a — REMOVES the mechanism.** Drop the `.take(history_limit)`.
    ///   `after` holds six rows at a limit of five: red on the first count and
    ///   on the "oldest is gone" assertion, green on the under-cap half.
    /// * **M14b — WEAKENS the mechanism.** `.take(history_limit + 1)`, the
    ///   off-by-one this finding *is*. Identical symptom at the cap, and it
    ///   also leaves the under-cap half green — which is why the under-cap
    ///   half alone could never pin this.
    #[test]
    fn the_after_graph_is_bounded_by_the_window_the_caller_read() {
        let before = vec![
            commit('5', 500, &['4']),
            commit('4', 400, &['3']),
            commit('3', 300, &['2']),
            commit('2', 200, &['1']),
            commit('1', 100, &[]),
        ];
        let refs = vec![
            git_ref("HEAD", RefKind::Head, '5'),
            git_ref("main", RefKind::Branch, '5'),
        ];
        let at = |history_limit: usize| {
            lay_out_preview(PreviewInput {
                before: before.clone(),
                refs: refs.clone(),
                head_branch: Some("main".into()),
                added: Some(commit('9', 600, &['5'])),
                ref_moves: vec![("HEAD".into(), oid('9')), ("main".into(), oid('9'))],
                history_limit,
            })
        };

        // At the cap: the window is full, so a new commit costs the oldest one.
        let full = at(5);
        assert_eq!(
            full.after.rows.len(),
            5,
            "the caller read five commits and a real walk of the same width \
             after the operation returns five rows, not six"
        );
        assert_eq!(full.after.rows[0].commit.id, oid('9'));
        assert_eq!(
            full.after.rows[4].commit.id,
            oid('2'),
            "the row that falls out is the OLDEST — `before` is newest-first, \
             so truncating its tail is what a re-walk of the same width does"
        );
        assert!(
            full.after.rows.iter().all(|r| r.commit.id != oid('1')),
            "commit 1 is off the bottom of the window now: {:?}",
            full.after
                .rows
                .iter()
                .map(|r| &r.commit.id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            full.before.rows.len(),
            5,
            "the `before` half is the repository as it IS — the cap on `after` \
             must not shrink it, or the current-state graph stops matching a \
             plain graph view taken at the same instant"
        );
        assert_eq!(
            full.before.rows[4].commit.id,
            oid('1'),
            "and commit 1 is still in it"
        );

        // One under the cap: nothing to drop, so the graph grows by one.
        let roomy = at(6);
        assert_eq!(
            roomy.after.rows.len(),
            6,
            "with room left the bound does nothing and the new commit is added"
        );
        assert_eq!(roomy.after.rows[5].commit.id, oid('1'));
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
