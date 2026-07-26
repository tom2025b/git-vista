//! The streaming branch-claim classifier paged history uses instead of the
//! whole-graph colour/stub/badge passes.
//!
//! [`super::color::assign_branch_colors`] needs the whole graph: it indexes every
//! commit to a row, sorts every branch ref by a priority key that contains a tip
//! *row*, and then walks first-parent chains from each tip. A page has none of
//! that — it holds `k` rows out of an unbounded history and must produce exactly
//! the same colours, badges and stubs for those rows as the whole-graph algorithm
//! would.
//!
//! So invert the algorithm. Instead of "for each seed, walk its chain", do "for
//! each row, take the best claim standing at it":
//!
//! 1. Collect the claims present at this row — the winning claim propagated down
//!    from the child whose **first parent** this row is, every branch ref whose
//!    tip *is* this row, and a synthetic `~<short-id>` claim of last resort.
//! 2. The winner is the smallest by the established priority key
//!    `(trunk_rank, is_remote, emitted_tip_row, branch_name)`. Its slot is the
//!    row's colour.
//! 3. Only the winner propagates, and only along the first parent.
//! 4. A **local** branch whose own tip claim lost is a [`FrameStub`] anchored at
//!    this commit by OID: it owns no commits, so it is drawn as its own short
//!    line instead of a second badge on the shared commit.
//!
//! That is provably the same answer — a claim can only enter a row as a ref tip
//! or by first-parent propagation from a strictly smaller row, and the
//! whole-graph passes process seeds in exactly this key order, so "the earliest
//! seed whose chain reaches this row" and "the smallest key standing at this
//! row" name the same claim. [`layout::tests::replay`](super::tests) proves it
//! against every colour/stub fixture in the crate, at every page boundary.
//!
//! ## Prefix replay
//!
//! Paging is stateless: a page starting at row `n` re-walks `[0, n)` to rebuild
//! this state. Those rows call [`ReplayClassifier::decorate`] with
//! `emit = false`, which reconstructs claims and **still advances the cumulative
//! stub offset** while returning no stubs and attaching no badges. Page 2's stub
//! columns are numbered off page 1's suppressed rows, so a prefix that skipped
//! the offset bookkeeping would silently collide two stubs in one column.
//!
//! ## Two faithful expansions of the whole-graph key
//!
//! * **Synthetic claims rank below every branch claim.** The whole-graph pass
//!   runs the `~<short-id>` fallback strictly *after* every branch seed
//!   ([`color`](super::color) lines 154-162), so no synthetic chain can ever
//!   pre-empt a branch chain whatever their rows. A 4-value `trunk_rank` cannot
//!   express that, so synthetics take `trunk_rank` 4 and are otherwise keyed
//!   identically; synthetic-versus-synthetic ties fall to `emitted_tip_row`,
//!   which reproduces the whole-graph pass's top-to-bottom row loop.
//! * **Same-anchor stubs are ordered by the whole priority key, not by name
//!   alone.** The whole-graph cascade emits same-anchor stubs in
//!   priority-sorted seed order ([`super::decorate`]), and at one anchor
//!   `is_remote` and `emitted_tip_row` are constant — so the key reduces to name
//!   order whenever the losers share a `trunk_rank` (every pre-existing fixture),
//!   while a checked-out branch (rank 2) losing beside an ordinary branch
//!   (rank 3) still matches the whole-graph order.
//!
//! ## Known divergence: stub columns across *different* anchors
//!
//! The whole-graph cascade numbers stub columns in priority-sorted **seed**
//! order, which is anchor-row order only while all stubs share a `trunk_rank`.
//! A streaming classifier necessarily numbers them in **row** order, because it
//! emits each stub on the page that owns its anchor and cannot see later rows.
//! The two therefore disagree on `lane_offset` (never on name, anchor, colour or
//! depth) for the one shape where a higher-ranked loser sits at a *larger* row
//! than a lower-ranked one — e.g. local `master` demoted to a stub at row 4 while
//! an ordinary branch is demoted at row 2. Paged history has no way to reproduce
//! the legacy numbering there; recorded as a Task 4 conflict.

use std::collections::HashMap;

use crate::color::stable_color_slot;
use crate::model::{FrameStub, GitRef, GraphRow, Oid, RefKind};

/// Local `main` — always the trunk when it exists (issue #30).
const RANK_MAIN: u8 = 0;
/// Local `master` — the trunk when there is no local `main`.
const RANK_MASTER: u8 = 1;
/// The checked-out local branch — the trunk of last resort.
const RANK_CHECKED_OUT: u8 = 2;
/// Any other branch ref, local or remote (`is_remote` splits those).
const RANK_BRANCH: u8 = 3;
/// A synthetic `~<short-id>` line. Ranks below **every** branch claim because
/// the whole-graph pass only runs the fallback once all branch seeds are done.
const RANK_SYNTHETIC: u8 = 4;

