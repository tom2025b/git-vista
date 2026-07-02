//! The icon system: Nerd Font glyphs for git concepts, with a plain-text
//! fallback set.
//!
//! Every icon the UI uses is a named field on [`GitIcons`], so call sites say
//! *what* they mean (`ic.branch`, `ic.merge`) and never embed a raw glyph.
//! Two complete sets exist:
//!
//!  * [`ICONS`] — Nerd Font glyphs (<https://www.nerdfonts.com/cheat-sheet>).
//!    These live in the Private Use Area, so they render only where a patched
//!    Nerd Font is installed; the `.nf` CSS class (styles.css) puts the Nerd
//!    Font families first in the stack for exactly these spans.
//!  * [`TEXT_ICONS`] — plain-text approximations that any font can render
//!    (ASCII plus a few universally-supported symbols), leaning on git's own
//!    conventions: `*` from `git log --graph`, diff-style `+`/`-`/`~`, `$`
//!    for a stash as in `__git_ps1`.
//!
//! The active set is chosen at render time by [`icon_set`], driven by the
//! user's "Icons" toggle in the topbar (persisted in `localStorage`; see
//! `app.rs`). Both sets share one struct, so switching can never miss a spot.
//!
//! Extending: add a field to [`GitIcons`], give it a value in *both* constants
//! (the exhaustive struct literals make the compiler enforce this), and use it.
//! Icons are `&'static str` rather than `char` so a fallback may be more than
//! one character ("GH", ">>").

/// One complete set of icons for the git concepts the UI shows. Fields are
/// grouped as: sources (git/GitHub/repo), graph objects (branch/commit/…),
/// then file & worktree statuses.
///
/// Deliberately a *complete* set: some statuses (modified/renamed/untracked,
/// clean/dirty, stash, pull request) have no UI surface yet — the commit detail
/// endpoint carries no per-file change list — but they're defined now so a
/// future diff view or worktree indicator just picks them up. Hence the allow.
#[allow(dead_code)]
pub struct GitIcons {
    // -- Sources -------------------------------------------------------------
    /// The git logo — brands the app itself (topbar title).
    pub git: &'static str,
    /// The GitHub mark — "open on GitHub" links and menu items.
    pub github: &'static str,
    /// A source repository — the "repository: …" status line.
    pub repository: &'static str,

    // -- Graph objects -------------------------------------------------------
    /// A branch — local branch badges and "create branch" actions.
    pub branch: &'static str,
    /// Alternate branch glyph — used for *remote* branch badges, so local and
    /// remote pills are distinguishable at a glance.
    pub branch_alt: &'static str,
    /// A single commit — commit meta lines, commit menu items, detail panel.
    pub commit: &'static str,
    /// A pull request.
    pub pull_request: &'static str,
    /// A merge — the "Merge ‘x’ into …" menu item.
    pub merge: &'static str,
    /// A tag — tag badges.
    pub tag: &'static str,

    // -- File statuses (diff/worktree) ---------------------------------------
    /// File added / staged addition.
    pub added: &'static str,
    /// File modified.
    pub modified: &'static str,
    /// File deleted — also colours the destructive "Delete branch" action.
    pub deleted: &'static str,
    /// File renamed.
    pub renamed: &'static str,
    /// Untracked file.
    pub untracked: &'static str,
    /// Merge conflict / something needs attention — also the error status line.
    pub conflict: &'static str,

    // -- Worktree summary ----------------------------------------------------
    /// Working tree clean / operation succeeded.
    pub clean: &'static str,
    /// Working tree dirty (uncommitted changes).
    pub dirty: &'static str,
    /// Stashed changes.
    pub stash: &'static str,
}

/// The Nerd Font set. Codepoints are from the Nerd Fonts cheat sheet; most sit
/// in the Private Use Area (Devicons `E7xx`, Octicons `F4xx`, Font Awesome
/// `F0xx`, Material Design `F02xx`), so they need a Nerd Font to render —
/// otherwise the browser shows tofu (□) and [`TEXT_ICONS`] is the answer.
pub const ICONS: GitIcons = GitIcons {
    git: "\u{E702}",          // nf-dev-git
    github: "\u{F09B}",       // nf-fa-github
    repository: "\u{F02A2}",  // nf-md-source_repository
    branch: "\u{E725}",       // nf-dev-git_branch
    branch_alt: "\u{F418}",   // nf-oct-git_branch
    commit: "\u{F417}",       // nf-oct-git_commit
    pull_request: "\u{F407}", // nf-oct-git_pull_request
    merge: "\u{F419}",        // nf-oct-git_merge
    tag: "\u{F02B}",          // nf-fa-tag
    added: "\u{F457}",        // nf-oct-diff_added
    modified: "\u{F459}",     // nf-oct-diff_modified
    deleted: "\u{F458}",      // nf-oct-diff_removed
    renamed: "\u{F45A}",      // nf-oct-diff_renamed
    untracked: "\u{F128}",    // nf-fa-question
    conflict: "\u{F071}",     // nf-fa-warning
    clean: "\u{F058}",        // nf-fa-check_circle
    dirty: "\u{25CF}",        // ● black circle (not PUA, but themed with the set)
    stash: "\u{F187}",        // nf-fa-archive
};

