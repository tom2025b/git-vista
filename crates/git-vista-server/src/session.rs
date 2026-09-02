//! Loopback session bootstrap, session store, and CSRF tokens (M1.04, #57).
//!
//! Binding to loopback is necessary but not sufficient: a malicious webpage in the
//! same browser — or a DNS-rebinding attack — can still reach `localhost:8080`, and
//! over an SSH tunnel *every* request arrives as loopback, so "is this loopback?"
//! is not identity. This module is the identity layer the
//! [`SECURITY_MODEL`](../../../docs/SECURITY_MODEL.md) "Local and SSH Session
//! Design" section prescribes:
//!
//!   * A high-entropy **bootstrap token** is minted at startup and written to a
//!     `0600` file (never a URL the server sees, never a log). The `gv` launcher —
//!     running as the same user, so the only one that can read that file — hands
//!     the operator a `http://localhost:8080/#s=<token>` URL. The token rides in
//!     the URL *fragment*, which the browser never sends to the server.
//!   * The SPA exchanges that token (once) for an **HttpOnly, `SameSite=Strict`
//!     session cookie** via `POST /api/session`. The token is **single-use**
//!     (exchanging it rotates in a fresh one) and **expires**, so a leaked or
//!     shoulder-surfed token is worthless.
//!   * Each session carries a **CSRF token** the SPA echoes in a request header on
//!     every state-changing call — defence in depth behind `SameSite=Strict`.
//!   * Sessions are **revocable** (`DELETE /api/session`) and expire when idle.
//!
//! The store is deliberately in-memory: a restart mints a new bootstrap token and
//! drops every session, which is exactly the "new secret at every service start"
//! the security model asks for. Nothing here is persisted across runs.
//!
//! This module owns only the *state and the rules*; the wire enforcement (Origin /
//! Host / content-type / method / cookie / CSRF) lives in [`crate::security`], and
//! the three endpoints in [`crate::handlers::session`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The session cookie name. HttpOnly + `SameSite=Strict` (set in [`crate::handlers::session`]).
pub(crate) const SESSION_COOKIE: &str = "gv_session";

/// The header the SPA echoes the session's CSRF token in on every state-changing
/// request. Defined once in the shared transport contract so the server's check
/// and the frontend's send can never drift; re-exported here for the auth layer.
pub(crate) const CSRF_HEADER: &str = git_vista_protocol::CSRF_HEADER;

/// How long an unused bootstrap token stays valid. Generous — the operator opens
/// the `gv`-printed URL by hand — but finite, so a token left in scrollback is not
/// forever live. A server restart mints a fresh one regardless.
const BOOTSTRAP_TTL: Duration = Duration::from_secs(60 * 60);

/// Rotate the bootstrap token before it enters its final validity window. The
/// old token still expires within the ADR's one-hour limit, while the token file
/// read by `gv --token` always points at a link with ample time left to open.
const BOOTSTRAP_REFRESH_WINDOW: Duration = Duration::from_secs(15 * 60);

/// How often the server checks whether the bootstrap token needs refreshing.
pub(crate) const BOOTSTRAP_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Idle lifetime of a session in seconds, refreshed on every validated request. A
/// tab left open keeps working; one abandoned for this long must re-bootstrap.
/// Also the cookie's `Max-Age`, so the browser drops the cookie in step with the
/// server dropping the session.
///
/// Sized to a working day (#369): at 12 h, closing the tab in the evening and
/// returning the next morning always landed past the idle window, so daily local
/// use meant re-running `gv --token` every morning. 16 h spans a day's work plus
/// the overnight gap while still expiring an abandoned session within the day.
/// This governs only how long an *already-authenticated* session survives idle —
/// the bootstrap token's own [`BOOTSTRAP_TTL`], and every wire check in
/// [`crate::security`] (Host, Origin, `SameSite=Strict`, CSRF), are unchanged.
pub(crate) const SESSION_MAX_AGE_SECS: u64 = 16 * 60 * 60;

/// [`SESSION_MAX_AGE_SECS`] as a [`Duration`], for the in-memory idle deadline.
const SESSION_IDLE_TTL: Duration = Duration::from_secs(SESSION_MAX_AGE_SECS);

/// Bytes of entropy behind every secret (token, session id, CSRF token): 256 bits
/// from the OS CSPRNG, hex-encoded to 64 ASCII chars.
const SECRET_BYTES: usize = 32;

