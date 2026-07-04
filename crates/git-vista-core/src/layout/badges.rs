//! Attaching refs to the rows they point at, as badges.
//!
//! Split out of `layout.rs`: the final decoration step of [`layout_with_refs`](super).
//! It runs after colouring, and is told which local branch names were drawn as
//! stub lines so it can skip badging those (they're their own lines now). The one
//! function here is `pub(super)` — the layout entry point in the parent calls it.

use std::collections::{HashMap, HashSet};

use crate::model::{GitRef, Graph, Oid};

/// Attach each ref to the row of the commit it points at, so the UI can badge it.
/// Refs whose target is outside the walked window are dropped (nothing to badge).
/// Local branches named in `skip` are *not* badged: they're drawn as stub lines
/// instead (see [`Graph::stubs`]), so badging them too would double them up.
pub(super) fn attach_ref_badges(graph: &mut Graph, refs: Vec<GitRef>, skip: &HashSet<String>) {
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
