//! The tag endpoints: `GET /api/tags` (the listing, M2.21b #236), the two
//! **local** tag writes `POST /api/tag` and `POST /api/delete-tag` (M2.21d
//! #238, ADR 0048), and the two **remote** tag writes `POST /api/push-tag`
//! and `POST /api/delete-remote-tag` (M2.21f #240).
//!
//! The read half of M2.21: what `GET /api/frame`'s ref badges throw away.
//! `read_refs` peels every `refs/tags/*` straight to a commit, so a badge can
//! say *that* a tag exists but never whether it is lightweight or annotated,
//! nor who tagged it or why. This endpoint answers all of that, as the
//! [`TagDetail`] DTO M2.21a (#235, ADR 0041) landed ahead of its producer.
//!
//! # Two boundaries meet here
//!
//! [`git_vista_git::read_tags`] produces raw, git-shaped facts (a
//! [`TagRecord`]); this module is the *only* place they become wire DTOs.
//! Keeping the mapping here rather than in `git-vista-git` is what lets that
//! crate stay free of a `git-vista-protocol` dependency, and it puts every
//! validated-newtype decision — which tags can be represented at all, what
//! happens to an over-long message — in one reviewable function,
//! [`tag_detail`].
//!
//! # Absence is modelled as absence
//!
//! A lightweight tag has no tag object, no tagger and no message; all three
//! are `null` on the wire, never `""`. A UI that renders an empty string in a
//! "Tagger" field shows a blank tagger, which is a different (and false)
//! claim from "this kind of tag has no tagger".
//!
//! # One spawn path, added for real signature verification (M2.21c, #237)
//!
//! `read_tags` itself still spawns nothing: it opens the repository once with
//! `gix::open_opts(.., isolated())` — the same posture as every other
//! `git-vista-git` read — and decodes each tag object out of the mapped object
//! database, including whether a PGP armour block is present at all. But
//! *whether that armour checks out* is not something `gix` (or this crate)
//! computes; only `gpg`, via `git verify-tag`, can say GOODSIG from BADSIG
//! from "no key to check against". [`verify_tag_signature`] therefore runs
//! one `git verify-tag --raw <name>` per **signed** tag — never per unsigned
//! or lightweight tag — through [`crate::git_cmd::git_output`], the same
//! `NetworkNeed::Local` chokepoint every other local read in this crate uses
//! (`git_cmd::rev_parse`, `is_ancestor`, `git_ref_exists`), which resolves to
//! the one sealed launcher, `sandbox::spawn::command_async`. Not a new spawn
//! path — the existing one, declared honestly: verify-tag reads the object
//! database and the operator's GPG keyring, never a remote.
//!
//! That keyring read is where the sandbox's own posture becomes visible in
//! the result: `~/.gnupg` is one of `sandbox::DEFAULT_SECRET_EXCLUDES`, so on
//! an untrusted repository (`Tier::Strict`, the default) `gpg` cannot open
//! it. See [`verify_tag_signature`]'s doc comment for what that does to the
//! answer.
//!
//! The two write handlers keep the pre-existing shape: like every other write
//! handler since M1.06b (#143) they validate their request, build one typed
//! [`GitOperation`], and hand it to [`crate::planner`] — the one place a
//! *mutating* git argv is constructed. `git tag` / `git tag -d` run in
//! `planner::exec_create_tag` / `planner::exec_delete_local_tag`, not here;
//! `verify_tag_signature`'s spawn is read-only and outside that path.

use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use git_vista_git::TagRecord;
use git_vista_protocol::dto::{
    CreateTagRequest, DeleteRemoteTagRequest, DeleteTagRequest, PushTagRequest, SignatureStatus,
    TagDetail, TagKind,
};
use git_vista_protocol::plan::{
    CommitOid, TagAnnotation, TagMessage, TagName, MAX_TAG_MESSAGE_LEN,
};
use git_vista_protocol::GitOperation;

use crate::handlers::read::{resolve_repo, RepoQuery};
use crate::planner;
use crate::state::reject_if_read_only;

/// What says a tag message was cut, so the cut is visible to whoever reads it.
///
/// A prefix that silently *looks* like the whole message is the failure this
/// exists to prevent: [`TagDetail`] has no `truncated` flag (it is a
/// `deny_unknown_fields` contract that shipped in M2.21a), so the only place
/// left to be honest is inside the display text itself.
///
/// Stored *without* a leading separator so it can also stand alone as the
/// whole message — see [`fit_message`] for the case where nothing survived the
/// reader's cap. When it follows retained text it is joined with
/// [`TRUNCATION_SEPARATOR`], which reproduces the wire text byte for byte.
const TRUNCATION_NOTE: &str =
    "[git-vista: this tag's message is longer than 16 KiB; it was cut here]";

/// What separates retained message text from [`TRUNCATION_NOTE`]: a blank
/// line, so the note reads as its own paragraph and never runs on from the
/// last retained line.
const TRUNCATION_SEPARATOR: &str = "\n\n";

/// Every tag in the repository, sorted by name.
///
/// `no-store` like the other live reads: a tag can be created or deleted at
/// any moment by git outside the app, so a cached listing is a wrong listing.
/// Note where that guarantee actually comes from through the router:
/// `security::require_auth` **overwrites** `Cache-Control` with `no-store` on
/// every authenticated API response, so a router-level assertion cannot tell
/// this line from its absence. The header is still set here, matching every
/// sibling read handler, and `the_handler_itself_marks_the_listing_no_store`
/// tests it where it *is* observable — by calling the handler directly.
///
/// A `SessionRequired` **read** (see `route_authz.rs`): it is registered on
/// both listeners, because a tag listing discloses exactly what the ref badges
/// on `/api/frame` already do — committed, published history — and nothing
/// about the working tree. That is the same line ADR 0005 draws for
/// `/api/status` (worktree contents: loopback only) versus `/api/frame`.
pub(crate) async fn tag_list(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    // `read_tags` opens a repository and mmaps packs — blocking work, so it
    // goes off the async workers exactly as `read_head_branch` does in
    // `planner::current_branch`. Cloned rather than moved: `repo` is needed
    // again below, to run `git verify-tag` against the same path.
    let read_repo = repo.clone();
    let records = tokio::task::spawn_blocking(move || git_vista_git::read_tags(&read_repo))
        .await
        .map_err(|e| {
            eprintln!("git-vista: /api/tags panicked while reading tags: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't read the repository's tags.".to_string(),
            )
        })?
        .map_err(|e| {
            eprintln!("git-vista: /api/tags couldn't read tags: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't read the repository's tags: {e}"),
            )
        })?;

    // `tag_detail` gives every signed tag a structural `Unverifiable`
    // placeholder (see its doc comment); this is where that placeholder
    // becomes `verify_tag_signature`'s real answer. Sequential on purpose —
    // one `git verify-tag` per **signed** tag, never per unsigned or
    // lightweight one, and a repository's tag count is not attacker-chosen
    // input this endpoint needs to bound concurrency against.
    let mut tags: Vec<TagDetail> = Vec::with_capacity(records.len());
    for record in &records {
        let Some(mut detail) = tag_detail(record) else {
            continue;
        };
        if record.signed {
            detail.signature = verify_tag_signature(&repo, &detail.name).await;
        }
        tags.push(detail);
    }
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(tags)))
}

/// Create a tag in the served repository (`POST /api/tag`, M2.21d #238, ADR
/// 0048): `git tag <name> <commit>` (lightweight) or `git tag -a -m <message>
/// <name> <commit>` (annotated), via [`GitOperation::CreateTag`].
///
/// # The refusal that matters is the empty annotation
///
/// A body carrying `"message": ""` (or nothing but whitespace) is asking for
/// an annotated tag with no text. On a terminal, `git tag -a` answers that by
/// opening `$EDITOR`; this server is headless, so the same request would hand
/// git a process nobody can ever finish. It is refused **here**, with words,
/// before an operation exists — a 400, not a hung request. Note that this is
/// the *only* way the shape can arise: `message: None` is a lightweight tag,
/// and a non-empty message becomes a [`TagMessage`], so once past this handler
/// "annotated" and "has a message" are the same fact (ADR 0048).
///
/// Everything else is git's own job, the B3 posture [`create_branch`] takes:
/// git validates the ref name, refuses a name that already exists, and its
/// stderr is forwarded verbatim. The two checks made here are the ones that
/// must happen before a value reaches an argv at all — non-empty, and not
/// option-shaped.
///
/// [`create_branch`]: crate::handlers::branch::create_branch
pub(crate) async fn create_tag(Json(req): Json<CreateTagRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let name = req.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't be empty.".to_string(),
        );
    }
    if name.starts_with('-') {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't start with '-'.".to_string(),
        );
    }
    let name = match TagName::new(name) {
        Ok(name) => name,
        // Unreachable after the two checks above; kept total rather than panic.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };

    let annotation = match annotation_for(req.message.as_deref(), req.sign) {
        Ok(annotation) => annotation,
        Err(refusal) => return (StatusCode::BAD_REQUEST, refusal),
    };

    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };
    // The operation pins an exact commit id, like `/api/branch`: the UI sends
    // the tapped node's full oid, and a symbolic or abbreviated start point in
    // a hand-crafted request is resolved first.
    let target = match planner::resolve_commit_oid(&repo, req.commit.trim()).await {
        Ok(target) => target,
        Err(refused) => return refused,
    };
    planner::plan_and_execute(GitOperation::CreateTag {
        name,
        target,
        annotation,
    })
    .await
}

