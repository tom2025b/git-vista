//! Per-branch colouring: the first-parent-chain walk that gives every commit a
//! stable branch colour *slot*.
//!
//! Split out of `layout.rs`: this is the colour half of the layout — it runs
//! after the [topology pass](super::topology) has placed every commit, and
//! also decides which local branches own no commits of their own and so become
//! stub lines (returned to the parent to draw). The palette and the slot
//! function themselves live in the crate-wide [`crate::color`] "Color God" — the
//! single source of truth this pass calls into; [`assign_branch_colors`] is
//! `pub(super)` for the layout entry points.

use std::collections::HashMap;

use crate::color::stable_color_slot;
use crate::model::{GitRef, Graph, Oid};

/// Give every commit a stable per-branch [`color`](crate::model::GraphRow::color)
/// palette slot.
///
/// A "branch" here is a **first-parent chain**: starting from a branch tip we
/// walk first-parent links down until we reach a commit another branch already
/// owns (the merge base), claiming each commit for that branch's colour. So a
/// branch keeps one colour for its whole mainline, and that colour is the same
/// everywhere the branch appears — independent of which lane it sits in (lanes
/// get reused; colours don't).
///
/// Branch tips (from `refs`) seed the colouring in priority order: the trunk
/// first (local `main`, then `master`, then the checked-out branch — it owns
/// slot 0, the one blue line, and never becomes a stub), then local branches,
/// then remote ones. Any commit still unclaimed afterwards (e.g. commits of a
/// deleted branch, reachable only as a merge's second parent) starts its own
/// synthetic line, walked the same way, so **every** commit ends up coloured.
///
/// Every non-trunk slot is [`stable_color_slot`] of the branch name — a pure
/// function of the name, not of allocation order — so a branch keeps its colour
/// whatever else changes in the repo, and a stub's colour equals the colour of
/// the line it becomes on its first commit. (Slots can collide; stable-but-
/// shared beats distinct-but-shuffling, which read as "main keeps changing
/// colour" in testing.)
///
/// Returns the stub seeds: local branches that owned no commits of their own —
/// their tip was already claimed by a higher-priority branch (e.g. a branch
/// just created from an existing commit) — each as `(name, anchor_row)`.
pub(super) fn assign_branch_colors(
    graph: &mut Graph,
    refs: &[GitRef],
    head_branch: Option<&str>,
) -> Vec<(String, usize)> {
    let index: HashMap<&Oid, usize> = graph.rows.iter().map(|r| (&r.commit.id, r.row)).collect();

    // Branch refs, in colouring priority. The order decides who *owns* a shared
    // first-parent chain (and so who takes the trunk colour and who is demoted to a
    // stub), so it matters a lot:
    //
    //  1. **The trunk first**, so it owns colour slot 0 — the one blue line. That
    //     is `main` (then `master`) whenever a local one exists, so `main` is
    //     *always* blue regardless of which branch happens to be checked out
    //     (Issue #30). Only if neither exists do we fall back to the checked-out
    //     branch. Claiming its tip before anyone else also keeps the trunk off the
    //     stub list even when a sibling branch sits on its very tip (what happens
    //     right after you branch from it).
    //  2. Local before remote — so a local branch's tip is never pre-claimed by a
    //     remote-tracking ref; remotes like `origin/main` stay ordinary badges.
    //  3. **Newest tip first** (smallest row). This is the fix for issue #28: if
    //     one branch's first-parent chain runs *through* another branch's tip —
    //     e.g. a branch just created at an older/interior commit of an existing
    //     line — the branch extending further has the newer tip (smaller row), so
    //     it claims the whole line and the ancestor-tip branch, owning nothing of
    //     its own, becomes a stub forking off that dot. Ordering by name instead
    //     let the freshly-created branch claim first and steal the lower half of
    //     the existing branch's line (splitting its colour and drawing a spurious
    //     line back to an earlier dot). Tips outside the walked window sort last.
    //  4. Name — a final, deterministic tiebreak (e.g. two branches on one commit).
    let mut seeds: Vec<&GitRef> = refs.iter().filter(|r| r.is_branch()).collect();
    seeds.sort_by_key(|r| {
        let is_local = matches!(r.kind, crate::model::RefKind::Branch);
        // Which branch owns the trunk (slot 0, blue): prefer local `main`, then
        // local `master`, then the checked-out branch — smaller rank wins.
        let trunk_rank = if is_local && r.name == "main" {
            0
        } else if is_local && r.name == "master" {
            1
        } else if is_local && head_branch == Some(r.name.as_str()) {
            2
        } else {
            3
        };
        let is_remote = matches!(r.kind, crate::model::RefKind::RemoteBranch);
        let tip_row = index.get(&r.target).copied().unwrap_or(usize::MAX);
        (trunk_rank, is_remote, tip_row, r.name.clone())
    });

    // The branch that owns the trunk colour (slot 0): the same priority
    // `trunk_reserve_tip` uses, so lane 0 and slot 0 always describe one line.
    let has_local = |name: &str| {
        refs.iter()
            .any(|r| matches!(r.kind, crate::model::RefKind::Branch) && r.name == name)
    };
    let trunk_name: Option<&str> = if has_local("main") {
        Some("main")
    } else if has_local("master") {
        Some("master")
    } else {
        head_branch.filter(|h| has_local(h))
    };

    // commit row -> colour slot.
    let mut color_of: HashMap<usize, usize> = HashMap::new();

    // Claim `tip`'s first-parent chain for `key`'s colour, stopping at the first
    // commit already owned (the merge base) or once out of the walked window.
    // The slot is a pure function of the key (trunk => 0, else its stable hash),
    // never of how many lines came before.
    let claim = |tip: Option<usize>, key: &str, color_of: &mut HashMap<usize, usize>| {
        let slot = if Some(key) == trunk_name { 0 } else { stable_color_slot(key) };
        let mut cur = tip;
        while let Some(row) = cur {
            if color_of.contains_key(&row) {
                break; // reached another branch's line
            }
            color_of.insert(row, slot);
            cur = graph.rows[row]
                .commit
                .parents
                .first()
                .and_then(|p| index.get(p).copied());
        }
    };

    // Local branches that turn out to own no commits become stub lines (collected
    // here as (name, anchor_row)). A stub is a local branch whose tip is already
    // coloured by the time we reach it — i.e. a higher-priority branch claimed it
    // first (it shares that branch's tip, or sits on an interior commit of it).
    // We only do this for *local* branches: priority puts locals before remotes,
    // so a local's tip is never pre-claimed by a remote, and remotes like
    // `origin/main` keep showing as ordinary badges on the shared commit.
    let mut stub_seeds: Vec<(String, usize)> = Vec::new();
    for seed in seeds {
        let tip = index.get(&seed.target).copied();
        let is_local = matches!(seed.kind, crate::model::RefKind::Branch);
        match tip {
            Some(row) if is_local && color_of.contains_key(&row) => {
                // Owns nothing of its own → draw it as a distinct stub line, not a
                // second badge. Don't claim (it has no chain to colour anyway).
                stub_seeds.push((seed.name.clone(), row));
            }
            _ => claim(tip, &seed.name, &mut color_of),
        }
    }
    // Synthetic fallback: any commit still unowned, top-to-bottom, starts a line
    // keyed by its own short hash so the slot is stable.
    for row in 0..graph.rows.len() {
        if color_of.contains_key(&row) {
            continue;
        }
        let key = format!("~{}", graph.rows[row].commit.id.short());
        claim(Some(row), &key, &mut color_of);
    }

    for row in &mut graph.rows {
        row.color = color_of.get(&row.row).copied().unwrap_or(0);
    }

    stub_seeds
}
