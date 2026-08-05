//! The commit-message modal (Issue #33; three modes since M2.19c, #224).
//!
//! Every decision this modal makes lives in `features::dialogs::commit` — which
//! of the three modes is active, what each one says, which files it will
//! contain, the amend request body, how the typed answer is read back, and what
//! happens when the compare-and-swap says HEAD moved. This file is the part
//! that needs a DOM: it renders those answers and calls what they decide. The
//! split is not stylistic — this module is wasm-only and never compiles under
//! `cargo test --workspace`, so anything decided here would be decided untested.

use leptos::*;

use crate::api::{amend_commit_request, create_commit_request, fetch_commit_detail, fetch_frame};
use crate::features::dialogs::commit::{
    amend_preflight, dialog_copy, head_tip, phase_view, published_advisory, scope_review,
    submit_path, AmendOutcome, AmendPhase, AmendTarget, CommitIntent, DialogCopy, PlainCommit,
    Preflight, Recheck, ScopeLine, ScopeReview, SubmitPath,
};
use crate::features::dialogs::core::{Dialog, ErrorNotice, PATH_LIST_LIMIT, TOUCH_TARGET_STYLE};
use crate::features::status::signals as status_state;
use crate::state::Features;

/// The dialog's button base style, with #65's 44x44 floor — the same
/// declaration `dialogs/confirm.rs` uses, and for the same reason: this modal
/// is inline-styled (see `dialogs/mod.rs`), so `features::a11y::audit`'s
/// stylesheet census cannot see these controls. The floor used to be missing
/// here entirely: `padding:6px 14px` on a 13px font lands around 30px tall,
/// under the minimum every other control was brought to in #65.
const BUTTON_BASE: &str = "padding:8px 16px; font:inherit; border-radius:6px; ";

/// Whether the confirm button may be pressed right now — a **tracked** read of
/// the message buffer, so the button re-renders as soon as the box stops being
/// empty.
///
/// `blocked` is `DialogCopy::blocked_reason.is_some()` (a mode the working-tree
/// status says cannot succeed) and `phase_blocks` is the amend phase's own
/// verdict (`phase_view`), already folded to "not in amend mode ⇒ false" by the
/// caller. Both decisions are made in the pure core; this only combines them
/// with the one condition that lives in a signal.
fn confirm_inert(
    dialogs: crate::features::dialogs::signals::Dialogs,
    intent: &CommitIntent,
    blocked: bool,
    phase_blocks: bool,
) -> bool {
    dialogs.message(intent).trim().is_empty() || blocked || phase_blocks
}

/// One `window.alert`, or nothing when there is no window to alert in.
fn alert(text: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.alert_with_message(text);
    }
}