/// The established colouring priority, in comparison order. Smaller wins.
///
/// `emitted_tip_row` is the row of the claim's *origin* — the ref tip (or the
/// synthetic line's own commit) — and it rides along unchanged as the claim
/// propagates, exactly as the whole-graph seed order is fixed before any chain
/// is walked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClaimKey {
    trunk_rank: u8,
    is_remote: bool,
    emitted_tip_row: usize,
    name: String,
}

/// A claim standing at a row: who it is, and the palette slot it paints.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    key: ClaimKey,
    slot: usize,
}

/// The ref-independent half of a branch ref's claim, computed once in
/// [`ReplayClassifier::new`]. `None` for HEAD and tags, which badge but never
/// claim.
#[derive(Debug, Clone)]
struct Seed {
    trunk_rank: u8,
    is_remote: bool,
    /// The name's stable slot — 0 for the trunk. Never recomputed per row.
    slot: usize,
}

/// Row-at-a-time colouring, badging and stub classification for paged history.
///
/// Construct one per walk, then call [`decorate`](Self::decorate) on every row
/// in row order (newest first, absolute rows). The rows before the page's first
/// row are replayed with `emit = false`.
pub struct ReplayClassifier {
    /// Every ref, in the caller's input order — the order badges are attached in.
    refs: Vec<GitRef>,
    /// Ref indices by target commit, each list in input order.
    by_target: HashMap<Oid, Vec<usize>>,
    /// Parallel to `refs`: the branch-claim data, or `None` for HEAD/tags.
    seeds: Vec<Option<Seed>>,
    /// Winning claims waiting at a not-yet-walked first parent. At most one per
    /// commit: when several children share a first parent, the smallest key wins
    /// it, which is the seed the whole-graph pass would have run first.
    pending: HashMap<Oid, Claim>,
    /// Stub columns handed out so far, across every row including suppressed
    /// ones. A stub's absolute lane is the page-final commit-lane high-water plus
    /// this.
    next_stub_offset: usize,
}

impl ReplayClassifier {
    /// Prepare the classifier from the snapshot's refs and checked-out branch.
    ///
    /// `O(refs)` and repository-free: the trunk decision and every branch's slot
    /// are pure functions of the ref names, so a Frame can be answered without
    /// walking anything.
    pub fn new(refs: &[GitRef], head_branch: Option<&str>) -> Self {
        let has_local = |name: &str| {
            refs.iter()
                .any(|r| matches!(r.kind, RefKind::Branch) && r.name == name)
        };
        // The branch that owns slot 0 — the same priority `trunk_reserve_tip`
        // uses for lane 0, so lane 0 and slot 0 always describe one line.
        let trunk_name: Option<&str> = if has_local("main") {
            Some("main")
        } else if has_local("master") {
            Some("master")
        } else {
            head_branch.filter(|h| has_local(h))
        };

        let seeds = refs
            .iter()
            .map(|r| {
                if !r.is_branch() {
                    return None;
                }
                let is_local = matches!(r.kind, RefKind::Branch);
                let trunk_rank = if is_local && r.name == "main" {
                    RANK_MAIN
                } else if is_local && r.name == "master" {
                    RANK_MASTER
                } else if is_local && head_branch == Some(r.name.as_str()) {
                    RANK_CHECKED_OUT
                } else {
                    RANK_BRANCH
                };
                let slot = if Some(r.name.as_str()) == trunk_name {
                    0
                } else {
                    stable_color_slot(&r.name)
                };
                Some(Seed {
                    trunk_rank,
                    is_remote: matches!(r.kind, RefKind::RemoteBranch),
                    slot,
                })
            })
            .collect();

        let mut by_target: HashMap<Oid, Vec<usize>> = HashMap::new();
        for (i, r) in refs.iter().enumerate() {
            by_target.entry(r.target.clone()).or_default().push(i);
        }

        Self {
            refs: refs.to_vec(),
            by_target,
            seeds,
            pending: HashMap::new(),
            next_stub_offset: 0,
        }
    }

