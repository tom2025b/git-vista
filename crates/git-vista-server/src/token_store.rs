//! GitHub token resolution (#583, M13.02) — the fallback chain
//! `state::credential_token` (M13.01, #582) was scaffolding for: an OS
//! keyring first, an environment variable second, a gitignored local file
//! last. See ADR 0122.
//!
//! Absence at every tier is the normal state: a public repository works
//! with no token at all, so nothing here treats "not found" as an error —
//! every source folds a real failure (no D-Bus session, no file, an unset
//! variable) into `None` rather than surfacing it.

use std::path::Path;

/// Which tier answered, so a caller can report *why* a resolution came out
/// the way it did — a user whose stale `GH_TOKEN` shadows a fresh keyring
/// entry has no way to diagnose that otherwise (#583).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenSource {
    Keyring,
    EnvGitVista,
    EnvGh,
    File,
}

impl TokenSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            TokenSource::Keyring => "OS keyring",
            TokenSource::EnvGitVista => "GIT_VISTA_GITHUB_TOKEN",
            TokenSource::EnvGh => "GH_TOKEN",
            TokenSource::File => "local token file",
        }
    }
}

/// One credential, addressed by these two fixed strings — not a
/// per-repository secret. ADR 0122 decision 6 keeps token scope to one
/// value for now; widening this to per-repo entries is a later issue's
/// problem, not #583's.
const KEYRING_SERVICE: &str = "git-vista";
const KEYRING_USERNAME: &str = "github-token";

const GIT_VISTA_ENV: &str = "GIT_VISTA_GITHUB_TOKEN";
const GH_ENV: &str = "GH_TOKEN";

/// Resolve the token by trying each source in the documented precedence
/// order, stopping at the first that has one. `None` means every tier came
/// up empty — the normal, unremarkable case for a public repository.
pub(crate) fn resolve_token() -> Option<(String, TokenSource)> {
    resolve_from(
        keyring_token(),
        env_token(GIT_VISTA_ENV),
        env_token(GH_ENV),
        file_token(),
    )
}

/// The precedence engine, isolated from every real source so a test can
/// assert the ORDER without touching the OS keyring, the environment, or
/// disk (#583 acceptance: "precedence is asserted, not assumed").
fn resolve_from(
    keyring: Option<String>,
    env_git_vista: Option<String>,
    env_gh: Option<String>,
    file: Option<String>,
) -> Option<(String, TokenSource)> {
    keyring
        .map(|t| (t, TokenSource::Keyring))
        .or_else(|| env_git_vista.map(|t| (t, TokenSource::EnvGitVista)))
        .or_else(|| env_gh.map(|t| (t, TokenSource::EnvGh)))
        .or_else(|| file.map(|t| (t, TokenSource::File)))
}

/// A blank or whitespace-only value is treated the same as absent, at every
/// tier — a stray `export GH_TOKEN=` left in a shell profile must not shadow
/// a real token further down the chain.
fn non_blank(s: String) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn env_token(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(non_blank)
}

/// Reads the OS keyring entry, when the platform has one available. Every
/// failure — no D-Bus session, no entry set, a locked store — collapses to
/// `None`; keyring absence must never surface as a server error (#583).
///
/// `keyring::Entry::new` performs a blocking round trip to the platform
/// credential store (a D-Bus call, on Linux) the first time it runs in this
/// process. That is acceptable here: this only runs once per clone/push
/// (`state::credential_token`'s one call site), a request that already
/// waits on a real child git process for as long as ten minutes (#216).
fn keyring_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME).ok()?;
    entry.get_password().ok().and_then(non_blank)
}

/// Where the tier-3 fallback file lives and what it holds, read as plain
/// text and trimmed. `state::token_file_path` documents the path itself;
/// this only reads it.
fn file_token() -> Option<String> {
    file_token_at(&crate::state::token_file_path())
}

fn file_token_at(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(non_blank)
}

