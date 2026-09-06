//! GitHub token resolution (#583/#692, M13.02) — the fallback chain
//! `state::credential_token` (M13.01, #582) was scaffolding for: an OS
//! keyring first, two environment variables, then a gitignored local file.
//! See ADR 0126 and ADR 0132.
//!
//! Absence at every tier is the normal state: a public repository works
//! with no token at all, so nothing here treats "not found" as an error —
//! every source folds a real failure (no D-Bus session, no file, an unset
//! variable) into `None` rather than surfacing it.

use std::path::Path;
use std::sync::RwLock;
use std::time::Duration;

use git_vista_protocol::TokenStatus;

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

pub(crate) const GIT_VISTA_ENV: &str = "GIT_VISTA_GITHUB_TOKEN";
pub(crate) const GH_ENV: &str = "GH_TOKEN";

/// Ambient variables from which this process may resolve the GitHub token.
/// A child that receives the resolved credential through the dedicated
/// helper channel must not also inherit either source variable: doing so
/// would make removing the helper channel irrelevant for an env-backed
/// token.
pub(crate) const TOKEN_SOURCE_ENV_VARS: &[&str] = &[GIT_VISTA_ENV, GH_ENV];

/// Resolve the token by trying each source in the documented precedence
/// order, stopping at the first that has one. `None` means every tier came
/// up empty — the normal, unremarkable case for a public repository.
pub(crate) fn resolve_token() -> Option<(String, TokenSource)> {
    resolve_from(
        keyring_token,
        || env_token(GIT_VISTA_ENV),
        || env_token(GH_ENV),
        file_token,
    )
}

/// The lazy precedence engine, isolated from every real source so tests can
/// assert both the order and the evaluation boundary without touching the OS
/// keyring, environment, or disk (#583/#692). Taking source functions rather
/// than already-built `Option`s is the important part: once one returns a
/// value, no lower-priority function can run.
fn resolve_from<K, V, G, F>(
    keyring: K,
    env_git_vista: V,
    env_gh: G,
    file: F,
) -> Option<(String, TokenSource)>
where
    K: FnOnce() -> Option<String>,
    V: FnOnce() -> Option<String>,
    G: FnOnce() -> Option<String>,
    F: FnOnce() -> Option<String>,
{
    if let Some(token) = keyring() {
        return Some((token, TokenSource::Keyring));
    }
    if let Some(token) = env_git_vista() {
        return Some((token, TokenSource::EnvGitVista));
    }
    if let Some(token) = env_gh() {
        return Some((token, TokenSource::EnvGh));
    }
    file().map(|token| (token, TokenSource::File))
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
/// This is synchronous, potentially blocking platform I/O. Credentialed git
/// operations call it while resolving a token for their already-blocking
/// operation path. The settings request path does not: it uses
/// [`RequestTokenResolver`]'s startup snapshot, and a settings write runs its
/// one requested keyring call on Tokio's blocking pool (#692).
fn keyring_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME).ok()?;
    entry.get_password().ok().and_then(non_blank)
}

/// The startup keyring probe is allowed this long to answer. A healthy local
/// Secret Service normally answers immediately; a locked store may wait for an
/// unlock interaction that an autologin session cannot complete. Timing out
/// makes that tier unavailable to Settings until restart. Dropping Tokio's
/// `JoinHandle` cannot cancel a synchronous D-Bus call already in progress, so
/// one blocking task may remain, but repeated Settings opens never create more.
const SETTINGS_KEYRING_PROBE_BUDGET: Duration = Duration::from_secs(2);

enum KeyringProbe {
    Completed(Option<String>),
    TimedOut,
    WorkerFailed(String),
}

async fn probe_keyring_with<F>(read: F, budget: Duration) -> KeyringProbe
where
    F: FnOnce() -> Option<String> + Send + 'static,
{
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(read)).await {
        Ok(Ok(token)) => KeyringProbe::Completed(token),
        Ok(Err(error)) => KeyringProbe::WorkerFailed(error.to_string()),
        Err(_) => KeyringProbe::TimedOut,
    }
}