/// 256 fresh bits straight from the OS CSPRNG — the single entropy tap behind
/// every secret this server holds: the session/bootstrap/CSRF secrets below
/// (hex-encoded via [`mint_secret`]) and the paged-history cursor-signing key
/// ([`crate::history`], M1.10 #63), which wants the raw bytes. Panics only if
/// the OS entropy source fails, which on a running server is unrecoverable.
pub(crate) fn random_secret_bytes() -> [u8; 32] {
    let mut bytes = [0_u8; SECRET_BYTES];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    bytes
}

/// A fresh, high-entropy secret as lowercase hex. The token text format is
/// unchanged: [`SECRET_BYTES`] = 32 bytes, 64 lowercase hex characters.
fn mint_secret() -> String {
    to_hex(&random_secret_bytes())
}

/// Lowercase-hex encode — a handful of bytes, so a tiny inline encoder beats a
/// dependency. Matches the request-id crate's "no crate for a trivial format".
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Constant-time byte-slice equality: the compare never short-circuits on the
/// first differing byte, so it leaks no timing signal about how much of a guessed
/// token was right. (Length still differs early — secrets here are fixed-length.)
/// Shared with [`crate::security`] for the CSRF-header compare.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The single outstanding bootstrap token and when it stops being valid.
struct Bootstrap {
    token: String,
    expires_at: Instant,
}

/// One live session: the CSRF token the client must echo, and the idle deadline
/// (refreshed on use). The session id itself is the map key in [`SessionManager`].
struct Session {
    csrf: String,
    expires_at: Instant,
    /// This session's selected repository (#588).
    ///
    /// Owned here, so its lifetime is the session's: [`SessionManager::revoke`]
    /// removes the record and the selection goes with it, which is why signing
    /// out cannot leave a repository behind for the next person. Nothing else
    /// holds a strong reference except the task currently serving a request for
    /// this session.
    selection: crate::state::SelectionCell,
}

/// What a successful bootstrap exchange hands back: the new session id (destined
/// for the HttpOnly cookie) and its CSRF token (handed to the SPA to echo).
pub(crate) struct NewSession {
    pub id: String,
    pub csrf: String,
}

/// The process-wide session state: the current bootstrap token and the live
/// sessions. Held behind an `Arc` and shared with the auth layer and the session
/// handlers (not a global `OnceLock`, so tests construct isolated instances).
pub(crate) struct SessionManager {
    bootstrap: Mutex<Bootstrap>,
    sessions: Mutex<HashMap<String, Session>>,
    /// Where the current bootstrap token is written `0600` for `gv` to read.
    /// `None` in tests, which never touch the disk.
    token_file: Option<PathBuf>,
}

impl SessionManager {
    /// Mint the startup bootstrap token and, when `token_file` is set, write it
    /// `0600` so only this user's `gv` can read it. A write failure is reported but
    /// not fatal: the server still runs, and `gv --token` will say the file is
    /// missing rather than the server silently refusing every request.
    pub(crate) fn new(token_file: Option<PathBuf>) -> Self {
        let token = mint_secret();
        if let Some(path) = &token_file {
            if let Err(e) = write_token_file(path, &token) {
                eprintln!(
                    "git-vista: couldn't write the bootstrap token to {}: {e}\n         \
                     the SPA can't authenticate until this is writable.",
                    path.display()
                );
            }
        }
        Self {
            bootstrap: Mutex::new(Bootstrap {
                token,
                expires_at: Instant::now() + BOOTSTRAP_TTL,
            }),
            sessions: Mutex::new(HashMap::new()),
            token_file,
        }
    }

    /// Exchange a bootstrap token for a new session, or `None` if the token is
    /// wrong or expired. On success the token is **single-use**: a fresh one is
    /// minted in its place (and rewritten to the `0600` file), so the just-used
    /// token can never be replayed while a second device can still bootstrap.
    pub(crate) fn exchange(&self, candidate: &str) -> Option<NewSession> {
        {
            let mut boot = self.bootstrap.lock().expect("bootstrap lock");
            let valid = Instant::now() < boot.expires_at
                && ct_eq(candidate.as_bytes(), boot.token.as_bytes());
            if !valid {
                return None;
            }
            // Single-use: rotate the token the moment it's spent.
            self.rotate_bootstrap(&mut boot);
        }
        let id = mint_secret();
        let csrf = mint_secret();
        self.sessions.lock().expect("sessions lock").insert(
            id.clone(),
            Session {
                csrf: csrf.clone(),
                expires_at: Instant::now() + SESSION_IDLE_TTL,
                // Empty: a new session has chosen nothing, so it resolves to the
                // launch repository rather than to the previous session's pick.
                selection: crate::state::new_selection_cell(),
            },
        );
        Some(NewSession { id, csrf })
    }

