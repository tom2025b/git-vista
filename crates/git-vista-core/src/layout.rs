//! Assigns commits to lanes (columns) for the vertical graph.
//!
//! Input must be ordered newest-first (row 0 sits at the top). This is a full
//! **active-lane tracker** (Phase 6): we walk the history once, top to bottom,
//! maintaining the set of lanes that are currently "live" and which commit each
//! one is waiting to draw next. From that we get clean routing for arbitrary
//! branch/merge topologies — including octopus merges — while keeping the graph
//! as narrow as the topology allows.
//!
//! ## The lane rule
//!
//! We track **active lanes** in a `Vec<Option<Oid>>`: `lanes[i] == Some(id)`
//! means lane `i` is reserved by an already-drawn child and expects (older)
//! commit `id` to appear in it next; `None` means the lane is free and reusable.
//!
//! For each commit, newest to oldest:
//!
//! 1. **Pick its lane.** If one or more lanes already expect this commit (its
//!    children reserved them), it takes the **leftmost** of those; the rest are
//!    **freed** — those sibling branch lines have converged here. If no lane
//!    expects it (a branch tip / the newest commit), it takes the **leftmost
//!    free lane**, only widening the graph when nothing is free.
//! 2. **Continue its first parent in the same lane**, so a branch keeps a stable
//!    column for its whole life (the mainline stays in lane 0). If the first
//!    parent is out of window the lane is freed.
//! 3. **Place each additional (merge) parent.** If some lane already expects that
//!    parent, the branches share it — no new lane. Otherwise it takes the
//!    leftmost free lane **strictly to the right** of this commit, so merge lines
//!    fan out rightward and never cross back over the mainline to the left.
//!
//! Because a merged-back branch frees its lane (step 1) and the next new branch
//! reuses the leftmost free one (steps 1/3), lanes are recycled: sequential side
//! branches stay narrow, while branches that are live at the same time always get
//! distinct lanes and never share a column.
//!
//! Lanes are assigned in one forward pass (each commit's *final* lane); edges are
//! wired in a second pass so they connect each commit to its parent's final lane
//! even when sibling lanes collapsed left at a merge.

use std::collections::{HashMap, HashSet};

use crate::model::{BranchStub, CommitSummary, Edge, GitRef, Graph, GraphRow, Oid};

/// Leftmost free (`None`) lane, growing the lane set only if none is free.
/// Used for branch tips, which have no incoming edge and so can safely take any
/// free column — picking the leftmost keeps the graph compact.
fn leftmost_free(lanes: &mut Vec<Option<Oid>>) -> usize {
    if let Some(i) = lanes.iter().position(Option::is_none) {
        i
    } else {
        lanes.push(None);
        lanes.len() - 1
    }
}

/// Leftmost free lane strictly to the right of `after`, appending a new lane if
/// none is free. Used for merge parents so a merge's branch lines always sit to
/// the right of the merge commit — they never reuse a lane to the left and cross
/// back over the mainline.
fn leftmost_free_right_of(lanes: &mut Vec<Option<Oid>>, after: usize) -> usize {
    if let Some(i) = (after + 1..lanes.len()).find(|&i| lanes[i].is_none()) {
        i
    } else {
        lanes.push(None);
        lanes.len() - 1
    }
}

/// Lay commits out into a [`Graph`], with no ref information. `commits` must be
/// newest-first. Every commit still gets a stable per-branch [`color`], derived
/// purely from topology (first-parent chains). Use [`layout_with_refs`] to also
/// attach branch/tag/HEAD badges and let real branch tips drive the colouring.
///
/// [`color`]: GraphRow::color
pub fn layout(commits: Vec<CommitSummary>) -> Graph {
    let mut graph = layout_topology(commits);
    assign_branch_colors(&mut graph, &[], None);
    graph
}