    /// Classify one row: set its `color`, attach its badges, and hand back the
    /// stubs anchored on it.
    ///
    /// Rows must arrive in row order (newest first), each exactly once — the same
    /// order [`StreamLayout`](super::stream::StreamLayout) produces them in.
    ///
    /// With `emit = false` this is **prefix replay**: the claim map and the
    /// cumulative stub offset advance exactly as they would for an emitted row,
    /// but no badge is attached and no stub is returned. `row.color` is still
    /// written (the caller discards the row).
    pub fn decorate(&mut self, row: &mut GraphRow, emit: bool) -> Vec<FrameStub> {
        let id = row.commit.id.clone();
        let at_this_row: Vec<usize> = self.by_target.get(&id).cloned().unwrap_or_default();

        // Every branch ref whose tip is this very row, in input order.
        let tip_claims: Vec<(usize, Claim)> = at_this_row
            .iter()
            .filter_map(|&i| {
                self.seeds[i].as_ref().map(|seed| {
                    (
                        i,
                        Claim {
                            key: ClaimKey {
                                trunk_rank: seed.trunk_rank,
                                is_remote: seed.is_remote,
                                emitted_tip_row: row.row,
                                name: self.refs[i].name.clone(),
                            },
                            slot: seed.slot,
                        },
                    )
                })
            })
            .collect();

        // The claim of last resort: this commit's own synthetic line. It loses to
        // every branch claim, and to any synthetic propagated from a smaller row
        // (children always sit above their parents), so keeping it unconditionally
        // is the same as only minting it when nothing else stands here.
        let synthetic_name = format!("~{}", id.short());
        let mut winner = Claim {
            key: ClaimKey {
                trunk_rank: RANK_SYNTHETIC,
                is_remote: false,
                emitted_tip_row: row.row,
                name: synthetic_name.clone(),
            },
            slot: stable_color_slot(&synthetic_name),
        };
        if let Some(propagated) = self.pending.remove(&id) {
            if propagated.key < winner.key {
                winner = propagated;
            }
        }
        for (_, candidate) in &tip_claims {
            if candidate.key < winner.key {
                winner = candidate.clone();
            }
        }

        row.color = winner.slot;

        // Only the winner carries on, and only down the first parent — that is
        // what gives a branch one colour for its whole mainline. A parent the walk
        // never reaches (out of window, or cut at a shallow boundary) simply never
        // collects its claim.
        if let Some(first_parent) = row.commit.parents.first() {
            let supersedes = match self.pending.get(first_parent) {
                Some(existing) => winner.key < existing.key,
                None => true,
            };
            if supersedes {
                self.pending.insert(first_parent.clone(), winner.clone());
            }
        }

        // A local branch whose own tip claim lost owns no commits: it becomes a
        // stub forking off this dot rather than a second badge on it. Remote refs
        // in the same position stay ordinary badges (priority puts every local
        // before every remote, so a local tip is never pre-claimed by a remote).
        let mut losers: Vec<(usize, Claim)> = tip_claims
            .into_iter()
            .filter(|(i, claim)| {
                matches!(self.refs[*i].kind, RefKind::Branch) && claim.key != winner.key
            })
            .collect();
        losers.sort_by(|a, b| a.1.key.cmp(&b.1.key));

        let mut stub_names: Vec<String> = Vec::with_capacity(losers.len());
        let mut stubs = Vec::new();
        for (depth, (i, _)) in losers.iter().enumerate() {
            let name = self.refs[*i].name.clone();
            let lane_offset = self.next_stub_offset;
            // Advance whether or not this page emits: a suppressed prefix row's
            // stub still owns its column, and page 2 numbers from where page 1
            // left off.
            self.next_stub_offset += 1;
            if emit {
                stubs.push(FrameStub {
                    name: name.clone(),
                    anchor_commit: id.clone(),
                    lane_offset,
                    // A stub wears its *name's* slot — the very colour its line
                    // will have once it owns a commit — never the trunk's 0, which
                    // it cannot hold anyway (the trunk is seeded first and so is
                    // never demoted to a stub).
                    color: stable_color_slot(&name),
                    depth,
                });
            }
            stub_names.push(name);
        }

        if emit {
            for &i in &at_this_row {
                let r = &self.refs[i];
                if matches!(r.kind, RefKind::Branch) && stub_names.iter().any(|n| n == &r.name) {
                    continue; // drawn as a stub line, not a badge
                }
                row.refs.push(r.clone());
            }
        }

        stubs
    }

    /// The stable named slots a Frame ships: every branch ref, in input order,
    /// paired with the palette slot its name owns.
    ///
    /// This is the *named* half of the colouring only — synthetic lines have no
    /// ref to name them, so `GraphRow::color` stays authoritative per row.
    pub fn branch_colors(&self) -> Vec<(String, usize)> {
        self.refs
            .iter()
            .zip(&self.seeds)
            .filter_map(|(r, seed)| seed.as_ref().map(|s| (r.name.clone(), s.slot)))
            .collect()
    }
}
