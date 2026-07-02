//! The commit detail panel (Phase 10). Docked to the right, it shows one
//! commit's full detail — the whole message body and both the author and
//! committer signatures — fetched lazily by hash. The chrome (title + close)
//! shows the instant a commit is picked; the body reacts to the fetch: a
//! "Loading…" line, git's error, or the detail once it lands. Clicking a parent
//! hash re-points the panel at that parent, so you can walk up the history.

use leptos::*;

use crate::datetime::local_timestamp;
use crate::icons::icon_set;
use crate::render::RenderCtx;
use crate::state::{DetailResource, Overlays, Settings};

/// Build the detail panel view. `detail` is the lazily-fetched commit keyed on
/// `detail_id`; `ctx` supplies the repo's GitHub base + pushed-commit set for the
/// "Open on GitHub" link.
pub fn detail_panel_view(
    overlays: Overlays,
    settings: Settings,
    detail: DetailResource,
    ctx: StoredValue<RenderCtx>,
) -> impl IntoView {
    let detail_id = overlays.detail_id;
    let nerd_icons = settings.nerd_icons;
    move || {
        detail_id.get().map(|open_id| {
            // Tracked read, like the menu: the panel re-renders live if the icon
            // style is toggled while it's open.
            let ic = icon_set(nerd_icons.get());
            let body = move || {
                // While the fetch is in flight `get()` is `None`; a stale value from
                // the previously-viewed commit is also treated as loading, so the
                // panel never shows one commit's chrome over another's detail.
                match detail.get().flatten() {
                    None => view! { <p class="detail-status">"Loading…"</p> }.into_view(),
                    Some(Err(e)) => view! {
                        <p class="detail-status detail-error">{format!("Couldn't load commit: {e}")}</p>
                    }
                    .into_view(),
                    Some(Ok(d)) if d.id.0 != open_id => {
                        view! { <p class="detail-status">"Loading…"</p> }.into_view()
                    }
                    Some(Ok(d)) => {
                        // Link to the commit on GitHub when the repo has a github.com
                        // origin *and* this commit is pushed — same rule the labels
                        // and menu use, so the link never 404s.
                        let github = ctx.with_value(|c| {
                            c.repo_url.as_ref().and_then(|base| {
                                c.remote_set
                                    .contains(&d.id.0)
                                    .then(|| format!("{base}/commit/{}", d.id.0))
                            })
                        });
                        // Author and committer lines. Show the committer only when it
                        // differs from the author (name/email or time) — for most
                        // commits they're identical and a second identical line is noise.
                        let committer_differs = d.committer_name != d.author_name
                            || d.committer_email != d.author_email
                            || d.commit_time != d.author_time;
                        let committer_row = committer_differs.then(|| {
                            view! {
                                <div class="detail-field">
                                    <span class="detail-key">"Committer"</span>
                                    <span class="detail-val">
                                        {format!("{} <{}>", d.committer_name, d.committer_email)}
                                        <span class="detail-date">
                                            {format!(" · {}", local_timestamp(d.commit_time))}
                                        </span>
                                    </span>
                                </div>
                            }
                        });
                        // Parents: each short hash re-points the panel at that parent,
                        // so you can walk up the history from within the panel.
                        let parents = if d.parents.is_empty() {
                            view! { <span class="detail-val detail-muted">"none (root commit)"</span> }
                                .into_view()
                        } else {
                            d.parents
                                .iter()
                                .map(|p| {
                                    let full = p.0.clone();
                                    let short = p.short().to_string();
                                    view! {
                                        <button
                                            class="detail-parent"
                                            on:click=move |_| detail_id.set(Some(full.clone()))
                                            title="View this parent"
                                        >
                                            {short}
                                        </button>
                                    }
                                    .into_view()
                                })
                                .collect_view()
                        };
                        let github_row = match github {
                            Some(url) => view! {
                                <a class="detail-github" href=url target="_blank" rel="noopener">
                                    // Same GitHub mark as the menu's external link.
                                    <span class="nf ctx-icon">{ic.github}</span>
                                    "Open on GitHub"
                                </a>
                            }
                            .into_view(),
                            None => ().into_view(),
                        };
                        view! {
                            <div class="detail-field">
                                <span class="detail-key">"Commit"</span>
                                <span class="detail-val detail-hash">{d.id.0.clone()}</span>
                            </div>
                            <div class="detail-field">
                                <span class="detail-key">"Author"</span>
                                <span class="detail-val">
                                    {format!("{} <{}>", d.author_name, d.author_email)}
                                    <span class="detail-date">
                                        {format!(" · {}", local_timestamp(d.author_time))}
                                    </span>
                                </span>
                            </div>
                            {committer_row}
                            <div class="detail-field">
                                <span class="detail-key">"Parents"</span>
                                <span class="detail-parents">{parents}</span>
                            </div>
                            {github_row}
                            <pre class="detail-msg">{d.message.clone()}</pre>
                        }
                        .into_view()
                    }
                }
            };
            view! {
                <aside class="detail-panel">
                    <div class="detail-head">
                        // The commit glyph titles the panel — it's one commit's view.
                        <span class="detail-title">
                            <span class="nf ctx-icon">{ic.commit}</span>
                            "Commit details"
                        </span>
                        <button
                            class="detail-close"
                            title="Close"
                            on:click=move |_| detail_id.set(None)
                        >
                            "×"
                        </button>
                    </div>
                    <div class="detail-body">{body}</div>
                </aside>
            }
        })
    }
}