/// The plain-text fallback set: every value renders in any font (ASCII, or a
/// symbol with universal system-font coverage), for devices without a Nerd
/// Font — e.g. an iPad, where only the system fonts exist. Values reuse git's
/// own textual conventions where one exists.
pub const TEXT_ICONS: GitIcons = GitIcons {
    git: "\u{B1}",     // ± — the classic git symbol in shell prompts
    github: "GH",
    repository: "\u{25C6}", // ◆
    branch: "\u{BB}",  // » — "forked off to…"
    branch_alt: ">>",  // remote branch: same idea, visibly different
    commit: "*",       // the commit marker in `git log --graph`
    pull_request: "PR",
    merge: "><",       // two lines joining
    tag: "#",          // a label
    added: "+",        // diff-style
    modified: "~",
    deleted: "-",
    renamed: "\u{2192}", // →
    untracked: "?",    // `git status --short` shows untracked as ??
    conflict: "!",
    clean: "\u{2713}", // ✓
    dirty: "*",        // `__git_ps1` marks a dirty tree with *
    stash: "$",        // `__git_ps1` marks a stash with $
};

/// The set to render with: Nerd Font glyphs when `nerd` is on (the default),
/// the plain-text fallback otherwise. Call sites take the whole set once and
/// pick fields off it, so one toggle switches everything.
pub fn icon_set(nerd: bool) -> &'static GitIcons {
    if nerd {
        &ICONS
    } else {
        &TEXT_ICONS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both sets, field by field, in one place — so a new field only needs
    /// adding here once to be covered by every test below.
    fn fields(set: &GitIcons) -> Vec<(&'static str, &'static str)> {
        vec![
            ("git", set.git),
            ("github", set.github),
            ("repository", set.repository),
            ("branch", set.branch),
            ("branch_alt", set.branch_alt),
            ("commit", set.commit),
            ("pull_request", set.pull_request),
            ("merge", set.merge),
            ("tag", set.tag),
            ("added", set.added),
            ("modified", set.modified),
            ("deleted", set.deleted),
            ("renamed", set.renamed),
            ("untracked", set.untracked),
            ("conflict", set.conflict),
            ("clean", set.clean),
            ("dirty", set.dirty),
            ("stash", set.stash),
        ]
    }

    #[test]
    fn nerd_icons_are_single_glyphs() {
        // Each Nerd Font icon is exactly one codepoint — a glyph, not a string —
        // so it occupies one cell in the monospace badge-width math.
        for (name, icon) in fields(&ICONS) {
            assert_eq!(icon.chars().count(), 1, "ICONS.{name} should be one glyph");
        }
    }

    #[test]
    fn fallback_icons_avoid_the_private_use_area() {
        // The whole point of TEXT_ICONS is rendering without a Nerd Font, so no
        // fallback may contain a PUA codepoint (U+E000–U+F8FF, or the
        // supplementary planes' PUA where the Material icons live).
        for (name, icon) in fields(&TEXT_ICONS) {
            assert!(!icon.is_empty(), "TEXT_ICONS.{name} should not be empty");
            for c in icon.chars() {
                let pua = ('\u{E000}'..='\u{F8FF}').contains(&c) || c >= '\u{F0000}';
                assert!(!pua, "TEXT_ICONS.{name} contains PUA char {c:?}");
            }
        }
    }

    #[test]
    fn icon_set_picks_the_requested_set() {
        // Compare by value, not address: a `const` is inlined at each use site,
        // so `&ICONS` has no single stable address to pointer-compare against.
        assert_eq!(icon_set(true).git, ICONS.git);
        assert_eq!(icon_set(false).git, TEXT_ICONS.git);
        assert_ne!(icon_set(true).git, icon_set(false).git);
    }
}
