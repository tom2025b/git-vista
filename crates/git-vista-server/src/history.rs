//! Paged-history snapshot state and representation validators (M1.10, #63).
//!
//! Task 3 of the M1.10 plan: the exact refs+HEAD+shallow snapshot a paged
//! history read is pinned to, its `history-v1:<decimal>` generation token, and
//! the exact-body Frame/Page representation ETags. The signed offset cursor and
//! the reusable drift gate land in later steps of the same task; Task 4 wires
//! all of it into the frame/page handlers.
//!
//! The generation here is deliberately a *third* recipe, distinct from both the
//! planner's (display-short ref keys plus a status field) and
//! `git_vista_git::read_generation_inputs` (which folds in the index): history
//! pages depend only on committed topology, so index and worktree state are
//! excluded, while the `$GIT_DIR/shallow` boundary set — which changes which
//! parents a traversal may see without moving a single ref — is included.

use git_vista_protocol::HeadState;
use std::fmt::Write as _;
use std::path::Path;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use git_vista_core::identity::{GenerationInputs, ObjectId, RepositoryHandle};
use git_vista_core::model::{GitRef, Oid};
use git_vista_protocol::GenerationToken;

use crate::session;

/// The cursor codec's MAC. HMAC-SHA256 over the existing direct `sha2`
/// digest — the plan's "reuse, don't add another digest implementation".
type HmacSha256 = Hmac<Sha256>;

/// One traversal seed: a ref tip retained under its **full** name
/// (`refs/heads/main`, never the display-short `main`), or the detached-HEAD
/// pseudo-tip under the deterministic name `HEAD`. Full names keep the seed
/// set unambiguous — two refs can share a short name across namespaces — and
/// give the sorted seed order Task 4's deterministic replay depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryTip {
    pub full_ref_name: String,
    pub object_id: Oid,
}

/// The exact repository state one paged-history read is pinned to: display
/// refs, both HEAD halves, the canonical shallow boundary set, the sorted
/// traversal seeds, and the `history-v1:<decimal>` generation those inputs
/// digest to. Everything comes from a single repository read, so the
/// generation can never describe a different moment than the tips.
#[derive(Debug)]
pub(crate) struct HistorySnapshot {
    /// Display refs (short badge names, HEAD first), for the Frame.
    pub refs: Vec<GitRef>,
    /// The checked-out branch's short name; `None` when detached.
    pub head_branch: Option<String>,
    /// What state HEAD is in, which `head_branch` cannot express: its `None`
    /// covers a healthy detached HEAD and a HEAD that resolves to nothing
    /// alike, and those are opposite situations for the person looking at the
    /// screen (#473).
    pub head_state: HeadState,
    /// The commit HEAD resolves to; `None` for an unborn HEAD.
    ///
    /// Carried because the plan's snapshot pins *both* HEAD halves, but read
    /// only by this module's tests: the digest and the seed set are built from
    /// `materials.resolved_head` before the snapshot exists, and the wire
    /// `HistoryFrame` has no resolved-head field to fill. The allow is
    /// `not(test)`-scoped on purpose — a blanket `allow` would also hide a
    /// genuinely unused field in the test build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub resolved_head: Option<Oid>,
    /// Validated, sorted, deduplicated `$GIT_DIR/shallow` boundary set.
    pub shallow_boundaries: Vec<Oid>,
    /// Traversal seeds, sorted by `(full_ref_name, object_id)`, deduplicated.
    pub tips: Vec<HistoryTip>,
    /// The snapshot/cursor token: `history-v1:<decimal>`. Not an ETag.
    pub generation: GenerationToken,
}

