//! Plain words and action availability, independent of the browser (#141).
use git_vista_core::status::RepoStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Stage,
    Commit,
    Resolve,
    Pull { remote: String, branch: String },
    Push { branch: String },
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stage => "Stage all changes",
            Self::Commit => "Review and commit…",
            Self::Resolve => "Review conflicts…",
            Self::Pull { .. } => "Choose how to pull…",
            Self::Push { .. } => "Review push…",
        }
    }

    pub fn explanation(&self) -> &'static str {
        match self {
            Self::Stage => "Staging adds all modified, deleted, and new files to the next commit. It does not create a commit or push anything.",
            Self::Commit => "Review the staged files and write a message before creating your commit.",
            Self::Resolve => "Open the conflicted files and review how to combine their changes before continuing.",
            Self::Pull { .. } => "Pull fetches the upstream branch and integrates its commits. Choose merge or rebase in the next dialog before anything runs; Git can fast-forward when no local commits need combining.",
            Self::Push { .. } => "Push sends your existing commits to origin. Review and confirm the push next; uncommitted changes stay here.",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Detail {
    pub sentences: Vec<String>,
    pub actions: Vec<Action>,
}

pub fn can_write(read_only: Option<bool>, lan: bool, online: bool, busy: bool) -> bool {
    read_only == Some(false) && !lan && online && !busy
}

/// A result retained by Leptos during reload is not a reading for a new scope.
pub fn reading_is_current(
    loading: bool,
    requested_epoch: u64,
    current_epoch: u64,
    requested_repo: Option<&str>,
    current_repo: Option<&str>,
) -> bool {
    !loading
        && requested_epoch == current_epoch
        && current_repo.is_some()
        && requested_repo == current_repo
}

/// The reply a status resource retained, resolved against the live frame.
///
/// The requested epoch and repository travel *inside* `reply`, because that is
/// what the fetch tagged them with. A call site therefore cannot pair the live
/// frame with some other request's scope, or pass the same value for both
/// sides of the comparison — the shape that a wasm-only call site made
/// possible and a source census could never see. All the view supplies is
/// live values; the decision itself is here, on the host.
pub fn current_reading(
    loading: bool,
    reply: Option<(u64, Option<String>, Option<RepoStatus>)>,
    current_epoch: u64,
    current_repo: Option<&str>,
) -> Option<RepoStatus> {
    let (requested_epoch, requested_repo, result) = reply?;
    reading_is_current(
        loading,
        requested_epoch,
        current_epoch,
        requested_repo.as_deref(),
        current_repo,
    )
    .then_some(result)
    .flatten()
}

/// How many staged files the context menu may offer to act on (#709).
///
/// Takes the retained reply in the same shape [`current_reading`] does, and
/// for the same reason: the requested epoch and repository travel *inside*
/// `reply`, so the menu cannot pair the live frame with some other request's
/// scope. Routing the count through `current_reading` rather than reading
/// `reply`'s payload directly is the whole point — a loading, failed,
/// old-epoch or old-repository reply counts **zero**, so "Unstage Changes"
/// and "Select Changes to Unstage…" stay absent rather than offering to
/// unstage an index belonging to a repository the user has already left.
///
/// Zero is also what the menu showed when its own fetch failed, so the
/// unknown arm is not a new state for that view — only a correct one.
pub fn actionable_staged_count(
    loading: bool,
    reply: Option<(u64, Option<String>, Option<RepoStatus>)>,
    current_epoch: u64,
    current_repo: Option<&str>,
) -> usize {
    current_reading(loading, reply, current_epoch, current_repo).map_or(0, |s| s.staged.len())
}

fn files(n: usize) -> String {
    format!("{n} {}", if n == 1 { "file" } else { "files" })
}

fn commits(n: u32) -> String {
    format!("{n} {}", if n == 1 { "commit" } else { "commits" })
}

/// `writable` requires a current Active frame, a loopback session, and connectivity.
/// Missing/failed/loading readings all take the unknown arm, never Default::clean.
pub fn status_detail(status: Option<&RepoStatus>, writable: bool) -> Detail {
    let Some(s) = status else {
        return Detail {
            sentences: vec!["I can’t tell the repository’s status yet. No current reading is available; the request may still be loading or may have failed.".into()],
            actions: vec![],
        };
    };
    let mut sentences = vec![];
    let mut actions = vec![];
    if s.is_clean() {
        sentences.push("Your working tree is clean. There are no uncommitted changes.".into());
    } else {
        sentences.push(format!("{} {} staged for the next commit; {} {} tracked changes that are not staged; {} {} new and not tracked by Git.",
            files(s.staged.len()), if s.staged.len() == 1 { "is" } else { "are" },
            files(s.unstaged.len()), if s.unstaged.len() == 1 { "has" } else { "have" },
            files(s.untracked.len()), if s.untracked.len() == 1 { "is" } else { "are" }));
        sentences.push(
            "A file can have both staged and unstaged changes, so it can appear in both counts."
                .into(),
        );
    }
    if !s.conflicted.is_empty() {
        sentences.push(format!(
            "{} {} unresolved conflicts. Review and resolve them before committing or pulling.",
            files(s.conflicted.len()),
            if s.conflicted.len() == 1 {
                "has"
            } else {
                "have"
            }
        ));
        actions.push(Action::Resolve);
    } else {
        if !s.unstaged.is_empty() || !s.untracked.is_empty() {
            actions.push(Action::Stage);
        }
        if !s.staged.is_empty() {
            actions.push(Action::Commit);
        }
    }
    match (s.branch.as_deref(), s.upstream.as_deref()) {
        (None, _) => sentences.push("HEAD is detached: you are viewing a commit without a checked-out branch, so there is no branch here to pull or push.".into()),
        (Some(branch), None) => sentences.push(format!("No upstream is configured for {branch}, so I can’t tell which commits need pulling or pushing.")),
        (Some(branch), Some(upstream)) => {
            if s.ahead == 0 && s.behind == 0 {
                // Porcelain omits `# branch.ab` when it cannot compute the
                // comparison, and both counts then default to 0 — the same
                // bits as a genuine match. Disclose that; do not assert a
                // distinction the wire format cannot carry.
                sentences.push(format!("The latest local reading reports no commits ahead of or behind {upstream} for {branch}. Git reports these same zeros when it could not compare them, so this reading cannot tell those two cases apart."));
            } else {
                if s.behind > 0 { sentences.push(format!("{} on {upstream} {} missing from {branch}.", commits(s.behind), if s.behind == 1 { "is" } else { "are" })); }
                if s.ahead > 0 { sentences.push(format!("{} on {branch} {} not on {upstream} yet.", commits(s.ahead), if s.ahead == 1 { "is" } else { "are" })); }
                if s.ahead > 0 && s.behind > 0 { sentences.push("The branches have diverged: each has commits the other does not. Integrate the upstream changes before pushing.".into()); }
            }
            sentences.push("These counts use the latest local tracking refs, not a fresh check of the remote.".into());
            // Existing push targets origin/<local branch>. Do not silently send
            // to that destination when this reading names a different upstream.
            if s.conflicted.is_empty() {
                if s.behind > 0 {
                    if s.is_clean() {
                        if let Some((remote, remote_branch)) = upstream.split_once('/') {
                            if !remote.is_empty() && remote_branch == branch {
                                actions.push(Action::Pull { remote: remote.into(), branch: branch.into() });
                            } else { sentences.push("Use the remote branch menu to choose this upstream explicitly before pulling.".into()); }
                        }
                    } else { sentences.push("Commit or otherwise put aside your uncommitted changes before pulling.".into()); }
                } else if s.ahead > 0 {
                    if upstream == format!("origin/{branch}") { actions.push(Action::Push { branch: branch.into() }); }
                    else { sentences.push("The existing push flow targets origin with the same branch name. Choose the correct destination in your Git client for this upstream.".into()); }
                }
            }
        }
    }
    if !writable {
        actions.clear();
    }
    Detail { sentences, actions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::status::{ChangeKind, FileChange};

    #[test]
    fn only_an_online_idle_active_loopback_session_can_offer_writes() {
        assert!(can_write(Some(false), false, true, false));
        for read_only in [None, Some(true), Some(false)] {
            for lan in [false, true] {
                for online in [false, true] {
                    for busy in [false, true] {
                        assert_eq!(
                            can_write(read_only, lan, online, busy),
                            (read_only, lan, online, busy) == (Some(false), false, true, false)
                        );
                    }
                }
            }
        }
    }

    /// A STRING census, and no more than that. It detects removal of the
    /// core-to-view calls and rendering. It does NOT verify behaviour: extra
    /// ungated buttons, or `writable` ignoring what `can_write` returned,
    /// still match these substrings. Real browser assertions cover opening,
    /// copy and the read-only DOM; the scope decision is pinned by
    /// `only_a_reply_matching_the_live_frame_becomes_a_reading`, which runs
    /// the code rather than reading it.
    #[test]
    fn wasm_view_renders_the_core_sentences_and_offers() {
        let view = include_str!("view.rs");
        assert!(view.contains("status_detail(read(status).as_ref(), writable())"));
        assert!(view.contains("can_write("));
        assert!(view.contains("detail().sentences.into_iter()"));
        assert!(view.contains("detail().actions.into_iter()"));
        assert!(view.contains("detail().actions.contains(&action)"));
        let signals = include_str!("../signals.rs");
        assert!(signals.contains("current_reading("));
        assert!(signals.contains("fetch_status_for(id)"));
        assert!(include_str!("../../../app/mod.rs").contains("status_chip_view("));
    }
    fn status(ahead: u32, behind: u32) -> RepoStatus {
        RepoStatus {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead,
            behind,
            ..Default::default()
        }
    }
    #[test]
    fn unknown_never_claims_clean_or_offers_a_write() {
        assert_eq!(status_detail(None, true), Detail { sentences: vec!["I can’t tell the repository’s status yet. No current reading is available; the request may still be loading or may have failed.".into()], actions: vec![] });
    }
    #[test]
    fn clean_and_zero_counts_say_what_the_local_reading_knows() {
        let d = status_detail(Some(&status(0, 0)), true);
        assert_eq!(
            d.sentences[0],
            "Your working tree is clean. There are no uncommitted changes."
        );
        // Zero-zero is a disclosure, not a distinction: porcelain omits
        // `# branch.ab` when it cannot compare, and both counts default to 0.
        // The lead sentence must not claim to tell those cases apart.
        assert_eq!(d.sentences[1], "The latest local reading reports no commits ahead of or behind origin/main for main. Git reports these same zeros when it could not compare them, so this reading cannot tell those two cases apart.");
        assert!(!d.sentences[1].contains("up to date"));
        assert_eq!(
            d.sentences[2],
            "These counts use the latest local tracking refs, not a fresh check of the remote."
        );
        assert!(d.actions.is_empty());
    }
    #[test]
    fn ahead_and_behind_are_not_reversed_and_agree_with_actions() {
        let ahead = status_detail(Some(&status(1, 0)), true);
        assert_eq!(
            ahead.sentences[1],
            "1 commit on main is not on origin/main yet."
        );
        assert_eq!(
            ahead.actions,
            vec![Action::Push {
                branch: "main".into()
            }]
        );
        let behind = status_detail(Some(&status(0, 3)), true);
        assert_eq!(
            behind.sentences[1],
            "3 commits on origin/main are missing from main."
        );
        assert_eq!(
            behind.actions,
            vec![Action::Pull {
                remote: "origin".into(),
                branch: "main".into()
            }]
        );
        let diverged = status_detail(Some(&status(2, 1)), true);
        assert_eq!(&diverged.sentences[1..4], ["1 commit on origin/main is missing from main.", "2 commits on main are not on origin/main yet.", "The branches have diverged: each has commits the other does not. Integrate the upstream changes before pushing."]);
        assert!(matches!(diverged.actions.as_slice(), [Action::Pull { .. }]));
    }
    #[test]
    fn no_upstream_and_detached_do_not_invent_a_comparison() {
        let mut s = status(8, 9);
        s.upstream = None;
        assert_eq!(status_detail(Some(&s), true).sentences[1], "No upstream is configured for main, so I can’t tell which commits need pulling or pushing.");
        assert!(status_detail(Some(&s), true).actions.is_empty());
        s.branch = None;
        assert_eq!(status_detail(Some(&s), true).sentences[1], "HEAD is detached: you are viewing a commit without a checked-out branch, so there is no branch here to pull or push.");
    }
    #[test]
    fn dirty_and_conflicted_sentences_have_correct_plural_forms() {
        let mut s = status(0, 2);
        s.staged.push(FileChange {
            path: "a".into(),
            kind: ChangeKind::Modified,
        });
        s.unstaged = s.staged.clone();
        s.untracked = vec!["b".into(), "c".into()];
        let d = status_detail(Some(&s), true);
        assert_eq!(d.sentences[0], "1 file is staged for the next commit; 1 file has tracked changes that are not staged; 2 files are new and not tracked by Git.");
        assert_eq!(d.actions, [Action::Stage, Action::Commit]);
        s.conflicted = vec!["d".into()];
        let d = status_detail(Some(&s), true);
        assert_eq!(d.sentences[2], "1 file has unresolved conflicts. Review and resolve them before committing or pulling.");
        assert_eq!(d.actions, [Action::Resolve]);
        s.conflicted.push("e".into());
        assert_eq!(status_detail(Some(&s), true).sentences[2], "2 files have unresolved conflicts. Review and resolve them before committing or pulling.");
    }
    #[test]
    fn explanation_only_never_renders_write_actions() {
        for s in [
            status(2, 0),
            status(0, 3),
            RepoStatus {
                untracked: vec!["new".into()],
                ..status(0, 0)
            },
        ] {
            let active = status_detail(Some(&s), true);
            let readonly = status_detail(Some(&s), false);
            assert!(!active.actions.is_empty());
            assert!(readonly.actions.is_empty());
            assert_eq!(active.sentences, readonly.sentences);
        }
    }
    #[test]
    fn unsupported_push_destination_is_not_silently_replaced() {
        let mut s = status(2, 0);
        s.upstream = Some("other/trunk".into());
        let d = status_detail(Some(&s), true);
        // Both halves of the claim: no retargeted push offer, AND the guidance
        // that replaces it. Asserting only the empty vec would stay green if
        // the guidance were deleted and the user left with no explanation.
        assert!(d.actions.is_empty());
        assert_eq!(d.sentences.last().unwrap(), "The existing push flow targets origin with the same branch name. Choose the correct destination in your Git client for this upstream.");
    }
    /// The wiring, not just the predicate. `current_reading` is what the wasm
    /// adapter calls, and the requested scope arrives inside the reply — so
    /// duplicating or swapping the two sides of the comparison is a change
    /// this host test can go red on, which the source census never could.
    #[test]
    fn only_a_reply_matching_the_live_frame_becomes_a_reading() {
        let s = status(0, 0);
        let reply = |epoch: u64, repo: &str| Some((epoch, Some(repo.to_string()), Some(s.clone())));
        assert_eq!(
            current_reading(false, reply(4, "a"), 4, Some("a")),
            Some(s.clone())
        );
        assert_eq!(current_reading(true, reply(4, "a"), 4, Some("a")), None);
        assert_eq!(current_reading(false, reply(3, "a"), 4, Some("a")), None);
        assert_eq!(current_reading(false, reply(4, "b"), 4, Some("a")), None);
        assert_eq!(current_reading(false, reply(4, "a"), 4, None), None);
        assert_eq!(current_reading(false, None, 4, Some("a")), None);
        assert_eq!(
            current_reading(false, Some((4, Some("a".into()), None)), 4, Some("a")),
            None
        );
    }
    /// #709: the menu path, decided here so it can go red on the host.
    ///
    /// `menu.rs` used to count staged files from its own **unscoped**
    /// `fetch_status()`. That reply named no repository, so switching
    /// repositories left the previous one's index describing the new one's
    /// menu — "Unstage Changes" offered on a tree with nothing staged, and
    /// (the direction that costs something) *withheld* on one that has. The
    /// old code is what the first assertion below reproduces: `reply`'s
    /// payload taken at face value would answer 3 for every case in this
    /// test. Routing it through `current_reading` answers 3 exactly once.
    #[test]
    fn only_a_reply_matching_the_live_frame_can_offer_an_unstage() {
        let mut s = status(0, 0);
        s.staged = vec![
            FileChange {
                path: "a".into(),
                kind: ChangeKind::Modified,
            },
            FileChange {
                path: "b".into(),
                kind: ChangeKind::Modified,
            },
            FileChange {
                path: "c".into(),
                kind: ChangeKind::Modified,
            },
        ];
        let reply = |epoch: u64, repo: &str| Some((epoch, Some(repo.to_string()), Some(s.clone())));
        assert_eq!(
            actionable_staged_count(false, reply(4, "a"), 4, Some("a")),
            3
        );
        // Still in flight: a retained reply is not a reading for this key.
        assert_eq!(
            actionable_staged_count(true, reply(4, "a"), 4, Some("a")),
            0
        );
        // Stale epoch — the repository moved under this reply.
        assert_eq!(
            actionable_staged_count(false, reply(3, "a"), 4, Some("a")),
            0
        );
        // Stale repository — the exact leak #709 names.
        assert_eq!(
            actionable_staged_count(false, reply(4, "b"), 4, Some("a")),
            0
        );
        // No accepted frame yet: nothing vouches for any reading.
        assert_eq!(actionable_staged_count(false, reply(4, "a"), 4, None), 0);
        // Never fetched, and fetched-but-failed, are both unknown, not zero
        // staged files that happen to read the same — the item is absent
        // either way, which is what the old fetch-failure arm already did.
        assert_eq!(actionable_staged_count(false, None, 4, Some("a")), 0);
        assert_eq!(
            actionable_staged_count(false, Some((4, Some("a".into()), None)), 4, Some("a")),
            0
        );
    }

    /// The wiring for the same claim, in the bytes that ship (#709).
    ///
    /// `menu.rs` and `api/status.rs` are both `#[cfg(target_arch = "wasm32")]`
    /// in `main.rs`, so `cargo test` compiles neither — the blind spot
    /// `offline_guard_audit` exists for. The test above proves the *decision*;
    /// this proves the menu reaches it, and that the unscoped entry point it
    /// used to reach instead is gone rather than merely unused.
    /// Whole-line `//` comments dropped, so a census asserting the *absence*
    /// of a call cannot be defeated — or, as happened while writing this, be
    /// tripped — by prose naming the call it forbids. A trailing comment on a
    /// line of code is left alone: that line still ships code.
    fn code_only(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_menu_counts_staged_files_from_the_pinned_read_only() {
        let menu = code_only(&format!(
            "{}{}",
            include_str!("../../../menu.rs"),
            include_str!("../../../menu/worktree_items.rs")
        ));
        assert!(menu.contains("status_state::staged_count(status)"));
        // No v1 status fetch of the menu's own, scoped or not. `fetch_status(`
        // is not a substring of `fetch_worktree_status(`, which the menu does
        // and should still call — that is the v2 per-path read.
        assert!(!menu.contains("fetch_status("));
        assert!(!menu.contains("fetch_status_for("));
        let api = code_only(&format!(
            "{}{}",
            include_str!("../../../api.rs"),
            include_str!("../../../api/status.rs")
        ));
        // The v1 read takes the repository by value: "unscoped" is not a
        // value any caller can pass, so this cannot regress by omission.
        assert!(api.contains("pub async fn fetch_status_for(repo: &str)"));
        assert!(api.contains("\"/api/status?t={}&repo={}\""));
        assert!(!api.contains("fn fetch_status()"));
        assert!(!api.contains("fetch_status,"));
    }

    #[test]
    fn retained_readings_must_match_epoch_and_repository() {
        assert!(reading_is_current(false, 4, 4, Some("a"), Some("a")));
        assert!(!reading_is_current(true, 4, 4, Some("a"), Some("a")));
        assert!(!reading_is_current(false, 3, 4, Some("a"), Some("a")));
        assert!(!reading_is_current(false, 4, 4, Some("a"), Some("b")));
        assert!(!reading_is_current(false, 4, 4, None, None));
    }
}