/// Masks `token` to its last 4 characters (`...abcd`) — never the full
/// value, regardless of input — wherever a token's existence must be shown
/// without showing the token itself (#583: "never printed... anywhere its
/// existence is shown"). A pure function so its edge cases (empty, shorter
/// than 4 characters) are covered directly rather than inferred from a
/// caller.
pub(crate) fn mask_token(token: &str) -> String {
    const VISIBLE: usize = 4;
    let chars: Vec<char> = token.chars().collect();
    if chars.is_empty() {
        return "<empty>".to_string();
    }
    if chars.len() <= VISIBLE {
        return "*".repeat(chars.len());
    }
    let tail: String = chars[chars.len() - VISIBLE..].iter().collect();
    format!("...{tail}")
}

/// One line, safe to print unconditionally, saying whether a token was
/// found and — if so — which tier answered, masked. This is the concrete
/// answer to #583's "the resolver says which source answered": a stale env
/// var shadowing a fresh keyring entry shows up here as `env var
/// GIT_VISTA_GITHUB_TOKEN (...wxyz)`, not silence.
pub(crate) fn provenance_line() -> String {
    match resolve_token() {
        Some((token, source)) => {
            format!(
                "git-vista: GitHub token: {} ({})",
                source.label(),
                mask_token(&token)
            )
        }
        None => "git-vista: GitHub token: none configured (only needed for private repositories)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- mask_token: pure, so every edge case gets its own direct assertion --

    #[test]
    fn mask_token_keeps_only_the_last_four_characters() {
        assert_eq!(mask_token("ghp_abcdefghijklmnopqrstuvwxyz"), "...wxyz");
    }

    #[test]
    fn mask_token_never_reveals_the_full_value() {
        let real = "ghp_abcdefghijklmnopqrstuvwxyz";
        let masked = mask_token(real);
        assert_ne!(masked, real);
        assert!(!masked.contains("abcdefghijklmnopqrstuv"));
    }

    #[test]
    fn mask_token_on_empty_input_does_not_panic_or_leak() {
        assert_eq!(mask_token(""), "<empty>");
    }

    #[test]
    fn mask_token_shorter_than_the_visible_window_is_fully_starred() {
        assert_eq!(mask_token("a"), "*");
        assert_eq!(mask_token("ab"), "**");
        assert_eq!(mask_token("abc"), "***");
    }

    #[test]
    fn mask_token_exactly_at_the_visible_window_is_fully_starred_not_shown() {
        // Exactly 4 characters is the boundary the issue calls out by name —
        // showing all 4 unmasked would defeat "masked to the last 4 characters"
        // for the shortest input where that phrase is even meaningful.
        assert_eq!(mask_token("abcd"), "****");
    }

    #[test]
    fn mask_token_one_past_the_window_shows_exactly_four() {
        assert_eq!(mask_token("abcde"), "...bcde");
    }

    // -- precedence: pure, dependency-injected, no real source touched --

    #[test]
    fn precedence_prefers_keyring_over_every_other_source() {
        let resolved = resolve_from(
            Some("from-keyring".to_string()),
            Some("from-env-gv".to_string()),
            Some("from-env-gh".to_string()),
            Some("from-file".to_string()),
        );
        assert_eq!(
            resolved,
            Some(("from-keyring".to_string(), TokenSource::Keyring))
        );
    }

    #[test]
    fn precedence_falls_back_to_git_vista_env_when_keyring_is_absent() {
        let resolved = resolve_from(
            None,
            Some("from-env-gv".to_string()),
            Some("from-env-gh".to_string()),
            Some("from-file".to_string()),
        );
        assert_eq!(
            resolved,
            Some(("from-env-gv".to_string(), TokenSource::EnvGitVista))
        );
    }

    #[test]
    fn precedence_falls_back_to_gh_env_when_keyring_and_git_vista_env_are_absent() {
        let resolved = resolve_from(
            None,
            None,
            Some("from-env-gh".to_string()),
            Some("from-file".to_string()),
        );
        assert_eq!(
            resolved,
            Some(("from-env-gh".to_string(), TokenSource::EnvGh))
        );
    }

    #[test]
    fn precedence_falls_back_to_file_when_every_other_source_is_absent() {
        let resolved = resolve_from(None, None, None, Some("from-file".to_string()));
        assert_eq!(resolved, Some(("from-file".to_string(), TokenSource::File)));
    }

    #[test]
    fn precedence_is_none_when_every_source_is_absent() {
        assert_eq!(resolve_from(None, None, None, None), None);
    }

    // -- env tier: blank values are absent, not "found but empty" --

    #[test]
    fn env_token_treats_a_blank_value_as_absent() {
        let guard = EnvGuard::set(GIT_VISTA_ENV, "   ");
        assert_eq!(env_token(GIT_VISTA_ENV), None);
        drop(guard);
    }

    #[test]
    fn env_token_trims_surrounding_whitespace_from_a_real_value() {
        let guard = EnvGuard::set(GIT_VISTA_ENV, "  a-real-token  ");
        assert_eq!(env_token(GIT_VISTA_ENV), Some("a-real-token".to_string()));
        drop(guard);
    }

    #[test]
    fn env_token_is_none_when_the_variable_is_unset() {
        let guard = EnvGuard::unset(GH_ENV);
        assert_eq!(env_token(GH_ENV), None);
        drop(guard);
    }

    /// Restores whatever an env var held before the test, on every exit path
    /// — the pattern `git-vista-session::auth`'s own env-var test uses, so a
    /// later env-reading test can't inherit a fake value by thread schedule.
    struct EnvGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let original = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, original }
        }

        fn unset(name: &'static str) -> Self {
            let original = std::env::var_os(name);
            std::env::remove_var(name);
            Self { name, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    // -- file tier: real filesystem, but an explicit tempdir path, never
    // `state::token_file_path()` (which the file-tier test would otherwise
    // race every other test in this crate that also resolves state_dir()) --

    #[test]
    fn file_token_reads_and_trims_the_file_at_the_given_path() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("github-token");
        std::fs::write(&path, "  a-file-token\n").expect("writing the fixture file");
        assert_eq!(file_token_at(&path), Some("a-file-token".to_string()));
    }

    #[test]
    fn file_token_is_none_when_the_file_does_not_exist() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("no-such-file");
        assert_eq!(file_token_at(&path), None);
    }

    #[test]
    fn file_token_is_none_when_the_file_is_blank() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("github-token");
        std::fs::write(&path, "\n\n").expect("writing the fixture file");
        assert_eq!(file_token_at(&path), None);
    }

    // -- the token never reaches a log line or an error body (#583 acceptance) --

    #[test]
    fn provenance_line_never_contains_the_resolved_token() {
        let guard = EnvGuard::set(GIT_VISTA_ENV, "super-secret-canary-value");
        let line = provenance_line();
        assert!(!line.contains("super-secret-canary-value"));
        assert!(line.contains("...alue"));
        drop(guard);
    }

    #[test]
    fn provenance_line_names_the_source_that_answered() {
        let guard = EnvGuard::set(GIT_VISTA_ENV, "some-token-value");
        assert!(provenance_line().contains(TokenSource::EnvGitVista.label()));
        drop(guard);
    }

    #[test]
    fn provenance_line_is_a_normal_sentence_when_nothing_is_configured() {
        // Best-effort: clears both env tiers so the assertion holds even when a
        // real keyring entry or tier-3 file happens to exist on the machine
        // running this test — a false failure here would be exactly the "absent
        // is an error" mistake #583 exists to prevent.
        let gv_guard = EnvGuard::unset(GIT_VISTA_ENV);
        let gh_guard = EnvGuard::unset(GH_ENV);
        if keyring_token().is_none() && file_token().is_none() {
            assert_eq!(
                provenance_line(),
                "git-vista: GitHub token: none configured (only needed for private repositories)"
            );
        }
        drop(gv_guard);
        drop(gh_guard);
    }
}
