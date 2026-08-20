//! The repo-scoped remote/rebase items: "Rebase onto main", "Fetch", "Pull"
//! (Issue #33 follow-up; #232, M2.20f; ADR 0044). Each acts on the
//! checked-out branch or the repository itself, never the clicked target, so
//! each is a single entry rather than one per branch.

use leptos::*;

use crate::api::fetch_head_branch;
use crate::features::core_traits::RequestTarget;
use crate::features::dialogs::core::{Dialog, ErrorNotice};
use crate::features::graph::core::{disabled_menu_item_copy, pull_label};
use crate::features::operations::core::PendingIntent;
use crate::features::operations::kind::{rebase_item_label, HeadBranch};
use crate::icons::GitIcons;
use crate::state::{Features, MenuData, PendingOp};
use git_vista_protocol::RebaseStatus;

/// Whether a Fetch or Pull is already running (#232, M2.20f).
///
/// Both share the single localStorage resume slot
/// (`prefs::INFLIGHT_REMOTE_OP_KEY` / `InFlightRemoteOp`) — see
/// that key's doc comment in `prefs.rs`. A second Fetch or Pull
/// admitted while one is in flight overwrites that one entry, so
/// on reload only the second resumes and the first is silently
/// lost (or the second settles and clears the key while the
/// first is still running). This closure is the actual gate;
/// `operations.core()` is the same public accessor
/// `in_flight_count` uses, just filtered to the two kinds that
/// share the slot rather than counting every in-flight write.
fn remote_op_running(features: Features) -> Option<String> {
    features.operations.core().with(|c| {
        c.in_flight()
            .find(|f| matches!(f.kind, PendingOp::Fetch { .. } | PendingOp::Pull { .. }))
            .map(|f| f.kind.describe())
    })
}