/// Turn a request's `message`/`sign` pair into the operation's optional
/// [`TagAnnotation`], or into the words a refusal should carry.
///
/// A pure function on purpose: this is the whole of the "no editor" decision
/// on the request side, and [`create_tag`] cannot be called in a test without
/// a registered process-global selection (`state::CURRENT`), so testing the
/// decision *through* the handler would mean either mutating shared state or
/// not testing it. Same split, same reasoning as `git_cmd::redact_if_remote`.
///
/// The three shapes and their answers:
///
/// * **absent** — a lightweight tag. No annotation, no object, no message.
/// * **present and blank** — refused. This is the editor-shaped request: it
///   asks for an annotation whose text was never supplied. Note it is refused
///   rather than *downgraded* to lightweight, which is the tempting lenient
///   reading: the caller asked for release notes, so quietly producing a tag
///   without them is a wrong outcome, not a forgiving one.
/// * **present with text** — an annotated tag, `sign` carried through for the
///   executor to attempt (M2.21e, #239): a real `git tag -s`, which this
///   server's own sandbox reliably fails today with a typed, actionable
///   reason rather than a silent drop or a raw stderr dump — see
///   `planner::classify_sign_failure`'s doc comment for the mechanism.
///
/// `sign` without a message is refused separately: a signature lives *in* the
/// tag object, so a signed lightweight tag is not a thing git can make. The
/// typed vocabulary already makes that unrepresentable ([`TagAnnotation`]
/// nests `sign` inside the annotation); this is the wire-side half of the same
/// rule, and it exists so the caller gets a sentence instead of watching a
/// `sign: true` they sent be silently dropped on the floor.
fn annotation_for(message: Option<&str>, sign: bool) -> Result<Option<TagAnnotation>, String> {
    let annotation = match message.map(str::trim) {
        None => None,
        Some("") => {
            return Err(
                "An annotated tag needs a message — this server has no editor to \
                        open for one. Send the message, or omit it for a lightweight tag."
                    .to_string(),
            )
        }
        // Reachable failure: `TagMessage` is capped at `MAX_TAG_MESSAGE_LEN`.
        Some(text) => Some(TagAnnotation {
            message: TagMessage::new(text).map_err(|e| e.to_string())?,
            sign,
        }),
    };
    if sign && annotation.is_none() {
        return Err("A signed tag is an annotated tag — send a message with it.".to_string());
    }
    Ok(annotation)
}

/// Delete a **local** tag (`POST /api/delete-tag`, M2.21d #238, ADR 0048):
/// `git tag -d <tag>` via [`GitOperation::DeleteLocalTag`].
///
/// Local only. Deleting the tag from a remote is
/// [`GitOperation::DeleteRemoteTag`] — a separate operation, on its own
/// route ([`delete_remote_tag`], `POST /api/delete-remote-tag`, M2.21f
/// #240), because it opens a socket with credentials on it. A caller who
/// deletes here and expects the remote to follow is wrong, but they are
/// wrong in the safe direction: nothing left the machine.
///
/// The plan this builds carries [`RiskLevel::Destructive`] and a
/// [`RecoveryStrategy::RecreateTag`] pinned to the tag ref's *unpeeled* value;
/// see ADR 0048 for why that one oid is what makes the undo an exact
/// restoration rather than a re-authored look-alike, and why the recovery ref
/// written from it is what keeps the tagged commit alive against `git gc`.
///
/// [`RiskLevel::Destructive`]: git_vista_protocol::RiskLevel::Destructive
/// [`RecoveryStrategy::RecreateTag`]: git_vista_protocol::RecoveryStrategy::RecreateTag
pub(crate) async fn delete_tag(Json(req): Json<DeleteTagRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let tag = req.tag.trim();
    if tag.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't be empty.".to_string(),
        );
    }
    if tag.starts_with('-') {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't start with '-'.".to_string(),
        );
    }
    let name = match TagName::new(tag) {
        Ok(name) => name,
        // Unreachable after the two checks above; kept total rather than panic.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };
    planner::plan_and_execute(GitOperation::DeleteLocalTag { name }).await
}

/// Publish a tag to a configured remote (`POST /api/push-tag`, M2.21f #240):
/// `git push <remote> refs/tags/<name>` via [`GitOperation::PushTag`].
///
/// # Never `--tags`, never `--force`
///
/// This endpoint can publish exactly the one tag it names. Publishing every
/// local tag, or force-overwriting one that already differs on the remote,
/// is not an operation this vocabulary can express — see `PushTag`'s doc in
/// plan.rs for why both are structurally absent rather than merely unused
/// here.
///
/// The two gates before the name reaches a validated [`TagName`] are the
/// same ones [`create_tag`] and [`delete_tag`] already apply — non-empty,
/// not option-shaped — and `remote` clears
/// [`crate::handlers::fetch::validate_remote`], the one gate every
/// remote-naming endpoint in this server shares.
pub(crate) async fn push_tag(Json(req): Json<PushTagRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let tag = req.tag.trim();
    if tag.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't be empty.".to_string(),
        );
    }
    if tag.starts_with('-') {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't start with '-'.".to_string(),
        );
    }
    let name = match TagName::new(tag) {
        Ok(name) => name,
        // Unreachable after the two checks above; kept total rather than panic.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };
    let remote = match crate::handlers::fetch::validate_remote(&req.remote) {
        Ok(remote) => remote,
        Err(refused) => return refused,
    };
    planner::plan_and_execute(GitOperation::PushTag { name, remote }).await
}

/// Delete a tag from a configured remote (`POST /api/delete-remote-tag`,
/// M2.21f #240): `git push <remote> --delete refs/tags/<name>` via
/// [`GitOperation::DeleteRemoteTag`].
///
/// The **local** counterpart is [`delete_tag`] — a separate operation on a
/// separate route, because this one opens a socket with credentials on it.
/// Deleting here never touches the local tag; a caller that wants both
/// calls both.
pub(crate) async fn delete_remote_tag(
    Json(req): Json<DeleteRemoteTagRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let tag = req.tag.trim();
    if tag.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't be empty.".to_string(),
        );
    }
    if tag.starts_with('-') {
        return (
            StatusCode::BAD_REQUEST,
            "Tag name can't start with '-'.".to_string(),
        );
    }
    let name = match TagName::new(tag) {
        Ok(name) => name,
        // Unreachable after the two checks above; kept total rather than panic.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };
    let remote = match crate::handlers::fetch::validate_remote(&req.remote) {
        Ok(remote) => remote,
        Err(refused) => return refused,
    };
    planner::plan_and_execute(GitOperation::DeleteRemoteTag { name, remote }).await
}

/// Map one [`TagRecord`] onto the wire DTO, or `None` when it cannot be
/// represented at all.
///
/// # Why this returns an `Option`
///
/// [`TagName`] and [`CommitOid`] are *validated* newtypes: a name that is
/// option-shaped (`-d`) or an id that is not hex cannot exist as a value.
/// Those shapes are not reachable through `git tag`, but they are reachable
/// through `git update-ref` and a hand-written ref file, and a repository is
/// allowed to be strange. The alternatives were both worse: `unwrap` turns a
/// strange repository into a 500 for the whole listing, and inventing a
/// substitute name would put a tag on screen that does not exist under that
/// name. Skipping one tag, loudly on stderr, keeps the other tags readable —
/// the same call `read_refs` makes for a ref that will not peel.
///
/// # The signature field, and why it is not always `Unsigned`
///
/// #236's scope note says to hardcode [`SignatureStatus::Unsigned`] as a
/// placeholder until M2.21c runs real verification. That is right for a tag
/// with **no signature block** and wrong for one that has it: `Unsigned` is
/// documented as "carries no signature at all", so reporting it for a signed
/// tag is a false negative in exactly the direction
/// [`SignatureStatus`]'s own doc warns about. A tag whose object *does* carry
/// armour gets [`SignatureStatus::Unverifiable`] — "a signature is present but
/// verification could not run at all" — which is exactly this function's own
/// truth: `tag_detail` never invokes gpg itself (the presence bit comes from
/// the object parser splitting the armour off the message), so from this
/// function's point of view verification genuinely has not run yet. It is a
/// **placeholder**, not the caller's last word: [`tag_list`] overwrites it
/// with [`verify_tag_signature`]'s real answer for every tag reported signed
/// here, and never touches one reported [`SignatureStatus::Unsigned`] — so
/// this function still only ever narrows toward the signed minority and
/// never has to widen `Unsigned` back out.
fn tag_detail(record: &TagRecord) -> Option<TagDetail> {
    let name = match TagName::new(&record.name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!(
                "git-vista: tag {:?} cannot be named on the wire ({e}); not listed",
                record.name
            );
            return None;
        }
    };
    let target = match CommitOid::new(&record.target.0) {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "git-vista: tag {:?} has an unusable target {:?} ({e}); not listed",
                record.name, record.target.0
            );
            return None;
        }
    };
    let tag_object = match &record.tag_object {
        None => None,
        Some(oid) => match CommitOid::new(&oid.0) {
            Ok(o) => Some(o),
            Err(e) => {
                eprintln!(
                    "git-vista: tag {:?} has an unusable tag object {:?} ({e}); not listed",
                    record.name, oid.0
                );
                return None;
            }
        },
    };
    Some(TagDetail {
        name,
        kind: if record.annotated {
            TagKind::Annotated
        } else {
            TagKind::Lightweight
        },
        target,
        tag_object,
        tagger: record.tagger.clone(),
        // Both halves of the reader's answer go in, not just the text: a
        // record can carry `message: None` *and* `message_truncated: true`
        // (see `fit_message`), and `.as_deref().and_then(..)` would drop the
        // truncation fact on the floor for exactly that record.
        message: fit_message(record.message.as_deref(), record.message_truncated),
        signature: if record.signed {
            SignatureStatus::Unverifiable
        } else {
            SignatureStatus::Unsigned
        },
    })
}

