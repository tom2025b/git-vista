//! DOM wiring only. Provider requests never participate in the graph resource.
use super::core::{
    accepts_response, availability_line, capability_line, check_state_label, detail_line, page_for,
    review_state_label, PullDetails,
};
use git_vista_protocol::forge::{ForgePage, PullRequestSummary};
use leptos::*;

fn pull_row(
    open: RwSignal<bool>,
    repo: Signal<Option<String>>,
    pr: PullRequestSummary,
    details_available: bool,
) -> impl IntoView {
    let loading = create_rw_signal(false);
    let result = create_rw_signal(None::<Result<PullDetails, String>>);
    let epoch = create_rw_signal(0u64);
    let number = pr.number;
    let load = move |_| {
        let Some(requested_repo) = repo.get_untracked() else {
            return;
        };
        let request_epoch = epoch.get_untracked().wrapping_add(1);
        epoch.set(request_epoch);
        result.set(None);
        loading.set(true);
        spawn_local(async move {
            let answer = crate::api::fetch_pull_details(&requested_repo, number).await;
            if open.get_untracked()
                && epoch.get_untracked() == request_epoch
                && repo.get_untracked().as_deref() == Some(requested_repo.as_str())
            {
                result.set(Some(answer));
                loading.set(false);
            }
        });
    };
    view! {
        <li style="margin:10px 0;">
            <a href=pr.web_url target="_blank" rel="noopener noreferrer">{format!("#{} {}", pr.number, pr.title)}</a>
            {if pr.draft { " · Draft" } else { "" }}
            <div>
                <button class="refresh" prop:disabled=move || loading.get() || !details_available
                    on:click=load>{move || if loading.get() { "Loading details…" } else { "Checks and reviews" }}</button>
            </div>
            {move || result.get().map(|answer| match answer {
                Err(error) => view! { <p role="alert">{error}</p> }.into_view(),
                Ok(details) => {
                    let message = detail_line(&details);
                    let ready = details.availability == git_vista_protocol::forge::ForgeAvailability::Ready;
                    let checks = details.checks;
                    let reviews = details.reviews;
                    let detail_body = if ready {
                        let check_view = if checks.is_empty() {
                            view! { <p>"No check runs reported."</p> }.into_view()
                        } else {
                            view! { <ul aria-label=format!("Checks for pull request #{number}")>{checks.into_iter().map(|check| view! {
                                <li>{format!("{} · {}", check.name, check_state_label(check.state))}</li>
                            }).collect_view()}</ul> }.into_view()
                        };
                        let review_view = if reviews.is_empty() {
                            view! { <p>"No reviews reported."</p> }.into_view()
                        } else {
                            view! { <ul aria-label=format!("Reviews for pull request #{number}")>{reviews.into_iter().map(|review| view! {
                                <li>{format!("{} · {}", review.reviewer, review_state_label(review.state))}</li>
                            }).collect_view()}</ul> }.into_view()
                        };
                        view! {
                            <div>
                                <h3>"Checks"</h3>
                                {check_view}
                                <h3>"Reviews"</h3>
                                {review_view}
                            </div>
                        }.into_view()
                    } else {
                        view! { <div></div> }.into_view()
                    };
                    view! {
                        <div class="forge-pr-details">
                            <p role="status">{message}</p>
                            {detail_body}
                        </div>
                    }.into_view()
                }
            })}
        </li>
    }
}

pub fn forge_view(open: RwSignal<bool>, repo: Signal<Option<String>>) -> impl IntoView {
    let page = create_rw_signal(1u32);
    let previous_repo = store_value(None::<String>);
    let refresh = create_rw_signal(0u64);
    let epoch = create_rw_signal(0u64);
    let result = create_rw_signal(None::<Result<ForgePage, String>>);
    let loading = create_rw_signal(false);
    // Clearing on close/selection change prevents reuse of private data. Each
    // response must still match both the request epoch and the repository.
    create_effect(move |_| {
        let is_open = open.get();
        let selected = repo.get();
        let requested_page = page_for(
            previous_repo.get_value().as_deref(),
            selected.as_deref(),
            page.get(),
        );
        // One effect decides the reset before fetching; a second reset effect
        // would issue both old-page and page-1 requests for the new repo.
        page.set_untracked(requested_page);
        previous_repo.set_value(selected.clone());
        refresh.get();
        let request_epoch = epoch.get_untracked().wrapping_add(1);
        epoch.set(request_epoch);
        result.set(None);
        loading.set(false);
        if let (true, Some(requested_repo)) = (is_open, selected) {
            loading.set(true);
            spawn_local(async move {
                let answer = crate::api::fetch_forge_page(&requested_repo, requested_page).await;
                if accepts_response(
                    open.get_untracked(),
                    request_epoch,
                    epoch.get_untracked(),
                    &requested_repo,
                    repo.get_untracked().as_deref(),
                ) {
                    result.set(Some(answer));
                    loading.set(false);
                }
            });
        }
    });
    let close = move || {
        open.set(false);
        result.set(None);
        page.set(1);
    };
    view! {
        <Show when=move || open.get()>
            <div style="position:fixed; inset:0; z-index:910; display:flex; align-items:center; justify-content:center; background:rgba(1,4,9,0.6);">
                <section role="dialog" aria-modal="true" aria-label="Pull requests"
                    style="width:620px; max-width:90vw; max-height:80vh; overflow:auto; padding:16px; background:#161b22; border:1px solid #30363d; border-radius:10px; color:var(--fg);">
                    <div style="display:flex; justify-content:space-between; align-items:center;">
                        <h2>"Pull requests"</h2>
                        <button class="refresh" on:click=move |_| close()>"Close"</button>
                    </div>
                    <p>"Read-only provider summaries. Pages are fetched on demand and are not saved."</p>
                    <Show when=move || repo.get().is_none()><p>"Select a repository first."</p></Show>
                    <Show when=move || loading.get()><p role="status">"Loading pull requests…"</p></Show>
                    {move || result.get().map(|answer| match answer {
                        Err(error) => view! { <p role="alert">{error}</p> }.into_view(),
                        Ok(data) => {
                            let message = availability_line(&data);
                            let capabilities = capability_line(&data.capabilities);
                            let details_available = data.capabilities.checks || data.capabilities.reviews;
                            let next = data.next_page;
                            view! {
                                {data.repository.map(|r| view! { <p><a href=r.web_url target="_blank" rel="noopener noreferrer">{r.name}</a></p> })}
                                <p role="status">{message}</p>
                                <p>{capabilities}</p>
                                <ul>{data.pulls.into_iter().map(|pr| pull_row(open, repo, pr, details_available)).collect_view()}</ul>
                                <div style="display:flex; gap:8px; align-items:center;">
                                    <button class="refresh" prop:disabled=move || page.get() <= 1 || loading.get()
                                        on:click=move |_| page.update(|p| *p = p.saturating_sub(1).max(1))>"Previous"</button>
                                    <span>{format!("Page {}", data.page)}</span>
                                    <button class="refresh" prop:disabled=move || next.is_none() || loading.get()
                                        on:click=move |_| { if let Some(n) = next { page.set(n); } }>"Next"</button>
                                </div>
                            }.into_view()
                        }
                    })}
                    <button class="refresh" prop:disabled=move || loading.get() || repo.get().is_none()
                        on:click=move |_| refresh.update(|n| *n = n.wrapping_add(1))>"Refresh pull requests"</button>
                </section>
            </div>
        </Show>
    }
}
