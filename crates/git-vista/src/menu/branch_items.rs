//! The branch operations (Issue #33 follow-up): checkout / compare / merge /
//! push / force-push / delete, one set per local branch living at this
//! target, plus the GitHub "Create Pull Request" link and the non-GitHub
//! forge branch link (ADR 0010).

use leptos::*;

use crate::api::{fetch_head_branch, preview_push};
use crate::features::core_traits::RequestTarget;
use crate::features::dialogs::core::{Dialog, ErrorNotice};
use crate::features::graph::core::{remote_tip_from_plan, RemoteTipKnowledge};
use crate::features::operations::core::PendingIntent;
use crate::features::operations::kind::{ForceWithLease, HeadBranch};
use crate::icons::GitIcons;
use crate::state::{Features, MenuData, PendingOp, ViewerDoc};
use git_vista_protocol::diff::DiffSpec;
use git_vista_protocol::plan::RefName;
use git_vista_protocol::RebaseStatus;

/// One `<button>`/`<a>` set per local branch at this target. Each write item
/// opens the confirm modal rather than acting immediately — the actual POST +
/// refresh happens there. Raise the confirm modal *before*
/// `shell.close_menu()`, which disposes this handler's reactive owner (same
/// ordering caveat `menu.rs`'s module doc opens with).
///
/// `rebase_status` is the live resource `push_item` reads at click time (not
/// render time) for its `set_upstream` decision — passed through as the
/// resource handle itself, not a snapshot, so that read still happens exactly
/// when the original inline closure made it.
pub(super) fn build_branch_items(
    features: Features,
    ic: &'static GitIcons,
    m: &MenuData,
    rebase_status: Resource<(bool, u64), Option<RebaseStatus>>,
) -> View {
    let Features {
        dialogs,
        operations,
        shell,
        ..
    } = features;
    m.branches
        .iter()
        .flat_map(|b| {
            let b = b.clone();
            // Checkout: switch HEAD (and the working tree) to this branch.
            // Like merge/delete, the "already on it?" test resolves *live*
            // on click, not from the possibly-stale graph — the confirm
            // dialog disables itself when this is the checked-out branch.
            let checkout_item = {
                let branch = b.clone();
                let on = move |_| {
                    let branch = branch.clone();
                    shell.close_menu();
                    // Identity is minted here, synchronously, before the await —
                    // it must record when the user tapped, not when the pre-check
                    // answered (M1.11, #64).
                    let seq = operations.next_seq();
                    let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                    spawn_local(async move {
                        let current = fetch_head_branch().await.unwrap_or(None);
                        let intent = PendingIntent {
                            seq,
                            key,
                            kind: PendingOp::Checkout { branch, current },
                        };
                        if !operations.admit_intent(&intent) {
                            return;
                        }
                        // Start the ghost-click guard when the modal opens.
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(intent.kind);
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        // The branch-switch glyph — HEAD moving between branches.
                        <span class="nf ctx-icon">{ic.checkout}</span>
                        {format!("Checkout ‘{b}’")}
                    </button>
                }
                .into_view()
            };
            // Merge into the checked-out branch. The target is resolved *live*
            // on click (not from the possibly-stale graph), so the item stays
            // generic — "into current branch" — and the confirm dialog names
            // the real HEAD branch once the fetch returns. Whether it's a
            // no-op self-merge, a detached HEAD, or a read that failed
            // outright (`HeadBranch::Unknown` — carried distinctly, never
            // folded into "detached") is decided there too.
            let merge_item = {
                let branch = b.clone();
                let on = move |_| {
                    let branch = branch.clone();
                    shell.close_menu();
                    let seq = operations.next_seq();
                    let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                    spawn_local(async move {
                        let into = HeadBranch::classify(fetch_head_branch().await);
                        let intent = PendingIntent {
                            seq,
                            key,
                            kind: PendingOp::Merge { branch, into },
                        };
                        if !operations.admit_intent(&intent) {
                            return;
                        }
                        // Start the ghost-click guard when the modal opens.
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(intent.kind);
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        // The merge glyph, matching the merge-dot marker.
                        <span class="nf ctx-icon">{ic.merge}</span>
                        {format!("Merge ‘{b}’ into current branch")}
                    </button>
                }
                .into_view()
            };
            // Push: always available; git reports if there's no origin/upstream.
            // #233: also offers `--set-upstream` when `b` is the
            // checked-out branch and `/api/rebase-status` said it
            // has none — `rebase_status` (above) already fetched
            // this under the same `!m.is_branch` gate `pull_item`
            // reads it behind, so this costs no new poll. Scoped to
            // the checked-out branch only: `RebaseStatus::has_upstream`
            // answers for HEAD alone (`OperationKind::Push`'s own doc
            // comment says why), so pushing any other branch from
            // this menu still gets `set_upstream: false`, matching
            // pre-#233 behaviour exactly.
            let push_item = {
                let branch = b.clone();
                let on = move |_| {
                    let set_upstream = rebase_status
                        .get()
                        .flatten()
                        .filter(|s| s.branch.as_deref() == Some(branch.as_str()))
                        .and_then(|s| s.has_upstream)
                        == Some(false);
                    dialogs.open(Dialog::Confirm);
                    shell.open_confirm(PendingOp::Push {
                        branch: branch.clone(),
                        set_upstream,
                        force: None,
                    });
                    shell.close_menu();
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        // Push updates the *remote* branch — its glyph.
                        <span class="nf ctx-icon">{ic.branch_alt}</span>
                        {format!("Push ‘{b}’")}
                    </button>
                }
                .into_view()
            };
            // Force-push (#233): a *separate* entry point from Push
            // above, on purpose — the acceptance criterion is that a
            // force-with-lease is "unreachable from the normal
            // one-tap push button", so this cannot be an escalation
            // Push falls into (unlike Delete → ForceDelete,
            // `operations::core::escalation` can't apply here: it
            // takes only `(kind, &str)` and has no way to produce an
            // oid or a server risk classification).
            //
            // Async and therefore raced like `merge_item`/`pull_item`
            // above — but with a longer window than either, and the
            // guard has to be placed accordingly.
            //
            // `admit_intent` can only ever *refuse* an intent at the
            // moment it is offered (`latest_wins` is a plain
            // `incoming.seq >= cur.seq`); it cannot retract a
            // continuation that already passed it. So admitting
            // *before* the network calls — as an earlier draft of this
            // handler did — buys nothing: an earlier tap's continuation
            // sails past the gate it already cleared and clobbers a
            // later tap's dialog, because `open_confirm` is an
            // unguarded `set`. The failure that makes this worth the
            // words: tap Force Push on `a`, tap it again on `b`, and
            // `a`'s slower plans can leave a danger-styled confirm on
            // screen that reads `b` but dispatches a force-with-lease
            // against `a`.
            //
            // Hence `still_current` below, re-offered after *every*
            // await and before *any* signal write. That is what
            // `merge_item`/`pull_item` get for free by admitting after
            // their single await; this handler makes two sequential
            // `/api/plan` round trips with the menu already closed and
            // nothing on screen — precisely the silent window that
            // invites the second tap — so it has to re-check
            // explicitly rather than inherit their shape.
            //
            // Re-offering is safe and idempotent: an un-superseded
            // continuation offers its own `seq` back and `seq >= seq`
            // holds. It also re-runs the key's epoch check, so a repo
            // that moved mid-flight (Refresh, repo switch, drift
            // reload) drops the continuation too — the same fencing
            // `RequestKey` exists for.
            //
            // Two `/api/plan` calls, not one: the first reads what
            // origin/`b` currently points at (a *plain*-push plan,
            // since the lease oid isn't known yet); the second reads
            // the server's actual `RiskLevel` for the lease plan
            // built from that oid, so the confirmation's danger
            // styling reflects the planner's own classification
            // rather than an assumption baked in here
            // (`push_confirm_copy`'s doc comment, `graph::core`).
            let force_push_item = {
                let branch = b.clone();
                let on = move |_| {
                    let branch = branch.clone();
                    shell.close_menu();
                    let seq = operations.next_seq();
                    let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                    spawn_local(async move {
                        let intent = PendingIntent {
                            seq,
                            key,
                            kind: PendingOp::Push {
                                branch: branch.clone(),
                                set_upstream: false,
                                force: None,
                            },
                        };
                        if !operations.admit_intent(&intent) {
                            return;
                        }
                        // Re-offer after each await; see the ordering
                        // note above this item for why admitting once,
                        // up front, does not hold.
                        let still_current = move || operations.admit_intent(&intent);
                        let plain = preview_push(
                            "origin",
                            &branch,
                            false,
                            git_vista_protocol::ForcePublish::None,
                        )
                        .await;
                        if !still_current() {
                            return;
                        }
                        let oid = match plain
                            .map(|p| remote_tip_from_plan(&p.expected_ref_changes))
                        {
                            Ok(RemoteTipKnowledge::Known(oid)) => oid,
                            Ok(RemoteTipKnowledge::NotYetPushed) => {
                                dialogs.open(Dialog::Error);
                                shell.open_error(ErrorNotice {
                                    title: "Nothing to force-push",
                                    // Says "no local record of", not
                                    // "isn't on origin". The planner
                                    // decides this from the local
                                    // remote-tracking ref and never
                                    // reads origin live — that is
                                    // force-with-lease working as
                                    // designed (planner.rs, the lease
                                    // is *by definition* what we last
                                    // saw). But it means a pruned or
                                    // stale tracking ref lands here
                                    // too, and telling the user the
                                    // branch "isn't on origin" would
                                    // then be a flat lie. Hence the
                                    // hedge, and the Fetch escape
                                    // hatch: this notice is one of the
                                    // few places the app can be wrong
                                    // about the remote and still be
                                    // behaving correctly.
                                    body: format!(
                                        "There's no local record of ‘{branch}’ on origin, \
                                         so there's no remote commit to lease against — a \
                                         plain Push already does everything a \
                                         force-with-lease would. If you expect it to be \
                                         there, Fetch first and try again."
                                    ),
                                });
                                return;
                            }
                            Ok(RemoteTipKnowledge::Unreadable) => {
                                dialogs.open(Dialog::Error);
                                shell.open_error(ErrorNotice {
                                    title: "Couldn't preview force push",
                                    body: format!(
                                        "Couldn't read what origin/{branch} currently \
                                         points at."
                                    ),
                                });
                                return;
                            }
                            Err(e) => {
                                dialogs.open(Dialog::Error);
                                shell.open_error(ErrorNotice {
                                    title: "Couldn't preview force push",
                                    body: e,
                                });
                                return;
                            }
                        };
                        let leased = preview_push(
                            "origin",
                            &branch,
                            false,
                            git_vista_protocol::ForcePublish::WithLease {
                                expected_remote_tip: oid.clone(),
                            },
                        )
                        .await;
                        if !still_current() {
                            return;
                        }
                        let risk = match leased {
                            Ok(plan) => plan.risk,
                            Err(e) => {
                                dialogs.open(Dialog::Error);
                                shell.open_error(ErrorNotice {
                                    title: "Couldn't preview force push",
                                    body: e,
                                });
                                return;
                            }
                        };
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(PendingOp::Push {
                            branch,
                            set_upstream: false,
                            force: Some(ForceWithLease {
                                expected_remote_tip: oid,
                                risk,
                            }),
                        });
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        // #233: a distinct glyph from Push's
                        // `ic.branch_alt`, so the danger-adjacent
                        // item reads as a different action at a
                        // glance, not a variant of the same one.
                        <span class="nf ctx-icon">{ic.push}</span>
                        {format!("Force Push ‘{b}’…")}
                    </button>
                }
                .into_view()
            };
            // Delete: like merge, the "is this the checked-out branch?" test is
            // resolved live on click, not from the possibly-stale graph. The
            // confirm dialog blocks deleting the current branch; git's safe
            // `-d` still refuses an unmerged one server-side. A pre-check that
            // *failed* travels as `HeadBranch::Unknown` — never folded into
            // "detached", which is the answer that would have enabled the
            // button — and the dialog refuses to offer the delete on it.
            let delete_item = {
                let branch = b.clone();
                let on = move |_| {
                    let branch = branch.clone();
                    shell.close_menu();
                    let seq = operations.next_seq();
                    let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                    spawn_local(async move {
                        let current = HeadBranch::classify(fetch_head_branch().await);
                        let intent = PendingIntent {
                            seq,
                            key,
                            kind: PendingOp::Delete { branch, current },
                        };
                        if !operations.admit_intent(&intent) {
                            return;
                        }
                        // Start the ghost-click guard when the modal opens.
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(intent.kind);
                    });
                };
                view! {
                    <button class="ctx-item danger" on:click=on>
                        // The diff-removed glyph, inheriting the item's red.
                        <span class="nf ctx-icon">{ic.deleted}</span>
                        {format!("Delete ‘{b}’")}
                    </button>
                }
                .into_view()
            };
            // "Create Pull Request": a real anchor to GitHub's compare page
            // (`…/compare/main...<branch>`), opening in a new tab — a live
            // link, not a scripted `window.open`, which iOS WebKit blocks
            // (same reason as "Open on GitHub"). Shown only on a GitHub repo;
            // omitted otherwise, since there's no compare page to point at.
            // "Compare with HEAD" (M2.16, #69): the explicit-source/target
            // diff, `DiffSpec::RefVsRef`. This is the capability nothing
            // else in the app has — `Diff` shows one commit against its
            // parent and `Staging` shows the two live worktree/index
            // diffs, so "what changed between these two branches" had no
            // surface until now.
            //
            // A read, so unlike its neighbours here it raises no confirm
            // dialog and mints no operation: it opens the viewer directly.
            // `base` is the branch tapped and `target` is HEAD, so the
            // title reads "<branch> → <head>" — the direction a user
            // asking "what does HEAD have that this branch does not"
            // expects.
            let compare_item = {
                let branch = b.clone();
                let on = move |_| {
                    let branch = branch.clone();
                    shell.close_menu();
                    spawn_local(async move {
                        let Ok(Some(head)) = fetch_head_branch().await else {
                            // Detached HEAD, or the read failed. Nothing
                            // sensible to compare against, and inventing
                            // a target would show a diff the user did not
                            // ask for — so this does nothing rather than
                            // guessing.
                            return;
                        };
                        let (Ok(base), Ok(target)) =
                            (RefName::new(&branch), RefName::new(&head))
                        else {
                            return;
                        };
                        shell.open_viewer(ViewerDoc::Spec {
                            spec: DiffSpec::RefVsRef { base, target },
                        });
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        <span class="nf ctx-icon">{ic.modified}</span>
                        {format!("Compare {b} with HEAD")}
                    </button>
                }
                .into_view()
            };
            let mut items = vec![
                checkout_item,
                compare_item,
                merge_item,
                push_item,
                force_push_item,
            ];
            // Non-GitHub forge branch link (ADR 0010): only when there is
            // no GitHub base, so it never duplicates the GitHub items.
            if m.repo_url.is_none() {
                if let Some(base) = m.remote_web_url.as_ref() {
                    let url = git_vista_core::forge::branch_url(base, &b);
                    let host = git_vista_core::forge::host_label(base);
                    let branch = b.clone();
                    items.push(
                        view! {
                            <a
                                class="ctx-item"
                                href=url
                                target="_blank"
                                rel="noopener"
                                on:click=move |_| shell.close_menu()
                            >
                                <span class="nf ctx-icon">{ic.github}</span>
                                {format!("View ‘{branch}’ on {host}")}
                            </a>
                        }
                        .into_view(),
                    );
                }
            }
            if let Some(base) = m.repo_url.as_ref() {
                let branch = b.clone();
                let url = format!("{base}/compare/main...{branch}");
                items.push(
                    view! {
                        <a
                            class="ctx-item"
                            href=url
                            target="_blank"
                            rel="noopener"
                            on:click=move |_| shell.close_menu()
                        >
                            // The pull-request glyph flags this GitHub action.
                            <span class="nf ctx-icon">{ic.pull_request}</span>
                            {format!("Create Pull Request for ‘{branch}’")}
                        </a>
                    }
                    .into_view(),
                );
            }
            items.push(delete_item);
            items
        })
        .collect_view()
}