/// Run `git verify-tag --raw <name>` and classify the result — the real
/// signature check M2.21c (#237) owns, replacing the `Unverifiable`
/// placeholder [`tag_detail`] gives every signed tag before this runs.
///
/// Only called (by [`tag_list`]) for a tag [`TagRecord::signed`] already
/// found `true`: an unsigned tag has nothing for `verify-tag` to check, and
/// calling it anyway would spawn a process this server never needs to spawn.
///
/// # Goes through the same chokepoint as every other git this server runs
///
/// [`crate::git_cmd::git_output`] — not a new spawn path — which resolves to
/// `sandbox::spawn::command_async` exactly like every other `Local`-declared
/// read here (`git_cmd::rev_parse`, `is_ancestor`, `git_ref_exists`).
/// `verify-tag` never reaches a remote — it reads the object database and,
/// for the `gpg` subprocess it launches internally, the operator's GPG
/// keyring — so `NetworkNeed::Local` is the truthful declaration, exactly
/// like `rev_parse`'s.
///
/// # The sandbox denies gpg its keyring, on purpose, in the tier this runs at
///
/// A tag read on an untrusted repository (the default) runs the `Strict`
/// tier, whose `$HOME` grant withholds `~/.gnupg` via
/// `sandbox::DEFAULT_SECRET_EXCLUDES` — the very keyring `verify-tag`'s
/// internal `gpg` needs to resolve a signer's public key. Measured *outside*
/// the sandbox, directly against a real signed tag with `GNUPGHOME` pointed
/// at an inaccessible directory: `gpg` still emits a well-formed
/// `ERRSIG`/`NO_PUBKEY` pair rather than failing outright, which this
/// function classifies as [`SignatureStatus::UnknownKey`] — a true statement
/// ("nothing can be said about the bytes either way") even though the actual
/// cause is sandbox policy, not an absent key.
///
/// **What the exact status is *inside* `Tier::Strict` is inferred, not
/// separately measured.** `Strict`'s seccomp filter denies
/// `socket(AF_UNIX)` unconditionally (`bin/gv-sandbox/seccomp_filter.rs`) —
/// the call `gpg` makes to reach `gpg-agent` at all — so `gpg` may never get
/// as far as emitting a status-protocol line, in which case this classifies
/// as [`SignatureStatus::Unverifiable`] (no recognised line) rather than
/// `UnknownKey`. Both are honest answers to "can this be verified here", and
/// either way the load-bearing claim holds: for the common case (an
/// untrusted repository), **no tag can ever classify as one of the verdicts
/// that require the crypto to have run** — [`Valid`], [`Invalid`], or any of
/// the three #335 added ([`ValidExpiredKey`], [`ValidExpiredSignature`],
/// [`Revoked`]). Every signed tag reports `UnknownKey` or `Unverifiable`
/// regardless of whether the signature is genuine, unless the operator has
/// explicitly trusted the repository (`Tier::Unsandboxed`, which applies no
/// exclude at all). This is a real, reachable limitation of the current
/// sandbox posture, not a bug in this function's parsing.
///
/// # Known cost, not yet addressed here (flagged for follow-up)
///
/// This runs once per **signed** tag, sequentially, inline in a `GET`
/// handler — so on the common untrusted-repository path it is N sandboxed
/// spawns (bwrap + Landlock + seccomp each) that are provably going to
/// answer `UnknownKey` before a single one of them runs, on a repository
/// that could carry thousands of release tags. A guard ahead of the loop —
/// untrusted repository skips straight to `UnknownKey`, no spawn — would
/// remove all of them, but the trust check this would need
/// (`sandbox::repo_is_trusted`) is a private `fn` in `sandbox::mod`, not
/// reachable from this module today, and widening a security-sensitive
/// visibility boundary is out of scope for this change. Left as drafted;
/// the cost is real and should be fixed by whoever next touches this path,
/// not silently absorbed.
///
/// [`Valid`]: SignatureStatus::Valid
/// [`Invalid`]: SignatureStatus::Invalid
/// [`ValidExpiredKey`]: SignatureStatus::ValidExpiredKey
/// [`ValidExpiredSignature`]: SignatureStatus::ValidExpiredSignature
/// [`Revoked`]: SignatureStatus::Revoked
async fn verify_tag_signature(repo: &std::path::Path, name: &TagName) -> SignatureStatus {
    let output =
        match crate::git_cmd::git_output(repo, &["verify-tag", "--raw", name.as_str()]).await {
            Ok(output) => output,
            Err(e) => {
                eprintln!(
                    "git-vista: couldn't run `git verify-tag` for {:?}: {e}",
                    name.as_str()
                );
                return SignatureStatus::Unverifiable;
            }
        };
    // `--raw` writes gpg's status-protocol lines to stderr, never stdout —
    // see `git help verify-tag`. Lossy on purpose: the status protocol is
    // ASCII keywords and hex, so a non-UTF-8 byte here would be gpg's own
    // corruption, not information this classification needs.
    classify_verify_tag_output(&output.stderr)
}

/// Every gpg status keyword `classify_verify_tag_output` **acts on**, in
/// resolution order: the first entry whose keyword appears anywhere in the run
/// decides the verdict, whatever order the lines actually arrived in.
///
/// Ordered most-alarming-first, and that ordering is the whole contract:
///
/// * `BADSIG` wins unconditionally — see the classifier's own doc for why a
///   provably forged signature may never be downgraded by a later line.
/// * `REVKEYSIG` outranks the two expiries and `GOODSIG`. All four mean the
///   bytes checked out; a revoked key is the one among them whose owner has
///   published that it should no longer be trusted, so it may never be
///   reported as the milder fact.
/// * `EXPKEYSIG` outranks `EXPSIG`: a key that expired invalidates every
///   signature it ever made, while a signature expiry is one signature's own
///   time-box, so the key-level fact is the larger one to report.
/// * `NO_PUBKEY` sits *below* `GOODSIG` — the pre-#335 behaviour, preserved
///   deliberately: gpg emits `NO_PUBKEY` for a *second*, unrelated key in some
///   runs, and a run that produced a good signature has answered the question.
///
/// Real gpg emits exactly one of the five sig-level lines per signature, so on
/// real input the order is unobservable. It is pinned anyway because the
/// function makes no assumption about line order, and a table whose rows can be
/// swapped without a test failing is not a contract.
const VERDICT_PRECEDENCE: &[(&str, SignatureStatus)] = &[
    ("BADSIG", SignatureStatus::Invalid),
    ("REVKEYSIG", SignatureStatus::Revoked),
    ("EXPKEYSIG", SignatureStatus::ValidExpiredKey),
    ("EXPSIG", SignatureStatus::ValidExpiredSignature),
    ("GOODSIG", SignatureStatus::Valid),
    ("NO_PUBKEY", SignatureStatus::UnknownKey),
];

/// Every gpg status keyword `classify_verify_tag_output` **deliberately
/// absorbs** — carries no verdict, so seeing one changes nothing.
///
/// # Why this list exists at all (#335)
///
/// Before #335 the classifier's `match` ended in a bare `_ => {}`, so a status
/// keyword nobody had thought about was indistinguishable from one that had
/// been considered and dismissed. `REVKEYSIG` — "the signature is good, and
/// the key that made it has been revoked" — went through that arm for two
/// milestones and surfaced as [`SignatureStatus::Unverifiable`], i.e. "we
/// could not check", for the single most alarming thing gpg can say about a
/// signature it *did* check.
///
/// A `match` cannot be made exhaustive over a vocabulary that lives in another
/// project's source, so the fallthrough stays — but it is no longer silent.
/// Anything outside this census and [`VERDICT_PRECEDENCE`] is reported on the
/// server's stderr by [`classify_verify_tag_output`], and
/// `every_status_line_in_every_fixture_is_acted_on_or_censused` fails the
/// build the moment a committed fixture carries one.
///
/// # Provenance
///
/// Every keyword below is a real status keyword of the gpg this project
/// targets — each was read out of the shipped `gpg` 2.4.4 binary's own string
/// table, not written from memory — and the ones marked *(observed)* also
/// appear verbatim in a fixture committed in this file's test module.
const ABSORBED_GPG_STATUS: &[&str] = &[
    // Framing and identification. (observed: all four)
    "NEWSIG",
    "KEY_CONSIDERED",
    "SIG_ID",
    "VALIDSIG",
    // The trust *computation*, which is a statement about the keyring's web of
    // trust and not about these bytes. gpg emits exactly one of these five per
    // verified signature; `TRUST_ULTIMATE` is the one a fixture here observes,
    // because a locally generated signing key is ultimately trusted by
    // construction. Deliberately absorbed rather than acted on: this surface
    // reports what the *cryptography* said, and ADR 0088 records the choice not
    // to fold trust into it. (observed: TRUST_ULTIMATE)
    "TRUST_UNDEFINED",
    "TRUST_NEVER",
    "TRUST_MARGINAL",
    "TRUST_FULLY",
    "TRUST_ULTIMATE",
    // Key-lifetime notes that accompany, and are subordinate to, the sig-level
    // line. `KEYEXPIRED` rides along with `EXPKEYSIG` and `KEYREVOKED` with
    // `REVKEYSIG`, but either can also describe a *different* key gpg
    // considered on the way, so neither may be read as a verdict on its own —
    // the sig-level line in `VERDICT_PRECEDENCE` is the one that names the key
    // that actually made this signature. (observed: both)
    "KEYEXPIRED",
    "KEYREVOKED",
    // "gpg could not finish", with no statement about the bytes. `ERRSIG` is
    // the partner of `NO_PUBKEY` (which *is* acted on) and, alone, means only
    // that verification did not complete — which is what an empty verdict
    // already reports as `Unverifiable`. (observed: both)
    "ERRSIG",
    "FAILURE",
    // Signature metadata carried by the signature itself. Displayed by gpg,
    // ignored here: none of it is a verdict.
    "NOTATION_NAME",
    "NOTATION_DATA",
    "NOTATION_FLAGS",
    "POLICY_URL",
    "VERIFICATION_COMPLIANCE_MODE",
    "PROGRESS",
];