/// Request-facing status policy for the keyring tier (#692).
///
/// Only a masked status is retained. A GET can therefore report a keyring
/// value observed during the bounded startup probe without storing another
/// copy of the credential or touching D-Bus. If startup found no usable
/// keyring (including a timeout), GETs resolve the non-blocking environment
/// and file tiers lazily. A successful settings POST replaces the snapshot
/// from the submitted value after the blocking-pool write succeeds; it never
/// reads the keyring back.
pub(crate) struct RequestTokenResolver {
    keyring_status: RwLock<Option<TokenStatus>>,
}

impl RequestTokenResolver {
    fn from_startup_keyring(token: Option<&str>) -> Self {
        let keyring_status =
            token.map(|token| token_status_of(Some((token.to_string(), TokenSource::Keyring))));
        Self {
            keyring_status: RwLock::new(keyring_status),
        }
    }

    #[cfg(test)]
    pub(crate) fn without_keyring() -> Self {
        Self::from_startup_keyring(None)
    }

    /// Resolve status under the request policy. There is deliberately no
    /// keyring source function here: a request can only read the masked
    /// startup/post-write snapshot, so repeated opens cannot re-enter D-Bus.
    pub(crate) fn status(&self) -> TokenStatus {
        self.status_from(
            || env_token(GIT_VISTA_ENV),
            || env_token(GH_ENV),
            file_token,
        )
    }

    fn status_from<V, G, F>(&self, env_git_vista: V, env_gh: G, file: F) -> TokenStatus
    where
        V: FnOnce() -> Option<String>,
        G: FnOnce() -> Option<String>,
        F: FnOnce() -> Option<String>,
    {
        let keyring_status = self
            .keyring_status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(status) = keyring_status {
            return status;
        }

        token_status_of(resolve_from(|| None, env_git_vista, env_gh, file))
    }

    /// Record a keyring write only after [`store_token`] returned success.
    /// The same normalization as the writer is applied, and only the masked
    /// DTO is retained. Returning that DTO lets POST respond without a second
    /// keyring lookup.
    pub(crate) fn record_successful_store(&self, token: &str) -> TokenStatus {
        let token =
            non_blank(token.to_string()).expect("store_token cannot succeed for a blank token");
        let status = token_status_of(Some((token, TokenSource::Keyring)));
        *self
            .keyring_status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(status.clone());
        status
    }
}

/// Startup result consumed by `main`: the request-only resolver, the ordinary
/// masked provenance line, and an optional diagnostic explaining why the
/// keyring is being treated as unavailable for Settings until restart.
pub(crate) struct StartupTokenPolicy {
    pub(crate) request_resolver: RequestTokenResolver,
    pub(crate) provenance_line: String,
    pub(crate) warning: Option<String>,
}

/// Probe Secret Service once, off the async worker and under a fixed budget,
/// then freeze only its masked request-facing status. Clone/push resolution
/// remains the ordinary live [`resolve_token`] path.
pub(crate) async fn initialize_request_token_policy() -> StartupTokenPolicy {
    let probe = probe_keyring_with(keyring_token, SETTINGS_KEYRING_PROBE_BUDGET).await;
    let (keyring, warning) = match probe {
        KeyringProbe::Completed(token) => (token, None),
        KeyringProbe::TimedOut => (
            None,
            Some(format!(
                "OS keyring did not answer within {}s; treating it as unavailable for Settings until restart",
                SETTINGS_KEYRING_PROBE_BUDGET.as_secs()
            )),
        ),
        KeyringProbe::WorkerFailed(reason) => (
            None,
            Some(format!(
                "OS keyring startup worker failed ({reason}); treating it as unavailable for Settings until restart"
            )),
        ),
    };

    let request_resolver = RequestTokenResolver::from_startup_keyring(keyring.as_deref());
    let resolved = resolve_from(
        || keyring,
        || env_token(GIT_VISTA_ENV),
        || env_token(GH_ENV),
        file_token,
    );
    let provenance_line = provenance_line_of(resolved.as_ref());

    StartupTokenPolicy {
        request_resolver,
        provenance_line,
        warning,
    }
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

/// Why a save attempt did not persist. Every variant's `Display` is safe to
/// return to the client verbatim: neither carries the submitted value —
/// [`Blank`](StoreTokenError::Blank) names no value at all, and
/// [`Keyring`](StoreTokenError::Keyring)'s string comes from the `keyring`
/// crate's own error text (backend/platform diagnostics such as "no storage
/// access"), never from anything this process wrote into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreTokenError {
    /// A blank or whitespace-only value — the same rule [`non_blank`] applies
    /// to every read tier, applied here to what would otherwise be written.
    Blank,
    /// The OS keyring rejected the write (no D-Bus session, no storage
    /// access, a locked store, …). Unlike a *read* failure, this must not
    /// collapse to a quiet `None` — the user asked for something to happen
    /// and it did not, so the reason is reported rather than swallowed.
    Keyring(String),
}