/// Lay commits out and decorate the graph with `refs`: attach each ref as a badge
/// on the commit it points at, and colour each branch consistently across the
/// whole graph (branch tips seed the colouring; `head_branch` — the checked-out
/// branch — is preferred for the trunk). A local branch that ends up with no
/// commits of its own (e.g. one just created from an existing commit) is drawn as
/// a distinct stub line via [`Graph::stubs`] rather than a second badge.
/// `commits` must be newest-first.
pub fn layout_with_refs(
    commits: Vec<CommitSummary>,
    refs: Vec<GitRef>,
    head_branch: Option<&str>,
) -> Graph {
    let mut graph = layout_topology(commits);
    // Colouring also tells us which local branches own no commits of their own
    // (their tip was already claimed by a higher-priority branch) — those become
    // distinct stub lines instead of a second badge on the shared commit.
    let (stub_seeds, used_slots) = assign_branch_colors(&mut graph, &refs, head_branch);

    // Lay the stubs out as *cascades*: all stubs that point at the same commit
    // stack into a staircase, each forking off the previous one's tip rather than
    // every one fanning back to the shared commit. So creating another branch at a
    // commit that already has a stub adds a new hollow dot off the last dot — a
    // visible fork from the stub you branched from, not another dot on the commit.
    // Grouping preserves first-appearance order (seed order = branch name), which
    // is the only deterministic order available (git records no "from which stub").
    let mut groups: Vec<(usize, Vec<String>)> = Vec::new();
    let mut stub_names = HashSet::new();
    for (name, anchor_row) in stub_seeds {
        stub_names.insert(name.clone());
        match groups.iter_mut().find(|(row, _)| *row == anchor_row) {
            Some((_, names)) => names.push(name),
            None => groups.push((anchor_row, vec![name])),
        }
    }
    // Each cascade gets its own block of consecutive lanes (right of the commit
    // lanes) so stub `depth` maps to lane `base + depth` — the connector for a
    // deeper stub starts one lane left, at the previous stub's dot. Colour slots
    // continue past the real branch lines' so every stub stays distinct.
    let mut next_lane = graph.lane_count;
    let mut stubs = Vec::new();
    let mut slot = used_slots;
    for (anchor_row, names) in groups {
        let base = next_lane;
        for (depth, name) in names.into_iter().enumerate() {
            stubs.push(BranchStub {
                name,
                anchor_row,
                anchor_lane: graph.rows[anchor_row].lane,
                lane: base + depth,
                color: slot,
                depth,
            });
            slot += 1;
            // The cascade occupies lanes base..=base+depth; keep the next cascade
            // clear of it.
            next_lane = base + depth + 1;
        }
    }
    // Widen the lane count to include the stub columns so the label column sits
    // to the right of them (and the gutter is wide enough to draw the stubs).
    graph.lane_count = graph.lane_count.max(next_lane);
    graph.stubs = stubs;

    // Badge the remaining refs on their commits — but not the stub branches, which
    // are drawn as their own lines (the whole point of this feature).
    attach_ref_badges(&mut graph, refs, &stub_names);
    graph
}

/// The pure topology pass: assign each commit a lane and wire edges. Leaves every
/// row's `refs` empty and `color` at 0 — [`assign_branch_colors`] fills those.
fn layout_topology(commits: Vec<CommitSummary>) -> Graph {
    // Row index per commit id, so an edge can find its parent's row, and so we
    // can tell in-window parents from dangling ones.
    let index: HashMap<Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(row, c)| (c.id.clone(), row))
        .collect();

    // Active lanes: `lanes[i] = Some(id)` means lane i currently expects commit
    // `id` next; `None` means the lane is free and reusable.
    let mut lanes: Vec<Option<Oid>> = Vec::new();
    // Each commit's final lane, decided as we walk newest -> oldest.
    let mut lane_of: HashMap<Oid, usize> = HashMap::new();
    let mut rows = Vec::with_capacity(commits.len());

    // Pass 1: assign every commit a lane and reserve its parents' lanes.
    for (row, commit) in commits.iter().enumerate() {
        // Lanes already expecting this commit (reserved by its children).
        let reserved: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref() == Some(&commit.id))
            .map(|(i, _)| i)
            .collect();

        let lane = match reserved.first() {
            // Take the leftmost reserved lane; the rest are sibling branch lines
            // that converge at this commit, so free them for reuse.
            Some(&leftmost) => {
                for &i in &reserved[1..] {
                    lanes[i] = None;
                }
                leftmost
            }
            // A branch tip: reuse the leftmost free lane.
            None => leftmost_free(&mut lanes),
        };
        lane_of.insert(commit.id.clone(), lane);

        // First parent continues this lane (or frees it if there's nothing
        // in-window to continue to). Keeping the first parent in the same lane is
        // what gives each branch a stable column down to its merge base.
        match commit.parents.first() {
            Some(p) if index.contains_key(p) => lanes[lane] = Some(p.clone()),
            _ => lanes[lane] = None,
        }
        // Extra (merge) parents: if a lane already expects this parent, the
        // branches share it; otherwise reserve the leftmost free lane to the
        // right, so merge lines fan out rightward and don't cross the mainline.
        for parent in commit.parents.iter().skip(1) {
            if !index.contains_key(parent) {
                continue; // dangling: no lane, no edge
            }
            if lanes.iter().any(|s| s.as_ref() == Some(parent)) {
                continue; // already reserved — merges into an existing line
            }
            let i = leftmost_free_right_of(&mut lanes, lane);
            lanes[i] = Some(parent.clone());
        }

        rows.push(GraphRow {
            commit: commit.clone(),
            row,
            lane,
            refs: Vec::new(),
            color: 0,
        });
    }

    // Pass 2: wire edges using each endpoint's final lane.
    let mut edges = Vec::new();
    for (row, commit) in commits.iter().enumerate() {
        let from_lane = lane_of[&commit.id];
        for parent in &commit.parents {
            if let Some(&to_row) = index.get(parent) {
                edges.push(Edge {
                    from_row: row,
                    from_lane,
                    to_row,
                    to_lane: lane_of[parent],
                });
            }
        }
    }

    let lane_count = rows.iter().map(|r| r.lane).max().map_or(0, |m| m + 1);
    Graph {
        rows,
        edges,
        lane_count,
        repo_url: None,
        remote_commits: Vec::new(),
        stubs: Vec::new(),
        repo_label: None,
        read_only: false,
    }
}