/// The commit-message modal (Issue #33). Shown while `commit_dialog` is `Some`;
/// a real overlay with a focused text box, so it prompts reliably where a native
/// `window.prompt()` gets blocked/flashed by the webview. Confirming POSTs the
/// commit and refreshes the graph; cancelling just closes it.
pub fn commit_dialog_view(features: Features) -> impl IntoView {
    let Features {
        graph,
        dialogs,
        shell,
        status,
        ..
    } = features;

    // The plain-commit / empty-commit path, unchanged since #226 except that it
    // reads its message through the intent-aware buffer.
    //
    // It takes a `PlainCommit`, not a `CommitIntent`, and that is deliberate:
    // `PlainCommit` has a private field and only `submit_path` builds one, so
    // there is no way to hand an amend to this closure. The dispatch below used
    // to be a match on `CommitIntent` in this wasm-only file, where sending
    // `Amend` here would have compiled, passed every test, and quietly written a
    // second commit instead of rewriting the tip.
    let submit_commit = move |plain: PlainCommit| {
        let intent = plain.into_intent();
        let message = dialogs.message_untracked(&intent).trim().to_string();
        if message.is_empty() {
            return; // Keep the dialog open; the confirm button is inert anyway.
        }
        // Captured NOW, not read in the callback: the served repository can
        // change while the POST is in flight, and the clear must target the
        // repository this message was submitted against (#226).
        let submitted_scope = dialogs.draft_scope_snapshot();
        let allow_empty = intent.allow_empty();
        let branch = intent.branch().map(str::to_string);
        shell.close_commit_dialog();
        spawn_local(async move {
            match create_commit_request(&message, allow_empty, branch.as_deref()).await {
                Ok(()) => {
                    // The message is consumed — discard the draft, signal and
                    // persisted copy both (#226). This is the clear the opener
                    // used to do; moved here so a suspension-recovered draft
                    // survives reopening but a submitted one never resurrects.
                    dialogs.clear_message_for(&intent, submitted_scope.as_deref());
                    graph.update(|g| {
                        g.force_bump();
                    });
                }
                // #316: the envelope's message in the app's own modal —
                // never the raw JSON body in a native alert().
                Err(e) => {
                    dialogs.open(Dialog::Error);
                    shell.open_error(ErrorNotice {
                        title: "Couldn't create commit",
                        body: e,
                    });
                }
            }
        });
    };

    // The amend path. Two things make it structurally different from the one
    // above, and both follow from what an amend is:
    //
    // 1. **The dialog stays open until the request settles.** A plain commit can
    //    close optimistically — a failure leaves the repository as it was and
    //    the draft is still in storage. An amend can fail in ways the user is
    //    expected to act on (a hook, a signing key, a moved HEAD) carrying a
    //    message that exists only in this modal's in-memory buffer. Closing
    //    first would throw it away exactly when it is needed.
    // 2. **A stale tip is not an error.** It routes to the guided re-check
    //    (`AmendPhase::Stale`), which keeps the confirm button inert until a
    //    fresh tip has been read and shown. That is what the compare-and-swap is
    //    for: retrying blind would rewrite a commit nobody reviewed.
    let submit_amend = move |target: AmendTarget| {
        let intent = target.intent();
        let message = dialogs.message_untracked(&intent).trim().to_string();
        if message.is_empty() {
            return;
        }
        // The pre-flight published-history ceremony (#225, ADR 0040). The gate
        // is here — before the POST, not after it — and it is decided in the
        // host-tested core: this file never compiles under `cargo test`, so a
        // gate spelled out here would be a gate nothing checks. `Confirm` sends
        // nothing at all; the banner it raises carries the only way on.
        let target = match amend_preflight(target, &dialogs.amend_knowledge()) {
            Preflight::Send(target) => target,
            Preflight::Confirm(target) => {
                dialogs.set_amend_phase(AmendPhase::AwaitingPublishedConfirm { target });
                return;
            }
        };
        let expected_tip = target.expected_tip().to_string();
        let submitted_scope = dialogs.draft_scope_snapshot();
        dialogs.set_amend_phase(AmendPhase::InFlight);
        spawn_local(async move {
            match amend_commit_request(&message, &expected_tip).await {
                AmendOutcome::Amended(success) => {
                    dialogs.clear_message_for(&intent, submitted_scope.as_deref());
                    shell.close_commit_dialog();
                    graph.update(|g| {
                        g.force_bump();
                    });
                    // The published-history advisory (#223, ADR 0040): the
                    // server never blocks an amend of pushed history, it reports
                    // it afterwards. Three-state — see `published_advisory`.
                    if let Some(note) = published_advisory(&success) {
                        alert(&note);
                    }
                }
                AmendOutcome::TipMoved { message } => {
                    dialogs.set_amend_phase(AmendPhase::Stale {
                        reviewed_tip: expected_tip,
                        message,
                        recheck: Recheck::Idle,
                    });
                }
                AmendOutcome::Refused { refusal, message } => {
                    dialogs.set_amend_phase(AmendPhase::Refused { refusal, message });
                }
                AmendOutcome::Unavailable(why) => {
                    dialogs.set_amend_phase(AmendPhase::Unavailable(why));
                }
            }
        });
    };

    // The ceremony's second step (#225): the user has read the warning and
    // agreed to rewrite a commit that is already on a remote.
    //
    // Recording the agreement and re-submitting are one closure so they cannot
    // drift apart, and the agreement is recorded against the target's own tip —
    // never "the current tip" — so a dialog retargeted between the warning and
    // the press cannot inherit consent given for a different commit.
    //
    // **The order of the two lines is load-bearing.** `submit_amend` re-reads
    // `dialogs.amend_knowledge()` synchronously to decide the pre-flight, so
    // recording second — or not at all — makes `amend_preflight` answer
    // `Confirm` again and re-enter `AwaitingPublishedConfirm`. That fails safe
    // in the sense that nothing is sent, but it is not benign: the banner's own
    // button becomes permanently inert and no amend of published history is
    // reachable through the UI at all. Nothing here compiles under
    // `cargo test`, so the order is pinned by a source census —
    // `features::a11y::audit::the_way_past_the_banner_records_the_agreement_before_it_resubmits`.
    let confirm_published = move |target: AmendTarget| {
        dialogs.confirm_amend_target(target.expected_tip());
        submit_amend(target);
    };

    // The guided re-check: read what the tip is *now*, show it, and point the
    // open dialog at it. Deliberately three visible steps rather than an
    // automatic retry — the user approved rewriting one specific commit, and a
    // different commit is a different decision.
    let recheck = move |reviewed_tip: String, server_message: String| {
        let stale = move |recheck| AmendPhase::Stale {
            reviewed_tip: reviewed_tip.clone(),
            message: server_message.clone(),
            recheck,
        };
        dialogs.set_amend_phase(stale(Recheck::Checking));
        spawn_local(async move {
            // The frame carries the ref list, and HEAD is always in it when it
            // resolves — so this answers "what would an amend rewrite right
            // now", which no branch ref can.
            let frame = match fetch_frame().await {
                Ok(frame) => frame,
                Err(e) => {
                    dialogs.set_amend_phase(stale(Recheck::Unavailable(e.to_string())));
                    return;
                }
            };
            let Some(new_tip) = head_tip(&frame.refs) else {
                dialogs.set_amend_phase(stale(Recheck::Unavailable(
                    "HEAD doesn't resolve to a commit here.".to_string(),
                )));
                return;
            };
            // Name the commit, don't just print its hash: "amend 3f2a91c
            // instead?" is not a question anyone can answer.
            let detail = match fetch_commit_detail(&new_tip).await {
                Ok(detail) => detail,
                Err(e) => {
                    dialogs.set_amend_phase(stale(Recheck::Unavailable(e)));
                    return;
                }
            };
            let summary = detail
                .message
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            // Retarget the open dialog at the fresh tip. This re-runs the view
            // closure below, which is exactly why the phase and the message live
            // in `Dialogs` rather than inside it.
            shell.open_commit_dialog(CommitIntent::Amend {
                expected_tip: new_tip.clone(),
            });
            // Offer the new tip's message — adopted only if the user has not
            // typed since the last seed, so a re-check never eats a message
            // written for the previous tip.
            //
            // This runs BEFORE the banner is set, and the order is the fix for a
            // real contradiction: the banner speaks about what the box holds,
            // and seeding can replace what the box holds. Announcing first meant
            // the banner said "your message below is unchanged" and then this
            // line changed it — for the commonest amend of all, the one where
            // the user only folds in staged files and never touches the
            // pre-filled text. `seed_amend_msg` reports what it actually did and
            // the phase carries that, so the banner states a fact rather than a
            // prediction.
            //
            // The pre-flight's input for the commit the dialog now points at
            // (#225). The same read that supplies the pre-fill supplies this,
            // so retargeting never leaves the gate answering for the old tip —
            // and because `PreflightKnowledge` is tip-scoped, the moment the
            // dialog retargets, a confirmation given for the previous commit
            // stops counting whether this read lands or not.
            //
            // The raw `record_amend_detail`, not the guarded
            // `Dialogs::apply_amend_detail` the menu's opener must use, and the
            // difference is the `open_commit_dialog` two statements up: this
            // path retargets the dialog and records the answer with **no
            // `await` between them**, so the target and the knowledge cannot
            // disagree. That is the proof of currency the menu's opener has no
            // way to make — its callback resumes after an `await`, by which
            // point the dialog may be pointed somewhere else entirely. Insert
            // an `await` anywhere between here and the retarget above and this
            // reasoning is void: route through `apply_amend_detail` instead,
            // and see `features::dialogs::commit::detail_read_use`.
            dialogs.record_amend_detail(&new_tip, detail.on_remote);
            let seeded = dialogs.seed_amend_msg(&detail.message);
            dialogs.set_amend_phase(stale(Recheck::Retargeted {
                new_tip,
                summary,
                message: seeded,
            }));
        });
    };

    move || {
        shell.commit_dialog().map(|intent| {
            // Tracked reads: the copy, the file list, the message and the banner
            // all re-render when the status lands or the amend phase moves.
            let repo_status = status_state::read(status);
            let DialogCopy {
                title,
                body,
                confirm_label,
                blocked_reason,
            } = dialog_copy(&intent, repo_status.as_ref());
            let review = scope_review(&intent, repo_status.as_ref(), PATH_LIST_LIMIT);
            let phase = dialogs.amend_phase();
            let phase_state = phase_view(&phase);
            let is_amend = intent.expected_tip().is_some();
            // A non-amend intent has no phase of its own: `phase_view` is only
            // consulted in amend mode, so a leftover banner can never gate the
            // plain commit button.
            let phase_blocks = is_amend && !phase_state.confirm_enabled;
            let busy = is_amend && phase_state.busy;
            let notice = if is_amend { phase_state.notice } else { None };

            // The banner's own button, when the phase offers one: the re-check
            // step after a stale tip, and the published-history ceremony's
            // second step (#225). Both are deliberately *not* the green confirm
            // button — each carries its own words for its own act.
            let banner_button_style = format!(
                "{BUTTON_BASE}{TOUCH_TARGET_STYLE}margin-top:10px; \
                 color:var(--fg); background:#21262d; border:1px solid #30363d;"
            );
            let notice_action = match (&phase, &notice) {
                (
                    AmendPhase::Stale {
                        reviewed_tip,
                        message,
                        ..
                    },
                    Some(n),
                ) => n.action.map(|label| {
                    let reviewed_tip = reviewed_tip.clone();
                    let message = message.clone();
                    view! {
                        <button
                            style=banner_button_style.clone()
                            on:click=move |_| recheck(reviewed_tip.clone(), message.clone())
                        >
                            {label}
                        </button>
                    }
                }),
                (AmendPhase::AwaitingPublishedConfirm { target }, Some(n)) => {
                    n.action.map(|label| {
                        let target = target.clone();
                        view! {
                            <button
                                style=banner_button_style.clone()
                                on:click=move |_| confirm_published(target.clone())
                            >
                                {label}
                            </button>
                        }
                    })
                }
                _ => None,
            };

            // Why the confirm button may be inert: an empty message, a mode the
            // status says cannot succeed, or an amend phase that demands a
            // re-check first. A free function rather than one shared closure —
            // the three consumers (the style, the ARIA state, the handler) each
            // need their own capture, and only the handler wants the untracked
            // read.
            let blocked_flag = blocked_reason.is_some();
            let style_intent = intent.clone();
            let aria_intent = intent.clone();
            let confirm_intent = intent.clone();
            let message_intent = intent.clone();
            let input_intent = intent.clone();

            // The message field is a <textarea>, NOT an <input>: the void <input>
            // element breaks Leptos' CSR <template> node-walk on iOS WebKit (which
            // parses void elements differently than Blink/Gecko), panicking the whole
            // view so the modal never mounts on iPad. A textarea is non-void — and is
            // fine for a commit message. Styles are inline and viewport-sized
            // (100vw/100vh) since that's what proved to render reliably on iOS.
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        //
                        // An amend in flight also pins the *backdrop*, which is the
                        // accidental-dismissal path: a stray tap there would hide the
                        // answer to a request that is still running, and on failure it
                        // would take the typed message with it. Cancel stays live on
                        // purpose — the deliberate exit must never be blocked for up to
                        // two request timeouts, and leaving mid-flight is safe: the
                        // completion callback refreshes the graph and shows the
                        // published-history advisory whether this modal is open or not.
                        if dialogs.may_dismiss() && !busy {
                            dialogs.close(Dialog::Commit);
                            shell.close_commit_dialog();
                        }
                    }
                >
                    <div
                        style="min-width:300px; max-width:90vw; max-height:90vh; \
                               overflow-y:auto; padding:16px; \
                               background:#161b22; border:1px solid #30363d; \
                               border-radius:10px; color:var(--fg); \
                               box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:8px;">{title}</div>
                        <div style="margin-bottom:12px; color:var(--muted); line-height:1.4;">
                            {body}
                        </div>
                        {notice.map(|n| view! {
                            // `role="status"`: the banner appears in response to a
                            // request the user made and has to be announced, not just
                            // drawn — a screen-reader user who cannot see the amber
                            // box would otherwise hear nothing at all happen.
                            <div
                                role="status"
                                style="margin-bottom:12px; padding:10px; \
                                       border:1px solid #9e6a03; border-radius:6px; \
                                       background:#1c1a12; line-height:1.4;"
                            >
                                <div style="font-weight:600;">{n.title}</div>
                                <div style="color:var(--muted);">{n.body}</div>
                                {notice_action}
                            </div>
                        })}
                        {staged_scope_view(review)}
                        <textarea
                            style="width:100%; box-sizing:border-box; padding:10px; \
                                   font:inherit; color:var(--fg); background:#0d1117; \
                                   border:1px solid #30363d; border-radius:6px; \
                                   resize:none;"
                            rows="3"
                            placeholder="Commit message"
                            aria-label="Commit message"
                            prop:value=move || dialogs.message(&message_intent)
                            on:input=move |ev| {
                                dialogs.set_message(&input_intent, event_target_value(&ev))
                            }
                        ></textarea>
                        {blocked_reason.map(|reason| view! {
                            // #65: a reason carried only by `title=` never surfaces on
                            // a tap and is never announced. It goes on screen as its
                            // own line, exactly as `confirm.rs` does it.
                            <div style="margin-top:10px; color:var(--muted); \
                                        line-height:1.4;">{reason}</div>
                        })}
                        <div style="display:flex; gap:8px; justify-content:flex-end; \
                                    margin-top:14px;">
                            <button
                                style=format!(
                                    "{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:var(--fg); \
                                     background:#21262d; border:1px solid #30363d;"
                                )
                                on:click=move |_| shell.close_commit_dialog()
                            >
                                "Cancel"
                            </button>
                            // `aria-disabled` and an inert handler, never
                            // `prop:disabled` — the rule `menu.rs` and `confirm.rs`
                            // both write out: a natively disabled button leaves the
                            // tab order and takes its own reason with it, and the
                            // reason (an empty message, nothing staged, a moved HEAD)
                            // is the whole point of the button being off.
                            <button
                                style=move || {
                                    if confirm_inert(dialogs, &style_intent, blocked_flag,
                                                     phase_blocks) {
                                        format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}\
                                                 color:var(--muted); background:#21262d; \
                                                 border:1px solid #30363d; opacity:0.6;")
                                    } else {
                                        format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}\
                                                 color:#fff; background:#238636; \
                                                 border:1px solid #2ea043;")
                                    }
                                }
                                aria-disabled=move || {
                                    confirm_inert(dialogs, &aria_intent, blocked_flag, phase_blocks)
                                        .to_string()
                                }
                                on:click=move |_| {
                                    // The untracked read: an event handler that
                                    // subscribed to the message signal would
                                    // re-run for every keystroke.
                                    if dialogs.message_untracked(&confirm_intent).trim().is_empty()
                                        || blocked_flag
                                        || phase_blocks
                                    {
                                        return;
                                    }
                                    // Which endpoint this press reaches is
                                    // decided in the host-tested core, and the
                                    // two arms carry types the other closure
                                    // cannot accept — swapping them here is a
                                    // compile error, not a silent rewrite of
                                    // history.
                                    match submit_path(&confirm_intent) {
                                        SubmitPath::Amend(target) => submit_amend(target),
                                        SubmitPath::Commit(plain) => submit_commit(plain),
                                    }
                                }
                            >
                                {if busy { "Amending…" } else { confirm_label }}
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}

