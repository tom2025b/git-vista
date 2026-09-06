//! Render the core's words and connect its offers to existing guarded flows.
use super::core::{can_write, status_detail, Action};
use crate::features::dialogs::core::{Dialog, ErrorNotice};
use crate::features::graph::core::Frame;
use crate::features::status::{core::chip_label, signals::read};
use crate::state::{CommitIntent, Features, PendingOp};
use leptos::*;

pub fn status_chip_view(
    features: Features,
    frame: Signal<Option<Frame>>,
    online: RwSignal<bool>,
    nerd_icons: RwSignal<bool>,
) -> impl IntoView {
    let Features {
        status,
        shell,
        dialogs,
        graph,
        operations,
        ..
    } = features;
    let open = create_rw_signal(false);
    let busy = create_rw_signal(false);
    let trigger = create_node_ref::<html::Button>();
    let close_button = create_node_ref::<html::Button>();
    let writable = move || {
        can_write(
            frame.get().map(|f| f.read_only),
            crate::features::session::signals::is_lan(),
            online.get(),
            busy.get() || operations.in_flight_count() > 0,
        )
    };
    let detail = move || status_detail(read(status).as_ref(), writable());
    // Selection and mode changes retire the panel before another reading lands.
    create_effect(move |_| {
        graph.get().epoch();
        open.set(false);
    });
    create_effect(move |_| {
        if open.get() {
            if let Some(button) = close_button.get() {
                let _ = button.focus();
            }
        }
    });
    let close = move || {
        open.set(false);
        if let Some(button) = trigger.get() {
            let _ = button.focus();
        }
    };
    let act = move |action: Action| {
        // Re-admit at click time: an offer rendered before a reload/mode change
        // cannot open a confirmation against the newly selected repository.
        if !detail().actions.contains(&action) {
            return;
        }
        match action {
            Action::Stage => {
                busy.set(true);
                let epoch = graph.get_untracked().epoch();
                spawn_local(async move {
                    let answer = crate::api::stage_request().await;
                    busy.set(false);
                    if graph.get_untracked().epoch() != epoch {
                        return;
                    }
                    status.refetch();
                    if let Err(body) = answer {
                        open.set(false);
                        dialogs.open(Dialog::Error);
                        shell.open_error(ErrorNotice {
                            title: "Couldn't stage changes",
                            body,
                        });
                    }
                });
            }
            Action::Commit => {
                close();
                dialogs.open(Dialog::Commit);
                shell.open_commit_dialog(CommitIntent::Staged);
                status.refetch();
            }
            Action::Resolve => {
                close();
                if !shell.activity_is_open() {
                    shell.toggle_activity();
                }
            }
            Action::Pull { remote, branch } => {
                close();
                dialogs.open_pull_picker(remote, branch);
            }
            Action::Push { branch } => {
                close();
                dialogs.open(Dialog::Confirm);
                shell.open_confirm(PendingOp::Push {
                    branch,
                    set_upstream: false,
                    force: None,
                });
            }
        }
    };
    let chip = move || {
        let Some(s) = read(status) else {
            return (
                "status-chip".to_string(),
                "Status unknown".to_string(),
                "No current status reading".to_string(),
            );
        };
        let ic = crate::icons::icon_set(nerd_icons.get());
        let label = chip_label(
            s.staged.len(),
            s.unstaged.len(),
            s.untracked.len(),
            s.conflicted.len(),
        );
        let (class, icon) = if !s.conflicted.is_empty() {
            ("conflict", ic.conflict)
        } else if !s.is_clean() {
            ("dirty", ic.dirty)
        } else {
            ("clean", ic.clean)
        };
        let age = (s.scanned_at > 0).then(|| (js_sys::Date::now() / 1000.0) as i64 - s.scanned_at);
        let freshness = crate::datetime::freshness_label(age);
        let sync = format!(
            "{}{}",
            if s.ahead > 0 {
                format!(" ↑{}", s.ahead)
            } else {
                String::new()
            },
            if s.behind > 0 {
                format!(" ↓{}", s.behind)
            } else {
                String::new()
            }
        );
        let title = format!(
            "{} staged · {} unstaged · {} untracked · {} conflicted · {}",
            s.staged.len(),
            s.unstaged.len(),
            s.untracked.len(),
            s.conflicted.len(),
            freshness
        );
        (
            format!(
                "status-chip {class}{}",
                if crate::datetime::is_stale(age) {
                    " stale"
                } else {
                    ""
                }
            ),
            format!("{icon} {label}{sync} · {freshness}"),
            title,
        )
    };
    view! {
        <button node_ref=trigger type="button" class=move || chip().0 title=move || chip().2
            aria-label="Repository status" aria-haspopup="dialog" aria-expanded=move || open.get().to_string()
            on:click=move |_| { if open.get() { close(); } else { status.refetch(); open.set(true); } }>
            <span aria-live="polite" aria-atomic="true">{move || chip().1}</span>
        </button>
        <Show when=move || open.get()>
            <div class="status-detail-backdrop">
                <section class="status-detail" role="dialog" aria-modal="true" aria-label="Repository status details"
                    on:keydown=move |e| { if e.key() == "Escape" { e.prevent_default(); e.stop_propagation(); close(); } }>
                    <div class="status-detail-heading"><h2>"Repository status"</h2>
                        <button node_ref=close_button type="button" class="refresh" on:click=move |_| close()>"Close"</button>
                    </div>
                    <div aria-live="polite" aria-atomic="true">
                        {move || detail().sentences.into_iter().map(|sentence| view! { <p>{sentence}</p> }).collect_view()}
                        <Show when=move || busy.get()><p>"Staging changes…"</p></Show>
                    </div>
                    {move || detail().actions.into_iter().map(|action| {
                        let explanation = action.explanation(); let label = action.label();
                        view! { <div class="status-detail-action"><p>{explanation}</p>
                            <button type="button" class="refresh" data-status-action="true" on:click=move |_| act(action.clone())>{label}</button>
                        </div> }
                    }).collect_view()}
                </section>
            </div>
        </Show>
    }
}
