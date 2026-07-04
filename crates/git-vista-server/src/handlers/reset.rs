//! `POST /api/reset-test-repo` (iPad-testing follow-up): restore a seeded *test
//! repo* to its recorded state. This is the one place in git-vista that deletes a
//! branch — and only ever inside an explicit reset of a repo opted in with
//! `gv --seed`. [`has_seed`] is the gate the graph read uses to offer the action
//! at all, so it lives here beside the reset it guards.

use std::path::Path;

use axum::http::StatusCode;

use git_vista_core::seed::{parse_seed, reset_plan, Seed};

use crate::git_cmd::{git_ok, rev_parse};
use crate::journal;
use crate::state::{current, reject_if_read_only};

/// Whether this repo carries a recorded test-repo seed (`gv --seed`) — the gate
/// for offering "Reset Test Repo" at all.
pub(crate) fn has_seed(repo: &Path) -> bool {
    journal::state_dir(repo)
        .is_some_and(|d| d.join("seed-refs").exists() && d.join("seed-head").exists())
}

/// The parsed seed, if this repo has one. `None` => not a test repo;
/// `Some(Err)` => the seed files exist but are corrupt (refuse to reset).
fn read_seed(repo: &Path) -> Option<Result<Seed, String>> {
    let dir = journal::state_dir(repo)?;
    let refs = std::fs::read_to_string(dir.join("seed-refs")).ok()?;
    let head = std::fs::read_to_string(dir.join("seed-head")).ok()?;
    Some(parse_seed(&refs, &head))
}

/// Reset a *test repo* to its recorded seed (iPad-testing follow-up): move
/// every seeded branch back to its recorded tip, check out the seeded HEAD
/// branch, force the worktree clean, DELETE branches the seed doesn't know —
/// allowed nowhere else in git-vista — and wipe the app journal (its events
/// describe history that no longer exists). Hard-gated: only a repo explicitly
/// opted in with `gv --seed <path>` has seed files, and a read-only clone is
/// refused outright. The seed's object bundle is unbundled first so seeded
/// commits exist even if git gc pruned them after they became unreachable.
pub(crate) async fn reset_test_repo() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    let seed = match read_seed(&repo) {
        None => {
            return (
                StatusCode::NOT_FOUND,
                "This repo has no recorded seed — it isn't marked as a test repo. \
                 Run `gv --seed <path>` once on the server machine to record its reset point."
                    .to_string(),
            )
        }
        Some(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The recorded seed is corrupt ({e}) — re-record it with `gv --seed`."),
            )
        }
        Some(Ok(seed)) => seed,
    };

    // Objects first, verification second: unbundle is best-effort (idempotent,
    // cheap), then every seeded tip must resolve or the reset refuses to start —
    // never a half-restore.
    if let Some(dir) = journal::state_dir(&repo) {
        let bundle = dir.join("seed.bundle");
        if bundle.exists() {
            let _ = git_ok(&repo, &["bundle", "unbundle", &bundle.display().to_string()]).await;
        }
    }
    for r in &seed.refs {
        if rev_parse(&repo, &format!("{}^{{commit}}", r.oid)).await.is_none() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "Seed commit {} for ‘{}’ no longer exists in this repo — \
                     re-record the seed with `gv --seed`.",
                    &r.oid[..7],
                    r.name
                ),
            );
        }
    }

    // What the repo looks like NOW, then the pure plan of moves + deletions.
    let current_refs = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["for-each-ref", "refs/heads", "--format=%(objectname) %(refname:short)"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.split_once(' ').map(|(oid, name)| (name.to_string(), oid.to_string())))
            .collect::<Vec<_>>(),
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't list the repo's current branches.".to_string(),
            )
        }
    };
    let plan = reset_plan(&seed, &current_refs);

    // Apply, in an order where each step makes the next valid: refs back first
    // (so the seed HEAD branch exists at the right tip), then a forced checkout
    // + hard reset + clean (so HEAD is off any branch about to be deleted and
    // the worktree matches the seed exactly), then the deletions.
    for r in &plan.update {
        if let Err(e) = git_ok(&repo, &["update-ref", &format!("refs/heads/{}", r.name), &r.oid]).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped while restoring ‘{}’: {e}", r.name),
            );
        }
    }
    for step in [
        &["checkout", "-f", seed.head.as_str()] as &[&str],
        &["reset", "--hard"],
        &["clean", "-fd"],
    ] {
        if let Err(e) = git_ok(&repo, step).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped at `git {}`: {e}", step.join(" ")),
            );
        }
    }
    let mut deleted = 0;
    for name in &plan.delete {
        // The ONLY place git-vista deletes a branch: a seeded test repo, inside
        // an explicit reset, for branches created after the seed was recorded.
        match git_ok(&repo, &["branch", "-D", name]).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Reset stopped deleting test branch ‘{name}’: {e}"),
                )
            }
        }
    }
    // The journal now describes history that no longer exists (dead undo
    // targets included) — wipe it with the snapshot; both regenerate.
    journal::clear(&repo);

    let msg = format!(
        "Reset to seed: {} branch(es) restored, {} deleted, HEAD → ‘{}’, working tree clean.",
        plan.update.len(),
        deleted,
        seed.head
    );
    println!("[/api/reset-test-repo] {msg}");
    (StatusCode::OK, msg)
}