/// The staged-scope review: exactly what the commit will contain, and what it
/// leaves out. Rendered from [`scope_review`]'s answer — this function decides
/// nothing, which is why the honesty rules (an unread status says so; a cut
/// list still states its full count) are testable at all.
fn staged_scope_view(review: ScopeReview) -> impl IntoView {
    let ScopeReview {
        heading,
        lines,
        hidden,
        notes,
    } = review;
    view! {
        <div
            style="margin-bottom:12px; padding:10px; border:1px solid #30363d; \
                   border-radius:6px; background:#0d1117;"
        >
            <div style="font-weight:600; margin-bottom:6px;">{heading}</div>
            <ul style="margin:0; padding-left:18px; line-height:1.5;">
                {lines
                    .into_iter()
                    .map(|ScopeLine { path, kind, also_modified }| {
                        view! {
                            <li>
                                <span style="color:var(--muted);">{kind}</span>
                                " "
                                {path}
                                {also_modified.then(|| view! {
                                    <span style="color:var(--muted);">
                                        " — the staged version; the file has changed again since"
                                    </span>
                                })}
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
            {(hidden > 0).then(|| view! {
                <div style="color:var(--muted); margin-top:4px;">
                    {format!("…and {hidden} more")}
                </div>
            })}
            {notes
                .into_iter()
                .map(|note| view! {
                    <div style="color:var(--muted); margin-top:6px; line-height:1.4;">{note}</div>
                })
                .collect_view()}
        </div>
    }
}