    /// Refresh a token approaching expiry so `gv --token` never advertises an
    /// already-dead link on a long-running server. Returns whether it rotated;
    /// callers need no token value and must never log one.
    pub(crate) fn refresh_bootstrap_if_expiring(&self) -> bool {
        let mut boot = self.bootstrap.lock().expect("bootstrap lock");
        let remaining = boot.expires_at.saturating_duration_since(Instant::now());
        if remaining > BOOTSTRAP_REFRESH_WINDOW {
            return false;
        }
        self.rotate_bootstrap(&mut boot);
        true
    }

    fn rotate_bootstrap(&self, boot: &mut Bootstrap) {
        boot.token = mint_secret();
        boot.expires_at = Instant::now() + BOOTSTRAP_TTL;
        if let Some(path) = &self.token_file {
            if let Err(e) = write_token_file(path, &boot.token) {
                eprintln!(
                    "git-vista: couldn't rotate the bootstrap token file {}: {e}",
                    path.display()
                );
            }
        }
    }

    /// Validate a session id (the cookie value), returning its CSRF token when the
    /// session is live. Refreshes the idle deadline as a side effect, and drops an
    /// expired session so a stale cookie is treated as no session at all.
    pub(crate) fn validate(&self, id: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().expect("sessions lock");
        let now = Instant::now();
        match sessions.get_mut(id) {
            Some(session) if now < session.expires_at => {
                session.expires_at = now + SESSION_IDLE_TTL;
                Some(session.csrf.clone())
            }
            Some(_) => {
                sessions.remove(id);
                None
            }
            None => None,
        }
    }

    /// This session's selection cell, when the session is live.
    ///
    /// Deliberately separate from [`validate`]: `validate` answers "may this
    /// request proceed", and the answer must not change shape because a caller
    /// also wants the selection. Does not refresh the idle deadline — the
    /// `validate` call in the same request already did.
    pub(crate) fn selection_cell(&self, id: &str) -> Option<crate::state::SelectionCell> {
        let sessions = self.sessions.lock().expect("sessions lock");
        let session = sessions.get(id)?;
        (Instant::now() < session.expires_at).then(|| std::sync::Arc::clone(&session.selection))
    }

    /// Revoke a session by id (logout). Returns whether a session was actually
    /// removed, so the handler can 404 an already-gone session honestly.
    pub(crate) fn revoke(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .expect("sessions lock")
            .remove(id)
            .is_some()
    }

    /// The current bootstrap token — the URL `gv` prints and the file it writes
    /// hold this. Used only to reprint on demand; requests never read it.
    #[cfg(test)]
    pub(crate) fn current_bootstrap(&self) -> String {
        self.bootstrap.lock().expect("bootstrap lock").token.clone()
    }
}

/// Write `token` to `path`, creating the parent directory, with `0600` permissions
/// on Unix so only the owner can read the secret. The whole file is the token plus
/// a trailing newline (so `gv` can read it with a plain `read`).
fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_token_file_inner(path, token)
}

#[cfg(unix)]
fn write_token_file_inner(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Publish through a fresh file in the same directory. Readers then observe
    // either the complete old token or the complete new one — never the empty or
    // partial contents exposed by truncating the live file during rotation.
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "bootstrap token path has no file name",
        )
    })?;

    // create_new plus the process/counter suffix avoids two server generations
    // sharing a temporary file. The final rename is atomic because both paths
    // live in the same directory.
    for _ in 0..32 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.tmp.{}.{}",
            file_name.to_string_lossy(),
            std::process::id(),
            id
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let write_result = writeln!(file, "{token}").and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        return Ok(());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "couldn't allocate a unique bootstrap-token temporary file",
    ))
}