/// Give every commit a stable per-branch [`color`](GraphRow::color) palette slot.
///
/// A "branch" here is a **first-parent chain**: starting from a branch tip we
/// walk first-parent links down until we reach a commit another branch already
/// owns (the merge base), claiming each commit for that branch's colour. So a
/// branch keeps one colour for its whole mainline, and that colour is the same
/// everywhere the branch appears — independent of which lane it sits in (lanes
/// get reused; colours don't).
///
/// Branch tips (from `refs`) seed the colouring in priority order: the checked-out
/// branch (`head_branch`) first (so the trunk takes colour 0 and never becomes a
/// stub), then the branch on HEAD's commit, then local branches, then remote ones,
/// each group by name. Any commit still unclaimed afterwards (e.g. commits of a
/// deleted branch, reachable only as a merge's second parent) starts its own
/// synthetic line, walked the same way, so **every** commit ends up coloured.
///
/// Colour slots are handed out in order of first appearance, so the same branch
/// always maps to the same slot for a given graph; the UI wraps the slot onto its
/// palette.
///
/// Returns `(stub_seeds, used_slots)`: `stub_seeds` are the local branches that
/// owned no commits of their own — their tip was already claimed by a
/// higher-priority branch (e.g. a branch just created from an existing commit) —
/// each as `(name, anchor_row)`; `used_slots` is how many colour slots the real
/// branch lines consumed, so the caller can give stubs fresh, distinct colours.
fn assign_branch_colors(
    graph: &mut Graph,
    refs: &[GitRef],
    head_branch: Option<&str>,
) -> (Vec<(String, usize)>, usize) {
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

    // commit row -> colour slot, and branch key -> slot so the same branch reuses
    // its slot even if it seeds several lines.
    let mut color_of: HashMap<usize, usize> = HashMap::new();
    let mut slot_of_key: HashMap<String, usize> = HashMap::new();

    // Claim `tip`'s first-parent chain for `key`'s colour, stopping at the first
    // commit already owned (the merge base) or once out of the walked window. A
    // colour slot is allocated lazily — only if the chain actually owns a commit
    // — so a branch whose tip another branch already claimed costs no slot, and
    // slots stay dense (which spreads further before the palette has to wrap).
    let claim = |tip: Option<usize>,
                 key: String,
                 color_of: &mut HashMap<usize, usize>,
                 slot_of_key: &mut HashMap<String, usize>| {
        // The unowned first-parent run this seed would claim.
        let mut chain = Vec::new();
        let mut cur = tip;
        while let Some(row) = cur {
            if color_of.contains_key(&row) {
                break; // reached another branch's line
            }
            chain.push(row);
            cur = graph.rows[row]
                .commit
                .parents
                .first()
                .and_then(|p| index.get(p).copied());
        }
        if chain.is_empty() {
            return; // nothing of our own to colour — don't burn a slot
        }
        let next_slot = slot_of_key.len();
        let slot = *slot_of_key.entry(key).or_insert(next_slot);
        for row in chain {
            color_of.insert(row, slot);
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
            _ => claim(tip, seed.name.clone(), &mut color_of, &mut slot_of_key),
        }
    }
    // Synthetic fallback: any commit still unowned, top-to-bottom, starts a line
    // keyed by its own short hash so the slot is stable.
    for row in 0..graph.rows.len() {
        if color_of.contains_key(&row) {
            continue;
        }
        let key = format!("~{}", graph.rows[row].commit.id.short());
        claim(Some(row), key, &mut color_of, &mut slot_of_key);
    }

    // Keep the whole trunk *line* one colour — Issue #30. `main` colours its
    // first-parent chain from its tip downward (the trunk slot, 0), but a branch
    // sitting ABOVE that tip on the same lane — e.g. a working branch that's ahead
    // of main and not merged back yet — occupies the very same vertical line, so
    // the trunk would turn from blue to that branch's colour as it climbs. Extend
    // the trunk colour upward along that lane: from the trunk tip, follow
    // first-parent *children* that stay in the trunk's lane, recolouring each to
    // the trunk slot so the mainline reads as one unbroken blue line top to bottom.
    // Side branches (which sit in other lanes) keep their own distinct colours.
    if let Some(mut cur) = color_of
        .iter()
        .filter(|&(_, &c)| c == 0)
        .map(|(&r, _)| r)
        .min()
    {
        let lane = graph.rows[cur].lane;
        // The child (if any) that continues this lane is the one whose *first*
        // parent is the current commit and which the layout kept in the same lane.
        while let Some(child) = graph.rows.iter().find(|r| {
            r.lane == lane
                && r.commit.parents.first().and_then(|p| index.get(p).copied()) == Some(cur)
        }) {
            let child_row = child.row;
            color_of.insert(child_row, 0);
            cur = child_row;
        }
    }

    for row in &mut graph.rows {
        row.color = color_of.get(&row.row).copied().unwrap_or(0);
    }

    // All slots consumed by real lines (branch + synthetic); stubs get colours
    // numbered from here so they don't collide with any line's colour.
    (stub_seeds, slot_of_key.len())
}