/// Classify one `git verify-tag --raw` run from its captured stderr.
///
/// Split from [`verify_tag_signature`] so the mapping is testable against
/// **exact bytes gpg was measured to emit** — for a genuine good signature, a
/// tampered one (same key, altered tag content), a signature checked with no
/// matching public key in the keyring, and (#335) one made by a key that has
/// since expired, one whose own expiry has passed, and one made by a revoked
/// key — rather than requiring a real gpg keypair and a spawned process in
/// this crate's test suite.
///
/// # Why `BADSIG` is checked first and wins unconditionally
///
/// This is the distinction [`verify_tag_signature`] exists for: a `BADSIG`
/// line means gpg ran, found the claimed key, and the bytes provably do
/// **not** match — a forged or corrupted signature. `NO_PUBKEY`/`ERRSIG`
/// mean the opposite kind of "no" — gpg never got far enough to say anything
/// about the bytes at all. Collapsing those into one status, or letting an
/// unrelated line downgrade a `BADSIG` to `UnknownKey`, is exactly the
/// false-negative this exists to prevent (a forged tag reported as merely
/// "unverifiable" reads as far less alarming than what it is). So this scans
/// every line before deciding, and `BADSIG` outranks everything else
/// regardless of what order the lines arrived in — as the first row of
/// [`VERDICT_PRECEDENCE`], which generalises that rule to all six verdicts.
///
/// # The fallthrough is deliberate, and no longer silent (#335)
///
/// gpg's status protocol is another project's vocabulary and grows without
/// asking us, so a keyword this build has never heard of must not stop a tag
/// from being described. It still classifies as
/// [`Unverifiable`](SignatureStatus::Unverifiable) — but it is now *named* on
/// the server's stderr, and [`ABSORBED_GPG_STATUS`] records every keyword that
/// reaches the fallthrough on purpose. That is the difference between "we
/// considered this and it carries no verdict" and "nobody ever looked", which
/// is the confusion `REVKEYSIG` hid behind for two milestones.
fn classify_verify_tag_output(stderr: &[u8]) -> SignatureStatus {
    let (status, unrecognised) = classify_verify_tag_output_with_census(stderr);
    for keyword in unrecognised {
        // `{:?}` and the length cap, not `{}`: this string came out of a
        // subprocess reading a repository the operator may not trust, and the
        // server's stderr is a terminal.
        let shown: String = keyword.chars().take(MAX_LOGGED_STATUS_KEYWORD).collect();
        eprintln!(
            "git-vista: `git verify-tag --raw` emitted gpg status {shown:?}, which this build \
             neither acts on nor lists in ABSORBED_GPG_STATUS; the tag was classified {status:?}. \
             Please report this (git-vista#335)."
        );
    }
    status
}

/// How much of an unrecognised status keyword [`classify_verify_tag_output`]
/// will print. Real keywords are short `SCREAMING_SNAKE_CASE` words; a long one
/// means the line was not what this function thinks it is, and there is no
/// reason to spill it whole into the operator's terminal.
const MAX_LOGGED_STATUS_KEYWORD: usize = 32;

/// The whole of [`classify_verify_tag_output`]'s work, with the census it kept
/// on the way: the verdict, plus every status keyword that was neither acted on
/// nor deliberately absorbed, de-duplicated and in first-seen order.
///
/// Separate from its caller purely so the tests can assert on the census
/// without capturing stderr — the caller adds nothing but the reporting.
fn classify_verify_tag_output_with_census(stderr: &[u8]) -> (SignatureStatus, Vec<String>) {
    let text = String::from_utf8_lossy(stderr);
    // The index into `VERDICT_PRECEDENCE` of the most alarming verdict seen so
    // far; `None` until a line carries one. Lower index wins, so this is a
    // running minimum and line order cannot change the answer.
    let mut verdict: Option<usize> = None;
    let mut unrecognised: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(status) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        let keyword = status.split_whitespace().next().unwrap_or("");
        if let Some(rank) = VERDICT_PRECEDENCE.iter().position(|(k, _)| *k == keyword) {
            verdict = Some(verdict.map_or(rank, |best| best.min(rank)));
        } else if !ABSORBED_GPG_STATUS.contains(&keyword)
            && !keyword.is_empty()
            && !unrecognised.iter().any(|seen| seen == keyword)
        {
            unrecognised.push(keyword.to_string());
        }
    }
    let status = match verdict {
        Some(rank) => VERDICT_PRECEDENCE[rank].1,
        // No line carrying a verdict at all: gpg didn't run (missing binary,
        // misconfigured `gpg.program`) or produced output this function does
        // not understand. "Could not run/complete verification" is the honest
        // bucket for both — and if it was the second, `unrecognised` now names
        // what was not understood instead of swallowing it.
        None => SignatureStatus::Unverifiable,
    };
    (status, unrecognised)
}