/// Save `token` to the OS keyring — the highest tier in
/// [`resolve_token`]'s precedence, so a value saved here is what every
/// subsequent resolution finds first (M13.03, #584).
///
/// Deliberately the ONLY tier a settings surface can write to. The
/// environment-variable tiers are read from whatever launched this process
/// and are not this program's to rewrite; the tier-3 file exists for a
/// human to place a token by hand on a machine with no keyring (its own doc,
/// [`crate::state::token_file_path`], says as much) rather than for the app
/// to write plaintext to disk on the user's behalf when a secure store is
/// sitting right there. A settings surface that could also fall back to
/// writing the plaintext file would make "did this persist securely"
/// depend on which machine it ran on, silently — see this module's own
/// header on why absence folds to `None` at every READ tier; a WRITE has no
/// equivalent safe default to fold into.
pub(crate) fn store_token(token: &str) -> Result<(), StoreTokenError> {
    let trimmed = non_blank(token.to_string()).ok_or(StoreTokenError::Blank)?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
        .map_err(|e| StoreTokenError::Keyring(e.to_string()))?;
    entry
        .set_password(&trimmed)
        .map_err(|e| StoreTokenError::Keyring(e.to_string()))
}

/// Pure status constructor, dependency-injected so the property that matters
/// most is host-tested directly rather than only reasoned about:
/// **the resolved token itself never reaches the returned value.** `masked`
/// is built from [`mask_token`] and `source` from [`TokenSource::label`] —
/// there is no path from `resolved`'s `String` into this function's return
/// value at all, which is the wire-level guarantee #584's acceptance
/// criterion asks for, proved at the type that IS the wire (see
/// `settings_suite`'s HTTP-level test for the other half: that the real
/// handler actually calls this and nothing else).
pub(crate) fn token_status_of(resolved: Option<(String, TokenSource)>) -> TokenStatus {
    match resolved {
        Some((token, source)) => TokenStatus {
            configured: true,
            masked: Some(mask_token(&token)),
            source: Some(source.label().to_string()),
        },
        None => TokenStatus {
            configured: false,
            masked: None,
            source: None,
        },
    }
}

/// One line, safe to print unconditionally, saying whether a token was
/// found and — if so — which tier answered, masked. This is the concrete
/// answer to #583's "the resolver says which source answered": a stale env
/// var shadowing a fresh keyring entry shows up here as `env var
/// GIT_VISTA_GITHUB_TOKEN (...wxyz)`, not silence.
#[cfg(test)]
pub(crate) fn provenance_line() -> String {
    let resolved = resolve_token();
    provenance_line_of(resolved.as_ref())
}

