//! The branch-op / undo / working-tree confirmation modal (Issue #33
//! follow-up; M2.18b, #220 added the second ceremony).
//!
//! Two confirmation strengths live here, and the difference is structural
//! rather than cosmetic: a branch operation or a discard is one tap on the
//! confirm button, while deleting untracked files — the one operation in this
//! app with no way back — leaves that button inert until a separate arm
//! control has been pressed. Which ceremony an operation gets, and every word
//! either one shows, is decided in `features::dialogs::core` (pure,
//! host-tested); this file is the part that needs a DOM.

use leptos::*;

use git_vista_core::activity::UndoAction;
use git_vista_protocol::{MergeStrategy, RepoMode};

// M11.03 (#548): `select_worktree_request`, not `select_request`. The offer
// this arm makes is to open a **linked worktree** the census discovered, and
// `/api/select` resolves ids through the catalog — which a worktree nobody
// scanned is not in, so a serviceable sibling answered `404 No such
// repository.` The offer was honest about failing, which is not the same as
// working; #651's body named this gap and this is where it closes.
use crate::api::select_worktree_request;
use crate::features::session::signals as session_state;

use crate::features::dialogs::core::preview_subject;
use crate::features::dialogs::core::{
    checkout_confirm_action, checkout_confirm_prompt, cherry_pick_confirm_prompt,
    delete_confirm_prompt, merge_confirm_prompt, worktree_confirm, CheckoutAction, ConfirmPrompt,
    Dialog, ErrorNotice, PullTarget, WorktreeAction, TOUCH_TARGET_STYLE,
};
use crate::features::preview::core::{preview_action, PreviewAction};

use crate::features::freshness::core::{blocked_by_staleness, confirm_enabled};
use crate::features::preview::signals::PreviewSlot;

use super::{freshness_notice_view, preview_panel_view};
use crate::features::explain::core::{render, LinkTarget, RenderedSection, Span};
use crate::features::graph::core::{disabled_menu_item_copy, push_confirm_copy};
use crate::features::operations::kind::OperationKind;
use crate::state::{Features, PendingOp};

/// The confirm/cancel button base style, with #65's 44x44 floor.
///
/// Every button in this modal carries it. The floor used to be missing here:
/// the old `padding:6px 14px` on a 13px font lands around 30px tall, under
/// the minimum the rest of the app was brought up to in #65 — and this modal
/// is inline-styled (see `dialogs/mod.rs` for why), so the stylesheet census
/// in `features::a11y::audit` never saw it.
const BUTTON_BASE: &str = "padding:8px 16px; font:inherit; border-radius:6px; ";

