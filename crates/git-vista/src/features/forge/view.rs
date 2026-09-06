//! DOM wiring only. Provider requests never participate in the graph resource.
use leptos::*;
use git_vista_protocol::forge::ForgePage;
use super::core::{accepts_response, availability_line, capability_line};

pub fn forge_view(open: RwSignal<bool>, repo: Signal<Option<String>>) -> impl IntoView {
    let page = create_rw_signal(1u32);
    let refresh = create_rw_signal(0u64);
    let epoch = create_rw_signal(0u64);
    let result = create_rw_signal(None::<Result<ForgePage, String>>);
    let loading = create_rw_signal(false);
    // Clearing on close/selection change prevents reuse of private data. Each
    // response must still match both the request epoch and the repository.
    create_effect(move |_| {
        let is_open = open.get();
        let selected = repo.get();
        let requested_page = page.get();
        refresh.get();
        let request_epoch = epoch.get_untracked().wrapping_add(1);
        epoch.set(request_epoch);
        result.set(None);
        loading.set(false);
        if let (true, Some(requested_repo)) = (is_open, selected) {
            loading.set(true);
            spawn_local(async move {
                let answer = crate::api::fetch_forge_page(&requested_repo, requested_page).await;
                if accepts_response(open.get_untracked(), request_epoch, epoch.get_untracked(), &requested_repo, repo.get_untracked().as_deref()) {
                    result.set(Some(answer));
                    loading.set(false);
                }
            });
        }
    });
    // Repository changes reset pagination even while the panel is closed.
    create_effect(move |_| { repo.get(); page.set(1); });
    let close = move || { open.set(false); result.set(None); page.set(1); };
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
                            let next = data.next_page;
                            view! {
                                {data.repository.map(|r| view! { <p><a href=r.web_url target="_blank" rel="noopener noreferrer">{r.name}</a></p> })}
                                <p role="status">{message}</p>
                                <p>{capabilities}</p>
                                <ul>{data.pulls.into_iter().map(|pr| view! {
                                    <li style="margin:10px 0;">
                                        <a href=pr.web_url target="_blank" rel="noopener noreferrer">{format!("#{} {}", pr.number, pr.title)}</a>
                                        {if pr.draft { " · Draft" } else { "" }}
                                    </li>
                                }).collect_view()}</ul>
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