fn provenance_line_of(resolved: Option<&(String, TokenSource)>) -> String {
    match resolved {
        Some((token, source)) => {
            format!(
                "git-vista: GitHub token: {} ({})",
                source.label(),
                mask_token(token)
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
        // Build the fixture at runtime so the tracked-file credential
        // tripwire never encounters a complete token-shaped literal.
        let token = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz");
        assert_eq!(mask_token(&token), "...wxyz");
    }

    #[test]
    fn mask_token_never_reveals_the_full_value() {
        let real = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz");
        let masked = mask_token(&real);
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

    // -- token_status_of: pure, dependency-injected, the settings surface's
    // read side (M13.03, #584) --

    #[test]
    fn token_status_of_absent_reports_unconfigured_with_nothing_else_set() {
        assert_eq!(
            token_status_of(None),
            TokenStatus {
                configured: false,
                masked: None,
                source: None,
            }
        );
    }

    #[test]
    fn token_status_of_present_names_the_source_and_masks_the_value() {
        let real = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz");
        let status = token_status_of(Some((real.clone(), TokenSource::Keyring)));
        assert!(status.configured);
        assert_eq!(status.source.as_deref(), Some(TokenSource::Keyring.label()));
        assert_eq!(status.masked.as_deref(), Some("...wxyz"));
        // The property #584's acceptance actually asks for, checked directly
        // rather than inferred from the struct's field list: the real value
        // is not merely absent from a specific field, it is absent from the
        // ENTIRE serialized value — the same wire bytes a client receives.
        let wire = serde_json::to_string(&status).expect("TokenStatus serializes");
        assert!(
            !wire.contains(&real),
            "the resolved token leaked into the wire representation: {wire}"
        );
    }

    #[test]
    fn token_status_of_never_lets_the_value_reach_the_wire_whichever_tier_answered() {
        // Every tier, not just keyring — a mutation that special-cased one
        // arm of `resolve_from` to leak would only be caught by exercising
        // all four.
        let real = "super-secret-canary-0123456789abcdef";
        for source in [
            TokenSource::Keyring,
            TokenSource::EnvGitVista,
            TokenSource::EnvGh,
            TokenSource::File,
        ] {
            let status = token_status_of(Some((real.to_string(), source)));
            let wire = serde_json::to_string(&status).expect("TokenStatus serializes");
            assert!(
                !wire.contains(real),
                "{source:?} tier leaked the token into the wire representation: {wire}"
            );
        }
    }

    // -- store_token: the write side (M13.03, #584) --

    #[test]
    fn store_token_rejects_a_blank_value_before_touching_the_keyring() {
        assert_eq!(store_token(""), Err(StoreTokenError::Blank));
        assert_eq!(store_token("   "), Err(StoreTokenError::Blank));
    }

    #[test]
    fn store_token_error_messages_never_contain_the_attempted_value() {
        // The keyring backend in this sandbox has no storage access, so this
        // exercises the real failure path store_token must report honestly
        // (unlike a read, a write failure is not folded to a quiet `None`).
        // Built at runtime, like every other token-shaped fixture in this
        // file (see `mask_token_keeps_only_the_last_four_characters` above)
        // — a literal here would be a real-length `ghp_` token shape in
        // tracked source, which the credential tripwire (#586, ADR 0123)
        // and gitleaks' own full-history scan both exist to catch, even
        // though the value itself grants access to nothing.
        let attempted = format!("ghp_{}", "wouldbeasecretifthisreallysaved0123456789");
        if let Err(e) = store_token(&attempted) {
            let message = match &e {
                StoreTokenError::Blank => String::new(),
                StoreTokenError::Keyring(reason) => reason.clone(),
            };
            assert!(
                !message.contains(&attempted),
                "a keyring failure message echoed the attempted token: {message}"
            );
        }
    }

    #[test]
    #[ignore = "needs a live OS keyring / D-Bus secret service; run manually with \
                `cargo test -p git-vista-server --bins token_store::tests::store_token_round_trip_against_a_real_backend -- --ignored`"]
    fn store_token_round_trip_against_a_real_backend() {
        // A throwaway service/username, never KEYRING_SERVICE/KEYRING_USERNAME
        // — this must not read, overwrite, or delete a real token a
        // developer has already stored. store_token itself always targets
        // the production entry, so this proves the WRITE mechanism (the
        // `keyring` crate's `set_password`) against a scratch entry rather
        // than calling `store_token` directly.
        let entry = keyring::Entry::new("git-vista-test", "token-store-write-round-trip")
            .expect("a live keyring backend for this manual test");
        entry
            .set_password("write-round-trip-canary")
            .expect("writing the test entry");
        let read_back = entry.get_password().expect("reading the test entry back");
        assert_eq!(read_back, "write-round-trip-canary");
        entry
            .delete_credential()
            .expect("cleaning up the test entry");
    }

    // -- keyring tier: the real glue, exercised directly --

    #[test]
    fn keyring_token_never_panics_regardless_of_platform_availability() {
        // Whether or not this machine has a working D-Bus secret service,
        // the call must fold every failure into `None` rather than panic —
        // keyring absence must never surface as a server error (#583).
        let _ = keyring_token();
    }

    #[test]
    #[ignore = "needs a live OS keyring / D-Bus secret service; run manually with \
                `cargo test -p git-vista-server --bins token_store::tests::keyring_round_trip_against_a_real_backend -- --ignored`"]
    fn keyring_round_trip_against_a_real_backend() {
        // A throwaway service/username, never the production
        // KEYRING_SERVICE/KEYRING_USERNAME — this must not read, overwrite,
        // or delete a real token a developer has already stored.
        let entry = keyring::Entry::new("git-vista-test", "token-store-round-trip")
            .expect("a live keyring backend for this manual test");
        entry
            .set_password("round-trip-canary")
            .expect("writing the test entry");
        let read_back = entry.get_password().expect("reading the test entry back");
        assert_eq!(read_back, "round-trip-canary");
        entry
            .delete_credential()
            .expect("cleaning up the test entry");
    }

    // -- precedence: pure, dependency-injected, no real source touched --

    #[test]
    fn precedence_prefers_keyring_over_every_other_source() {
        let resolved = resolve_from(
            || Some("from-keyring".to_string()),
            || Some("from-env-gv".to_string()),
            || Some("from-env-gh".to_string()),
            || Some("from-file".to_string()),
        );
        assert_eq!(
            resolved,
            Some(("from-keyring".to_string(), TokenSource::Keyring))
        );
    }

    #[test]
    fn precedence_falls_back_to_git_vista_env_when_keyring_is_absent() {
        let resolved = resolve_from(
            || None,
            || Some("from-env-gv".to_string()),
            || Some("from-env-gh".to_string()),
            || Some("from-file".to_string()),
        );
        assert_eq!(
            resolved,
            Some(("from-env-gv".to_string(), TokenSource::EnvGitVista))
        );
    }

    #[test]
    fn precedence_falls_back_to_gh_env_when_keyring_and_git_vista_env_are_absent() {
        let resolved = resolve_from(
            || None,
            || None,
            || Some("from-env-gh".to_string()),
            || Some("from-file".to_string()),
        );
        assert_eq!(
            resolved,
            Some(("from-env-gh".to_string(), TokenSource::EnvGh))
        );
    }

    #[test]
    fn precedence_falls_back_to_file_when_every_other_source_is_absent() {
        let resolved = resolve_from(|| None, || None, || None, || Some("from-file".to_string()));
        assert_eq!(resolved, Some(("from-file".to_string(), TokenSource::File)));
    }

    #[test]
    fn precedence_is_none_when_every_source_is_absent() {
        assert_eq!(resolve_from(|| None, || None, || None, || None), None);
    }

    fn counted_source<'a>(
        calls: &'a std::cell::Cell<usize>,
        value: Option<&'static str>,
    ) -> impl FnOnce() -> Option<String> + 'a {
        move || {
            calls.set(calls.get() + 1);
            value.map(str::to_string)
        }
    }

    fn assert_lazy_resolution(
        values: [Option<&'static str>; 4],
        expected_calls: [usize; 4],
        expected_source: Option<TokenSource>,
    ) {
        let calls: [std::cell::Cell<usize>; 4] = std::array::from_fn(|_| 0.into());
        let resolved = resolve_from(
            counted_source(&calls[0], values[0]),
            counted_source(&calls[1], values[1]),
            counted_source(&calls[2], values[2]),
            counted_source(&calls[3], values[3]),
        );

        assert_eq!(
            calls.each_ref().map(std::cell::Cell::get),
            expected_calls,
            "a source below the winning tier was evaluated"
        );
        assert_eq!(resolved.map(|(_, source)| source), expected_source);
    }

    #[test]
    fn resolution_evaluates_source_functions_only_through_the_winning_tier() {
        assert_lazy_resolution(
            [Some("keyring"), Some("gv"), Some("gh"), Some("file")],
            [1, 0, 0, 0],
            Some(TokenSource::Keyring),
        );
        assert_lazy_resolution(
            [None, Some("gv"), Some("gh"), Some("file")],
            [1, 1, 0, 0],
            Some(TokenSource::EnvGitVista),
        );
        assert_lazy_resolution(
            [None, None, Some("gh"), Some("file")],
            [1, 1, 1, 0],
            Some(TokenSource::EnvGh),
        );
        assert_lazy_resolution(
            [None, None, None, Some("file")],
            [1, 1, 1, 1],
            Some(TokenSource::File),
        );
    }

    #[test]
    fn request_status_uses_the_masked_keyring_snapshot_without_lower_reads() {
        let resolver = RequestTokenResolver::from_startup_keyring(Some("keyring-secret-tail"));
        let lower_calls = std::cell::Cell::new(0);
        let status = resolver.status_from(
            || {
                lower_calls.set(lower_calls.get() + 1);
                None
            },
            || {
                lower_calls.set(lower_calls.get() + 1);
                None
            },
            || {
                lower_calls.set(lower_calls.get() + 1);
                None
            },
        );

        assert_eq!(lower_calls.get(), 0);
        assert_eq!(status.source.as_deref(), Some(TokenSource::Keyring.label()));
        assert_eq!(status.masked.as_deref(), Some("...tail"));
    }

    #[tokio::test]
    async fn repeated_request_status_calls_never_repeat_the_startup_keyring_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let keyring_calls = Arc::new(AtomicUsize::new(0));
        let calls_in_probe = keyring_calls.clone();
        let probe = probe_keyring_with(
            move || {
                calls_in_probe.fetch_add(1, Ordering::SeqCst);
                None
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(probe, KeyringProbe::Completed(None)));

        let resolver = RequestTokenResolver::without_keyring();
        for _ in 0..3 {
            let status = resolver.status_from(|| None, || None, || None);
            assert!(!status.configured);
        }
        assert_eq!(keyring_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stuck_startup_keyring_probe_times_out() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let probe = probe_keyring_with(
            move || {
                let _ = release_rx.recv();
                None
            },
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(probe, KeyringProbe::TimedOut));
        let _ = release_tx.send(());
    }

    #[test]
    fn a_successful_store_updates_status_without_a_keyring_read_back() {
        let resolver = RequestTokenResolver::without_keyring();
        let status = resolver.record_successful_store("  newly-saved-tail  ");
        assert!(status.configured);
        assert_eq!(status.source.as_deref(), Some(TokenSource::Keyring.label()));
        assert_eq!(status.masked.as_deref(), Some("...tail"));
        assert_eq!(resolver.status(), status);
    }

    // -- env tier: blank values are absent, not "found but empty" --

    /// Every test in this file that mutates `GIT_VISTA_GITHUB_TOKEN` or
    /// `GH_TOKEN` must hold this for its whole body. `std::env` is process-
    /// wide, `cargo test` runs on multiple threads by default, and these
    /// tests genuinely race each other without it — caught the hard way:
    /// `mutation_check`'s baseline leg (no `--test-threads=1`, same as a
    /// real CI run) went red on unrelated env-tier tests with no mutation
    /// applied at all. `git-vista-session::auth`'s equivalent test dodges
    /// this by folding two cases into one `#[test]`; a lock is used here
    /// instead so each case keeps its own name and failure message.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn env_token_treats_a_blank_value_as_absent() {
        let _lock = lock_env();
        let guard = EnvGuard::set(GIT_VISTA_ENV, "   ");
        assert_eq!(env_token(GIT_VISTA_ENV), None);
        drop(guard);
    }

    #[test]
    fn env_token_trims_surrounding_whitespace_from_a_real_value() {
        let _lock = lock_env();
        let guard = EnvGuard::set(GIT_VISTA_ENV, "  a-real-token  ");
        assert_eq!(env_token(GIT_VISTA_ENV), Some("a-real-token".to_string()));
        drop(guard);
    }

    #[test]
    fn env_token_is_none_when_the_variable_is_unset() {
        let _lock = lock_env();
        let guard = EnvGuard::unset(GH_ENV);
        assert_eq!(env_token(GH_ENV), None);
        drop(guard);
    }

    /// Restores whatever an env var held before the test, on every exit path
    /// — the pattern `git-vista-session::auth`'s own env-var test uses, so a
    /// later env-reading test can't inherit a fake value by thread schedule.
    /// Callers must hold [`lock_env`] for the guard's whole lifetime — this
    /// type has no lock of its own (two guards in one test would deadlock a
    /// non-reentrant `Mutex` against itself).
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
        let _lock = lock_env();
        let guard = EnvGuard::set(GIT_VISTA_ENV, "super-secret-canary-value");
        let line = provenance_line();
        assert!(!line.contains("super-secret-canary-value"));
        assert!(line.contains("...alue"));
        drop(guard);
    }

    #[test]
    fn provenance_line_names_the_source_that_answered() {
        let _lock = lock_env();
        let guard = EnvGuard::set(GIT_VISTA_ENV, "some-token-value");
        assert!(provenance_line().contains(TokenSource::EnvGitVista.label()));
        drop(guard);
    }

    #[test]
    fn provenance_line_is_a_normal_sentence_when_nothing_is_configured() {
        let _lock = lock_env();
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