/// Attach each ref to the row of the commit it points at, so the UI can badge it.
/// Refs whose target is outside the walked window are dropped (nothing to badge).
/// Local branches named in `skip` are *not* badged: they're drawn as stub lines
/// instead (see [`Graph::stubs`]), so badging them too would double them up.
fn attach_ref_badges(graph: &mut Graph, refs: Vec<GitRef>, skip: &HashSet<String>) {
    let index: HashMap<Oid, usize> = graph
        .rows
        .iter()
        .map(|r| (r.commit.id.clone(), r.row))
        .collect();
    for r in refs {
        if matches!(r.kind, crate::model::RefKind::Branch) && skip.contains(&r.name) {
            continue; // drawn as a stub line, not a badge
        }
        if let Some(&row) = index.get(&r.target) {
            graph.rows[row].refs.push(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Oid, RefKind};

    fn gitref(name: &str, kind: RefKind, target: &str) -> GitRef {
        GitRef {
            name: name.into(),
            kind,
            target: Oid(target.into()),
        }
    }

    fn color_of(g: &Graph, id: &str) -> usize {
        g.rows.iter().find(|r| r.commit.id.0 == id).unwrap().color
    }

    fn ref_names(g: &Graph, id: &str) -> Vec<String> {
        g.rows
            .iter()
            .find(|r| r.commit.id.0 == id)
            .unwrap()
            .refs
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    /// A branch created from an existing commit (its tip already owned by another
    /// branch) is drawn as a distinct stub line, not a second badge: it owns no
    /// commits, gets its own lane and a colour distinct from the branch it forked
    /// off, and its name is removed from the shared commit's badges.
    #[test]
    fn branch_with_no_own_commits_becomes_a_distinct_stub() {
        // c2 <- c1 <- c0 ; `main` and a freshly-created `feature` both at c2.
        let commits = vec![
            commit("c2", &["c1"]),
            commit("c1", &["c0"]),
            commit("c0", &[]),
        ];
        let refs = vec![
            gitref("HEAD", RefKind::Head, "c2"),
            gitref("main", RefKind::Branch, "c2"),
            gitref("feature", RefKind::Branch, "c2"),
        ];
        // We're on `main` (HEAD) and just created `feature` from its tip.
        let g = layout_with_refs(commits, refs, Some("main"));

        // `feature` owns nothing, so it's a stub — not a badge on c2.
        assert!(!ref_names(&g, "c2").contains(&"feature".to_string()));
        assert!(ref_names(&g, "c2").contains(&"main".to_string()));

        assert_eq!(g.stubs.len(), 1);
        let stub = &g.stubs[0];
        assert_eq!(stub.name, "feature");
        // Anchored to c2's row, in its own lane to the right, distinct colour.
        assert_eq!(stub.anchor_row, 0);
        assert_eq!(stub.anchor_lane, lane_of(&g, "c2"));
        assert!(stub.lane >= g.rows.iter().map(|r| r.lane).max().unwrap());
        assert_ne!(stub.color, color_of(&g, "c2"));
        // The lane count was widened to include the stub lane (so the label
        // column sits to the right of it).
        assert!(g.lane_count > stub.lane);
    }

    /// Issue #30: a stub has its own identity and the *correct* tip commit. The
    /// stub's anchor row must be the exact commit its branch points at — that hash
    /// is what the UI's menu shows and what "branch from the stub" forks off, so if
    /// it drifted to some other commit, the hollow dot would misrepresent the
    /// branch and branching would target the wrong commit.
    #[test]
    fn a_stub_anchor_is_its_branchs_own_tip_commit() {
        // A coloured side branch `feature` (tip F2), plus a brand-new branch `fork`
        // created at feature's *tip* F2 — so `fork` owns nothing and is a stub.
        //   D  main tip
        //   F2 feature tip  <- `fork` also points here
        //   C
        //   F1
        //   B  fork point
        //   A
        let commits = vec![
            commit("D", &["C"]),
            commit("F2", &["F1"]),
            commit("C", &["B"]),
            commit("F1", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ];
        let refs = vec![
            gitref("HEAD", RefKind::Head, "D"),
            gitref("main", RefKind::Branch, "D"),
            gitref("feature", RefKind::Branch, "F2"),
            gitref("fork", RefKind::Branch, "F2"),
        ];
        let g = layout_with_refs(commits, refs, Some("main"));

        // `feature` is the real line; `fork` (created at its tip) is the stub.
        let stub = g.stubs.iter().find(|s| s.name == "fork").expect("fork is a stub");
        assert!(g.stubs.iter().all(|s| s.name != "feature"), "feature is a real line");
        // The stub's tip is exactly F2 — feature's own tip, the commit `fork`
        // points at — so branching from the stub forks off F2, not some parent.
        assert_eq!(
            g.rows[stub.anchor_row].commit.id.0, "F2",
            "the stub's tip must be its branch's own commit"
        );
        // And its colour slot is distinct from the branch it forked off.
        assert_ne!(stub.color, color_of(&g, "F2"), "a new branch differs from its parent");
    }

    /// Issue #30: several branches created at the *same* commit cascade — each is
    /// its own stub, stacked so a deeper one forks off the previous stub's tip (one
    /// lane to the right) rather than every stub fanning back to the shared commit.
    /// This is what makes "create a branch from a hollow dot" draw a new dot off
    /// the dot you clicked.
    #[test]
    fn stubs_sharing_a_commit_cascade_off_one_another() {
        // main at c2; `aaa` and `bbb` both freshly created at c2 (own nothing).
        let commits = vec![
            commit("c2", &["c1"]),
            commit("c1", &["c0"]),
            commit("c0", &[]),
        ];
        let refs = vec![
            gitref("HEAD", RefKind::Head, "c2"),
            gitref("main", RefKind::Branch, "c2"),
            gitref("aaa", RefKind::Branch, "c2"),
            gitref("bbb", RefKind::Branch, "c2"),
        ];
        let g = layout_with_refs(commits, refs, Some("main"));

        assert_eq!(g.stubs.len(), 2, "both new branches are stubs");
        // Ordered deterministically by name: aaa is the base of the cascade, bbb
        // stacks above it.
        let aaa = g.stubs.iter().find(|s| s.name == "aaa").unwrap();
        let bbb = g.stubs.iter().find(|s| s.name == "bbb").unwrap();
        // Both anchor at the same commit (c2, row 0).
        assert_eq!(aaa.anchor_row, 0);
        assert_eq!(bbb.anchor_row, 0);
        // First forks off the commit; second forks off the first (one deeper, one
        // lane further right — that's how the connector finds the previous tip).
        assert_eq!(aaa.depth, 0, "first stub forks off the commit");
        assert_eq!(bbb.depth, 1, "second stub forks off the first stub's tip");
        assert_eq!(bbb.lane, aaa.lane + 1, "the deeper stub sits one lane right");
        // Distinct colours, and neither is the trunk slot.
        assert_ne!(aaa.color, bbb.color);
        assert_ne!(aaa.color, 0);
        assert_ne!(bbb.color, 0);
    }

    /// Issue #28: a branch created at an *interior* commit of an existing branch's
    /// line must become a stub forking off that commit — it must NOT claim the
    /// lower half of the existing branch's first-parent chain. Ordering by name
    /// used to let `aaa` (created at F1, inside `feature`) claim F1..base and split
    /// `feature`'s colour in two, drawing a spurious line back to an earlier dot.
    /// Now the branch with the newer tip (`feature`, tip F2) owns the whole line
    /// and `aaa` is a stub.
    #[test]
    fn a_branch_at_an_interior_commit_is_a_stub_not_a_stolen_line() {
        // main: D -> C -> B -> A ; feature: F2 -> F1 -> B ; aaa points at F1.
        // Rows are newest-first (row 0 at top).
        let commits = vec![
            commit("D", &["C"]),  // 0  main tip
            commit("F2", &["F1"]), // 1  feature tip
            commit("C", &["B"]),  // 2
            commit("F1", &["B"]), // 3  aaa points here (interior of feature)
            commit("B", &["A"]),  // 4  fork point
            commit("A", &[]),     // 5
        ];
        let refs = vec![
            gitref("HEAD", RefKind::Head, "D"),
            gitref("main", RefKind::Branch, "D"),
            gitref("feature", RefKind::Branch, "F2"),
            gitref("aaa", RefKind::Branch, "F1"),
        ];
        let g = layout_with_refs(commits, refs, Some("main"));

        // `feature` keeps ONE colour down its whole line (F2 and F1 match), and
        // it's distinct from main's trunk colour.
        assert_eq!(
            color_of(&g, "F2"),
            color_of(&g, "F1"),
            "feature must not be split in two by aaa stealing F1"
        );
        assert_ne!(color_of(&g, "F1"), color_of(&g, "D"), "feature isn't the trunk");
        assert_eq!(color_of(&g, "D"), 0, "main (checked out) owns the trunk colour");

        // `aaa` owns nothing → it's a stub anchored at F1, not a badge, not a line.
        assert_eq!(g.stubs.len(), 1);
        assert_eq!(g.stubs[0].name, "aaa");
        assert_eq!(g.stubs[0].anchor_row, 3, "stub forks off F1's dot");
        assert!(!ref_names(&g, "F1").contains(&"aaa".to_string()));
        // `feature` stays a real line: it's badged on its tip, not a stub.
        assert!(g.stubs.iter().all(|s| s.name != "feature"));
        assert!(ref_names(&g, "F2").contains(&"feature".to_string()));
    }

    fn commit(id: &str, parents: &[&str]) -> CommitSummary {
        CommitSummary {
            id: Oid(id.into()),
            parents: parents.iter().map(|p| Oid((*p).into())).collect(),
            summary: format!("commit {id}"),
            author: "tester".into(),
            time: 0,
        }
    }

    fn lane_of(g: &Graph, id: &str) -> usize {
        g.rows.iter().find(|r| r.commit.id.0 == id).unwrap().lane
    }

    /// The edge for a specific commit -> parent link, found by id (independent of
    /// the order edges happen to be emitted in).
    fn edge(g: &Graph, from: &str, to: &str) -> Edge {
        let from_row = g.rows.iter().position(|r| r.commit.id.0 == from).unwrap();
        let to_row = g.rows.iter().position(|r| r.commit.id.0 == to).unwrap();
        g.edges
            .iter()
            .find(|e| e.from_row == from_row && e.to_row == to_row)
            .cloned()
            .unwrap_or_else(|| panic!("no edge {from} -> {to}"))
    }

    /// Sanity invariants every laid-out graph must satisfy, whatever its shape:
    /// rows are top-to-bottom, every node's lane is in range, every edge runs
    /// downward (child above parent) between in-range lanes, and there's exactly
    /// one edge per in-window parent link.
    fn assert_well_formed(g: &Graph) {
        for (i, r) in g.rows.iter().enumerate() {
            assert_eq!(r.row, i, "rows are sequential top-to-bottom");
            assert!(r.lane < g.lane_count, "node lane within lane_count");
        }
        let mut expected_edges = 0;
        let present: std::collections::HashSet<&str> =
            g.rows.iter().map(|r| r.commit.id.0.as_str()).collect();
        for r in &g.rows {
            for p in &r.commit.parents {
                if present.contains(p.0.as_str()) {
                    expected_edges += 1;
                }
            }
        }
        assert_eq!(g.edges.len(), expected_edges, "one edge per in-window link");
        for e in &g.edges {
            assert!(e.to_row > e.from_row, "child {e:?} sits above its parent");
            assert!(e.from_lane < g.lane_count && e.to_lane < g.lane_count);
        }
    }

    #[test]
    fn empty_history_yields_empty_graph() {
        let g = layout(vec![]);
        assert!(g.rows.is_empty());
        assert!(g.edges.is_empty());
        assert_eq!(g.lane_count, 0);
    }

    #[test]
    fn linear_history_stays_in_lane_zero() {
        let g = layout(vec![
            commit("c", &["b"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_well_formed(&g);
        assert_eq!(g.rows.len(), 3);
        assert!(g.rows.iter().all(|r| r.lane == 0), "all in the trunk lane");
        assert_eq!(g.lane_count, 1);
        assert_eq!(g.edges.len(), 2); // c->b, b->a
        assert_eq!(g.rows[0].commit.id.short(), "c"); // newest at row 0
                                                      // Linear links are straight (same lane), top to bottom.
        assert_eq!(
            edge(&g, "c", "b"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 1,
                to_lane: 0
            }
        );
        assert_eq!(
            edge(&g, "b", "a"),
            Edge {
                from_row: 1,
                from_lane: 0,
                to_row: 2,
                to_lane: 0
            }
        );
    }

    #[test]
    fn dangling_parents_are_skipped() {
        // Parent "z" is outside the walked window — no edge, and no lane spent.
        let g = layout(vec![commit("a", &["z"])]);
        assert!(g.edges.is_empty());
        assert_eq!(g.lane_count, 1); // just "a" itself
    }

    #[test]
    fn branch_and_merge_routes_to_the_right() {
        // A feature branch off B that merges back at M. The mainline keeps lane 0;
        // the feature takes lane 1 (to the *right* of the merge), and both
        // collapse back into lane 0 at their shared base B.
        //
        //   M        merge[C, D]
        //   |\
        //   C D
        //   |/
        //   B
        //   |
        //   A
        let g = layout(vec![
            commit("M", &["C", "D"]),
            commit("C", &["B"]),
            commit("D", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "M"), 0);
        assert_eq!(lane_of(&g, "C"), 0, "first parent keeps the merge's lane");
        assert_eq!(
            lane_of(&g, "D"),
            1,
            "the merged branch takes the lane to the right"
        );
        assert_eq!(
            lane_of(&g, "B"),
            0,
            "both branches collapse into lane 0 at B"
        );
        assert_eq!(lane_of(&g, "A"), 0);
        assert_eq!(g.lane_count, 2);

        // The merge's second parent sits to the right of the merge commit...
        assert!(
            lane_of(&g, "D") > lane_of(&g, "M"),
            "no leftward (crossing) merge"
        );
        // ...so the merge edge fans right, and D's edge collapses back to lane 0.
        assert_eq!(
            edge(&g, "M", "D"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 1
            }
        );
        assert_eq!(
            edge(&g, "D", "B"),
            Edge {
                from_row: 2,
                from_lane: 1,
                to_row: 3,
                to_lane: 0
            }
        );
    }

    #[test]
    fn octopus_merge_fans_each_parent_into_its_own_lane() {
        // A 3-parent (octopus) merge O of three branches that all fork from the
        // same root R. Each merged parent gets its own lane to the right of the
        // merge, in parent order, and they all collapse back at R.
        //
        //   O        merge[A, B, C]
        //  /|\
        // A B C
        //  \|/
        //   R
        let g = layout(vec![
            commit("O", &["A", "B", "C"]),
            commit("A", &["R"]),
            commit("B", &["R"]),
            commit("C", &["R"]),
            commit("R", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "O"), 0);
        assert_eq!(lane_of(&g, "A"), 0, "first parent keeps the merge lane");
        assert_eq!(lane_of(&g, "B"), 1, "second parent fans one lane right");
        assert_eq!(lane_of(&g, "C"), 2, "third parent fans two lanes right");
        assert_eq!(
            lane_of(&g, "R"),
            0,
            "all branches collapse back at the root"
        );
        assert_eq!(g.lane_count, 3);

        // Every merge parent is to the right of the octopus node (no crossing).
        for p in ["A", "B", "C"] {
            assert!(lane_of(&g, p) >= lane_of(&g, "O"));
        }
        // The three merge edges fan out to lanes 0, 1, 2.
        assert_eq!(
            edge(&g, "O", "A"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 1,
                to_lane: 0
            }
        );
        assert_eq!(
            edge(&g, "O", "B"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 1
            }
        );
        assert_eq!(
            edge(&g, "O", "C"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 3,
                to_lane: 2
            }
        );
    }

    #[test]
    fn sequential_branches_reuse_a_freed_lane() {
        // Two features in sequence: the first merges back (freeing its lane)
        // before the second starts, so the second REUSES lane 1 instead of
        // opening a lane 2 — the whole graph stays only 2 lanes wide.
        //
        //   M2          merge[M1, F2]
        //   |\
        //   | F2        [M1]
        //   |/
        //   M1          merge[B, F1]
        //   |\
        //   | F1        [A]
        //   B |         [A]
        //   |/
        //   A
        let g = layout(vec![
            commit("M2", &["M1", "F2"]),
            commit("F2", &["M1"]),
            commit("M1", &["B", "F1"]),
            commit("F1", &["A"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "F2"), 1, "first feature uses lane 1");
        assert_eq!(
            lane_of(&g, "F1"),
            1,
            "second feature REUSES lane 1, not lane 2"
        );
        assert_eq!(g.lane_count, 2, "graph stays 2 lanes wide thanks to reuse");
    }

    #[test]
    fn concurrent_branches_get_distinct_lanes() {
        // Here feature2 is still open (its base B is deep) when feature1 is merged,
        // so the two branches are live at the same time and must NOT share a lane:
        // feature1 is pushed out to lane 2 while feature2 holds lane 1.
        //
        //   M2          merge[M1, f2]
        //   |\
        //   | f2        [B]   (feature2 — stays open across M1)
        //   M1 |        merge[B, f1]
        //   |\ |
        //   | f1|       [A]   (feature1)
        //   |  /
        //   B /         [A]
        //   |/
        //   A
        let g = layout(vec![
            commit("M2", &["M1", "f2"]),
            commit("f2", &["B"]),
            commit("M1", &["B", "f1"]),
            commit("f1", &["A"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "M2"), 0);
        assert_eq!(lane_of(&g, "M1"), 0, "mainline keeps lane 0");
        assert_eq!(
            lane_of(&g, "f2"),
            1,
            "feature2 holds lane 1 while it's open"
        );
        assert_eq!(
            lane_of(&g, "f1"),
            2,
            "feature1 can't reuse lane 1 — it's still busy"
        );
        assert_ne!(
            lane_of(&g, "f1"),
            lane_of(&g, "f2"),
            "concurrent branches never share"
        );
        assert_eq!(g.lane_count, 3);
    }

    #[test]
    fn a_long_running_branch_keeps_one_stable_lane() {
        // A side branch with two commits of its own, parallel to the mainline,
        // should keep a single stable lane for its whole life (no lane hopping)
        // — its internal link is a straight, same-lane edge.
        //
        //   M           merge[main2, side2]
        //   |\
        //   m2 s2
        //   |  |
        //   m1 s1
        //   |/
        //   base
        let g = layout(vec![
            commit("M", &["main2", "side2"]),
            commit("main2", &["main1"]),
            commit("side2", &["side1"]),
            commit("main1", &["base"]),
            commit("side1", &["base"]),
            commit("base", &[]),
        ]);
        assert_well_formed(&g);

        // Mainline stays in lane 0 the whole way down.
        for c in ["M", "main2", "main1", "base"] {
            assert_eq!(lane_of(&g, c), 0, "{c} stays on the mainline lane");
        }
        // The side branch keeps lane 1 for both its commits — no mislabeling.
        assert_eq!(lane_of(&g, "side2"), 1);
        assert_eq!(lane_of(&g, "side1"), 1);
        assert_eq!(g.lane_count, 2);

        // Same-lane links are straight: the side branch's internal edge and the
        // mainline's edges don't change lanes.
        assert_eq!(
            edge(&g, "side2", "side1"),
            Edge {
                from_row: 2,
                from_lane: 1,
                to_row: 4,
                to_lane: 1
            }
        );
        assert_eq!(
            edge(&g, "main2", "main1"),
            Edge {
                from_row: 1,
                from_lane: 0,
                to_row: 3,
                to_lane: 0
            }
        );
    }

    #[test]
    fn linear_history_is_one_colour() {
        let g = layout(vec![
            commit("c", &["b"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        // Nothing branches, so every commit shares branch colour 0.
        assert!(
            g.rows.iter().all(|r| r.color == 0),
            "one branch, one colour"
        );
    }

    /// Issue #30: `main` owns the trunk colour (slot 0, the one blue line) even
    /// when a *different* branch is checked out. Here HEAD is on `feature`, yet
    /// `main`'s line must still be blue and `feature` a distinct non-trunk colour.
    #[test]
    fn main_owns_the_trunk_colour_even_when_not_checked_out() {
        let g = layout_with_refs(
            vec![
                commit("M", &["C", "D"]),
                commit("C", &["B"]),
                commit("D", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            vec![
                gitref("HEAD", RefKind::Head, "D"),
                gitref("main", RefKind::Branch, "M"),
                gitref("feature", RefKind::Branch, "D"),
            ],
            // Checked out on `feature`, not `main`.
            Some("feature"),
        );
        assert_eq!(color_of(&g, "M"), 0, "main is the trunk (slot 0) regardless of HEAD");
        assert_ne!(color_of(&g, "D"), 0, "the checked-out feature is not the trunk");
        assert!(g.stubs.is_empty(), "both branches own commits — neither is a stub");
    }

    /// Issue #30 follow-up: a branch ahead of `main` that hasn't been merged back
    /// sits in the trunk lane, directly above main's tip, so it *is* the visible
    /// continuation of the trunk line. The whole line must stay the trunk colour
    /// top to bottom — not turn to the ahead-branch's colour going up — while a
    /// genuine side branch (in another lane) keeps its own distinct colour.
    #[test]
    fn the_trunk_line_stays_one_colour_when_a_branch_is_ahead_of_main() {
        // E,D (feature, ahead) -> C (main tip) -> B -> A, all lane 0; S is a side
        // branch off B in its own lane.
        //   E   feature tip (lane 0)
        //   D   feature      (lane 0)
        //   | S side tip     (lane 1)
        //   C   main tip     (lane 0)
        //   |/
        //   B
        //   A
        let commits = vec![
            commit("E", &["D"]),
            commit("D", &["C"]),
            commit("S", &["B"]),
            commit("C", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ];
        let refs = vec![
            gitref("HEAD", RefKind::Head, "E"),
            gitref("main", RefKind::Branch, "C"),
            gitref("feature", RefKind::Branch, "E"),
            gitref("side", RefKind::Branch, "S"),
        ];
        // Checked out on `feature`, which is ahead of `main`.
        let g = layout_with_refs(commits, refs, Some("feature"));

        // The entire trunk line is the trunk colour, including the un-merged
        // feature commits that sit above main's tip on the same lane.
        for c in ["E", "D", "C", "B", "A"] {
            assert_eq!(color_of(&g, c), 0, "{c} is on the trunk line and must be blue");
        }
        // The genuine side branch (different lane) keeps its own, non-trunk colour.
        assert_ne!(color_of(&g, "S"), 0, "a real side branch is not the trunk colour");
    }

    #[test]
    fn each_branch_gets_its_own_stable_colour() {
        // HEAD on main; a feature branch tip at D. Main's first-parent chain
        // (M→C→B→A) is one colour; the feature line (D) is a different one.
        //
        //   M        merge[C, D]
        //   |\
        //   C D      (D = feature tip)
        //   |/
        //   B
        //   |
        //   A
        let g = layout_with_refs(
            vec![
                commit("M", &["C", "D"]),
                commit("C", &["B"]),
                commit("D", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            vec![
                gitref("HEAD", RefKind::Head, "M"),
                gitref("main", RefKind::Branch, "M"),
                gitref("feature", RefKind::Branch, "D"),
            ],
            Some("main"),
        );

        // The whole mainline (incl. the shared base B/A) is HEAD's branch colour.
        let main = color_of(&g, "M");
        for c in ["M", "C", "B", "A"] {
            assert_eq!(color_of(&g, c), main, "{c} is on the main line");
        }
        assert_eq!(main, 0, "HEAD's branch takes colour slot 0 (the trunk)");
        // The feature commit is a different, consistent colour.
        assert_ne!(color_of(&g, "D"), main, "the feature branch differs");
    }

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

    #[test]
    fn a_tag_only_side_commit_still_gets_a_colour() {
        // A side commit S reachable only as M's second parent, with no branch ref
        // (only a tag). It must still be coloured — via the synthetic fallback —
        // and distinct from the trunk.
        //
        //   M     merge[C, S]
        //   |\
        //   C S   (S tagged, no branch)
        //   |/
        //   B
        let g = layout_with_refs(
            vec![
                commit("M", &["C", "S"]),
                commit("C", &["B"]),
                commit("S", &["B"]),
                commit("B", &[]),
            ],
            vec![
                gitref("HEAD", RefKind::Head, "M"),
                gitref("main", RefKind::Branch, "M"),
                gitref("v2", RefKind::Tag, "S"),
            ],
            Some("main"),
        );
        assert_eq!(ref_names(&g, "S"), vec!["v2"], "the tag still badges S");
        assert_ne!(
            color_of(&g, "S"),
            color_of(&g, "M"),
            "the un-branched side line gets its own colour"
        );
        // Every row is coloured (no commit left out).
        assert_eq!(g.rows.len(), 4);
    }
}