/// Read one consistent [`HistorySnapshot`].
///
/// The repository is opened once ([`git_vista_git::read_history_materials`]),
/// so refs, HEAD, and `repo.shallow_commits()` all describe the same moment.
/// Malformed or unreadable shallow metadata is an explicit error — a shallow
/// repository must never be silently treated as unshallow, because the
/// boundary set decides which parents a page traversal may see.
pub(crate) async fn read_history_snapshot(
    repo: &Path,
) -> Result<HistorySnapshot, (StatusCode, String)> {
    let materials = git_vista_git::read_history_materials(repo).map_err(|e| {
        eprintln!("git-vista: history snapshot failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Canonicalise the shallow set: validate every object id, sort, dedupe.
    // `gix` already rejects malformed lines; the parse here re-checks each id
    // against the core grammar because these become generation fields.
    let mut shallow_boundaries = Vec::with_capacity(materials.shallow.len());
    for oid in &materials.shallow {
        validated(oid)?;
        shallow_boundaries.push(oid.clone());
    }
    shallow_boundaries.sort_by(|a, b| a.0.cmp(&b.0));
    shallow_boundaries.dedup();

    // Traversal seeds: every full-named ref tip, plus HEAD under its
    // deterministic pseudo-name — but only when its target isn't already
    // represented, so an attached HEAD never duplicates its branch's seed.
    let mut tips: Vec<HistoryTip> = materials
        .full_ref_targets
        .iter()
        .map(|(full_name, target)| HistoryTip {
            full_ref_name: full_name.clone(),
            object_id: target.clone(),
        })
        .collect();
    if let Some(resolved) = &materials.resolved_head {
        if !tips.iter().any(|tip| &tip.object_id == resolved) {
            tips.push(HistoryTip {
                full_ref_name: "HEAD".to_string(),
                object_id: resolved.clone(),
            });
        }
    }
    tips.sort_by(|a, b| {
        (a.full_ref_name.as_str(), a.object_id.0.as_str())
            .cmp(&(b.full_ref_name.as_str(), b.object_id.0.as_str()))
    });
    tips.dedup();

    // The history generation: recipe discriminator, both HEAD halves keyed by
    // the FULL symbolic name, every ref under its full name, and one field per
    // unique shallow boundary. Deliberately no `index()`/`worktree()` calls —
    // paged history depends only on committed topology — and deliberately the
    // existing `GenerationInputs` digest, not a new one.
    let mut inputs = GenerationInputs::new();
    inputs.field("recipe", "history-v1");
    let resolved_head_id = materials
        .resolved_head
        .as_ref()
        .map(validated)
        .transpose()?;
    inputs.head(
        materials.head_symbolic_full.as_deref(),
        resolved_head_id.as_ref(),
    );
    for (full_name, target) in &materials.full_ref_targets {
        inputs.reference(full_name, &validated(target)?);
    }
    for oid in &shallow_boundaries {
        inputs.field(format!("shallow:{}", oid.0), "boundary");
    }
    let generation =
        GenerationToken::new(format!("history-v1:{}", inputs.generation())).map_err(|e| {
            eprintln!("git-vista: history snapshot: building the generation token: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("building the generation token: {e}"),
            )
        })?;

    // The same four-way reading `read_refs_at` applies to HEAD (ADR 0071):
    // a branch name and a commit, a branch name and none, a commit and no
    // branch, or neither.
    let head_state = match (&materials.head_branch, &materials.resolved_head) {
        (Some(_), Some(_)) => HeadState::OnBranch,
        (Some(_), None) => HeadState::Unborn,
        (None, Some(_)) => HeadState::Detached,
        (None, None) => HeadState::Unresolvable,
    };

    Ok(HistorySnapshot {
        refs: materials.refs,
        head_branch: materials.head_branch,
        head_state,
        resolved_head: materials.resolved_head,
        shallow_boundaries,
        tips,
        generation,
    })
}

/// Which paged-history representation an ETag names. The two kinds are
/// deliberately type-separated: a Frame and a Page that happened to serialize
/// to identical bytes must still carry different validators, because they are
/// different resources with different conditional-request semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepresentationKind {
    Frame,
    Page,
}

impl RepresentationKind {
    /// The tag's type prefix. `gv4-` names the protocol major these
    /// representations belong to.
    fn prefix(self) -> &'static str {
        match self {
            RepresentationKind::Frame => "gv4-frame",
            RepresentationKind::Page => "gv4-page",
        }
    }
}

/// The strong, exact-body representation ETag: SHA-256 over the exact
/// serialized response bytes, emitted once as `"gv4-frame:<hex-sha256>"` or
/// `"gv4-page:<hex-sha256>"`. This is a *representation* validator — the
/// history generation stays only the JSON/cursor snapshot token and never
/// doubles as an ETag.
pub(crate) fn representation_etag(kind: RepresentationKind, serialized_body: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(serialized_body);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Infallible: writing into a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    let tag = format!("\"{}:{hex}\"", kind.prefix());
    HeaderValue::from_str(&tag).expect("a quoted ascii-hex etag is a valid header value")
}

/// RFC 9110 `If-None-Match` weak comparison against the current representation
/// tag: an exact strong match, a `W/`-prefixed weak match, a matching member of
/// a comma-separated list, and `*` all succeed. A malformed field (unquoted
/// text, an unterminated quote, a non-string header value) is an ordinary
/// nonmatch, never an error.
pub(crate) fn if_none_match(headers: &HeaderMap, current: &HeaderValue) -> bool {
    let Ok(current) = current.to_str() else {
        return false;
    };
    let current = weak_stripped(current);
    for value in headers.get_all(header::IF_NONE_MATCH) {
        // A header value that isn't visible ASCII can't hold our tag: skip it
        // as one more malformed, nonmatching field.
        let Ok(fields) = value.to_str() else {
            continue;
        };
        for field in fields.split(',') {
            let field = field.trim();
            if field == "*" {
                return true;
            }
            // Weak comparison ignores the weakness indicator on either side
            // and compares the quoted opaque tags byte-for-byte.
            let opaque = weak_stripped(field);
            if !is_quoted_opaque_tag(opaque) {
                continue;
            }
            if opaque == current {
                return true;
            }
        }
    }
    false
}

/// Strip a leading weakness indicator (`W/`), leaving the opaque tag.
fn weak_stripped(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// A minimally well-formed entity tag: double-quoted, with no interior quote.
/// (Our own tags never contain commas, so list-splitting on `,` above cannot
/// cut a valid matching member in half.)
fn is_quoted_opaque_tag(tag: &str) -> bool {
    tag.len() >= 2
        && tag.starts_with('"')
        && tag.ends_with('"')
        && !tag[1..tag.len() - 1].contains('"')
}

// ---------------------------------------------------------------------------
// Signed offset cursors (plan Task 3, Steps 4–5)

/// The longest encoded cursor the decoder will even look at. Checked FIRST,
/// before any base64 decode allocates, so an attacker can't make the server
/// buffer megabytes of "cursor". An honest cursor — opaque 32-byte scope,
/// `history-v1:<u64>` generation, one `usize` row — sits comfortably under
/// this even at maximum values.
pub(crate) const MAX_ENCODED_CURSOR_LEN: usize = 512;

/// The signed envelope's format version. Checked only *after* the signature
/// verifies (an unauthenticated version byte is still attacker input); a
/// mismatch is the same generic rejection as every other codec failure.
const CURSOR_FORMAT_VERSION: u8 = 1;

/// The HMAC-SHA256 tag is truncated to 128 bits on the wire: far beyond
/// brute-force for a per-process key that rotates on restart, and it keeps
/// the whole cursor short enough for a query parameter.
const CURSOR_TAG_BYTES: usize = 16;

/// The paging state itself: the absolute row the next page starts at. Choice
/// A re-walks the topology from the seeds on every page, so one number is the
/// *entire* server-side state a page request needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryCursor {
    /// The absolute row offset the deterministic replay skips to.
    pub next_row: usize,
}

/// The opaque, fixed-size *target* binding inside every cursor: a keyed,
/// domain-separated HMAC-SHA256 of which repository/worktree (or degraded
/// path target) the cursor pages through. Target identity only — never the
/// target's state (that is the generation's job) and never raw ids or paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CursorScope([u8; 32]);

/// What actually gets signed and carried: format version, opaque target
/// scope, the pinned generation, and the paging state. Wire form is
/// `BASE64URL_NO_PAD(json-envelope).BASE64URL_NO_PAD(tag[0..16])`.
#[derive(Serialize, Deserialize)]
struct CursorEnvelope<T> {
    version: u8,
    scope: CursorScope,
    generation: GenerationToken,
    state: T,
}

/// A successfully authenticated, version-checked cursor: what Task 4's page
/// handler compares (scope against the resolved target, generation against
/// the fresh snapshot) before walking anything.
pub(crate) struct DecodedCursor<T> {
    pub scope: CursorScope,
    pub generation: GenerationToken,
    pub state: T,
}

/// Any cursor codec failure. Deliberately one opaque unit: too long, bad
/// base64, wrong part count, bad signature, malformed envelope, and wrong
/// version are indistinguishable to a caller, so a probing client learns
/// nothing about *which* gate refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorError;

impl CursorError {
    /// The single generic HTTP 400 every codec error maps to (Task 4's
    /// handlers return this for any unusable cursor, tampered or expired).
    pub(crate) fn response(self) -> (StatusCode, String) {
        (
            StatusCode::BAD_REQUEST,
            "invalid history cursor".to_string(),
        )
    }
}

/// Signs and verifies history cursors with a per-process random key. A
/// restart mints a new key, so every pre-restart cursor deliberately fails
/// verification and the frontend restarts its aggregate at page 1 — exactly
/// the "no durable server-side paging state" the design demands.
pub(crate) struct CursorCodec {
    key: [u8; 32],
}

impl CursorCodec {
    /// A codec keyed from the OS CSPRNG — built once per process (Task 4
    /// holds it alongside the routers).
    pub(crate) fn new() -> Self {
        Self {
            key: session::random_secret_bytes(),
        }
    }

    /// A codec with a caller-chosen key, so tests are deterministic and can
    /// model "a different process" with a second key.
    #[cfg(test)]
    pub(crate) fn with_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// The opaque scope a cursor for `resolved_canonical_target` must carry.
    ///
    /// A registered target is its identity pair: both `RepositoryHandle`
    /// halves bind, so a cursor can follow neither a different repository nor
    /// a sibling worktree of the same one. A degraded target has no ids, only
    /// its resolved canonical path — bound through the per-process key, so
    /// the path never appears on the wire in any recoverable form and the
    /// binding dies with the process. The two arms share a key but not a
    /// domain, so registered and degraded scopes can never collide.
    pub(crate) fn scope_for_target(
        &self,
        handle: Option<&RepositoryHandle>,
        resolved_canonical_target: &Path,
    ) -> CursorScope {
        let mut mac = self.mac(b"git-vista.cursor-scope.v1");
        match handle {
            Some(handle) => {
                mac.update(b"registered\0");
                // Both ids are fixed-width (16 bytes), so the concatenation
                // is unambiguous without separators.
                mac.update(handle.repository.as_uuid().as_bytes());
                mac.update(handle.worktree.as_uuid().as_bytes());
            }
            None => {
                mac.update(b"degraded\0");
                mac.update(resolved_canonical_target.as_os_str().as_encoded_bytes());
            }
        }
        CursorScope(mac.finalize().into_bytes().into())
    }

    /// Sign `state` for `scope` at `generation` into the wire form. Refuses
    /// to mint anything `decode` would refuse to read.
    pub(crate) fn encode<T: Serialize>(
        &self,
        scope: CursorScope,
        generation: &GenerationToken,
        state: &T,
    ) -> Result<String, CursorError> {
        let envelope = CursorEnvelope {
            version: CURSOR_FORMAT_VERSION,
            scope,
            generation: generation.clone(),
            state,
        };
        let payload = serde_json::to_vec(&envelope).map_err(|_| CursorError)?;
        let tag = self.tag(&payload);
        let encoded = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(tag)
        );
        if encoded.len() > MAX_ENCODED_CURSOR_LEN {
            return Err(CursorError);
        }
        Ok(encoded)
    }

    /// Authenticate and open a cursor. The gate order is load-bearing:
    ///
    /// 1. length guard — before any base64 allocation;
    /// 2. exactly one dot — payload, tag, nothing else;
    /// 3. bounded base64 decodes of both parts;
    /// 4. recomputed HMAC-SHA256, compared constant-time over the fixed
    ///    16-byte tag ([`session::ct_eq`]) — the last gate before parsing;
    /// 5. only then `serde_json::from_slice`, so attacker-shaped bytes never
    ///    reach a deserializer without a valid signature;
    /// 6. envelope version check, on authenticated bytes only.
    pub(crate) fn decode<T: DeserializeOwned>(
        &self,
        encoded: &str,
    ) -> Result<DecodedCursor<T>, CursorError> {
        if encoded.len() > MAX_ENCODED_CURSOR_LEN {
            return Err(CursorError);
        }
        let mut parts = encoded.split('.');
        let (Some(payload_b64), Some(tag_b64), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(CursorError);
        };
        let payload = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| CursorError)?;
        let presented = URL_SAFE_NO_PAD.decode(tag_b64).map_err(|_| CursorError)?;
        if presented.len() != CURSOR_TAG_BYTES {
            return Err(CursorError);
        }
        if !session::ct_eq(&self.tag(&payload), &presented) {
            return Err(CursorError);
        }
        let envelope: CursorEnvelope<T> =
            serde_json::from_slice(&payload).map_err(|_| CursorError)?;
        if envelope.version != CURSOR_FORMAT_VERSION {
            return Err(CursorError);
        }
        Ok(DecodedCursor {
            scope: envelope.scope,
            generation: envelope.generation,
            state: envelope.state,
        })
    }

    /// A fresh keyed MAC opened under `domain` (NUL-terminated), so the scope
    /// binding and the wire tag can never be confused for one another even
    /// though they share the process key.
    fn mac(&self, domain: &[u8]) -> HmacSha256 {
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC-SHA256 accepts a 32-byte key");
        mac.update(domain);
        mac.update(b"\0");
        mac
    }

    /// The truncated wire tag over the exact payload bytes.
    fn tag(&self, payload: &[u8]) -> [u8; CURSOR_TAG_BYTES] {
        let mut mac = self.mac(b"git-vista.cursor-tag.v1");
        mac.update(payload);
        let digest = mac.finalize().into_bytes();
        let mut tag = [0_u8; CURSOR_TAG_BYTES];
        tag.copy_from_slice(&digest[..CURSOR_TAG_BYTES]);
        tag
    }
}