#[cfg(not(unix))]
fn write_token_file_inner(path: &Path, token: &str) -> std::io::Result<()> {
    // Best effort on non-Unix: no 0600 equivalent here, but the file still lives
    // under the user's own state dir. The primary (Linux/macOS) targets take the
    // Unix path above.
    std::fs::write(path, format!("{token}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> SessionManager {
        SessionManager::new(None)
    }

    #[test]
    fn hex_encodes_bytes_lowercase_and_fixed_width() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(mint_secret().len(), SECRET_BYTES * 2);
    }

    #[test]
    fn constant_time_compare_matches_only_on_equal_bytes() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn a_valid_bootstrap_token_exchanges_for_a_session() {
        let m = manager();
        let token = m.current_bootstrap();
        let session = m.exchange(&token).expect("valid token should exchange");
        // The new session validates and yields the CSRF token it was minted with.
        assert_eq!(
            m.validate(&session.id).as_deref(),
            Some(session.csrf.as_str())
        );
    }

    #[test]
    fn a_bootstrap_token_is_single_use_and_rotates() {
        let m = manager();
        let token = m.current_bootstrap();
        assert!(m.exchange(&token).is_some());
        // The same token can't be spent twice…
        assert!(m.exchange(&token).is_none());
        // …but the rotated-in token still works, so a second device can bootstrap.
        let fresh = m.current_bootstrap();
        assert_ne!(fresh, token);
        assert!(m.exchange(&fresh).is_some());
    }

    #[test]
    fn a_wrong_bootstrap_token_is_refused() {
        let m = manager();
        assert!(m.exchange("not-the-token").is_none());
    }

    #[test]
    fn an_expired_bootstrap_token_is_refused() {
        let m = manager();
        let token = m.current_bootstrap();
        // Force expiry by winding the deadline into the past.
        m.bootstrap.lock().unwrap().expires_at = Instant::now() - Duration::from_secs(1);
        assert!(m.exchange(&token).is_none());
    }

    #[test]
    fn an_expiring_bootstrap_token_rotates_before_it_dies() {
        let m = manager();
        let old = m.current_bootstrap();
        m.bootstrap.lock().unwrap().expires_at = Instant::now() + BOOTSTRAP_REFRESH_WINDOW;

        assert!(m.refresh_bootstrap_if_expiring());
        let fresh = m.current_bootstrap();
        assert_ne!(fresh, old);
        assert!(m.exchange(&old).is_none());
        assert!(m.exchange(&fresh).is_some());
    }

    #[test]
    fn a_fresh_bootstrap_token_is_not_rotated_early() {
        let m = manager();
        let token = m.current_bootstrap();
        assert!(!m.refresh_bootstrap_if_expiring());
        assert_eq!(m.current_bootstrap(), token);
    }

    #[test]
    fn a_session_carries_a_csrf_token_matched_constant_time() {
        let m = manager();
        let token = m.current_bootstrap();
        let s = m.exchange(&token).unwrap();
        // This mirrors the auth layer's check: validate the session id, then
        // constant-time compare the presented CSRF against the stored one.
        let expected = m.validate(&s.id).expect("session should be live");
        assert!(ct_eq(expected.as_bytes(), s.csrf.as_bytes()));
        assert!(!ct_eq(expected.as_bytes(), b"wrong-csrf"));
        assert!(m.validate("wrong-session").is_none());
    }

    #[test]
    fn a_revoked_session_stops_validating() {
        let m = manager();
        let token = m.current_bootstrap();
        let s = m.exchange(&token).unwrap();
        assert!(m.revoke(&s.id));
        assert!(m.validate(&s.id).is_none());
        // Revoking an already-gone session reports no-op.
        assert!(!m.revoke(&s.id));
    }

    #[test]
    fn an_idle_expired_session_is_dropped() {
        let m = manager();
        let token = m.current_bootstrap();
        let s = m.exchange(&token).unwrap();
        m.sessions
            .lock()
            .unwrap()
            .get_mut(&s.id)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);
        assert!(m.validate(&s.id).is_none());
        // And it was actually removed, not just reported invalid.
        assert!(m.sessions.lock().unwrap().get(&s.id).is_none());
    }

    #[test]
    fn the_token_file_is_written_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("bootstrap.token");
        let m = SessionManager::new(Some(path.clone()));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.trim(), m.current_bootstrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        }
    }

    #[cfg(unix)]
    #[test]
    fn token_rotation_atomically_replaces_the_published_file() {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap.token");
        write_token_file(&path, "old-token").unwrap();

        // A handle opened before an atomic rename keeps seeing the complete old
        // inode. Truncate-in-place would instead change this handle's contents.
        let mut old_handle = std::fs::File::open(&path).unwrap();
        let old_inode = old_handle.metadata().unwrap().ino();

        write_token_file(&path, "new-token").unwrap();

        let new_metadata = std::fs::metadata(&path).unwrap();
        assert_ne!(old_inode, new_metadata.ino());
        assert_eq!(new_metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new-token\n");

        let mut old_contents = String::new();
        old_handle.read_to_string(&mut old_contents).unwrap();
        assert_eq!(old_contents, "old-token\n");

        let published_files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(published_files, 1, "temporary token file was left behind");
    }
}