/// Builds `(rebase_item, fetch_item, pull_item)`.
///
/// `rebase_status` is the live resource each of these three reads at render
/// time, exactly where the original inline code read it — passed through as
/// the resource handle itself so the reactive tracking is unchanged.
pub(super) fn build_remote_items(
    features: Features,
    ic: &'static GitIcons,
    m: &MenuData,
    rebase_status: Resource<(bool, u64), Option<RebaseStatus>>,
) -> (Option<View>, Option<View>, Option<View>) {
    let Features {
        dialogs,
        operations,
        shell,
        ..
    } = features;
    // "Rebase onto main" (Issue #33 follow-up). Rebase acts on the *checked-
    // out* branch, not the clicked target — like the "Commit …" items — so
    // it's a single entry, not one per branch. Gated on the live
    // `/api/rebase-status`: disabled (with the reason) when the branch is
    // already based on the base, HEAD is detached, or there's no main —
    // a rebase that would do nothing shouldn't look available. While the
    // status is still loading the item stays enabled; the server answers
    // a raced no-op with "Already up to date" rather than a phantom
    // rebase. Resolve the live HEAD branch on click, then open the
    // confirm modal. Omitted on a branch stub: a zero-commit branch has
    // nothing to replay, and the item would silently target the checked-
    // out branch instead ("Rebase ‘main’ onto main?" from the stub's own
    // menu).
    //
    // Issue #328: the label itself must say *what* it rebases, not only
    // `base`. This item sits in the per-commit menu next to Merge/Checkout/
    // Undo, which all act on the commit that was clicked — Rebase is the one
    // exception, and a bare "Rebase onto {base}" read as if it were another
    // one, so a click here silently ignored the clicked commit and rebased
    // the checked-out branch instead. `rebase_item_label` (kind.rs, host-
    // tested) restates the same subject the confirm dialog already names
    // afterward, one step earlier — visible before the click, not only in
    // the modal that follows it. Chose this over moving the item to a
    // repo-scoped surface (option 2) because no such surface exists yet in
    // this app (checked: no toolbar/header component, nothing else calls
    // this "repo-scoped") and building one is out of this lane's file set;
    // over a real per-commit rebase (option 1) because that needs a new
    // operation variant, planner support and its own risk classification —
    // too large for a menu-label fix.
    let rebase_item = (!m.is_branch).then(|| {
        let status = rebase_status.get().flatten();
        let base = status
            .as_ref()
            .map_or_else(|| "main".to_string(), |s| s.base.clone());
        let branch = status.as_ref().and_then(|s| s.branch.clone());
        let label = rebase_item_label(branch.as_deref(), &base);
        let reason = status.as_ref().and_then(|s| {
            if s.branch.is_none() {
                Some("HEAD is detached — no branch to rebase".to_string())
            } else if !s.base_exists {
                Some(format!("No ‘{}’ branch to rebase onto", s.base))
            } else if s.up_to_date {
                let b = s.branch.as_deref().unwrap_or("HEAD");
                Some(format!(
                    "‘{b}’ is already based on {} — nothing to rebase",
                    s.base
                ))
            } else {
                None
            }
        });
        if let Some(reason) = reason {
            let (aria_label, visible_reason) = disabled_menu_item_copy(&label, &reason);
            return view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.merge}</span>
                    {label}
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view();
        }
        let on = move |_| {
            let base = base.clone();
            shell.close_menu();
            // Rebase targets the checked-out branch, not a named one, so its
            // request identity is the repository itself.
            let seq = operations.next_seq();
            let key = operations.request_key(RequestTarget::Repository);
            spawn_local(async move {
                let current = fetch_head_branch().await.unwrap_or(None);
                let intent = PendingIntent {
                    seq,
                    key,
                    kind: PendingOp::Rebase { current, base },
                };
                if !operations.admit_intent(&intent) {
                    return;
                }
                dialogs.open(Dialog::Confirm);
                shell.open_confirm(intent.kind);
            });
        };
        view! {
            <button class="ctx-item" on:click=on>
                // The merge glyph — rebase reintegrates onto another base.
                <span class="nf ctx-icon">{ic.merge}</span>
                {label}
            </button>
        }
        .into_view()
    });
    // "Fetch" (#232, M2.20f): repo-scoped like Rebase, not per-branch
    // like Push — there's no per-branch remote-tracking surface in
    // this menu. Single tap, styled exactly like `push_item`: no
    // live pre-check needed, because a fetch has no branch
    // dependency the way merge/checkout/delete do. ADR 0047 records
    // that in practice only `origin` is ever in play, and #232's
    // scope names no remote picker, so the remote is fixed rather
    // than offered as a choice.
    //
    // Disabled (with reason, #65) while a Fetch or Pull is already
    // in flight — see `remote_op_running` above.
    let fetch_item = (!m.is_branch).then(|| {
        if let Some(running) = remote_op_running(features) {
            let reason = format!("{running} — only one Fetch or Pull can run at a time");
            let (aria_label, visible_reason) = disabled_menu_item_copy("Fetch", &reason);
            return view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.branch_alt}</span>
                    "Fetch"
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view();
        }
        let on = move |_| {
            dialogs.open(Dialog::Confirm);
            shell.open_confirm(PendingOp::Fetch {
                remote: "origin".to_string(),
            });
            shell.close_menu();
        };
        view! {
            <button class="ctx-item" on:click=on>
                // Reuses the remote-branch glyph — both actions talk
                // to the remote, and this app has no dedicated
                // fetch/pull icon yet.
                <span class="nf ctx-icon">{ic.branch_alt}</span>
                "Fetch"
            </button>
        }
        .into_view()
    });
    // "Pull" (#232, M2.20f, ADR 0044): repo-scoped like Rebase.
    // Unlike every other branch op here, this cannot open the shared
    // `Dialog::Confirm` modal directly: `MergeStrategy` has exactly
    // two variants, derives no `Default`, and carries no sentinel
    // "not yet chosen" value (plan.rs:307-316), so there is no
    // `OperationKind::Pull` this click could build before the user
    // has picked one — inventing a placeholder to "correct before
    // dispatch" would be exactly the silent default ADR 0044 spent
    // three enforcement layers ruling out at the wire layer. Instead
    // this opens the picker (`Dialogs::open_pull_picker`), which
    // holds only `{remote, branch}` until a tap on Merge or Rebase
    // supplies the missing field; only the picker's own confirm tap
    // constructs `OperationKind::Pull`, at the same instant it is
    // dispatched.
    //
    // The branch is resolved live on click, exactly like
    // `rebase_item`'s `fetch_head_branch()` pre-check above, guarded
    // by the same click-order race protection (`admit_intent`) every
    // other live-checked item here uses: a slower response from an
    // earlier tap must not reopen the picker over a dialog a later
    // tap is already showing. The intent's `kind` is never sent
    // anywhere — `operations.dispatch` is never called with it —
    // `MergeStrategy::Merge` is an inert placeholder that exists only
    // to satisfy `PendingIntent`'s shape and is discarded the
    // instant `admit_intent` returns; it never reaches the picker,
    // the wire, or the screen.
    //
    // The label (#325 follow-up) names the branch the same way
    // `rebase_item`'s does — read from `rebase_status` above, which
    // already carries the checked-out branch (`RebaseStatus::branch`,
    // itself `git_vista_git::read_head_branch`) under the identical
    // `!m.is_branch` gate this item renders behind, so this costs no
    // new resource or poll. `pull_label` (features::graph::core) is
    // pure so the composition is host-tested; `None` (status still
    // loading, or a detached HEAD) degrades to naming just the
    // remote rather than the bare "Pull" this replaces.
    //
    // Disabled (with reason, #65) while a Fetch or Pull is already
    // in flight — see `remote_op_running` above `fetch_item`.
    let pull_item = (!m.is_branch).then(|| {
        let branch = rebase_status.get().flatten().and_then(|s| s.branch);
        let label = pull_label(branch.as_deref(), "origin");
        if let Some(running) = remote_op_running(features) {
            let reason = format!("{running} — only one Fetch or Pull can run at a time");
            let (aria_label, visible_reason) = disabled_menu_item_copy(&label, &reason);
            return view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.merge}</span>
                    {label}
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view();
        }
        let on = move |_| {
            shell.close_menu();
            let seq = operations.next_seq();
            let key = operations.request_key(RequestTarget::Repository);
            spawn_local(async move {
                let remote = "origin".to_string();
                match HeadBranch::classify(fetch_head_branch().await) {
                    HeadBranch::Known(branch) => {
                        let intent = PendingIntent {
                            seq,
                            key,
                            kind: PendingOp::Pull {
                                remote: remote.clone(),
                                branch: branch.clone(),
                                strategy: git_vista_protocol::plan::MergeStrategy::Merge,
                            },
                        };
                        if !operations.admit_intent(&intent) {
                            return;
                        }
                        dialogs.open_pull_picker(remote, branch);
                    }
                    // No branch to pull into. #316 pattern: the app's
                    // own modal, never a silent no-op and never a
                    // native alert().
                    HeadBranch::Detached => {
                        dialogs.open(Dialog::Error);
                        shell.open_error(ErrorNotice {
                            title: "Can't pull",
                            body: "HEAD is detached — check out a branch first."
                                .to_string(),
                        });
                    }
                    // The read itself failed. Saying "HEAD is detached"
                    // here would be a diagnosis the app never made and
                    // advice that can't help — say what happened.
                    HeadBranch::Unknown(err) => {
                        dialogs.open(Dialog::Error);
                        shell.open_error(ErrorNotice {
                            title: "Can't pull",
                            body: format!(
                                "Couldn't read which branch is checked out — \
                                 {err}\n\nTry again."
                            ),
                        });
                    }
                }
            });
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.merge}</span>
                {label}
            </button>
        }
        .into_view()
    });
    (rebase_item, fetch_item, pull_item)
}
