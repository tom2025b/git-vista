//! The three "Commit …" items (Issue #33, M2.19c/#224): "Commit Changes",
//! "Create empty commit", and "Amend last commit".

use leptos::*;

use crate::api::fetch_commit_detail;
use crate::features::dialogs::commit::{amend_offer, AmendOffer};
use crate::features::dialogs::core::Dialog;
use crate::features::graph::core::disabled_menu_item_copy;
use crate::icons::GitIcons;
use crate::state::{CommitIntent, Features, MenuData};

/// Builds `(commit_changes, commit_empty, amend_item)`.
///
/// The two "Commit …" items (Issue #33). Clicking one closes the menu
/// and opens the commit-message modal (below); the actual POST + refresh
/// happens when the user confirms there.
///
/// On a commit dot they're enabled only on the HEAD tip — the one place
/// a plain `git commit` lands where the user clicked. On a branch stub,
/// "Create empty commit" is enabled too and targets the stub's own
/// branch (the server writes the commit object and moves just that ref,
/// no checkout needed) — it's exactly how an empty new branch takes its
/// first commit. Staged changes belong to the checked-out branch's
/// index, so that item stays HEAD-only everywhere. Anything else
/// renders disabled with the reason in its hover title.
pub(super) fn build_commit_items(
    features: Features,
    ic: &'static GitIcons,
    m: &MenuData,
) -> (View, View, View) {
    let Features {
        dialogs,
        status,
        shell,
        ..
    } = features;
    let is_head = m.is_head;
    let is_stub = m.is_branch;
    // A stub carries exactly its own branch name (see `MenuData::branches`).
    let stub_branch = is_stub.then(|| m.branches.first().cloned()).flatten();
    // `icon` is the glyph beside the item — the commit glyph for both
    // commit variants ("Stage Changes", in `worktree_items`, uses the diff-added glyph).
    let make_commit_item = move |icon: &'static str, label: &'static str, allow_empty: bool| {
        let stub_branch = stub_branch.clone();
        let enabled = is_head || (allow_empty && stub_branch.is_some());
        if !enabled {
            let reason = if is_stub {
                "Staged changes can only be committed on the checked-out branch"
            } else {
                "Only available on the current HEAD commit"
            };
            let (aria_label, visible_reason) = disabled_menu_item_copy(label, reason);
            return view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{icon}</span>
                    {label}
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view();
        }
        let on_commit = move |_| {
            // Open the dialog *before* closing the menu: `shell.close_menu()`
            // synchronously disposes this handler's own reactive owner, so
            // any signal write after it is unreliable. Set the dialog first.
            //
            // No draft clear here (#226): opening is how a
            // suspension-recovered draft comes back, so the opener must not
            // wipe it. The draft clears on successful submit instead
            // (`dialogs/commit.rs`'s `clear_message_for`), which is what
            // actually consumes it. Note what `dialogs.open` *does* reset:
            // the amend buffer and phase (#224), which belong to a different
            // question than the one this item is asking.
            dialogs.open(Dialog::Commit);
            shell.open_commit_dialog(if allow_empty {
                CommitIntent::Empty {
                    branch: stub_branch.clone(),
                }
            } else {
                CommitIntent::Staged
            });
            // The dialog's staged-scope review renders from the shared
            // status read, and the menu may have been sitting open since
            // before the last stage/unstage. Refetching here is what makes
            // the list the user is about to approve a statement about the
            // repository *now* rather than whenever the panel last looked.
            status.refetch();
            shell.close_menu();
        };
        view! {
            <button class="ctx-item" on:click=on_commit>
                <span class="nf ctx-icon">{icon}</span>
                {label}
            </button>
        }
        .into_view()
    };
    let commit_changes = make_commit_item(ic.commit, "Commit Changes", false);
    let commit_empty = make_commit_item(ic.commit, "Create empty commit", true);
    // "Amend last commit" (M2.19c, #224) — the third commit mode, beside the
    // other two and gated the same way, with one extra restriction: unlike an
    // empty commit, it is never offered on a branch stub. `GitOperation::
    // AmendCommit` has no branch target at all (it always rewrites the
    // checked-out branch's own tip), so there is no "amend that stub" to offer.
    //
    // The tapped commit's id is the compare-and-swap pin the request carries.
    // That is the point of taking it from here rather than re-reading HEAD at
    // submit time: it is the commit the user was looking at when they chose to
    // rewrite it, and the server refuses if the tip has moved since — which the
    // dialog then turns into a guided re-check rather than an error.
    //
    // The gate itself is `amend_offer`, in the host-tested core, not a
    // condition spelled out here: this file is wasm-only, so an inverted
    // or dropped condition would put "Amend last commit" on every stub —
    // or take it away everywhere — with nothing in the suite going red.
    let amend_tip = m.commit.clone();
    let amend_item = match amend_offer(is_head, is_stub) {
        AmendOffer::Offered => {
            let on_amend = move |_| {
                let tip = amend_tip.clone();
                dialogs.open(Dialog::Commit);
                shell.open_commit_dialog(CommitIntent::Amend {
                    expected_tip: tip.clone(),
                });
                // Hold the confirm button until the read below answers
                // whether this commit is already on a remote (#225).
                // Opening is synchronous and the read is not, so
                // without this the dialog spends the whole request
                // showing an *enabled* Amend button over a pre-flight
                // that has nothing to read — and `amend_preflight`
                // sends on "nothing read". Two ordering constraints,
                // both pinned by `features::a11y::audit` because
                // nothing here compiles under `cargo test`: after
                // `dialogs.open` (which resets the phase), and before
                // `shell.close_menu()` (which disposes this handler's
                // reactive owner, after which writes are unreliable).
                dialogs.begin_publication_read(&tip);
                status.refetch();
                shell.close_menu();
                // Pre-fill with the tip's *whole* message (summary and body), not
                // the graph row's first line: `git commit --amend -m` replaces the
                // message outright, so seeding from a summary would silently drop
                // the body of every commit amended from here. A failed read leaves
                // the box empty and the confirm button disabled, which is the safe
                // direction — the dialog never invents a message.
                //
                // The same read answers two questions (#225): the
                // pre-fill, and whether this commit is already on a
                // remote — `CommitDetail::on_remote`, an exact
                // per-commit walk rather than membership of whatever
                // page is loaded. Recorded against `tip` so it can only
                // ever gate an amend of this commit. A failed read
                // records nothing, and `amend_preflight` treats "not
                // read" as unknown; see its doc comment for why unknown
                // sends rather than escalates.
                //
                // Both answers go through `apply_amend_detail` rather
                // than being written here, and that is the fix for a
                // second window as real as the one the hold above
                // closes: this callback resumes after an `await`, by
                // which point the dialog may have been reopened on
                // another commit. `PreflightKnowledge` holds one read
                // at a time, so writing an abandoned tip's answer here
                // *evicts* the answer for the commit on screen and the
                // ceremony silently stops firing for it. The currency
                // check lives in `detail_read_use`, where it is
                // host-tested; nothing in this file is.
                spawn_local(async move {
                    if let Ok(detail) = fetch_commit_detail(&tip).await {
                        dialogs.apply_amend_detail(&tip, detail.on_remote, &detail.message);
                    }
                    // Outside the `Ok` arm on purpose: a failed read
                    // has to release the button too, or one bad GET
                    // would make amend permanently unreachable. That
                    // lands on the documented `Unknown` ⇒ send path,
                    // which is a stated gap rather than a new one.
                    dialogs.finish_publication_read(&tip);
                });
            };
            view! {
                <button class="ctx-item" on:click=on_amend>
                    <span class="nf ctx-icon">{ic.commit}</span>
                    "Amend last commit"
                </button>
            }
            .into_view()
        }
        AmendOffer::Blocked(reason) => {
            let (aria_label, visible_reason) = disabled_menu_item_copy("Amend last commit", reason);
            view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.commit}</span>
                    "Amend last commit"
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view()
        }
    };
    (commit_changes, commit_empty, amend_item)
}