/// Fit a tag's annotation into a [`TagMessage`], appending [`TRUNCATION_NOTE`]
/// whenever bytes were dropped.
///
/// Takes **both halves** of what [`git_vista_git::read_tags`] found: the text
/// it retained (`None` when nothing was worth keeping) and `already_truncated`,
/// its own byte-level fact from [`TagRecord::message_truncated`] — never
/// re-derived from this string's length, which has already been trimmed.
///
/// The three input shapes are genuinely different facts and each gets its own
/// answer:
///
/// * `(None, false)` — the tag has no annotation body. `None` on the wire.
/// * `(Some(text), _)` — the usual case: the text, with the note appended if
///   anything was cut.
/// * `(None, true)` — **the case this signature exists for.** The retained
///   prefix trimmed down to nothing while real bytes were dropped past the cap.
///   That happens when an annotation's first [`MAX_TAG_MESSAGE_LEN`] bytes are
///   all whitespace (reachable straight through porcelain:
///   `git tag -a --cleanup=verbatim -F`). Reporting `None` here would be
///   byte-identical on the wire to a tag with no annotation at all, so a tag
///   carrying megabytes of content would render as "no annotation" — a
///   stronger version of the very "prefix that reads like the whole message"
///   failure [`TRUNCATION_NOTE`] exists to prevent. The note therefore stands
///   alone as the entire message, and it leads (no separator), so a
///   first-line preview shows the note rather than a blank line.
fn fit_message(message: Option<&str>, already_truncated: bool) -> Option<TagMessage> {
    let retained = match (message, already_truncated) {
        (None, false) => return None,
        (None, true) => "",
        (Some(text), _) => text,
    };
    if !already_truncated && retained.len() <= MAX_TAG_MESSAGE_LEN {
        return TagMessage::new(retained).ok();
    }
    if retained.is_empty() {
        return TagMessage::new(TRUNCATION_NOTE).ok();
    }
    // Leave room for the note and its separator, and cut on a character
    // boundary so a multi-byte character is dropped whole rather than mangled.
    let mut end = MAX_TAG_MESSAGE_LEN
        .saturating_sub(TRUNCATION_NOTE.len() + TRUNCATION_SEPARATOR.len())
        .min(retained.len());
    while end > 0 && !retained.is_char_boundary(end) {
        end -= 1;
    }
    TagMessage::new(format!(
        "{}{TRUNCATION_SEPARATOR}{TRUNCATION_NOTE}",
        &retained[..end]
    ))
    .ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use git_vista_core::model::Oid;

    // -----------------------------------------------------------------------
    // Captured gpg status output — the six verification outcomes this
    // classifier distinguishes
    // -----------------------------------------------------------------------
    //
    // Every one of these is **verbatim stderr** from a real
    // `git verify-tag --raw` run (git 2.43, gpg 2.4.4), not a hand-written
    // approximation of one. Named as consts rather than inlined into a single
    // test each so that
    // `every_status_line_in_every_fixture_is_acted_on_or_censused` can sweep
    // the whole set: that guard is only worth having if it sees every shape of
    // real output this file knows about.

    /// A genuinely signed, unmodified tag with the signer's public key present
    /// in the keyring — the case that must classify `Valid`.
    const FIXTURE_GOODSIG: &[u8] =
        b"[GNUPG:] NEWSIG\n\
          [GNUPG:] KEY_CONSIDERED 55D729CA8C0B4F896D1053CC41815B16FFE44E12 0\n\
          [GNUPG:] SIG_ID 2J09+YBlXtuNyycCev8X5A6r47Q 2026-08-06 1786044942\n\
          [GNUPG:] GOODSIG 41815B16FFE44E12 Test Signer <signer@example.com>\n\
          [GNUPG:] VALIDSIG 55D729CA8C0B4F896D1053CC41815B16FFE44E12 2026-08-06 1786044942 0 4 0 22 10 00 55D729CA8C0B4F896D1053CC41815B16FFE44E12\n\
          [GNUPG:] TRUST_ULTIMATE 0 pgp\n";

    /// The same signature and the same known key, with a message byte flipped
    /// after signing — a **tampered** tag object.
    const FIXTURE_BADSIG: &[u8] = b"[GNUPG:] NEWSIG\n\
          [GNUPG:] KEY_CONSIDERED 55D729CA8C0B4F896D1053CC41815B16FFE44E12 0\n\
          [GNUPG:] BADSIG 41815B16FFE44E12 Test Signer <signer@example.com>\n\
          [GNUPG:] FAILURE gpg-exit 33554433\n";

    /// The genuine, untampered signature verified against an **empty
    /// keyring** — what this server's `Strict` tier produces for every signed
    /// tag on an untrusted repository.
    const FIXTURE_NO_PUBKEY: &[u8] =
        b"[GNUPG:] NEWSIG\n\
          [GNUPG:] ERRSIG 41815B16FFE44E12 22 10 00 1786044942 9 55D729CA8C0B4F896D1053CC41815B16FFE44E12\n\
          [GNUPG:] NO_PUBKEY 41815B16FFE44E12\n\
          [GNUPG:] FAILURE gpg-exit 33554433\n";

    /// #335, case 1. A key generated with a one-day lifetime under
    /// `gpg --faked-system-time 20260101T000000!`, used to sign a tag at that
    /// same frozen instant, then verified at real wall-clock time — so the
    /// signature is sound and the key expired months ago. Note `EXPKEYSIG`
    /// standing exactly where `GOODSIG` stands in [`FIXTURE_GOODSIG`]: that
    /// substitution is the entire difference, and it is why the pre-#335 code
    /// reporting `Valid` here was not an obviously wrong reading — merely a
    /// dishonest one.
    const FIXTURE_EXPKEYSIG: &[u8] =
        b"[GNUPG:] NEWSIG\n\
          [GNUPG:] KEYEXPIRED 1767312000\n\
          [GNUPG:] KEY_CONSIDERED 959E350133ED3193BCBD77ED13869CFAF336A08E 0\n\
          [GNUPG:] KEYEXPIRED 1767312000\n\
          [GNUPG:] SIG_ID SpX+uvn3TtT0vg7gHJmrCq/wL/I 2026-01-01 1767225600\n\
          [GNUPG:] EXPKEYSIG 13869CFAF336A08E Expired Key Signer <expkey@example.com>\n\
          [GNUPG:] VALIDSIG 959E350133ED3193BCBD77ED13869CFAF336A08E 2026-01-01 1767225600 0 4 0 22 10 00 959E350133ED3193BCBD77ED13869CFAF336A08E\n";

    /// #335, case 2. A key with a five-year lifetime, signing at the same
    /// frozen instant but through `gpg --default-sig-expire 1d` — so the
    /// **key** is still perfectly good today and the **signature** carries its
    /// own expiry, which has passed. The distinguishing fact is not in the
    /// `EXPSIG` line but one line below it: `VALIDSIG`'s third field is the
    /// signature's expiration timestamp, `1767312000` here against `0` (never)
    /// in every other fixture.
    const FIXTURE_EXPSIG: &[u8] =
        b"[GNUPG:] NEWSIG\n\
          [GNUPG:] KEY_CONSIDERED 030FA93A5DCE2EFE33EEBF7D75E72AD4EED63BE2 0\n\
          [GNUPG:] SIG_ID 04AU39FHJ6QKmPrPZdzxKb6D2s0 2026-01-01 1767225600\n\
          [GNUPG:] EXPSIG 75E72AD4EED63BE2 Expired Sig Signer <expsig@example.com>\n\
          [GNUPG:] VALIDSIG 030FA93A5DCE2EFE33EEBF7D75E72AD4EED63BE2 2026-01-01 1767225600 1767312000 4 0 22 10 00 030FA93A5DCE2EFE33EEBF7D75E72AD4EED63BE2\n\
          [GNUPG:] TRUST_ULTIMATE 0 pgp\n\
          [GNUPG:] FAILURE gpg-exit 33554433\n";

    /// #335, case 3, and the one the issue was filed for. A tag signed by a
    /// live, trusted key, after which the key's own revocation certificate was
    /// imported — the real shape of "the signer believes this key is
    /// compromised". gpg is emphatic: `REVKEYSIG` where `GOODSIG` would be,
    /// plus a standalone `KEYREVOKED`. Before #335 **neither** keyword was
    /// matched, so this classified `Unverifiable` — "we could not check" — for
    /// output in which gpg checked, succeeded, and then said the key must not
    /// be trusted.
    const FIXTURE_REVKEYSIG: &[u8] =
        b"[GNUPG:] NEWSIG\n\
          [GNUPG:] KEY_CONSIDERED 849A5B092888495017F2AB5BDD8E770D34A7C578 0\n\
          [GNUPG:] SIG_ID 8v4c2pA6lm5iszvv5eLYAwr3c+U 2026-08-26 1787730169\n\
          [GNUPG:] REVKEYSIG DD8E770D34A7C578 Revoked Key Signer <revkey@example.com>\n\
          [GNUPG:] VALIDSIG 849A5B092888495017F2AB5BDD8E770D34A7C578 2026-08-26 1787730169 0 4 0 22 10 00 849A5B092888495017F2AB5BDD8E770D34A7C578\n\
          [GNUPG:] KEYREVOKED\n\
          [GNUPG:] KEY_CONSIDERED 849A5B092888495017F2AB5BDD8E770D34A7C578 0\n\
          [GNUPG:] TRUST_ULTIMATE 0 pgp\n";

    /// Every captured fixture with the verdict it must produce — the table the
    /// sweep guards iterate.
    const ALL_FIXTURES: &[(&str, &[u8], SignatureStatus)] = &[
        ("GOODSIG", FIXTURE_GOODSIG, SignatureStatus::Valid),
        ("BADSIG", FIXTURE_BADSIG, SignatureStatus::Invalid),
        ("NO_PUBKEY", FIXTURE_NO_PUBKEY, SignatureStatus::UnknownKey),
        (
            "EXPKEYSIG",
            FIXTURE_EXPKEYSIG,
            SignatureStatus::ValidExpiredKey,
        ),
        (
            "EXPSIG",
            FIXTURE_EXPSIG,
            SignatureStatus::ValidExpiredSignature,
        ),
        ("REVKEYSIG", FIXTURE_REVKEYSIG, SignatureStatus::Revoked),
    ];

    /// What [`build_tagged_fixture`] planted, read back **out of git** so the
    /// router test can compare the response against git's own answers rather
    /// than against anything git-vista computed.
    pub(crate) struct TaggedFixture {
        /// The opaque worktree id the repository is addressable by (`?repo=`).
        pub repo_id: String,
        /// `HEAD` — what the lightweight tag marks.
        pub tip: String,
        /// `HEAD~1` — what the annotated tag's *peeled target* must be.
        pub root: String,
        /// The annotated tag's own object id (`rev-parse refs/tags/v1.0`).
        pub tag_object: String,
        /// The exact `tagger` header line git wrote into that object.
        pub tagger: String,
    }

    /// Build a repository with one lightweight and one annotated tag, register
    /// it in the catalog, and report both the addressable id and git's own
    /// view of what was planted.
    ///
    /// # Why this lives here and not in `main.rs`
    ///
    /// `main.rs` is the deliberate negative control for
    /// `argv_boundary::every_allowlist_entry_names_a_live_spawn_site` ("if
    /// `main.rs` ever starts spawning, this line will fail") and it is the one
    /// file `sandbox::escape_contract`'s R7 scans for the server's `GIT_*`
    /// surface. A `#[cfg(test)]` git fixture there would trip both guards for
    /// no reason. So the router test in `main.rs` calls this, and the
    /// `Command` + `GIT_*` env stay in a file the argv allowlist covers as
    /// test-fixture setup.
    ///
    /// The repository is addressed by `?repo=<id>` rather than by the default
    /// selection on purpose: id resolution reads the *catalog*, which only
    /// grows, so the test cannot be perturbed by another test in this binary
    /// moving the process-global `CURRENT` selection out from under it.
    pub(crate) fn build_tagged_fixture(dir: &std::path::Path) -> TaggedFixture {
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Ada Lovelace")
                .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada Lovelace")
                .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .env("GIT_AUTHOR_DATE", "@1753300000 +0000")
                .env("GIT_COMMITTER_DATE", "@1753300000 +0000")
                .status()
                .expect("git should be runnable")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        let git_out = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git should be runnable");
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        git(&["commit", "-q", "--allow-empty", "-m", "second"]);
        // One of each kind, in one repository, so a response cannot pass by
        // getting a single kind right.
        git(&["tag", "tip-marker", "HEAD"]);
        git(&["tag", "-a", "-m", "one\n\nrelease notes", "v1.0", "HEAD~1"]);

        crate::state::allow_repo_root(dir);
        let handle = crate::state::set_current(dir, git_vista_protocol::RepoMode::Active)
            .expect("the fixture registers in the catalog");

        TaggedFixture {
            repo_id: handle.worktree.to_string(),
            tip: git_out(&["rev-parse", "HEAD"]),
            root: git_out(&["rev-parse", "HEAD~1"]),
            tag_object: git_out(&["rev-parse", "refs/tags/v1.0"]),
            tagger: git_out(&["cat-file", "-p", "refs/tags/v1.0"])
                .lines()
                .find_map(|l| l.strip_prefix("tagger ").map(str::to_string))
                .expect("git wrote a tagger header"),
        }
    }

    /// The handler's own response, without the auth middleware in the way.
    ///
    /// Through the router, `security::require_auth` overwrites `Cache-Control`
    /// on every authenticated API response, so the router test cannot fail if
    /// this handler stops setting it — that mutation was tried and survived
    /// there. Calling the handler directly is where the header is the
    /// handler's own fact, and where breaking it goes red.
    #[tokio::test]
    async fn the_handler_itself_marks_the_listing_no_store() {
        crate::state::with_isolated_test_current(
            the_handler_itself_marks_the_listing_no_store_in_scope(),
        )
        .await;
    }

    async fn the_handler_itself_marks_the_listing_no_store_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = build_tagged_fixture(dir.path());
        let response = tag_list(axum::extract::Query(RepoQuery {
            repo: Some(fixture.repo_id),
        }))
        .await
        .expect("the fixture repository reads")
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    /// The same direct call, proving the handler really composes
    /// `read_tags` → `tag_detail` — not only that the router reaches it.
    #[tokio::test]
    async fn the_handler_composes_the_reader_and_the_mapping() {
        crate::state::with_isolated_test_current(
            the_handler_composes_the_reader_and_the_mapping_in_scope(),
        )
        .await;
    }

    async fn the_handler_composes_the_reader_and_the_mapping_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = build_tagged_fixture(dir.path());
        let response = tag_list(axum::extract::Query(RepoQuery {
            repo: Some(fixture.repo_id),
        }))
        .await
        .expect("the fixture repository reads")
        .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let body: Vec<TagDetail> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].kind, TagKind::Lightweight);
        assert_eq!(body[0].target.as_str(), fixture.tip);
        assert_eq!(body[1].kind, TagKind::Annotated);
        assert_eq!(body[1].target.as_str(), fixture.root);
        assert_eq!(
            body[1].tag_object.as_ref().map(|o| o.as_str()),
            Some(fixture.tag_object.as_str())
        );
    }

    /// A lightweight record, as `read_tags` would produce one.
    fn lightweight(name: &str) -> TagRecord {
        TagRecord {
            name: name.to_string(),
            annotated: false,
            target: Oid("2".repeat(40)),
            tag_object: None,
            tagger: None,
            message: None,
            message_truncated: false,
            signed: false,
        }
    }

    /// An annotated record with a tagger and a message.
    fn annotated(name: &str) -> TagRecord {
        TagRecord {
            name: name.to_string(),
            annotated: true,
            target: Oid("2".repeat(40)),
            tag_object: Some(Oid("8".repeat(40))),
            tagger: Some("Ada Lovelace <ada@example.com> 1753300000 +0000".to_string()),
            message: Some("first stable release".to_string()),
            message_truncated: false,
            signed: false,
        }
    }

    /// M2.21d (#238), the request-side half of the no-editor guarantee (ADR
    /// 0048): the only three shapes a body can carry, and the two that must
    /// be refused in words rather than acted on.
    ///
    /// The blank-message case is the one that matters. A "lenient" reading
    /// would drop the empty annotation and create a lightweight tag; the
    /// assertion here is that it is an `Err` instead, because the request
    /// asked for release notes and a tag without them is a different tag.
    /// Deserialising the same shapes off the wire is
    /// `dto::tests::tag_requests_roundtrip_and_cannot_ask_for_an_annotation_with_no_message`;
    /// this covers what the handler does with them afterwards.
    #[test]
    fn a_blank_annotation_is_refused_in_words_not_quietly_downgraded() {
        assert_eq!(
            annotation_for(None, false),
            Ok(None),
            "no message is a lightweight tag, not an error"
        );

        let annotated = annotation_for(Some("release notes"), false)
            .unwrap()
            .unwrap();
        assert_eq!(annotated.message.as_str(), "release notes");
        assert!(!annotated.sign);
        assert!(
            annotation_for(Some("release notes"), true)
                .unwrap()
                .unwrap()
                .sign,
            "a signing request rides through to the executor, which refuses it"
        );

        // Whitespace is trimmed before the newtype, so the stored message is
        // never padded — and a message that is *only* whitespace is the
        // editor-shaped request.
        assert_eq!(
            annotation_for(Some("  release notes  "), false)
                .unwrap()
                .unwrap()
                .message
                .as_str(),
            "release notes"
        );
        for blank in ["", "   ", "\n", " \t\n "] {
            let refusal = annotation_for(Some(blank), false)
                .expect_err("an annotation with no text must be refused");
            assert!(
                refusal.contains("needs a message"),
                "the refusal must say what is missing: {refusal}"
            );
        }
        // A signature has nowhere to live without a tag object.
        let refusal =
            annotation_for(None, true).expect_err("sign without a message must be refused");
        assert!(refusal.contains("annotated tag"), "{refusal}");
    }

    /// The mapping is asserted against **hand-written wire JSON**, not against
    /// `tag_detail`'s own output re-serialised — a round trip through the
    /// function that defines the mapping would bless any mapping at all.
    #[test]
    fn a_lightweight_tag_serialises_with_nulls_not_empty_strings() {
        let wire = serde_json::to_value(tag_detail(&lightweight("tip-marker")).unwrap()).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "name": "tip-marker",
                "kind": "lightweight",
                "target": "2".repeat(40),
                "tag_object": null,
                "tagger": null,
                "message": null,
                "signature": "unsigned",
            })
        );
    }

    #[test]
    fn an_annotated_tag_carries_its_object_tagger_and_message() {
        let wire = serde_json::to_value(tag_detail(&annotated("v1.0.0")).unwrap()).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "name": "v1.0.0",
                "kind": "annotated",
                "target": "2".repeat(40),
                "tag_object": "8".repeat(40),
                "tagger": "Ada Lovelace <ada@example.com> 1753300000 +0000",
                "message": "first stable release",
                "signature": "unsigned",
            })
        );
    }

    /// The deviation from #236's literal wording, pinned so it cannot be
    /// "fixed" back to a false negative without this test going red.
    #[test]
    fn a_signed_tag_is_unverifiable_not_unsigned() {
        let mut record = annotated("v-signed");
        record.signed = true;
        assert_eq!(
            tag_detail(&record).unwrap().signature,
            SignatureStatus::Unverifiable
        );
        // …and the unsigned twin still says `Unsigned`, so the test above
        // cannot pass by making every tag unverifiable.
        assert_eq!(
            tag_detail(&annotated("v-plain")).unwrap().signature,
            SignatureStatus::Unsigned
        );
    }

    #[test]
    fn a_tag_whose_name_cannot_ride_the_wire_is_skipped_not_renamed() {
        // `-d` is option-shaped, so `TagName` refuses it. Reachable via
        // `git update-ref refs/tags/-d <oid>`.
        assert!(tag_detail(&lightweight("-d")).is_none());
        // A name that only *contains* a dash is fine — the skip is narrow.
        assert!(tag_detail(&lightweight("v1.0-rc1")).is_some());
    }

    #[test]
    fn a_tag_with_an_unusable_target_is_skipped() {
        let mut record = lightweight("v1.0");
        record.target = Oid("not-a-hex-object-id".to_string());
        assert!(tag_detail(&record).is_none());

        let mut record = annotated("v2.0");
        record.tag_object = Some(Oid("zz".repeat(20)));
        assert!(tag_detail(&record).is_none());
    }

    #[test]
    fn an_over_long_message_is_cut_visibly_and_still_fits_the_newtype() {
        let mut record = annotated("v-long");
        record.message = Some("x".repeat(MAX_TAG_MESSAGE_LEN));
        record.message_truncated = true;

        let detail = tag_detail(&record).unwrap();
        let message = detail.message.expect("an over-long message is not dropped");
        assert!(
            message.as_str().ends_with(TRUNCATION_NOTE),
            "the cut has to be visible to whoever reads the message"
        );
        assert!(
            message.as_str().len() <= MAX_TAG_MESSAGE_LEN,
            "the result must satisfy TagMessage's own cap, or the newtype \
             would have refused it and the message would have vanished"
        );
        assert!(message.as_str().starts_with("xxxx"));
    }

    /// The flag is load-bearing on its own: a message short enough to fit can
    /// still be a prefix of a much longer one that the reader cut.
    #[test]
    fn the_truncation_flag_alone_triggers_the_note() {
        let mut record = annotated("v-short-but-cut");
        record.message = Some("only the first line".to_string());
        record.message_truncated = true;
        let message = tag_detail(&record).unwrap().message.unwrap();
        assert!(message.as_str().ends_with(TRUNCATION_NOTE));
        assert!(message.as_str().starts_with("only the first line"));

        // …and an untruncated message of the same text gets no note at all.
        let mut clean = annotated("v-whole");
        clean.message = Some("only the first line".to_string());
        assert_eq!(
            tag_detail(&clean).unwrap().message.unwrap().as_str(),
            "only the first line"
        );
    }

    /// A multi-byte character straddling the cut point is dropped whole.
    #[test]
    fn a_cut_never_splits_a_character() {
        let room = MAX_TAG_MESSAGE_LEN - (TRUNCATION_NOTE.len() + TRUNCATION_SEPARATOR.len());
        // One byte of a two-byte 'é' would land past the boundary.
        let mut text = "x".repeat(room - 1);
        text.push('é');
        text.push_str("tail");
        let fitted = fit_message(Some(&text), true).unwrap();
        assert!(!fitted.as_str().contains('\u{FFFD}'));
        assert_eq!(
            fitted.as_str().len(),
            room - 1 + TRUNCATION_SEPARATOR.len() + TRUNCATION_NOTE.len()
        );
    }

    /// The regression this pair of tests exists for (#236 review).
    ///
    /// `git_vista_git::annotation_message` answers with **two** facts, and
    /// `(None, true)` is a reachable pair: an annotation whose first 16 KiB is
    /// all whitespace retains nothing yet really was cut. The mapping used to
    /// read `record.message.as_deref().and_then(|m| fit_message(m, ..))`, which
    /// short-circuits on the `None` and never consults the flag — so the wire
    /// said `message: null`, indistinguishable from a tag with no annotation
    /// at all, for a tag carrying arbitrarily much content past the cap.
    ///
    /// The negative leg is the load-bearing half: a genuinely empty annotation
    /// must *still* be `null`, or this fix would have bought honesty about
    /// truncation by inventing a message for every unannotated tag.
    #[test]
    fn a_message_cut_down_to_nothing_still_says_it_was_cut() {
        let mut record = annotated("v-all-whitespace-prefix");
        record.message = None;
        record.message_truncated = true;

        let message = tag_detail(&record)
            .unwrap()
            .message
            .expect("a truncated message must never be reported as absent");
        assert_eq!(
            message.as_str(),
            TRUNCATION_NOTE,
            "with nothing retained the note is the whole message"
        );
        assert!(
            !message.as_str().starts_with('\n'),
            "the note has to lead, or a first-line preview shows a blank line \
             and the reader learns nothing"
        );
    }

    #[test]
    fn a_tag_that_really_has_no_annotation_is_still_null() {
        let mut record = annotated("v-blank");
        record.message = None;
        record.message_truncated = false;
        assert_eq!(
            tag_detail(&record).unwrap().message,
            None,
            "no message and nothing cut is a genuinely absent annotation"
        );

        // And a lightweight tag, which can never have either.
        assert_eq!(tag_detail(&lightweight("v-light")).unwrap().message, None);
    }

    /// Splitting the note from its separator must not have moved a single byte
    /// on the wire: the appended form is still exactly what it always was.
    #[test]
    fn the_appended_note_is_byte_identical_to_the_single_constant_it_replaced() {
        let fitted = fit_message(Some("head"), true).unwrap();
        assert_eq!(
            fitted.as_str(),
            "head\n\n[git-vista: this tag's message is longer than 16 KiB; it was cut here]"
        );
    }

    /// Real bytes captured from `git verify-tag --raw` (git 2.43, gpg 2.4.4)
    /// against a genuinely signed, unmodified tag, with the signer's public
    /// key present in the keyring — the case that must classify `Valid`.
    #[test]
    fn a_verified_signature_classifies_valid() {
        assert_eq!(
            classify_verify_tag_output(FIXTURE_GOODSIG),
            SignatureStatus::Valid
        );
    }

    /// # The critical distinction (issue #237)
    ///
    /// Real bytes captured the same way, but against a **tampered** tag
    /// object: the same signature, the same known key, and a message byte
    /// flipped after signing. `gpg` still finds the key (`KEY_CONSIDERED`)
    /// and still runs the check — it just fails it. This must classify
    /// `Invalid`, and — the assertion that matters — it must **not**
    /// classify the same as "we have no public key to check against"
    /// ([`SignatureStatus::UnknownKey`]) or "verification could not run"
    /// ([`SignatureStatus::Unverifiable`]). Conflating a provably forged
    /// signature with either of those is the security defect this test
    /// exists to catch.
    #[test]
    fn a_forged_signature_classifies_invalid_never_unknown_key_or_unverifiable() {
        let status = classify_verify_tag_output(FIXTURE_BADSIG);
        assert_eq!(status, SignatureStatus::Invalid);
        assert_ne!(status, SignatureStatus::UnknownKey);
        assert_ne!(status, SignatureStatus::Unverifiable);
    }

    /// Real bytes captured verifying the same genuine, untampered signature
    /// against an **empty keyring** — precisely what this server's `Strict`
    /// sandbox tier produces for every signed tag on an untrusted repository
    /// (`~/.gnupg` is withheld by `sandbox::DEFAULT_SECRET_EXCLUDES`). `gpg`
    /// never gets to say whether the bytes check out, so this must classify
    /// `UnknownKey` — and, the other half of the same distinction, it must
    /// never read as `Invalid`: an absent key is not evidence of forgery.
    #[test]
    fn a_signature_with_no_matching_pubkey_classifies_unknown_key_never_invalid() {
        let status = classify_verify_tag_output(FIXTURE_NO_PUBKEY);
        assert_eq!(status, SignatureStatus::UnknownKey);
        assert_ne!(status, SignatureStatus::Invalid);
    }

    /// No `[GNUPG:] ` status line at all — `gpg` did not run (missing
    /// binary, broken `gpg.program`) or the sandbox denied it something more
    /// fundamental than a missing key. The honest answer is "verification
    /// could not run", not a guess at which of the other four states
    /// applies.
    #[test]
    fn output_with_no_status_lines_classifies_unverifiable() {
        assert_eq!(
            classify_verify_tag_output(b"fatal: cannot exec 'gpg': No such file or directory\n"),
            SignatureStatus::Unverifiable
        );
        assert_eq!(
            classify_verify_tag_output(b""),
            SignatureStatus::Unverifiable
        );
    }

    /// Adversarial ordering: a `BADSIG` line arriving *after* a
    /// `GOODSIG`-shaped line in the same run must still win. Real `gpg`
    /// never emits both for one signature, but `classify_verify_tag_output`
    /// makes no assumption about line order — scanning the whole output and
    /// letting `BADSIG` outrank everything else, unconditionally, is what
    /// its own doc comment claims; this pins that claim against the one
    /// input shape that would expose a short-circuit-on-first-match bug.
    #[test]
    fn a_badsig_line_outranks_a_goodsig_line_appearing_earlier_in_the_same_run() {
        let raw = b"[GNUPG:] GOODSIG 41815B16FFE44E12 Test Signer <signer@example.com>\n\
                     [GNUPG:] BADSIG 41815B16FFE44E12 Test Signer <signer@example.com>\n";
        assert_eq!(classify_verify_tag_output(raw), SignatureStatus::Invalid);
    }
    /// #335, case 1 — the honesty defect this issue was opened for. gpg said
    /// `EXPKEYSIG`: it checked the bytes, they matched, **and** the key has
    /// since expired. Before this change that reported `Valid` — wire-identical
    /// to a live, trusted signature — so a reader could not tell a maintained
    /// release key from one abandoned years ago.
    ///
    /// The `assert_ne!` is the load-bearing half: `ValidExpiredKey` must not
    /// collapse back into `Valid`, and equally must not over-correct into
    /// `Invalid` or `Unverifiable`, because nothing here failed and nothing
    /// went unchecked.
    #[test]
    fn an_expired_key_signature_classifies_valid_expired_key_never_plain_valid() {
        let status = classify_verify_tag_output(FIXTURE_EXPKEYSIG);
        assert_eq!(status, SignatureStatus::ValidExpiredKey);
        assert_ne!(status, SignatureStatus::Valid);
        assert_ne!(status, SignatureStatus::Invalid);
        assert_ne!(status, SignatureStatus::Unverifiable);
    }

    /// #335, case 2. gpg said `EXPSIG` — the signature carried its own expiry
    /// and that date has passed, while the key itself is still good. Same
    /// pre-#335 report as case 1 (`Valid`), and it must not now be conflated
    /// with case 1 either: an expired *key* and an expired *signature* are
    /// different facts about different things, which is why they get two
    /// variants rather than one `Expired`.
    #[test]
    fn an_expired_signature_classifies_valid_expired_signature_never_plain_valid() {
        let status = classify_verify_tag_output(FIXTURE_EXPSIG);
        assert_eq!(status, SignatureStatus::ValidExpiredSignature);
        assert_ne!(status, SignatureStatus::Valid);
        assert_ne!(status, SignatureStatus::ValidExpiredKey);
    }

    /// #335, case 3, and the most serious of the three. gpg said `REVKEYSIG`:
    /// the bytes check out and the key that made them has been **revoked** —
    /// which is what a signer publishes when they believe the key is
    /// compromised. Before this change no arm matched `REVKEYSIG` at all, so it
    /// fell through to `Unverifiable`: a shrug ("we could not check") for the
    /// one answer in the vocabulary that most warrants alarm, on output where
    /// gpg had checked and had a great deal to say.
    ///
    /// So the three `assert_ne!`s name all three ways this must not be
    /// reported: not as the old shrug, not as ordinary validity, and not as
    /// either of the milder expiry facts.
    #[test]
    fn a_revoked_key_signature_classifies_revoked_never_unverifiable() {
        let status = classify_verify_tag_output(FIXTURE_REVKEYSIG);
        assert_eq!(status, SignatureStatus::Revoked);
        assert_ne!(status, SignatureStatus::Unverifiable);
        assert_ne!(status, SignatureStatus::Valid);
        assert_ne!(status, SignatureStatus::ValidExpiredKey);
    }

    /// All six captured outcomes, swept as a table: every fixture classifies to
    /// its own status, and no two share one.
    ///
    /// Written as a sweep rather than six `assert_eq!`s because the property
    /// that matters is *distinctness across the set*. A mutation that maps two
    /// gpg statuses to the same variant fails here on the pair, even if each
    /// individual expectation was quietly updated to match.
    #[test]
    fn the_six_captured_outcomes_classify_to_six_distinct_statuses() {
        let mut seen: std::collections::BTreeMap<String, &str> = std::collections::BTreeMap::new();
        for (name, raw, expected) in ALL_FIXTURES {
            let got = classify_verify_tag_output(raw);
            assert_eq!(got, *expected, "{name} fixture");
            if let Some(other) = seen.insert(format!("{got:?}"), name) {
                panic!("{name} and {other} both classify {got:?} — the vocabulary collapsed");
            }
        }
        assert_eq!(seen.len(), ALL_FIXTURES.len());
    }

    /// **The exhaustiveness guard (#335, acceptance 4).**
    ///
    /// A `match` cannot be exhaustive over gpg's status vocabulary — it lives
    /// in another project's source and grows without asking us — so the
    /// classifier keeps a fallthrough. What #335 fixed is that the fallthrough
    /// was *silent*: `REVKEYSIG` sat in it for two milestones, reported as
    /// "could not check", and nothing anywhere failed.
    ///
    /// This is the replacement guarantee, and it is deliberately checked
    /// against **real gpg output** rather than against a list someone wrote
    /// down: every status keyword appearing in any committed fixture must be
    /// either acted on ([`VERDICT_PRECEDENCE`]) or consciously absorbed
    /// ([`ABSORBED_GPG_STATUS`]). A future contributor who captures a new
    /// fixture carrying a keyword nobody has classified gets a red test naming
    /// it, instead of a tag that quietly reads "unverifiable".
    #[test]
    fn every_status_line_in_every_fixture_is_acted_on_or_censused() {
        for (name, raw, _) in ALL_FIXTURES {
            let (_, unrecognised) = classify_verify_tag_output_with_census(raw);
            assert!(
                unrecognised.is_empty(),
                "the {name} fixture carries gpg status {unrecognised:?}, which this build neither \
                 acts on nor lists in ABSORBED_GPG_STATUS — classify it or absorb it deliberately"
            );
        }
    }

    /// The other half of the guard: a keyword in neither list is **named**, not
    /// swallowed.
    ///
    /// This is what makes the test above more than a tautology. The census is
    /// only worth keeping if failing to be in it has a consequence, so here a
    /// status keyword gpg 2.4.4 does not have is fed through a fixture that is
    /// otherwise an ordinary good signature: the verdict is unchanged (an
    /// unknown line may never move a verdict), and the keyword comes back in
    /// the census so the caller can report it.
    #[test]
    fn an_unmodelled_gpg_status_is_named_rather_than_silently_absorbed() {
        let raw = b"[GNUPG:] NEWSIG\n\
                    [GNUPG:] GOODSIG 41815B16FFE44E12 Test Signer <signer@example.com>\n\
                    [GNUPG:] SOMEFUTURESIG 41815B16FFE44E12 whatever gpg adds next\n";
        let (status, unrecognised) = classify_verify_tag_output_with_census(raw);
        assert_eq!(
            status,
            SignatureStatus::Valid,
            "an unrecognised line must never move the verdict"
        );
        assert_eq!(unrecognised, vec!["SOMEFUTURESIG".to_string()]);

        // And with no verdict line at all it still classifies `Unverifiable` —
        // the pre-#335 behaviour — but the reason is no longer invisible.
        let (status, unrecognised) =
            classify_verify_tag_output_with_census(b"[GNUPG:] SOMEFUTURESIG 1\n");
        assert_eq!(status, SignatureStatus::Unverifiable);
        assert_eq!(unrecognised, vec!["SOMEFUTURESIG".to_string()]);
    }

    /// A keyword is reported once however many times it appears, so a run that
    /// repeats an unknown line per-subkey cannot flood the operator's terminal.
    #[test]
    fn an_unmodelled_status_is_reported_once_not_once_per_line() {
        let raw = b"[GNUPG:] SOMEFUTURESIG a\n\
                    [GNUPG:] SOMEFUTURESIG b\n\
                    [GNUPG:] OTHERFUTURESIG c\n\
                    [GNUPG:] SOMEFUTURESIG d\n";
        let (_, unrecognised) = classify_verify_tag_output_with_census(raw);
        assert_eq!(
            unrecognised,
            vec!["SOMEFUTURESIG".to_string(), "OTHERFUTURESIG".to_string()],
            "de-duplicated, and in first-seen order"
        );
    }

    /// The precedence table, pinned row by row against a literal written out
    /// here rather than read back from the table itself.
    ///
    /// The order is a security contract, not an implementation detail — it is
    /// what stops a revoked key from being reported as merely expired, or a
    /// forged signature from being softened by a later line — and gpg emits
    /// exactly one sig-level line per signature, so **no fixture can catch a
    /// reordering**. Only this pin can.
    #[test]
    fn the_verdict_precedence_table_is_pinned_most_alarming_first() {
        assert_eq!(
            VERDICT_PRECEDENCE,
            &[
                ("BADSIG", SignatureStatus::Invalid),
                ("REVKEYSIG", SignatureStatus::Revoked),
                ("EXPKEYSIG", SignatureStatus::ValidExpiredKey),
                ("EXPSIG", SignatureStatus::ValidExpiredSignature),
                ("GOODSIG", SignatureStatus::Valid),
                ("NO_PUBKEY", SignatureStatus::UnknownKey),
            ]
        );
    }

    /// The absorbed census, pinned the same way and for the same reason: adding
    /// a keyword to it is a decision that a status carries no verdict, and a
    /// decision nobody had to write down twice is one that can be made by
    /// accident while silencing a warning.
    #[test]
    fn the_absorbed_status_census_is_pinned() {
        assert_eq!(
            ABSORBED_GPG_STATUS,
            &[
                "NEWSIG",
                "KEY_CONSIDERED",
                "SIG_ID",
                "VALIDSIG",
                "TRUST_UNDEFINED",
                "TRUST_NEVER",
                "TRUST_MARGINAL",
                "TRUST_FULLY",
                "TRUST_ULTIMATE",
                "KEYEXPIRED",
                "KEYREVOKED",
                "ERRSIG",
                "FAILURE",
                "NOTATION_NAME",
                "NOTATION_DATA",
                "NOTATION_FLAGS",
                "POLICY_URL",
                "VERIFICATION_COMPLIANCE_MODE",
                "PROGRESS",
            ]
        );
        // Nothing may sit in both lists: a keyword that carries a verdict and
        // is also "deliberately ignored" is a contradiction, and whichever list
        // the code consulted first would silently win.
        for (acted_on, _) in VERDICT_PRECEDENCE {
            assert!(
                !ABSORBED_GPG_STATUS.contains(acted_on),
                "{acted_on} is both acted on and absorbed"
            );
        }
    }

    /// **Severity decides the verdict, whatever order the lines arrive in —
    /// checked over EVERY ordered pair, not a hand-picked few.**
    ///
    /// Real gpg will not emit two verdict keywords in one run, so the
    /// precedence table is the only thing standing behind this. The first
    /// version of this test wrote each pair with the *milder* keyword first,
    /// which defends against first-match-wins — and nothing else. An outside
    /// review (codex, 2026-08-26) showed by simulation that a reducer
    /// regressing to last-line-wins (`verdict = Some(rank)` instead of
    /// `best.min(rank)`) passed all five hand-picked pairs, while
    /// `BADSIG` then `REVKEYSIG` would return `Revoked` — **downgrading a
    /// forged signature to a softer verdict**, which is the one outcome this
    /// classifier exists to make impossible.
    ///
    /// So the pairs are generated rather than chosen: for every ordered pair
    /// of distinct entries in [`VERDICT_PRECEDENCE`], the stronger one (lower
    /// index) must win in BOTH orders. A hand-written list can be complete
    /// today and silently partial after the next keyword is added; this
    /// cannot.
    ///
    /// MUTATION 1 (last-line-wins): `verdict = Some(rank)` — red on the pairs
    ///   where the stronger keyword comes first.
    /// MUTATION 2 (first-line-wins): `verdict = verdict.or(Some(rank))` — red
    ///   on the pairs where the milder keyword comes first.
    /// Neither mutation is caught by both halves, which is exactly why both
    /// orders must be asserted.
    #[test]
    fn severity_decides_the_verdict_over_every_ordered_pair() {
        for (i, (strong_kw, strong_status)) in VERDICT_PRECEDENCE.iter().enumerate() {
            for (weak_kw, _) in VERDICT_PRECEDENCE.iter().skip(i + 1) {
                // Stronger first, then milder.
                let strong_first = format!(
                    "[GNUPG:] {strong_kw} 4181 Signer <s@example.com>\n\
                     [GNUPG:] {weak_kw} 4181 Signer <s@example.com>\n"
                );
                assert_eq!(
                    classify_verify_tag_output(strong_first.as_bytes()),
                    *strong_status,
                    "{strong_kw} must outrank {weak_kw} when it comes FIRST \
                     (a last-line-wins reducer fails here)"
                );

                // Milder first, then stronger — the same verdict.
                let weak_first = format!(
                    "[GNUPG:] {weak_kw} 4181 Signer <s@example.com>\n\
                     [GNUPG:] {strong_kw} 4181 Signer <s@example.com>\n"
                );
                assert_eq!(
                    classify_verify_tag_output(weak_first.as_bytes()),
                    *strong_status,
                    "{strong_kw} must outrank {weak_kw} when it comes SECOND \
                     (a first-match-wins reducer fails here)"
                );
            }
        }

        // Anti-vacuity: the loop above must actually have compared something.
        // A table trimmed to one entry would make every assertion above
        // unreachable and leave this test green while proving nothing.
        assert!(
            VERDICT_PRECEDENCE.len() >= 6,
            "the precedence table lost entries; this test's coverage is only \
             as wide as the table it iterates"
        );
    }

    /// The `KEYEXPIRED` / `KEYREVOKED` lines that ride along with the two
    /// interesting fixtures are **not** what the verdict is read from.
    ///
    /// Both can describe a different key gpg considered on the way, so keying
    /// off them would classify a signature by the state of a key that did not
    /// make it. Here each appears with a plain `GOODSIG` and must change
    /// nothing — the shape a lazier implementation of #335 would have got
    /// wrong, since both keywords are present in the fixtures it was written
    /// against.
    #[test]
    fn a_key_lifetime_line_beside_a_goodsig_does_not_become_the_verdict() {
        assert_eq!(
            classify_verify_tag_output(
                b"[GNUPG:] KEYEXPIRED 1767312000\n\
                  [GNUPG:] GOODSIG 4181 Signer <s@example.com>\n"
            ),
            SignatureStatus::Valid
        );
        assert_eq!(
            classify_verify_tag_output(
                b"[GNUPG:] KEYREVOKED\n\
                  [GNUPG:] GOODSIG 4181 Signer <s@example.com>\n"
            ),
            SignatureStatus::Valid
        );
    }
}