impl Default for CursorCodec {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The reusable drift gate (plan Task 3, Step 6)

/// Require that the generation a paged read is pinned to (`expected`, from the
/// client's cursor or query) still equals the freshly re-read one (`actual`).
///
/// A mismatch is HTTP 409 — `ErrorCode::Conflict` / `"conflict"` on the wire —
/// with the exact `"history moved"` wording the frontend keys "restart the
/// aggregate at page 1" on. Task 4 runs this gate before traversal and, per
/// the verifier amendment, re-reads and runs it again after a walk error so a
/// concurrent ref move surfaces as drift rather than as a phantom read error.
pub(crate) fn require_same_generation(
    expected: &GenerationToken,
    actual: &GenerationToken,
) -> Result<(), (StatusCode, String)> {
    if expected == actual {
        Ok(())
    } else {
        Err((StatusCode::CONFLICT, "history moved".to_string()))
    }
}

/// Validate one gix-produced hex id against the core [`ObjectId`] grammar.
/// gix only emits well-formed ids, so a failure is an internal contract break,
/// surfaced as the snapshot's explicit read error.
fn validated(oid: &Oid) -> Result<ObjectId, (StatusCode, String)> {
    ObjectId::parse(&oid.0).map_err(|e| {
        eprintln!("git-vista: history snapshot: invalid object id from git: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid object id from git: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_fixtures::seeded as seeded_repo;
    use std::path::{Path, PathBuf};

    use axum::http::StatusCode;
    use git_vista_core::model::{Oid, RefKind};

    // ---- fixtures -----------------------------------------------------------
    // Private copies of the git fixture helpers, the same shape as
    // `handlers::read::tests` and `git_cmd::tests`: those are module-private by
    // design, so each suite carries its own.

    /// `git <args…>` in `repo`; asserts success.
    fn run(repo: &Path, args: &[&str]) {
        run_env(repo, args, &[]);
    }

    /// `git <args…>` in `repo` with extra environment variables; asserts success.
    fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(repo);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let status = cmd.status().unwrap();
        assert!(status.success(), "git {args:?} failed in {repo:?}");
    }

    /// `git <args…>` in `repo`, returning trimmed stdout; asserts success.
    fn out(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {repo:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// An initialised repository with no commit yet — HEAD names `main` and
    /// nothing is under it.
    fn empty_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        (dir, repo)
    }

    /// Write `content` into `file`, stage everything, commit.
    fn commit_file(repo: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(repo.join(file), content).unwrap();
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "-m", msg]);
    }

    async fn snapshot(repo: &Path) -> HistorySnapshot {
        read_history_snapshot(repo)
            .await
            .expect("snapshot should succeed")
    }

    // Deterministic fixture: a fixed author/committer identity and fixed dates
    // make the commit OIDs identical across independently built repositories,
    // so two repos differing ONLY in ref creation order hold byte-identical
    // repository state.
    const DATE_1: &str = "1700000000 +0000";
    const DATE_2: &str = "1700000100 +0000";

    fn deterministic_commit(repo: &Path, file: &str, content: &str, msg: &str, date: &str) {
        std::fs::write(repo.join(file), content).unwrap();
        run(repo, &["add", "-A"]);
        run_env(
            repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", msg],
            &[("GIT_AUTHOR_DATE", date), ("GIT_COMMITTER_DATE", date)],
        );
    }

    /// Two deterministic commits on `main`, then the same three refs created in
    /// the caller's order: a branch at `HEAD~1`, a lightweight tag at `HEAD`,
    /// and a remote-tracking ref at `HEAD`.
    fn deterministic_repo_with_refs(order: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        deterministic_commit(&repo, "a.txt", "one\n", "c1", DATE_1);
        deterministic_commit(&repo, "a.txt", "two\n", "c2", DATE_2);
        for step in order {
            match *step {
                "branch" => run(&repo, &["branch", "alpha", "HEAD~1"]),
                "tag" => run(&repo, &["tag", "v1", "HEAD"]),
                "remote" => {
                    let head = out(&repo, &["rev-parse", "HEAD"]);
                    run(&repo, &["update-ref", "refs/remotes/origin/main", &head]);
                }
                other => panic!("unknown ref step {other:?}"),
            }
        }
        (dir, repo)
    }

    // ---- snapshot/generation (plan Task 3, Steps 1–2) -----------------------

    /// The generation is a canonical digest: reading twice, and reading two
    /// byte-identical repositories whose refs were created in a different
    /// order, all yield the same `history-v1:<decimal>` token.
    #[tokio::test]
    async fn history_generation_is_stable_across_ref_enumeration_order() {
        let (_da, a) = deterministic_repo_with_refs(&["branch", "tag", "remote"]);
        let (_db, b) = deterministic_repo_with_refs(&["remote", "tag", "branch"]);
        // The fixture only proves anything if both repos really hold the same
        // commits; a determinism break should fail loudly here, not as a
        // confusing token mismatch below.
        assert_eq!(
            out(&a, &["rev-parse", "HEAD"]),
            out(&b, &["rev-parse", "HEAD"])
        );

        let snap_a = snapshot(&a).await;
        let snap_b = snapshot(&b).await;
        assert_eq!(snap_a.generation, snap_b.generation);

        // Reading the same repository again is also stable.
        assert_eq!(snapshot(&a).await.generation, snap_a.generation);

        // The token is the history recipe's own encoding: `history-v1:<decimal>`.
        let text = snap_a.generation.as_str();
        let decimal = text
            .strip_prefix("history-v1:")
            .expect("token must carry the history-v1 discriminator");
        decimal
            .parse::<u64>()
            .expect("token payload must be the decimal RepositoryGeneration");
    }

    #[tokio::test]
    async fn history_generation_changes_when_ref_moves() {
        let (_dir, repo) = seeded_repo();
        commit_file(&repo, "b.txt", "b\n", "second");
        run(&repo, &["branch", "feature", "HEAD~1"]);

        let before = snapshot(&repo).await;
        // Retarget only `refs/heads/feature`; HEAD (symbolic and resolved)
        // stays put, so the ref move alone must carry the change.
        let head = out(&repo, &["rev-parse", "HEAD"]);
        run(&repo, &["update-ref", "refs/heads/feature", &head]);
        let after = snapshot(&repo).await;

        assert_eq!(before.head_branch, after.head_branch);
        assert_eq!(before.resolved_head, after.resolved_head);
        assert_ne!(before.generation, after.generation);
    }

    /// #473: `head_branch` is `None` for a healthy detached HEAD **and** for a
    /// HEAD that resolves to nothing. The frame must tell those apart, because
    /// one is normal and the other means the repository is broken.
    ///
    /// The assertion that matters is the `assert_ne!` — a test that only
    /// checked "a dangling HEAD reports Unresolvable" would pass against a
    /// server that reported `Unresolvable` for a healthy detached HEAD too,
    /// which is the same silence in a different costume.
    ///
    /// MUTATION 1: map `(None, None)` to `Detached` — red, the two states
    ///   collapse again.
    /// MUTATION 2: map `(Some(_), None)` to `OnBranch` — red on the unborn
    ///   row, which claims a commit that does not exist.
    #[tokio::test]
    async fn a_broken_head_and_a_healthy_detached_head_are_not_the_same_state() {
        let (_dir, repo) = seeded_repo();

        let on_branch = snapshot(&repo).await;
        assert_eq!(on_branch.head_state, HeadState::OnBranch);

        run(&repo, &["checkout", "-q", "--detach"]);
        let detached = snapshot(&repo).await;
        assert_eq!(detached.head_branch, None);
        assert_eq!(
            detached.head_state,
            HeadState::Detached,
            "a detached HEAD at a real commit is a normal state, not a fault"
        );

        // A well-formed object id with no object behind it.
        std::fs::write(repo.join(".git/HEAD"), "0".repeat(40) + "\n").unwrap();
        let broken = snapshot(&repo).await;
        assert_eq!(broken.head_branch, None, "still no branch name to show");
        assert_eq!(broken.head_state, HeadState::Unresolvable);

        assert_ne!(
            detached.head_state, broken.head_state,
            "both arrive as head_branch: None — if the state does not separate \
             them, the payload still cannot say the repository is broken"
        );

        // An unborn HEAD: a branch name, and no commit under it.
        let (_dir2, fresh) = empty_repo();
        let unborn = snapshot(&fresh).await;
        assert_eq!(unborn.head_state, HeadState::Unborn);
        assert_ne!(
            unborn.head_state, broken.head_state,
            "a fresh repository is not a broken one"
        );
    }

    #[tokio::test]
    async fn history_generation_changes_when_symbolic_or_resolved_head_moves() {
        let (_dir, repo) = seeded_repo();
        commit_file(&repo, "b.txt", "b\n", "second");
        // A second branch on the same tip, created before the first snapshot,
        // so the ref set itself never changes below — only HEAD moves.
        run(&repo, &["branch", "other"]);
        let on_main = snapshot(&repo).await;

        // The symbolic half moves (main → other); the resolved commit does not.
        run(&repo, &["symbolic-ref", "HEAD", "refs/heads/other"]);
        let on_other = snapshot(&repo).await;
        assert_eq!(on_main.resolved_head, on_other.resolved_head);
        assert_ne!(on_main.generation, on_other.generation);

        // Detaching drops the symbolic half at the same resolved commit.
        run(&repo, &["checkout", "-q", "--detach"]);
        let detached = snapshot(&repo).await;
        assert_eq!(detached.head_branch, None);
        assert_eq!(on_other.resolved_head, detached.resolved_head);
        assert_ne!(on_other.generation, detached.generation);

        // The resolved half moves while detached (the symbolic stays `None`).
        let older = out(&repo, &["rev-parse", "HEAD~1"]);
        run(&repo, &["checkout", "-q", &older]);
        let moved = snapshot(&repo).await;
        assert_eq!(moved.head_branch, None);
        assert_ne!(detached.generation, moved.generation);
    }

    #[tokio::test]
    async fn history_generation_changes_when_shallow_boundary_changes_without_ref_move() {
        let (_dir, repo) = seeded_repo();
        commit_file(&repo, "b.txt", "b\n", "second");
        commit_file(&repo, "c.txt", "c\n", "third");
        let older = out(&repo, &["rev-parse", "HEAD~1"]);
        let oldest = out(&repo, &["rev-parse", "HEAD~2"]);
        let shallow_file = repo.join(".git").join("shallow");

        let full = snapshot(&repo).await;
        assert!(full.shallow_boundaries.is_empty());

        // Rewrite ONLY `$GIT_DIR/shallow`: every canonical ref and both HEAD
        // halves stay byte-identical, yet the generation must move.
        std::fs::write(&shallow_file, format!("{older}\n")).unwrap();
        let shallow_older = snapshot(&repo).await;
        assert_eq!(full.refs, shallow_older.refs);
        assert_eq!(full.head_branch, shallow_older.head_branch);
        assert_eq!(full.resolved_head, shallow_older.resolved_head);
        assert_eq!(full.tips, shallow_older.tips);
        assert_eq!(shallow_older.shallow_boundaries, vec![Oid(older.clone())]);
        assert_ne!(full.generation, shallow_older.generation);

        // A deepen (a different boundary) moves it again.
        std::fs::write(&shallow_file, format!("{oldest}\n")).unwrap();
        let shallow_oldest = snapshot(&repo).await;
        assert_eq!(shallow_oldest.shallow_boundaries, vec![Oid(oldest.clone())]);
        assert_ne!(shallow_older.generation, shallow_oldest.generation);

        // Duplicate lines canonicalise to one boundary — same state, same token.
        std::fs::write(&shallow_file, format!("{oldest}\n{oldest}\n")).unwrap();
        let deduped = snapshot(&repo).await;
        assert_eq!(deduped.shallow_boundaries, vec![Oid(oldest.clone())]);
        assert_eq!(shallow_oldest.generation, deduped.generation);

        // Unshallowing (the file removed) moves it once more — back to the
        // full repository's token, because the digest is deterministic.
        std::fs::remove_file(&shallow_file).unwrap();
        let unshallowed = snapshot(&repo).await;
        assert_ne!(shallow_oldest.generation, unshallowed.generation);
        assert_eq!(full.generation, unshallowed.generation);
    }

    #[tokio::test]
    async fn history_generation_ignores_index_and_worktree_changes() {
        let (_dir, repo) = seeded_repo();
        let clean = snapshot(&repo).await;

        // A tracked edit and a new untracked file: worktree-only state.
        std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "new\n").unwrap();
        let dirty = snapshot(&repo).await;
        assert_eq!(clean.generation.as_str(), dirty.generation.as_str());

        // Staging rewrites the index; history generation still must not move.
        run(&repo, &["add", "-A"]);
        let staged = snapshot(&repo).await;
        assert_eq!(clean.generation.as_str(), staged.generation.as_str());
    }

    #[tokio::test]
    async fn history_tips_use_sorted_full_names_and_ids() {
        let (_dir, repo) = seeded_repo();
        commit_file(&repo, "b.txt", "b\n", "second");
        commit_file(&repo, "c.txt", "c\n", "third");
        let c1 = out(&repo, &["rev-parse", "HEAD~2"]);
        let c2 = out(&repo, &["rev-parse", "HEAD~1"]);
        let c3 = out(&repo, &["rev-parse", "HEAD"]);

        run(&repo, &["branch", "zeta", &c1]);
        // An annotated tag: the tip must peel to the commit, not the tag object.
        run(&repo, &["tag", "-a", "-m", "one", "v1.0", &c1]);
        run(&repo, &["update-ref", "refs/remotes/origin/main", &c3]);
        // The remote's symbolic default-branch pointer is never a tip.
        run(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        let tip = |name: &str, id: &str| HistoryTip {
            full_ref_name: name.to_string(),
            object_id: Oid(id.to_string()),
        };

        // On `main`, HEAD's target is already represented by refs/heads/main,
        // so no pseudo-tip appears; tips are FULL names sorted by (name, id).
        let on_branch = snapshot(&repo).await;
        assert_eq!(on_branch.head_branch.as_deref(), Some("main"));
        assert_eq!(
            on_branch.tips,
            vec![
                tip("refs/heads/main", &c3),
                tip("refs/heads/zeta", &c1),
                tip("refs/remotes/origin/main", &c3),
                tip("refs/tags/v1.0", &c1),
            ]
        );

        // Display refs stay short badge names — tips are never derived from
        // them, and they never carry full names.
        assert!(on_branch.refs.iter().all(|r| !r.name.starts_with("refs/")));
        assert!(on_branch
            .refs
            .iter()
            .any(|r| r.name == "origin/main" && r.kind == RefKind::RemoteBranch));
        assert!(on_branch.refs.iter().all(|r| r.name != "origin/HEAD"));

        // Detached at c2 — a commit no ref represents — HEAD must be seeded
        // under the deterministic pseudo-name, sorted with the rest.
        run(&repo, &["checkout", "-q", &c2]);
        let detached = snapshot(&repo).await;
        assert_eq!(detached.head_branch, None);
        assert_eq!(detached.resolved_head, Some(Oid(c2.clone())));
        assert_eq!(
            detached.tips,
            vec![
                tip("HEAD", &c2),
                tip("refs/heads/main", &c3),
                tip("refs/heads/zeta", &c1),
                tip("refs/remotes/origin/main", &c3),
                tip("refs/tags/v1.0", &c1),
            ]
        );
    }

    // ---- representation ETags (plan Task 3, Step 3) -------------------------

    use axum::http::{header, HeaderMap, HeaderValue};

    /// Independently computed SHA-256 test vector for the bytes `hello`.
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn representation_etag_is_type_prefixed_sha256_of_exact_body() {
        let frame = representation_etag(RepresentationKind::Frame, b"hello");
        let page = representation_etag(RepresentationKind::Page, b"hello");

        // SHA-256 over the exact serialized bytes, as a quoted strong tag.
        assert_eq!(
            frame.to_str().unwrap(),
            format!("\"gv4-frame:{HELLO_SHA256}\"")
        );
        assert_eq!(
            page.to_str().unwrap(),
            format!("\"gv4-page:{HELLO_SHA256}\"")
        );
        // Type separation: identical bytes, different validators.
        assert_ne!(frame, page);
    }

    #[test]
    fn changed_body_changes_representation_etag() {
        // One flipped byte anywhere in the serialized body moves the validator.
        assert_ne!(
            representation_etag(RepresentationKind::Frame, b"hello"),
            representation_etag(RepresentationKind::Frame, b"hellp"),
        );
        assert_ne!(
            representation_etag(RepresentationKind::Page, b"body-a"),
            representation_etag(RepresentationKind::Page, b"body-b"),
        );
    }

    #[test]
    fn if_none_match_uses_weak_comparison_for_exact_list_weak_and_star() {
        let current = representation_etag(RepresentationKind::Frame, b"x");
        let exact = current.to_str().unwrap().to_string();

        // Exact strong match.
        assert!(if_none_match(&headers_with(&exact), &current));
        // RFC weak comparison: a `W/` client validator still matches.
        assert!(if_none_match(
            &headers_with(&format!("W/{exact}")),
            &current
        ));
        // A comma-separated list whose middle member matches.
        assert!(if_none_match(
            &headers_with(&format!("\"other\", {exact}, \"another\"")),
            &current
        ));
        // `*` matches any current representation.
        assert!(if_none_match(&headers_with("*"), &current));
    }

    #[test]
    fn if_none_match_treats_malformed_or_different_as_nonmatch() {
        let current = representation_etag(RepresentationKind::Frame, b"x");
        let exact = current.to_str().unwrap().to_string();

        // No header at all.
        assert!(!if_none_match(&HeaderMap::new(), &current));
        // A well-formed but different validator.
        assert!(!if_none_match(
            &headers_with("\"gv4-frame:deadbeef\""),
            &current
        ));
        // Same bytes, other type: type separation applies to matching too.
        let page = representation_etag(RepresentationKind::Page, b"x");
        assert!(!if_none_match(
            &headers_with(page.to_str().unwrap()),
            &current
        ));
        // Malformed fields are ordinary nonmatches, never errors — including
        // the right opaque text without its required quotes.
        assert!(!if_none_match(
            &headers_with(exact.trim_matches('"')),
            &current
        ));
        assert!(!if_none_match(&headers_with("W/unquoted"), &current));
        assert!(!if_none_match(&headers_with("\"unterminated"), &current));
        assert!(!if_none_match(&headers_with("W/\"one\" \"two\""), &current));
        // A list of malformed and different members still misses.
        assert!(!if_none_match(
            &headers_with("bogus, \"nope\", W/plain"),
            &current
        ));
    }

    #[tokio::test]
    async fn malformed_shallow_metadata_is_snapshot_error() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join(".git").join("shallow"), "not-hex\n").unwrap();

        let error = read_history_snapshot(&repo)
            .await
            .expect_err("malformed shallow metadata must be an explicit error, not 'unshallow'");
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            error.1.contains("shallow"),
            "the error should say the shallow read failed: {}",
            error.1
        );
    }

    // ---- signed offset cursors (plan Task 3, Steps 4–5) ---------------------

    use std::sync::atomic::{AtomicBool, Ordering};

    // `Engine` (for `.encode`/`.decode` on `URL_SAFE_NO_PAD`) comes in through
    // `use super::*` from the codec's own imports.
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde::{Deserialize, Serialize};

    use git_vista_core::identity::{RepositoryHandle, RepositoryId, WorktreeId};

    /// A codec with a fixed, test-chosen key, so cursors are deterministic.
    fn codec() -> CursorCodec {
        CursorCodec::with_key([0x5a; 32])
    }

    /// A codec with a *different* key: what the same code becomes in another
    /// process (or this one, after a restart rotates the key).
    fn other_process_codec() -> CursorCodec {
        CursorCodec::with_key([0xa5; 32])
    }

    fn generation(text: &str) -> GenerationToken {
        GenerationToken::new(text).unwrap()
    }

    /// A registered handle from synthetic canonical dirs — the v5-UUID id
    /// derivation is deterministic and never touches the filesystem.
    fn handle(common_dir: &str, git_dir: &str) -> RepositoryHandle {
        RepositoryHandle::new(
            RepositoryId::from_common_dir(common_dir),
            WorktreeId::from_git_dir(git_dir),
        )
    }

    /// Sign an arbitrary envelope with the codec's own key and wire format,
    /// minus `encode`'s ceiling — so tests can build *correctly signed* inputs
    /// (wrong version, oversized state) that `encode` refuses to produce.
    fn forge<T: Serialize>(codec: &CursorCodec, envelope: &CursorEnvelope<T>) -> String {
        let payload = serde_json::to_vec(envelope).unwrap();
        let tag = codec.tag(&payload);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(tag)
        )
    }

    #[test]
    fn signed_cursor_round_trips_with_target_scope() {
        let codec = codec();
        let scope = codec.scope_for_target(
            Some(&handle("/fixtures/repo/.git", "/fixtures/repo/.git")),
            Path::new("/fixtures/repo"),
        );
        // A real target scope is never the all-zero default.
        assert_ne!(scope.0, [0_u8; 32]);

        // Maximum row and maximum decimal generation: the worst-case honest
        // cursor must still round-trip inside the decode length guard.
        let generation = generation(&format!("history-v1:{}", u64::MAX));
        let cursor = HistoryCursor {
            next_row: usize::MAX,
        };
        let encoded = codec.encode(scope, &generation, &cursor).unwrap();
        assert!(
            encoded.len() <= MAX_ENCODED_CURSOR_LEN,
            "worst-case cursor is {} bytes",
            encoded.len()
        );
        // The wire form is payload.tag — exactly one dot.
        assert_eq!(encoded.matches('.').count(), 1);

        let decoded: DecodedCursor<HistoryCursor> = codec.decode(&encoded).unwrap();
        assert_eq!(decoded.scope, scope);
        assert_eq!(decoded.generation, generation);
        assert_eq!(decoded.state, cursor);
    }

    #[test]
    fn cursor_scope_binds_repository_worktree_and_degraded_target() {
        let codec = codec();
        let target = Path::new("/fixtures/checkout");

        // Same registered target twice: the same scope, deterministically.
        let base = codec.scope_for_target(
            Some(&handle("/fixtures/a/.git", "/fixtures/a/.git")),
            target,
        );
        assert_eq!(
            base,
            codec.scope_for_target(
                Some(&handle("/fixtures/a/.git", "/fixtures/a/.git")),
                target
            )
        );
        // Varying the repository alone changes the scope…
        assert_ne!(
            base,
            codec.scope_for_target(
                Some(&handle("/fixtures/b/.git", "/fixtures/a/.git")),
                target
            )
        );
        // …and varying the worktree alone changes it too.
        assert_ne!(
            base,
            codec.scope_for_target(
                Some(&handle("/fixtures/a/.git", "/fixtures/a/.git/worktrees/wt")),
                target
            )
        );
        // A registered scope binds the identity pair, not the resolved path.
        assert_eq!(
            base,
            codec.scope_for_target(
                Some(&handle("/fixtures/a/.git", "/fixtures/a/.git")),
                Path::new("/somewhere/else")
            )
        );

        // A degraded target binds its resolved canonical path…
        let degraded_dir = "/degraded/very-distinctive-target-dir";
        let degraded = codec.scope_for_target(None, Path::new(degraded_dir));
        assert_eq!(
            degraded,
            codec.scope_for_target(None, Path::new(degraded_dir))
        );
        assert_ne!(
            degraded,
            codec.scope_for_target(None, Path::new("/degraded/another-target"))
        );
        // …never collides with a registered scope…
        assert_ne!(degraded, base);
        // …and is keyed per process: a different process key gives a different
        // binding, so pre-restart degraded cursors deliberately die.
        assert_ne!(
            degraded,
            other_process_codec().scope_for_target(None, Path::new(degraded_dir))
        );

        // The path itself never reaches the wire: neither the encoded text nor
        // the authenticated payload bytes carry any fragment of it.
        let encoded = codec
            .encode(
                degraded,
                &generation("history-v1:7"),
                &HistoryCursor { next_row: 7 },
            )
            .unwrap();
        assert!(!encoded.contains("degraded"));
        assert!(!encoded.contains("very-distinctive-target-dir"));
        let payload_b64 = encoded.split('.').next().unwrap();
        let payload_text = String::from_utf8(URL_SAFE_NO_PAD.decode(payload_b64).unwrap()).unwrap();
        assert!(!payload_text.contains("degraded"));
        assert!(!payload_text.contains("very-distinctive-target-dir"));
    }

    /// Flipped by [`TamperProbe`]'s deserialize visitor. Only the payload-tamper
    /// test touches it: if the codec ever hands unauthenticated bytes to serde,
    /// this turns true and that test fails.
    static PROBE_DESERIALIZED: AtomicBool = AtomicBool::new(false);

    /// Test-only cursor state that *records* being deserialized. Serializes as
    /// a bare number; its visitor flips [`PROBE_DESERIALIZED`] on the way in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    struct TamperProbe(u64);

    impl<'de> Deserialize<'de> for TamperProbe {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ProbeVisitor;
            impl serde::de::Visitor<'_> for ProbeVisitor {
                type Value = TamperProbe;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a probe number")
                }
                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<TamperProbe, E> {
                    PROBE_DESERIALIZED.store(true, Ordering::SeqCst);
                    Ok(TamperProbe(value))
                }
            }
            deserializer.deserialize_u64(ProbeVisitor)
        }
    }

    #[test]
    fn cursor_rejects_payload_tamper_before_deserialize() {
        let codec = codec();
        let scope = codec.scope_for_target(None, Path::new("/fixtures/tamper"));
        let encoded = codec
            .encode(scope, &generation("history-v1:1"), &TamperProbe(9))
            .unwrap();

        // Swap one payload character for a different base64url character —
        // away from the final char, whose trailing bits strict decoding
        // constrains — so the part still decodes but the bytes under the
        // signature changed.
        let (payload_b64, tag_b64) = encoded.split_once('.').unwrap();
        let index = 10;
        let mut tampered_payload = payload_b64.as_bytes().to_vec();
        tampered_payload[index] = if tampered_payload[index] == b'A' {
            b'B'
        } else {
            b'A'
        };
        let tampered = format!("{}.{tag_b64}", String::from_utf8(tampered_payload).unwrap());
        assert_ne!(tampered, encoded);

        assert!(!PROBE_DESERIALIZED.load(Ordering::SeqCst));
        assert!(codec.decode::<TamperProbe>(&tampered).is_err());
        // The forged payload was never deserialized: the HMAC gate came first.
        assert!(
            !PROBE_DESERIALIZED.load(Ordering::SeqCst),
            "tampered payload reached serde before signature verification"
        );

        // The probe instrument itself works: the untampered cursor decodes
        // and flips the flag, so the stillness above was the codec's doing.
        let decoded: DecodedCursor<TamperProbe> = codec.decode(&encoded).unwrap();
        assert_eq!(decoded.state, TamperProbe(9));
        assert!(PROBE_DESERIALIZED.load(Ordering::SeqCst));
    }

    #[test]
    fn cursor_rejects_tag_tamper() {
        let codec = codec();
        let scope = codec.scope_for_target(None, Path::new("/fixtures/tag-tamper"));
        let encoded = codec
            .encode(
                scope,
                &generation("history-v1:1"),
                &HistoryCursor { next_row: 3 },
            )
            .unwrap();
        let (payload_b64, tag_b64) = encoded.split_once('.').unwrap();

        // One flipped tag character.
        let mut flipped = tag_b64.as_bytes().to_vec();
        flipped[0] = if flipped[0] == b'A' { b'B' } else { b'A' };
        let flipped = format!("{payload_b64}.{}", String::from_utf8(flipped).unwrap());
        assert!(codec.decode::<HistoryCursor>(&flipped).is_err());

        // A truncated tag: still valid base64, no longer 16 bytes.
        let truncated = format!("{payload_b64}.{}", &tag_b64[..tag_b64.len() - 4]);
        assert!(codec.decode::<HistoryCursor>(&truncated).is_err());

        // A perfectly valid tag — for a *different* payload.
        let other = codec
            .encode(
                scope,
                &generation("history-v1:2"),
                &HistoryCursor { next_row: 4 },
            )
            .unwrap();
        let (_, other_tag) = other.split_once('.').unwrap();
        assert_ne!(other_tag, tag_b64);
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}.{other_tag}"))
            .is_err());

        // The untampered cursor still decodes: the rejections were the tag's.
        assert!(codec.decode::<HistoryCursor>(&encoded).is_ok());
    }

    #[test]
    fn cursor_rejects_malformed_base64_and_extra_part() {
        let codec = codec();
        let scope = codec.scope_for_target(None, Path::new("/fixtures/malformed"));
        let encoded = codec
            .encode(
                scope,
                &generation("history-v1:5"),
                &HistoryCursor { next_row: 5 },
            )
            .unwrap();
        let (payload_b64, tag_b64) = encoded.split_once('.').unwrap();

        // No dot at all.
        assert!(codec
            .decode::<HistoryCursor>(&encoded.replace('.', ""))
            .is_err());
        // More than one dot: a third part must not be silently ignored…
        assert!(codec
            .decode::<HistoryCursor>(&format!("{encoded}.extra"))
            .is_err());
        // …including an empty part between two dots.
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}..{tag_b64}"))
            .is_err());
        // Characters outside the base64url alphabet, in either part.
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}!.{tag_b64}"))
            .is_err());
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}.{tag_b64}!"))
            .is_err());
        // The standard-alphabet characters base64url deliberately excludes.
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}+.{tag_b64}"))
            .is_err());
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}/.{tag_b64}"))
            .is_err());
        // Empty parts, and the empty string entirely.
        assert!(codec.decode::<HistoryCursor>(".").is_err());
        assert!(codec
            .decode::<HistoryCursor>(&format!(".{tag_b64}"))
            .is_err());
        assert!(codec
            .decode::<HistoryCursor>(&format!("{payload_b64}."))
            .is_err());
        assert!(codec.decode::<HistoryCursor>("").is_err());
    }

    #[test]
    fn cursor_rejects_wrong_version() {
        let codec = codec();
        let scope = codec.scope_for_target(None, Path::new("/fixtures/version"));
        let generation_token = generation("history-v1:6");

        // Correctly *signed*, wrong format version: the signature passes, so
        // rejection can only come from the post-verification version gate.
        let wrong = forge(
            &codec,
            &CursorEnvelope {
                version: CURSOR_FORMAT_VERSION + 1,
                scope,
                generation: generation_token.clone(),
                state: HistoryCursor { next_row: 6 },
            },
        );
        assert!(codec.decode::<HistoryCursor>(&wrong).is_err());

        // The same forge with the current version decodes, so the rejection
        // above was the version's doing, not the forge helper's.
        let right = forge(
            &codec,
            &CursorEnvelope {
                version: CURSOR_FORMAT_VERSION,
                scope,
                generation: generation_token.clone(),
                state: HistoryCursor { next_row: 6 },
            },
        );
        let decoded: DecodedCursor<HistoryCursor> = codec.decode(&right).unwrap();
        assert_eq!(decoded.state.next_row, 6);
        assert_eq!(decoded.generation, generation_token);
    }

    /// Padding for the oversized-cursor test: a state `encode` would balloon
    /// past the wire ceiling.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BigState {
        pad: String,
    }

    #[test]
    fn cursor_rejects_over_512_bytes_before_decode() {
        let codec = codec();
        let scope = codec.scope_for_target(None, Path::new("/fixtures/oversized"));
        let generation_token = generation("history-v1:8");

        // Correctly signed, correctly versioned, correctly encoded: the ONLY
        // thing wrong with this cursor is its length, so rejection proves the
        // length guard itself, not some later check.
        let oversized = forge(
            &codec,
            &CursorEnvelope {
                version: CURSOR_FORMAT_VERSION,
                scope,
                generation: generation_token.clone(),
                state: BigState {
                    pad: "x".repeat(600),
                },
            },
        );
        assert!(oversized.len() > MAX_ENCODED_CURSOR_LEN);
        assert!(codec.decode::<BigState>(&oversized).is_err());

        // The identical construction under the limit decodes: size alone
        // was the failure above.
        let small = forge(
            &codec,
            &CursorEnvelope {
                version: CURSOR_FORMAT_VERSION,
                scope,
                generation: generation_token,
                state: BigState {
                    pad: "x".repeat(32),
                },
            },
        );
        assert!(
            small.len() <= MAX_ENCODED_CURSOR_LEN,
            "fixture must stay under the limit: {} bytes",
            small.len()
        );
        let decoded: DecodedCursor<BigState> = codec.decode(&small).unwrap();
        assert_eq!(decoded.state.pad.len(), 32);

        // And `encode` refuses to mint what `decode` would refuse to read.
        assert!(codec
            .encode(
                scope,
                &generation("history-v1:8"),
                &BigState {
                    pad: "x".repeat(600)
                }
            )
            .is_err());
    }

    // ---- the reusable drift gate (plan Task 3, Step 6) ----------------------

    #[test]
    fn require_same_generation_returns_conflict_on_move() {
        let pinned = generation("history-v1:41");
        let same = generation("history-v1:41");
        assert!(require_same_generation(&pinned, &same).is_ok());

        // A moved generation is HTTP 409 with the exact "history moved"
        // wording — the signal the frontend keys "restart at page 1" on.
        let moved = generation("history-v1:42");
        let (status, message) =
            require_same_generation(&pinned, &moved).expect_err("a moved generation must conflict");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(message, "history moved");
    }
}