/// The branch-op confirmation modal (Issue #33 follow-up). Reuses the commit
/// modal's iPad-proven inline-styled overlay, minus any text input (so no void
/// `<input>` to trip the WebKit CSR bug). Confirming hands the operation to the
/// `operations` feature; cancelling or a backdrop tap closes it.
pub fn confirm_modal_view(features: Features) -> impl IntoView {
    let Features {
        dialogs,
        operations,
        shell,
        preview,
        // M12.05 (#555): the change feed. Read here so a plan whose repository
        // moved under it withdraws its own confirmation.
        freshness,
        // M11.02 (#547): the "open that worktree instead" path selects a
        // different worktree, which changes what every resource should be
        // reading — the same epoch bump the picker makes after its own
        // `/api/select`.
        graph,
        ..
    } = features;

    // Confirming used to clear the dialog and then `spawn_local` a future nothing held —
    // so the write existed nowhere between the tap and its reply, and closing a panel
    // mid-flight lost every trace of it (M1.11, #64, acceptance criterion 2). Now the
    // dialog only *raises* the operation; `operations` owns it from here, and it is held
    // above the canvas, so it outlives this modal and the re-read its completion triggers.
    let run_confirmed = move || {
        let Some(op) = shell.confirm_op_untracked() else {
            return;
        };
        // M11.02 (#547): one dialog, two possible outcomes. When the branch a
        // checkout names is already open at a worktree this app may serve,
        // the confirm button says "Open Worktree" and *selects that worktree*
        // rather than running a checkout git would certainly refuse. Which of
        // the two it is is `checkout_confirm_action`'s decision — pure and
        // host-tested, paired with `checkout_confirm_prompt` so the button's
        // label and the button's effect cannot drift apart.
        //
        // This is a courtesy, not the enforcement: the server refuses the
        // checkout on its own precondition whatever this file does.
        if let PendingOp::Checkout { elsewhere, .. } = &op {
            if let CheckoutAction::OpenWorktree { id, name } = checkout_confirm_action(elsewhere) {
                shell.close_confirm();
                spawn_local(async move {
                    // The posture the session is already in, and **never an
                    // escalation**: a refused checkout must not be a way to
                    // acquire Active mode. An unknown mode falls back to
                    // `Visualize`, the read-only one — the user re-picks from
                    // the picker if they want more, which is where that
                    // choice has always been made.
                    let mode = session_state::ui_mode().unwrap_or(RepoMode::Visualize);
                    match select_worktree_request(&id, mode).await {
                        Ok(()) => graph.update(|g| {
                            g.force_bump();
                        }),
                        Err(e) => shell.open_error(ErrorNotice {
                            title: "Couldn't open that worktree",
                            body: format!(
                                "‘{name}’ holds the branch, but selecting it failed: {e}"
                            ),
                        }),
                    }
                });
                return;
            }
        }
        shell.close_confirm();
        operations.dispatch(op);
    };

    // git's safe `branch -d` refuses an unmerged branch with "not fully merged"; rather
    // than dead-end on that, the modal re-opens offering `-D`. The *rule* now lives in
    // the operations core (`escalation`, host-tested); this effect is only the part that
    // needs a dialog. `take_escalation` consumes the entry, so the offer cannot repeat.
    create_effect(move |_| {
        // Subscribe to the registry so this runs whenever an operation settles.
        operations.core().with(|c| c.recent().count());
        if let Some(next) = operations.take_escalation() {
            // Restamp the ghost-click guard, exactly as when the modal is first shown:
            // the modal never visually closes, but it is now asking a different question.
            // `open` also disarms the two-tap control, so a re-asked question never
            // inherits an arm the user gave the previous one.
            dialogs.open(Dialog::Confirm);
            shell.open_confirm(next);
        }
    });

    // The graph preview (M10.08 A6, #594). Runs whenever the dialog's operation
    // changes — which includes opening, closing, and the escalation above
    // re-asking a different question in a modal that never visually closed.
    //
    // Three things this effect is careful about:
    //
    //  * It asks only for the operations the engine previews and the app has
    //    dialogs for — merge, revert and cherry-pick as of #594.
    //    `previewable` is where that list lives, host-tested, and is the
    //    authority: do not re-derive the count from this comment,
    //    because "which dialogs get a preview" is exactly the decision whose
    //    absence created #594.
    //  * It **clears** on every other case, `None` included. A close is what
    //    invalidates an in-flight request: `Preview::clear` bumps the
    //    generation, so a reply already on the wire cannot paint the next
    //    dialog with the last one's picture.
    //  * It never touches `enabled`. The preview informs and does not gate —
    //    every operation here was confirmable before previews existed.
    //
    // The composition itself is `preview_action`, host-tested;
    // `the_confirm_dialog_routes_its_preview_through_core` reads this file back
    // to pin that these two arms stay its only outlets. ADR 0115 records why
    // the decision lives there rather than here.
    //
    // `preview_subject` is spelled as a call in both arms rather than handed to
    // `.map()` as a function value, on purpose: `reachability_census` reads
    // call sites, and a core function reachable only as a value looks dead to
    // it. Trading one line for a call a reader and a test can both see is the
    // right side of that trade.
    create_effect(move |_| {
        let action = match &shell.confirm_op() {
            Some(op) => preview_action(Some(preview_subject(op))),
            None => preview_action(None),
        };
        match action {
            PreviewAction::Start(operation) => preview.start(operation),
            PreviewAction::Clear => preview.clear(),
        }
    });

    move || {
        shell.confirm_op().map(|op| {
            // Tracked read: the arm control and the confirm button both re-render the
            // moment step one is taken.
            let armed = dialogs.confirm_armed();
            // `enabled` gates the confirm button: a merge into itself or a detached
            // HEAD has no valid target, so the dialog is informational (Cancel only)
            // — and a live HEAD read that failed outright disables the destructive
            // arms too, because "couldn't tell" is never "safe to offer".
            // Explain Mode (M6.39b, #545). Present only where the operation
            // actually carries a plan — today that is the force-with-lease
            // push, whose menu entry already fetches one to read `risk`.
            //
            // `None` here means "this operation has no plan to explain", NOT
            // "this operation is simple". Every other confirmation in this
            // modal is built from its arguments and has never seen a plan;
            // giving them a panel means giving them a plan first, which is a
            // server round trip and a new failure mode per dialog. #545
            // carries that argument in full.
            let explanation = match &op {
                PendingOp::Push {
                    force: Some(f), ..
                } => Some(f.explanation.clone()),
                _ => None,
            };
            let ConfirmPrompt {
                title,
                body,
                confirm_label,
                danger,
                enabled,
                arm,
                blocked_reason,
            } = match &op {
                // All string composition — including the refusal when the live
                // HEAD read itself failed (`HeadBranch::Unknown`) — lives in
                // `merge_confirm_prompt` (features::dialogs::core, pure and
                // host-tested); this arm only plugs its answer in, the same
                // shape the Push arm below took for `push_confirm_copy`.
                PendingOp::Merge { branch, into } => merge_confirm_prompt(branch, into),
                PendingOp::CherryPick { commit, onto } => {
                    cherry_pick_confirm_prompt(commit, onto)
                }
                // #233: a plain push keeps the single-tap ceremony this
                // operation has always had; a force-with-lease push (reached
                // only through the menu's separate force-push entry point,
                // never this button) escalates to the danger tier
                // `ForceDelete`/`Undo` above already set the bar for. All
                // string composition — including which tier applies — lives
                // in `push_confirm_copy` (features::graph::core, pure and
                // host-tested); this arm only plugs its answer into the
                // shape every other arm here already builds.
                PendingOp::Push {
                    branch,
                    set_upstream,
                    force,
                } => {
                    let copy = push_confirm_copy(
                        branch,
                        *set_upstream,
                        force.as_ref().map(|f| (&f.expected_remote_tip, f.risk)),
                        force.as_ref().map_or(&[][..], |f| &f.advisories),
                    );
                    ConfirmPrompt::plain(
                        copy.title,
                        copy.body,
                        copy.confirm_label,
                        copy.danger,
                        true,
                    )
                }
                // Every word — including the refusal when another worktree
                // holds the branch, and the one when the census could not be
                // read at all — is `checkout_confirm_prompt`'s
                // (features::dialogs::core, pure and host-tested), the same
                // extraction the Merge and Delete arms already had.
                PendingOp::Checkout {
                    branch,
                    current,
                    elsewhere,
                } => checkout_confirm_prompt(branch, current, elsewhere),
                // Same extraction as the Merge arm above: the decision — most
                // importantly that a failed HEAD read never enables the delete
                // — is `delete_confirm_prompt`'s, host-tested in the pure core.
                PendingOp::Delete { branch, current } => delete_confirm_prompt(branch, current),
                // Reached only after a safe delete was refused for "not fully merged"
                // (see `run_confirmed`): offer the override, spelling out the risk.
                PendingOp::ForceDelete { branch } => ConfirmPrompt::plain(
                    "Force delete branch",
                    format!("‘{branch}’ isn't fully merged — force-deleting it discards any commits it holds that aren't on another branch. This can't be undone. Force delete it anyway?"),
                    "Force Delete",
                    true,
                    true,
                ),
                // Delete a local tag (M2.21d, #238). Danger-styled like the
                // branch delete arm above, but with no "is this the one
                // you're on?" gate — a tag has no checked-out state, so
                // there's nothing here to disable the button over, unlike
                // `PendingOp::Delete`'s `current == branch` case. The body
                // makes no reversibility claim either way: the server keeps
                // a recovery pin (`lifecycle_suite.rs`, ranked `Destructive`
                // not `Irreversible`), but there is no frontend Undo
                // affordance for it today, so claiming recoverability here
                // would promise a button that doesn't exist.
                PendingOp::DeleteLocalTag { tag } => ConfirmPrompt::plain(
                    "Delete tag",
                    format!("Delete tag ‘{tag}’? This removes it from this repository only — a copy already pushed to a remote is untouched."),
                    "Delete",
                    true,
                    true,
                ),
                // The undo confirmation (step 5). The server-built label already
                // says exactly what will happen ("Undo merge — reset ‘main’ to
                // abc1234"); the body adds what that means for history, and the
                // pushed warning when the discarded state is on the remote.
                PendingOp::Undo(u) => {
                    // M2.20e (#231): this used to read "git-vista never
                    // force-pushes", which stopped being true the day an
                    // explicit force-with-lease publish existed. The invariant
                    // it was describing is narrower and still holds — an *undo*
                    // never rewrites the remote — so the sentence now says that,
                    // and points at the thing a user would otherwise go looking
                    // for.
                    let warn = if u.warn_pushed {
                        " The discarded state is already pushed: undoing here \
                         changes nothing on origin (an undo never force-pushes), \
                         so the branch will show as behind until it's pushed \
                         again. Rewriting what origin has is a separate, \
                         explicit force-publish."
                    } else {
                        ""
                    };
                    match &u.action {
                        UndoAction::ResetBranch { .. } => ConfirmPrompt::plain(
                            "Undo — move branch back",
                            format!(
                                "{}? The discarded commits leave the graph but stay \
                                 in the reflog.{warn}",
                                u.label
                            ),
                            "Undo",
                            true,
                            true,
                        ),
                        UndoAction::RestoreBranch { .. } => ConfirmPrompt::plain(
                            "Restore branch",
                            format!(
                                "{}? This re-creates the branch exactly where it \
                                 last pointed — nothing else changes.",
                                u.label
                            ),
                            "Restore",
                            false,
                            true,
                        ),
                        UndoAction::RevertCommit { .. } => ConfirmPrompt::plain(
                            "Revert commit",
                            format!(
                                "{}? This adds a new commit that reverses it — \
                                 history is kept, so it's safe even when pushed.",
                                u.label
                            ),
                            "Revert",
                            false,
                            true,
                        ),
                    }
                }
                PendingOp::Rebase { current, base } => match current {
                    Some(branch) => ConfirmPrompt::plain(
                        "Rebase branch",
                        format!("Rebase ‘{branch}’ onto {base}? This replays ‘{branch}’’s commits on top of the latest {base} and rewrites its history."),
                        "Rebase",
                        false,
                        true,
                    ),
                    None => ConfirmPrompt::plain(
                        "Rebase branch",
                        "HEAD is detached, so there's no branch to rebase. Check out a branch first.".to_string(),
                        "Rebase",
                        false,
                        false,
                    ),
                },
                // "Fetch" (#232, M2.20f) — the mildest confirmation in this
                // match, and the only new arm here that is actually reached:
                // `menu.rs`'s `fetch_item` opens this modal directly, because
                // unlike merge/checkout/delete a fetch has no branch a live
                // pre-check could find it wrong about. Single tap, never
                // danger-styled, and the body says what it does *not* touch —
                // the whole reason a fetch is the safe half of the pair.
                PendingOp::Fetch { remote } => ConfirmPrompt::plain(
                    "Fetch from remote",
                    format!(
                        "Fetch from ‘{remote}’? This updates what this repository knows about \
                         the remote's branches. Nothing local moves — not the branch you're on, \
                         not the working tree, not a single commit."
                    ),
                    "Fetch",
                    false,
                    true,
                ),
                // "Pull" (#232, M2.20f, ADR 0044) — **not reached today.**
                // `menu.rs`'s `pull_item` opens `Dialog::PullStrategy`
                // instead, because `MergeStrategy` carries no "not yet
                // chosen" value this modal could show (see `pull_picker_view`
                // below, and `features::dialogs::core::PullTarget`).
                //
                // The arm exists because this match is exhaustive and a
                // `unreachable!()` in a view is a panic in the browser, not a
                // diagnostic. It is written to be *correct* rather than a
                // placeholder: a future opener that does route a fully-formed
                // pull here gets a true prompt naming the strategy that would
                // actually run, not a lie that says "merge" while a rebase is
                // dispatched — which is precisely the class of silent default
                // ADR 0044 exists to forbid.
                PendingOp::Pull {
                    remote,
                    branch,
                    strategy,
                } => ConfirmPrompt::plain(
                    "Pull branch",
                    match strategy {
                        MergeStrategy::Merge => format!(
                            "Pull ‘{branch}’ from ‘{remote}’ and merge? New commits on the \
                             remote are fetched and merged into ‘{branch}’, making a merge \
                             commit if the two have diverged."
                        ),
                        MergeStrategy::Rebase => format!(
                            "Pull ‘{branch}’ from ‘{remote}’ and rebase? New commits on the \
                             remote are fetched, then ‘{branch}’’s own commits are replayed on \
                             top of them — which rewrites their history."
                        ),
                    },
                    "Pull",
                    false,
                    true,
                ),
                // The two working-tree operations (M2.18b, #220). Both prompts —
                // wording, which ceremony, what is enabled — come from the pure
                // core, so the asymmetry between them is decided somewhere a host
                // test can read it rather than inside this wasm-only view.
                PendingOp::DiscardTrackedPaths { paths } => {
                    worktree_confirm(WorktreeAction::DiscardTracked, paths, armed)
                }
                PendingOp::DeleteUntrackedPaths { paths } => {
                    worktree_confirm(WorktreeAction::DeleteUntracked, paths, armed)
                }
            };
            // M12.05 (#555): a plan whose repository moved after its picture was
            // drawn withdraws its own confirmation. The composition is
            // `freshness::core::confirm_enabled`, host-tested, rather than an
            // `&&` written here where no test runner compiles it.
            //
            // This does not contradict `PreviewView::advisory_only`. That rule
            // is about the preview's *content* — a conflict, an unsupported
            // operation — never deciding whether an operation may proceed, and
            // it still holds exactly as written. This asks a different question
            // with a different answer: not "what does the picture show" but
            // "does the picture still describe the repository". A picture the
            // repository has moved past is not advice, it is a receipt.
            let plan_freshness = preview.plan().map(|plan| freshness.of(&plan));
            let enabled = confirm_enabled(enabled, plan_freshness.as_ref());
            let blocked_reason = blocked_by_staleness(plan_freshness.as_ref()).or(blocked_reason);
            // The confirm button is muted when disabled, red for a destructive
            // delete, green otherwise.
            let confirm_style = if !enabled {
                format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:var(--muted); \
                         background:#21262d; border:1px solid #30363d; opacity:0.6;")
            } else if danger {
                format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff; \
                         background:#da3633; border:1px solid #f85149;")
            } else {
                format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff; \
                         background:#238636; border:1px solid #2ea043;")
            };
            // #65: a reason conveyed only through `title=` never surfaces on a tap
            // and is never announced, so it goes into the button's `aria-label`
            // *and* onto the screen as its own line. `disabled_menu_item_copy` is
            // the same composition `menu.rs`'s disabled items use — reused rather
            // than restated, so there is one rule for it.
            let (confirm_aria, visible_reason) = match blocked_reason {
                Some(reason) => {
                    let (aria, visible) = disabled_menu_item_copy(confirm_label, reason);
                    (aria, Some(visible))
                }
                None => (confirm_label.to_string(), None),
            };
            // Step one of the two-tap ceremony, for the operation that has one.
            // A `<button>` with `aria-pressed`, not a checkbox: this modal takes
            // no form controls (see the module doc), and the state change has to
            // be announced, not merely visible.
            let arm_control = arm.map(|step| {
                let arm_style = if step.pressed {
                    format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                             margin-bottom:12px; color:#f0f6fc; background:#5a1e1e; \
                             border:1px solid #f85149;")
                } else {
                    format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                             margin-bottom:12px; color:var(--fg); background:#21262d; \
                             border:1px solid #30363d;")
                };
                view! {
                    <button
                        style=arm_style
                        aria-pressed=if step.pressed { "true" } else { "false" }
                        on:click=move |_| dialogs.arm_confirm()
                    >
                        {step.label}
                    </button>
                }
            });
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if dialogs.may_dismiss() {
                            dialogs.close(Dialog::Confirm);
                            shell.close_confirm();
                        }
                    }
                >
                    <div
                        // A confirmation that draws a before/after graph needs
                        // room for two pictures side by side; every other one
                        // is a paragraph and looks wrong stretched to fit a
                        // canvas it does not have. `has_picture` is the right
                        // question and not `matches!(slot, Ready(_))`: a
                        // conflict list is a *successful* preview with no
                        // picture in it, and wants the narrow modal.
                        style=move || {
                            let wide = matches!(
                                preview.slot(),
                                PreviewSlot::Ready(ref v) if v.has_picture()
                            );
                            let width = if wide {
                                "min-width:320px; max-width:min(96vw, 780px);"
                            } else {
                                "min-width:300px; max-width:90vw;"
                            };
                            format!(
                                "{width} padding:16px; background:#161b22; \
                                 border:1px solid #30363d; border-radius:10px; \
                                 color:var(--fg); \
                                 box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                            )
                        }
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:12px;">{title}</div>
                        // `pre-wrap`: the working-tree prompts list one path per line,
                        // and every other prompt is a single paragraph either way.
                        <div style="margin-bottom:14px; line-height:1.4; \
                                    white-space:pre-wrap; max-height:50vh; \
                                    overflow-y:auto;">{body}</div>
                        {explanation.map(|e| explanation_panel_view(&e))}
                        {freshness_notice_view(preview, freshness)}
                        {preview_panel_view(preview)}
                        {arm_control}
                        {visible_reason.map(|reason| view! {
                            <div style="margin-bottom:10px; color:var(--muted); \
                                        line-height:1.4;">{reason}</div>
                        })}
                        <div style="display:flex; gap:8px; justify-content:flex-end;">
                            <button
                                style=format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}\
                                               color:var(--fg); background:#21262d; \
                                               border:1px solid #30363d;")
                                on:click=move |_| shell.close_confirm()
                            >
                                "Cancel"
                            </button>
                            // Two ways to be inert, and which one applies turns
                            // on whether this button carries its own reason.
                            //
                            // A branch arm's reason lives in the body text
                            // (`blocked_reason: None`), so `prop:disabled`
                            // stays exactly as it was — no behaviour change to
                            // anything that predates #220.
                            //
                            // A working-tree arm's reason is folded into
                            // `aria-label`, and a genuinely disabled button
                            // leaves the tab order — which would make that
                            // reason unreachable by the exact user it was
                            // written for (#65's finding, again). Those stay
                            // focusable and are refused in the handler instead.
                            // That guard is also what makes the two-tap
                            // ceremony real rather than decorative: `disabled`
                            // is the browser's to honour, `enabled` is ours.
                            <button
                                style=confirm_style
                                prop:disabled=!enabled && blocked_reason.is_none()
                                aria-disabled=if enabled { "false" } else { "true" }
                                aria-label=confirm_aria
                                on:click=move |_| {
                                    if enabled {
                                        run_confirmed();
                                    }
                                }
                            >
                                {confirm_label}
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}

/// Explain Mode's panel (M6.39b, #545): the plan, in ordinary language,
/// under the confirmation it belongs to.
///
/// Every word here comes from `features::explain::core`, which is pure and
/// host-tested; this function only arranges what that module returns. Keeping
/// the split that strict is what makes #92's criterion 5 checkable at all —
/// `cargo test` never compiles this file.
fn explanation_panel_view(explanation: &git_vista_protocol::Explanation) -> impl IntoView {
    let sections = render(explanation);
    view! {
        <div style="margin-bottom:14px; border:1px solid #30363d; \
                    border-radius:8px; overflow:hidden;">
            <div style="padding:8px 12px; background:#0d1117; color:var(--muted); \
                        font-size:12px; letter-spacing:0.04em; \
                        text-transform:uppercase;">
                "What this plan says"
            </div>
            {sections.into_iter().map(section_view).collect_view()}
        </div>
    }
}

/// One collapsible section.
///
/// **Default expanded**, and the collapsed state persists per topic rather
/// than per plan — see `features::explain::core::storage_key` for why that
/// distinction is the whole feature rather than a detail. A teaching panel
/// that starts closed is one a learner never opens; an expert closes it once
/// and it stays closed for every operation afterwards.
fn section_view(section: RenderedSection) -> impl IntoView {
    let topic = section.topic;
    let (open, set_open) = create_signal(crate::prefs::load_explain_section_open(topic));
    let heading = section.heading;
    let when_empty = section.when_empty;
    let lines = section.lines;
    let empty = lines.is_empty();

    view! {
        <div style="border-top:1px solid #21262d;">
            // A `<button>` rather than `<details>`: this modal takes no form
            // controls (see the module doc), and the open/closed change has to
            // be announced, not merely visible — the same reasoning the arm
            // control above is built on.
            <button
                style=format!(
                    "{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                     border-radius:0; color:var(--fg); background:#161b22; \
                     border:none; display:flex; align-items:center; gap:8px;"
                )
                aria-expanded=move || if open.get() { "true" } else { "false" }
                on:click=move |_| {
                    let next = !open.get_untracked();
                    set_open.set(next);
                    crate::prefs::store_explain_section_open(topic, next);
                }
            >
                <span style="color:var(--muted); width:1em;">
                    {move || if open.get() { "▾" } else { "▸" }}
                </span>
                <span style="font-weight:600;">{heading}</span>
            </button>
            <div style=move || {
                if open.get() {
                    "padding:0 12px 10px 30px; line-height:1.5;".to_string()
                } else {
                    "display:none;".to_string()
                }
            }>
                {if empty {
                    // Stated, never hidden: "nothing has to be true first" is
                    // itself the answer, and a section that vanishes teaches
                    // nothing (ADR 0091, decision 5).
                    view! {
                        <div style="color:var(--muted);">{when_empty}</div>
                    }.into_view()
                } else {
                    lines.into_iter().map(line_view).collect_view()
                }}
            </div>
        </div>
    }
}

/// One line of a section.
///
/// `data-explain-ref` / `data-explain-commit` carry the object the line names
/// — #92's criterion 3. The attribute is deliberately the whole of it for now:
/// the panel *identifies* the ref the graph draws, and wiring the click
/// through to focus that node is left to the graph slice rather than
/// half-built here. A browser test can assert the attribute; nothing claims
/// the line is navigable yet.
fn line_view(line: crate::features::explain::core::Line) -> impl IntoView {
    let (ref_attr, commit_attr) = match &line.link {
        Some(LinkTarget::Ref(r)) => (Some(r.clone()), None),
        Some(LinkTarget::Commit(c)) => (None, Some(c.clone())),
        None => (None, None),
    };
    let parts = crate::features::explain::core::spans(&line.text);
    view! {
        <div
            style="margin:6px 0;"
            data-explain-ref=ref_attr
            data-explain-commit=commit_attr
        >
            {parts.into_iter().map(|p| match p {
                Span::Text(t) => view! { <span>{t}</span> }.into_view(),
                // git's own words, set apart so a branch called `main` cannot
                // be read as the English word.
                Span::Code(c) => view! {
                    <span style="font-family:ui-monospace,monospace; \
                                 background:#0d1117; padding:1px 4px; \
                                 border-radius:4px;">{c}</span>
                }.into_view(),
            }).collect_view()}
        </div>
    }
}

/// The pull merge/rebase strategy picker (#232, M2.20f, ADR 0044).
///
/// A separate modal rather than a tenth arm of [`confirm_modal_view`] above,
/// for a reason the type system states outright: every arm of that match
/// destructures a `PendingOp`, and there is no `PendingOp::Pull` this dialog
/// could be built from — `MergeStrategy` has exactly two variants, derives no
/// `Default`, and carries no sentinel "not yet chosen" value
/// (`git-vista-protocol/src/plan.rs`). The missing field is supplied by a tap
/// *inside* this dialog, and `OperationKind::Pull` is constructed for the
/// first time at the same instant it is dispatched.
///
/// Everything else is deliberately the same recipe as the two modals around
/// it — full-viewport backdrop, the iOS ghost-click guard, the `#161b22` card,
/// [`BUTTON_BASE`] + [`TOUCH_TARGET_STYLE`] on every control — because that
/// recipe is what this app has proven on the iPad it is used from, and a new
/// modal is not the place to re-litigate it.
///
/// # ADR 0044's acceptance criterion, and where it is enforced
///
/// "No pre-selected option" is not a styling choice here. Three separate
/// layers hold it:
///
/// * the wire type refuses an omitted `strategy` (a deserialize error, never
///   a fallback);
/// * `Dialogs::open` resets `confirm_strategy` to `None` on **every** open, so
///   a remembered last choice cannot become a delayed default;
/// * this view renders the two toggles identically until one is tapped, and
///   the Pull button is inert while
///   `features::dialogs::core::pull_confirm_enabled` says nothing is chosen.
///
/// That last gate is `aria-disabled` + a refusal in the handler, **not**
/// `prop:disabled` — the same distinction M2.18b drew for the two-tap delete
/// ceremony, and for the same #65 reason: a genuinely disabled button leaves
/// the tab order, which would make its explanation unreachable by exactly the
/// user it was written for. #232's device checklist asks for VoiceOver to
/// focus *every* control in this dialog and hear something meaningful; a
/// button the browser has removed from the tab order cannot answer that.
pub fn pull_picker_view(features: Features) -> impl IntoView {
    let Features {
        dialogs,
        operations,
        ..
    } = features;

    move || {
        // A tracked read that doubles as the picker's visibility: `Some`
        // exactly while it is up, cleared by `close_pull_picker`.
        dialogs.pull_target().map(|PullTarget { remote, branch }| {
            // Tracked too: both toggles' pressed state and the Pull button's
            // enabled state re-render the moment a strategy is chosen.
            let chosen = dialogs.pull_strategy();
            let enabled = dialogs.pull_enabled();

            // The two toggles are styled *identically* until one is tapped —
            // ADR 0044's "neither pre-selected, highlighted or bolded", which
            // the device checklist inspects by eye. The selected state is a
            // blue tint, not the green of a confirm or the red of a danger:
            // choosing a strategy is neither, it only unlocks the choice.
            let toggle_style = |is: bool| {
                if is {
                    format!(
                        "{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                         margin-bottom:8px; color:#f0f6fc; background:#1c2f4a; \
                         border:1px solid #388bfd;"
                    )
                } else {
                    format!(
                        "{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                         margin-bottom:8px; color:var(--fg); background:#21262d; \
                         border:1px solid #30363d;"
                    )
                }
            };
            let merge_chosen = chosen == Some(MergeStrategy::Merge);
            let rebase_chosen = chosen == Some(MergeStrategy::Rebase);

            let confirm_style = if enabled {
                format!(
                    "{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff; background:#238636; \
                     border:1px solid #2ea043;"
                )
            } else {
                format!(
                    "{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:var(--muted); background:#21262d; \
                     border:1px solid #30363d; opacity:0.6;"
                )
            };
            // #65 again: the reason goes on the screen *and* into the label,
            // never only into a `title=` no tap ever surfaces.
            let (confirm_aria, blocked_line) = if enabled {
                ("Pull".to_string(), None)
            } else {
                let reason = "Choose Merge or Rebase first — git-vista never picks one for you.";
                (
                    disabled_menu_item_copy("Pull", reason).0,
                    Some(reason.to_string()),
                )
            };

            // Captured for the dispatch below: the picker closes before the
            // operation is raised, so these cannot be read back off the
            // signal at that point.
            let dispatch_remote = remote.clone();
            let dispatch_branch = branch.clone();

            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if dialogs.may_dismiss() {
                            dialogs.close_pull_picker();
                        }
                    }
                >
                    <div
                        style="min-width:300px; max-width:90vw; padding:16px; \
                               background:#161b22; border:1px solid #30363d; \
                               border-radius:10px; color:var(--fg); \
                               box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:12px;">"Pull branch"</div>
                        <div style="margin-bottom:14px; line-height:1.4;">
                            {format!(
                                "Fetch ‘{branch}’ from ‘{remote}’ and integrate it. \
                                 How should the two histories be joined?"
                            )}
                        </div>
                        // Two buttons, not a `<select>` or a pair of radios: this
                        // modal takes no form controls at all (see the module doc
                        // — a void `<input>` panics Leptos' CSR node-walk on iOS
                        // WebKit), and `aria-pressed` is what announces the choice.
                        <button
                            style=toggle_style(merge_chosen)
                            aria-pressed=if merge_chosen { "true" } else { "false" }
                            on:click=move |_| dialogs.set_pull_strategy(MergeStrategy::Merge)
                        >
                            "Merge — keep both histories, adding a merge commit if they diverged"
                        </button>
                        <button
                            style=toggle_style(rebase_chosen)
                            aria-pressed=if rebase_chosen { "true" } else { "false" }
                            on:click=move |_| dialogs.set_pull_strategy(MergeStrategy::Rebase)
                        >
                            "Rebase — replay your commits on top, rewriting their history"
                        </button>
                        {blocked_line.map(|reason| view! {
                            <div style="margin:10px 0; color:var(--muted); \
                                        line-height:1.4;">{reason}</div>
                        })}
                        <div style="display:flex; gap:8px; justify-content:flex-end; \
                                    margin-top:6px;">
                            <button
                                style=format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}\
                                               color:var(--fg); background:#21262d; \
                                               border:1px solid #30363d;")
                                on:click=move |_| dialogs.close_pull_picker()
                            >
                                "Cancel"
                            </button>
                            // Focusable even when inert, and refused here rather
                            // than by the browser — see this function's doc
                            // comment for why that is the a11y-correct shape and
                            // not a missing `prop:disabled`.
                            <button
                                style=confirm_style
                                aria-disabled=if enabled { "false" } else { "true" }
                                aria-label=confirm_aria
                                on:click=move |_| {
                                    // Untracked by construction: `pull_strategy`
                                    // is read again here rather than trusting the
                                    // `chosen` captured at render time, so the tap
                                    // dispatches what is on screen *now*.
                                    let Some(strategy) = dialogs.pull_strategy() else {
                                        return;
                                    };
                                    // The one place in the client an
                                    // `OperationKind::Pull` is ever built — and it
                                    // is dispatched in the same breath, so a
                                    // half-decided pull has no moment in which to
                                    // exist.
                                    let op = OperationKind::Pull {
                                        remote: dispatch_remote.clone(),
                                        branch: dispatch_branch.clone(),
                                        strategy,
                                    };
                                    dialogs.close_pull_picker();
                                    operations.dispatch(op);
                                }
                            >
                                "Pull"
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}

/// The write-failure notice modal (#316) — the error path finally meeting the
/// bar this file's confirmation path set. Same backdrop, card, ghost-click
/// guard and 44x44 buttons as `confirm_modal_view`; one OK button instead of
/// Cancel/Confirm, because an error is never "confirmed", only read.
///
/// What lands in `shell.error_notice()` is the server's `error.message`,
/// already unwrapped from the wire envelope by
/// `features::dialogs::core::split_error_response` — the request id went to
/// the console at the call site, never here.
pub fn error_modal_view(features: Features) -> impl IntoView {
    let Features { dialogs, shell, .. } = features;

    move || {
        shell.error_notice().map(|notice| {
            let title = notice.title;
            let body = notice.body;
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if dialogs.may_dismiss() {
                            dialogs.close(Dialog::Error);
                            shell.close_error();
                        }
                    }
                >
                    <div
                        style="min-width:300px; max-width:90vw; padding:16px; \
                               background:#161b22; border:1px solid #30363d; \
                               border-radius:10px; color:var(--fg); \
                               box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:12px;">{title}</div>
                        // `pre-wrap`: git's stderr is often multi-line.
                        <div style="margin-bottom:14px; line-height:1.4; \
                                    white-space:pre-wrap; max-height:50vh; \
                                    overflow-y:auto;">{body}</div>
                        <div style="display:flex; justify-content:flex-end;">
                            <button
                                style=format!(
                                    "{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff; \
                                     background:#21262d; border:1px solid #30363d;"
                                )
                                aria-label="Dismiss this error"
                                on:click=move |_| {
                                    dialogs.close(Dialog::Error);
                                    shell.close_error();
                                }
                            >
                                "OK"
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}
