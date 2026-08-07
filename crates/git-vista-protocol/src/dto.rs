//! Shared request/response transport DTOs — the exact structs the frontend
//! serialises and the server deserialises across the HTTP/JSON boundary.
//!
//! These are *transport*, not *domain*: wire messages for operations, kept out of
//! `git-vista-core` (which owns the repository/graph domain model) so the API
//! contract versions independently of the internal model. Each request body
//! carries `#[serde(deny_unknown_fields)]` so an unexpected key is a hard
//! deserialization error (a `400`), not a silently-ignored value — part of the
//! guarantee that a request cannot smuggle in a field the server might act on,
//! such as a repository path (the repo is server-selected, never chosen by a
//! request; see `docs/adr/0002-versioned-api-contract.md`).

use serde::{Deserialize, Serialize};

/// Body of a `POST /api/branch` request (Issue #18): create a branch named
/// `name` pointing at the commit `commit` (full hex id). Shared so the frontend
/// serialises exactly what the backend deserialises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBranchRequest {
    pub name: String,
    pub commit: String,
}

/// Body of a `POST /api/commit` request (Issue #33): create a commit with the
/// message `message`. When `allow_empty` is true the commit is made even with
/// nothing staged (`git commit --allow-empty`); otherwise git commits the
/// staged changes and fails if there are none.
///
/// `branch` names the branch the commit should land on. `None` — and any name
/// that turns out to be the checked-out branch — means a plain `git commit` on
/// HEAD, exactly as before this field existed. A *different* branch is allowed
/// only for empty commits (there's no meaning to committing HEAD's staged tree
/// onto another branch): the backend writes the commit with `git commit-tree`
/// and advances the ref with a compare-and-swap `git update-ref`, never
/// touching HEAD or the working tree. This is how a branch stub — a new branch
/// with no commits of its own — takes its first (empty) commit from the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCommitRequest {
    pub message: String,
    pub allow_empty: bool,
    #[serde(default)]
    pub branch: Option<String>,
}

/// Body of a `POST /api/amend-commit` request (M2.19, #72; contract landed
/// with M2.19a #222, execution wired by M2.19b #223 — see ADR 0040).
///
/// `expected_tip` is the compare-and-swap: the full hex commit id the client
/// last saw as the checked-out branch's tip, matching
/// [`crate::GitOperation::AmendCommit`]'s own `expected_tip` field one-to-one.
/// It is a *separate* field from `message`/`allow_empty` rather than folded
/// into [`CreateCommitRequest`] because amend has no `branch` field to begin
/// with (it always targets the checked-out branch's own tip — see that
/// variant's doc comment) and, unlike a plain commit, is only safe to run at
/// all when the tip a reviewer approved rewriting is still there; a shared
/// DTO would let a `branch`-carrying request silently no-op past that check
/// or would need the CAS field bolted onto a shape that has no other use for
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmendCommitRequest {
    pub message: String,
    pub allow_empty: bool,
    pub expected_tip: String,
}

/// Why a `POST /api/amend-commit` execution failed, as a typed tag the client
/// can branch on (M2.19b, #223) — the whole point is that the frontend
/// (M2.19d) renders each case distinctly *without* regex-sniffing git's
/// stderr, which is gettext-translated and version-dependent. The server owns
/// the (documented, tested) classification heuristics; the wire carries only
/// this tag.
///
/// Wire values are `snake_case` strings. The classification is honest about
/// its limits: git prints **nothing of its own** when a hook rejects a commit
/// (verified against git 2.43 — a silently-failing `pre-commit` produces
/// exit 1 with empty stderr and stdout), so `HookRejected` is a documented
/// heuristic (see `planner::classify_amend_failure`), not a certainty, and a
/// hook that fakes git's own `fatal:` wording lands in `Other` — the safe
/// direction, since `Other` promises nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmendFailureKind {
    /// The compare-and-swap refused: HEAD is not at `expected_tip`, so the
    /// commit the reviewer approved rewriting is not the commit that would
    /// be rewritten. The client's remedy is refresh-and-re-review, never
    /// retry-as-is.
    StaleTip,
    /// A repository hook (`pre-commit`, `prepare-commit-msg`, `commit-msg`)
    /// exited non-zero. Actionable: the hook's own output (if any) is in
    /// `message`, and the user fixes whatever the hook checks.
    HookRejected,
    /// Commit signing was configured and the signer failed (`gpg failed to
    /// sign the data`, an unloadable ssh signing key, …). Actionable: a
    /// signing-setup problem, not a content problem.
    SigningFailed,
    /// Everything else, reported with git's own words in `message`.
    Other,
}

/// Body of a **failed** `POST /api/amend-commit` (status 400): the typed
/// classification plus git's own explanation. Every 400 this endpoint
/// returns carries this JSON shape — handler-level validation refusals
/// included — so a client can always parse a 400 body as this type. Other
/// statuses (403/409/5xx) keep the server-wide prose contract; they are the
/// shared refusals (auth, staleness, busy, git-couldn't-run) every endpoint
/// answers identically and the client already handles generically.
///
/// A response DTO, so no `deny_unknown_fields`: a future server may add
/// fields, and an older client must keep parsing (M1.02 additive rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmendCommitError {
    pub kind: AmendFailureKind,
    pub message: String,
}

/// Body of a **successful** `POST /api/amend-commit` (status 200).
///
/// `old_tip` is the amended-away commit (the request's own `expected_tip`,
/// echoed so the response is self-contained); `new_tip` is the rewritten
/// tip, `None` only if git could not be re-read after the amend succeeded
/// (the amend still happened — same posture as the journal's
/// "resulting tip unknown" note).
///
/// `amended_published_commit` is the published-history guard (#223):
/// `Some(true)` means the amended-away commit was reachable from a
/// remote-tracking ref, so local and remote history have now diverged and a
/// plain push will be refused — the client should warn. `Some(false)` means
/// the walk ran and found nothing. `None` means the walk itself failed, so
/// the server honestly does not know — distinct from `false` on purpose, the
/// same three-state honesty `Obs` gives observations server-side. The flag
/// is **advisory, deliberately**: the server never blocks an amend of
/// published history (the user may be doing exactly that, knowingly — the
/// pre-flight ceremony is the client's, M2.19d); this is defense in depth
/// and after-the-fact truth, per ADR 0040.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmendCommitSuccess {
    pub message: String,
    pub old_tip: String,
    #[serde(default)]
    pub new_tip: Option<String>,
    #[serde(default)]
    pub amended_published_commit: Option<bool>,
}

/// Body of the branch-operation requests (Issue #33 follow-up): merge
/// (`POST /api/merge`), delete (`POST /api/delete-branch`),
/// force-delete (`POST /api/force-delete-branch`), and checkout (`POST /api/checkout`).
/// All act on a single named branch, so they share one shape. `branch` is a
/// local branch name; the backend validates it and forwards git's own error text.
///
/// `POST /api/push` used this shape too until M2.20e (#231) gave it
/// [`PushRequest`], which is this plus the two things a push can now say for
/// itself. A `{"branch": …}` body still parses as a `PushRequest`, so nothing
/// that spoke to `/api/push` before has to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRequest {
    pub branch: String,
}

/// Body of a `POST /api/tag` request (M2.21d, #238): create the tag `name` at
/// the commit `commit` (full hex id, or a symbolic start point the server
/// resolves — same posture as [`CreateBranchRequest`]).
///
/// # `message` is what chooses the *kind* of tag
///
/// `None` (or absent) means a **lightweight** tag: `refs/tags/<name>` points
/// straight at `commit` and no object is written. `Some(text)` means an
/// **annotated** tag: git writes a tag object carrying the message, the
/// tagger and the date, and the ref points at *that*.
///
/// Modelling the kind as "is there a message?" rather than as a separate
/// `annotated: bool` beside an `Option<String>` is deliberate, and it is the
/// same nesting [`crate::TagAnnotation`] uses for the operation itself: the
/// pair `{annotated: true, message: null}` would be representable on the
/// wire, would mean "annotate this tag with a message I did not give you",
/// and is precisely the request that makes `git tag -a` open an editor. A
/// headless server has no editor and no one to type into it, so that request
/// must not be expressible — see ADR 0048. With this shape it is not: there
/// is no way to ask for an annotated tag without also supplying its text.
///
/// # `sign`
///
/// Accepted so a client can ask, and **refused** by the executor until M2.21e
/// wires signing (#74). Refused rather than silently ignored: quietly
/// producing an unsigned tag for a request that asked for a signed one is a
/// wrong outcome the user cannot see. `sign: true` with no `message` is
/// refused by the handler — a signed tag is annotated by definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTagRequest {
    pub name: String,
    pub commit: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub sign: bool,
}

/// Body of a `POST /api/delete-tag` request (M2.21d, #238): delete the **local**
/// tag `tag` (`git tag -d`). Nothing here reaches a remote — deleting a
/// remote tag is [`crate::GitOperation::DeleteRemoteTag`], its own operation
/// with its own route still to come (#74), because it opens a socket with
/// credentials on it.
///
/// Its own type rather than a reuse of [`BranchRequest`]: the field names the
/// thing being deleted, and a body whose key is `branch` on a tag endpoint
/// invites exactly the copy-paste that deletes the wrong kind of ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteTagRequest {
    pub tag: String,
}

/// Body of a `POST /api/push` request (M2.20e, #231): publish the local branch
/// `branch` to the server-configured remote, optionally recording it as the
/// branch's upstream and optionally force-publishing **under a lease**.
///
/// # The two optional fields, and why they are optional *here* and nowhere else
///
/// [`GitOperation::PushBranch`](crate::plan::GitOperation::PushBranch) carries
/// `set_upstream` and `force` with **no** `#[serde(default)]` and
/// [`ForcePublish`](crate::plan::ForcePublish) derives no `Default`, so nothing
/// in the reviewed operation vocabulary can be constructed without stating both
/// (M2.20a, #227). That guarantee is about the *plan a user approves*, and it is
/// untouched: this DTO is a request, the handler maps it onto the operation, and
/// the operation still has to say both out loud.
///
/// What defaulting buys is that an omitted field means the endpoint's
/// long-standing behaviour — a plain fast-forward push with no upstream write —
/// so a client written before this slice keeps working byte-for-byte. The
/// defaults are the *safe* end of both axes, which is the direction a default is
/// allowed to point: absent means "do less", never "force".
///
/// # There is no `remote`
///
/// `/api/push` has always pushed to `origin`, and adding a client-named remote
/// here would be a separate decision with its own hazard (a request that can
/// name where this server's credentials get sent). `force`'s
/// `expected_remote_tip` is likewise never a URL and never a path — it is an
/// object id, validated by [`CommitOid`](crate::plan::CommitOid).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushRequest {
    pub branch: String,
    /// `git push --set-upstream` — record `<remote>/<branch>` as this branch's
    /// upstream on success. Absent ⇒ `false`, the behaviour `/api/push` has
    /// always had.
    #[serde(default)]
    pub set_upstream: bool,
    /// Absent ⇒ [`ForcePublish::None`](crate::plan::ForcePublish::None).
    ///
    /// `Option` rather than a `#[serde(default)]` on `ForcePublish` itself: a
    /// `Default` impl on that enum would let *every* construction site omit the
    /// force mode, including ones that should have to think about it, and #227's
    /// whole posture is that a force is never something a type supplies quietly.
    /// The handler turns `None` into `ForcePublish::None` in one visible place.
    #[serde(default)]
    pub force: Option<crate::plan::ForcePublish>,
}

/// Body of `POST /api/discard-tracked-paths` / `POST /api/delete-untracked-paths`
/// (#219, M2.18a): the working-tree paths to discard uncommitted changes to,
/// or delete outright. `paths` must be non-empty (the backend rejects an
/// empty list); each element is validated into a `WorktreePath` before it
/// ever reaches an argv — never absolute, never carrying a `..` component,
/// never option-shaped. Shared by both endpoints because the request shape
/// is identical; the two `GitOperation` variants it builds into are not (#71).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreePathsRequest {
    pub paths: Vec<String>,
}

/// Body of a `POST /api/fetch` request (M2.20c, #229): fetch from the named
/// configured remote.
///
/// A remote *name*, never a URL — the server resolves it through the
/// repository's own configuration and the plan's
/// [`Precondition::RemoteConfigured`](crate::plan::Precondition::RemoteConfigured)
/// gate. That is the whole point of the field being what it is: a request
/// that could name a URL would let a client point this server's credentials
/// at a host of its choosing, which is the same class of hazard as a request
/// naming a repository path.
///
/// There is deliberately no `refspec`, no `--prune`, no `--tags` and no
/// `--depth`. Each of those changes what a fetch *does* to local refs, and
/// [`GitOperation::FetchRemote`](crate::plan::GitOperation::FetchRemote) — the
/// typed vocabulary #227 fixed — has no field for any of them. A body field
/// with nowhere to land in the operation would be a field the reviewer of a
/// plan never sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    pub remote: String,
}

/// One remote-tracking ref that moved during a fetch (M2.20c, #229).
///
/// **Observed, never parsed from git's prose.** The server lists
/// `refs/remotes/<remote>/*` before and after the fetch and diffs the two
/// listings, so this is true regardless of git's locale, version, or which
/// of its "From …/  a1b2c3..d4e5f6  main -> origin/main" summary lines it
/// chose to print — the lesson #284 wrote down for branch deletion, applied
/// to the one question a cancelled fetch has to answer honestly.
///
/// `old_oid` is `None` for a ref that did not exist before (a new branch on
/// the remote); `new_oid` is `None` for one that is gone afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRefUpdate {
    /// Full ref name, e.g. `refs/remotes/origin/main`.
    pub ref_name: String,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
}

/// Why a `POST /api/fetch` execution failed, as a typed tag the client can
/// branch on (M2.20c, #229) — the same contract, and the same honesty about
/// its limits, as [`AmendFailureKind`] above.
///
/// One of these is an **observed fact**, not a heuristic: [`Self::Cancelled`]
/// is set because *this server* killed the child on an operator's cancel
/// request, and it is never inferred from output. The remaining four are
/// classified from git's stderr against a documented marker set (see
/// `planner::fetch::classify_failure`), which is gettext-translated under a
/// non-English locale and version-dependent — so a failure that does not
/// match any marker lands in [`Self::Other`] rather than being forced into
/// the nearest-looking box. In every case git's own words are forwarded in
/// `message`; the tag is an addition to them, never a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchFailureKind {
    /// The remote demanded credentials this server could not supply
    /// (HTTP 401, or a rejected SSH key). Actionable: the user configures a
    /// credential helper or an SSH agent on the host — this server never
    /// prompts, and since #228 it cannot be made to (`-c core.askpass=`).
    ///
    /// This is genuinely the case that "no credential helper is configured"
    /// or "the configured one has nothing to offer" describes. It is
    /// deliberately **not** the case below — see
    /// [`Self::CredentialHelperBlocked`]'s doc comment for the distinction,
    /// which matters because this variant's own advice ("configure a
    /// credential helper") is *wrong* for that other case.
    AuthenticationFailed,
    /// The remote could not be reached at all: connection refused, no route,
    /// DNS failure, timeout. Actionable: a network problem, not a repository
    /// one — retrying later is the remedy.
    RemoteUnreachable,
    /// The remote answered and refused: no such repository, access denied to
    /// an authenticated user, upload-pack disabled. Actionable: the remote's
    /// configuration or the user's access to it, not the local repository.
    RemoteRejected,
    /// A credential helper *is* configured, but crashed before it could run
    /// because this server's sandbox denies it the file it needs just to
    /// start — not because the user never configured one.
    ///
    /// Distinct from [`Self::AuthenticationFailed`] on purpose (#325 lane 4):
    /// git's own fallback message for this case ("could not read Username")
    /// already matches that variant's marker set, which would tell the user
    /// to do the one thing that's already done. Concretely, on this host the
    /// global `credential.helper = !gh auth git-credential` crashes with
    /// `failed to create root command: failed to read configuration: open
    /// ~/.config/gh/config.yml: permission denied`, because the sandbox's
    /// Network tier withholds `~/.config/gh` wholesale — it holds
    /// `hosts.yml`, the operator's OAuth token, and there is no file-level
    /// carve-out for the plain-prefs half of that directory
    /// (`sandbox::DEFAULT_SECRET_EXCLUDES`; predicted and flagged, unresolved,
    /// in the original M1.13b sandbox design doc under "F2"). Actionable: use
    /// an `ssh://` remote instead — the sandbox's SSH agent-socket and
    /// `known_hosts` carve-outs (#188, `sandbox::ssh_remote`) are the one
    /// credential path this server's Network tier actually admits end to end
    /// (`docs/SECURITY_MODEL.md`'s implemented-vs-aspirational table).
    /// Widening the `.config/gh` exclusion is a real sandbox weakening (every
    /// Network-tier spawn, including a hostile hook from a served repo, would
    /// gain read access to the OAuth token) and is out of scope here.
    CredentialHelperBlocked,
    /// An operator cancelled the operation and the server terminated the
    /// running `git fetch`. `updated_refs` on the error body says whether
    /// anything had already moved.
    Cancelled,
    /// Everything else, reported with git's own words in `message`.
    Other,
}

/// Body of a **failed** `POST /api/fetch` (status 400, or 409 for a cancel):
/// the typed classification, git's own explanation, and — the part that
/// matters for a cancel — exactly which remote-tracking refs had already
/// moved when it stopped.
///
/// A response DTO, so no `deny_unknown_fields` (M1.02 additive rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchError {
    pub kind: FetchFailureKind,
    pub message: String,
    /// Remote-tracking refs that moved before the failure. Empty is the
    /// common case and the reassuring one: nothing local changed.
    #[serde(default)]
    pub updated_refs: Vec<RemoteRefUpdate>,
}

/// Body of a **successful** `POST /api/fetch` (status 200).
///
/// `updated_refs` is the observed before/after diff of
/// `refs/remotes/<remote>/*` — empty when the fetch was a no-op ("already up
/// to date"), which is a success, not a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchSuccess {
    /// The remote fetched from, echoed so the response is self-contained.
    pub remote: String,
    /// A short human sentence for the UI; git's own summary is not reused
    /// here because it is prose the server would then be promising the shape
    /// of. The machine-readable answer is `updated_refs`.
    pub message: String,
    #[serde(default)]
    pub updated_refs: Vec<RemoteRefUpdate>,
}

/// Body of a `POST /api/pull` request (M2.20d, #230): fetch from `remote` and
/// integrate its `branch` into the checked-out branch using `strategy`.
///
/// # `strategy` has no default, here or anywhere — that is the endpoint
///
/// [`MergeStrategy`] has no `Auto` variant and this field carries no
/// `#[serde(default)]`, so a body that omits it **cannot deserialize**. The
/// handler turns that into a `400` naming the two legal values rather than
/// falling back to anything, because the fallback `git pull` itself would use
/// is `pull.rebase` / `branch.<name>.rebase` — a value that lives in a config
/// file this app never shows, so two people running "Pull" on the same branch
/// could get two different histories and neither reviewed which. #230's whole
/// reason to exist is that this choice is always the caller's, stated.
///
/// # A remote *name* and a remote *branch*, never a URL
///
/// Same posture as [`FetchRequest`], for the same reason: a body that could
/// name a URL would let any authenticated client point this server — and
/// whatever credential helper or SSH agent the host offers it — at a host of
/// the client's choosing. `branch` is the branch **on the remote**
/// (`git pull origin main` ⇒ `main`); the destination is always whatever
/// branch is checked out, exactly as for
/// [`GitOperation::MergeBranch`](crate::plan::GitOperation::MergeBranch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequest {
    pub remote: String,
    pub branch: String,
    pub strategy: crate::plan::MergeStrategy,
}

/// Why a `POST /api/pull` failed, as a typed tag the client can branch on
/// (M2.20d, #230).
///
/// Three groups, and the distinction between them is the point:
///
///  * **The fetch half failed.** [`Self::AuthenticationFailed`],
///    [`Self::RemoteUnreachable`], [`Self::RemoteRejected`],
///    [`Self::CredentialHelperBlocked`] and [`Self::Other`] are the same
///    taxonomy [`FetchFailureKind`] already defines, mapped across
///    one-to-one so a client that already handles a failed fetch handles a
///    failed pull's fetch half identically. Nothing was integrated, so the
///    checked-out branch never moved.
///  * **The integration half failed.** [`Self::Conflict`] and
///    [`Self::ConflictLeftInProgress`] are *observed* states of the
///    repository, not classifications of git's prose — see each variant.
///  * **Neither ran.** [`Self::StrategyRequired`] is the missing-`strategy`
///    refusal above; [`Self::NoSuchRemoteBranch`] is the fetch having
///    succeeded without producing the ref the caller asked to integrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullFailureKind {
    /// The request did not name an integration strategy. Actionable in the
    /// most literal sense: re-send with `"strategy": "merge"` or
    /// `"strategy": "rebase"`. Never produced by anything the repository did.
    StrategyRequired,
    /// The remote demanded credentials this server could not supply — the
    /// [`FetchFailureKind::AuthenticationFailed`] case.
    AuthenticationFailed,
    /// The remote could not be reached at all — the
    /// [`FetchFailureKind::RemoteUnreachable`] case.
    RemoteUnreachable,
    /// The remote answered and refused — the
    /// [`FetchFailureKind::RemoteRejected`] case.
    RemoteRejected,
    /// A credential helper is configured but the sandbox blocked its own
    /// startup — the [`FetchFailureKind::CredentialHelperBlocked`] case; see
    /// its doc comment for why this is distinct from
    /// [`Self::AuthenticationFailed`].
    CredentialHelperBlocked,
    /// An operator cancelled the operation. `updated_refs` says whether the
    /// fetch half had already moved anything; the integration half never ran,
    /// so the checked-out branch is untouched.
    Cancelled,
    /// The fetch succeeded but `refs/remotes/<remote>/<branch>` does not
    /// exist afterwards, so there is nothing to integrate. **Observed** by
    /// listing the ref, not read out of a git error, so it is true under any
    /// locale. Nothing was integrated.
    NoSuchRemoteBranch,
    /// The integration conflicted and was aborted; the checked-out branch is
    /// back at its pre-pull tip and no merge or rebase is in progress.
    ///
    /// **This is an outcome, not a server error.** It is reported `409`, with
    /// `worktree_restored: true` — a state a browser-only user can act on
    /// (resolve upstream, or pull with the other strategy) rather than a
    /// `500` that says the server broke.
    Conflict,
    /// The integration conflicted **and** the abort did not restore the
    /// repository: a merge or rebase is still in progress in the working
    /// tree. Reported separately from [`Self::Conflict`] because the two
    /// demand opposite things of the user — one is "nothing happened, choose
    /// again", the other is "your working tree needs attention, and this app
    /// cannot finish it for you". `worktree_restored` is `false`.
    ConflictLeftInProgress,
    /// Everything else, reported with git's own words in `message`.
    Other,
}

/// Body of a **failed** `POST /api/pull` (status 400, or 409 for a cancel or
/// a conflict): the typed classification, git's own explanation, whatever the
/// fetch half moved before stopping, and whether the repository is back where
/// it started.
///
/// A response DTO, so no `deny_unknown_fields` (M1.02 additive rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullError {
    pub kind: PullFailureKind,
    pub message: String,
    /// Remote-tracking refs the fetch half moved before the pull stopped. A
    /// failed *integration* still leaves these — the objects arrived and the
    /// tracking refs advanced, which is exactly why a retry with the other
    /// strategy has nothing left to download.
    #[serde(default)]
    pub updated_refs: Vec<RemoteRefUpdate>,
    /// Whether the checked-out branch is at its pre-pull tip **and** no merge
    /// or rebase is in progress — both re-read from the repository after the
    /// abort, never inferred from the abort command's exit status. `false`
    /// means the working tree needs a human.
    pub worktree_restored: bool,
}

/// Body of a **successful** `POST /api/pull` (status 200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullSuccess {
    /// The remote pulled from, echoed so the response is self-contained.
    pub remote: String,
    /// The branch **on the remote** that was integrated.
    pub branch: String,
    /// The strategy that actually ran, echoed back. A client never has to
    /// remember what it asked for to render what happened — and a response
    /// that echoed the *other* strategy would be a bug this field makes
    /// visible instead of invisible.
    pub strategy: crate::plan::MergeStrategy,
    /// A short human sentence for the UI; the machine-readable answers are
    /// `updated_refs` and `advanced`.
    pub message: String,
    /// The fetch half's observed before/after diff of
    /// `refs/remotes/<remote>/*`, exactly as [`FetchSuccess`] reports it.
    #[serde(default)]
    pub updated_refs: Vec<RemoteRefUpdate>,
    /// Whether the integration moved the checked-out branch. Observed by
    /// reading its tip before and after; a read that fails **refuses the
    /// operation** rather than reporting a guess, so this is always a fact
    /// and `false` always means "already up to date".
    pub advanced: bool,
}

/// Body of a `POST /api/clone` request (Phase 12): clone the public repository at
/// `url` into the persistent clones store (ADR 0008) and open it look-only
/// pending the operator's mode choice. `url` is a git-cloneable URL (typically
/// `https://…`); the backend
/// validates its scheme with [`validate_clone_url`] and forwards git's own error
/// text on failure. There is deliberately no destination field — the server picks
/// the clone directory, so a request can never point the server at a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneRequest {
    pub url: String,
}

/// The substring `handlers/clone.rs::admit_clone` guarantees appears in the
/// message of its `409 Conflict` refusal for a clone that is still *running*
/// (as opposed to the sibling 409 for a key reused with a different URL,
/// which does not contain it). The frontend's `clone_response_should_poll`
/// (#278) matches on this substring to decide whether that 409 is worth
/// polling `GET /api/clone-status/{key}` for, rather than treating it as
/// terminal.
///
/// Both sides reference this one constant (#289) so the coupling is a
/// compile-time fact: the surrounding sentence is free to be reworded, but
/// moving or dropping the sentinel itself is a deliberate, single-place edit
/// instead of a silent break in the client's polling.
pub const CLONE_IN_PROGRESS_SENTINEL: &str = "already in progress";

/// Which experience a repository is opened in (ADR 0006/0007). `Visualize` is
/// look-only: the server refuses every mutation while it is the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoMode {
    Visualize,
    Active,
}

/// Body of `POST /api/select` (ADR 0007): make the repository addressed by the
/// opaque `worktree` id the current selection, opened in `mode`. The id resolves
/// only through the server-owned catalog — an unknown/forged id is a 404, and
/// like every request body this cannot carry a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectRequest {
    pub worktree: String,
    pub mode: RepoMode,
}

/// Body of `POST /api/delete-clone` (ADR 0008): delete the *clone* addressed by
/// the opaque `worktree` id — its catalog entry and its directory. The server
/// refuses any id whose path does not canonicalize inside the clones root, so
/// this can only ever remove server-made clones, never a user repository; it
/// also refuses the currently open repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteCloneRequest {
    pub worktree: String,
}

/// Body of a `POST /api/session` request (M1.04, #57): the one-time bootstrap
/// `token` the SPA read from the `#s=<token>` URL fragment, exchanged for an
/// HttpOnly session cookie. The only `/api` write body that legitimately carries
/// a secret — which is why it travels in the JSON body (never a query string, so
/// it can't land in a server log) and the endpoint is served over loopback only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    pub token: String,
}

/// The sandbox policy a repository's hooks actually run under
/// (`SECURITY_MODEL.md:236`, #66 — declared by ADR 0025, made real by M1.13b's
/// tier dispatch). Not a `bool`, for ADR 0015's reason: a closed, *named*
/// vocabulary can grow without a breaking wire change, which is exactly what
/// happened here.
///
/// # These four names are the server's own `sandbox::Tier`, plus one
///
/// [`Strict`](Self::Strict), [`Network`](Self::Network) and
/// [`Unsandboxed`](Self::Unsandboxed) are the three tiers
/// `git-vista-server`'s `sandbox::tier_for` dispatches to, reported on the
/// wire under the same names so the disclosed value and the enforced value
/// cannot drift into two vocabularies. [`Blocked`](Self::Blocked) is the
/// fourth: hooks suppressed outright.
///
/// # What changed from ADR 0025, and why it is not additive
///
/// ADR 0025 shipped two variants, `allow` / `restricted`, *session*-scoped and
/// **declared, not enforced** — no code read them. INV-15 needs a per-repository
/// value that names the tier the git-spawn chokepoint really uses, and the
/// banner polarity inverts with it (ADR 0025 flew the banner on `allow`; INV-15
/// flies it on anything that is **not** `strict` — see [`requires_banner`]).
/// The old wire strings still *deserialize* (`restricted` → `Strict`, `allow` →
/// `Unsandboxed`) so a stored older value is not a hard error, but they are
/// never emitted again. This is a value-domain change, not an additive field:
/// see the note in `dto_golden.rs` when its fixture is regenerated.
///
/// [`requires_banner`]: Self::requires_banner
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPolicy {
    /// Landlock + seccomp inside a bwrap pid/net/ipc/uts/cgroup namespace
    /// (`sandbox::Tier::Strict`). Hooks run, and they reach no network and no
    /// unix socket.
    ///
    /// **This is not "confined to the repository."** The same policy also
    /// grants read-only system trees, `$HOME` read-only minus the secret
    /// exclusion set, and a private `/dev` read-write. What it guarantees is
    /// narrower and stated in the server's own `sandbox` module docs: nothing
    /// is *written* outside the declared read-write trees, and nothing is
    /// *read* outside the declared read-only grants minus `secret_excludes`.
    /// The banner is silent for this one variant only, so overstating it here
    /// would be the one place a reader could be misled.
    #[serde(alias = "restricted")]
    Strict,
    /// Landlock + seccomp with no network namespace (`sandbox::Tier::Network`)
    /// — the only tier under which `git push`/`fetch`/`clone` can work, because
    /// a network namespace breaks DNS resolution. Hooks run with `AF_INET`
    /// reachable, which is why this flies the banner even though it is
    /// sandboxed.
    Network,
    /// No sandbox at all (`sandbox::Tier::Unsandboxed`). Reachable only through
    /// explicit, persisted, per-repository operator trust
    /// (`sandbox::trust::grant`), and it flies a permanent banner.
    ///
    /// ADR 0025's `allow` deserializes here, and that is the truthful reading
    /// rather than a convenient one: when M1.13a emitted `allow` there was no
    /// sandbox anywhere in the server, so hooks really did run unconfined.
    #[serde(alias = "allow")]
    Unsandboxed,
    /// Hooks do not run at all — `core.hooksPath` pointed at a server-owned
    /// empty directory.
    ///
    /// **No production policy constructor yields this today**, and that is
    /// checked, not merely asserted: `sandbox::escape_contract`'s R8 scan fails
    /// if any production `Policy` literal in the server sets a blocked hook
    /// mode. The variant exists because [`default`](Self::default) needs a
    /// value meaning "hooks are not known to be running," and because ADR 0029
    /// rules out the one mapping that would otherwise produce it: a host that
    /// cannot supply the strict tier must **refuse the operation**, not run it
    /// with hooks suppressed.
    Blocked,
}

impl HookPolicy {
    /// INV-15: a persistent, non-dismissible banner is required for anything
    /// other than [`Strict`](Self::Strict).
    ///
    /// The polarity is inverted from ADR 0025 on purpose. ADR 0025's `Allow`
    /// meant "hooks run unrestricted," so the banner marked the *permissive*
    /// case; these four name tiers, so the banner marks everything that is not
    /// the fullest isolation — including [`Blocked`](Self::Blocked), because
    /// "your hooks silently did not run" is a surprise a user must be told
    /// about just as much as "your hooks ran unsandboxed."
    ///
    /// Written as `!matches!(self, Strict)` rather than as an exhaustive match
    /// so a variant added later flies the banner by default. That is the
    /// fail-safe direction: a new tier no one has written banner text for
    /// over-warns instead of going silent.
    pub fn requires_banner(self) -> bool {
        !matches!(self, HookPolicy::Strict)
    }
}

impl Default for HookPolicy {
    /// Fail-closed, and one notch stricter than ADR 0025's `Restricted`: if the
    /// field is missing on the wire (an older server's response read by a newer
    /// client, via `SessionInfo`'s / `RepositoryDescriptor`'s
    /// `#[serde(default)]`), assume hooks are not running rather than assume
    /// they are running under a guarantee nothing measured.
    ///
    /// [`Strict`](Self::Strict) would be the wrong default precisely because it
    /// is the one variant that claims a guarantee *and* silences the banner —
    /// defaulting to it would turn an absent field into an unearned green
    /// light.
    fn default() -> Self {
        HookPolicy::Blocked
    }
}

// The `#[doc(hidden)]` transition constants `HookPolicy::{Allow, Restricted}`
// that stood here — a Rust-level migration shim so call sites written against
// ADR 0025's two-variant vocabulary kept compiling while the enum widened —
// are **deleted** (#202). Every call site now spells a tier name directly:
// `git-vista-server/src/security.rs`, `git-vista/src/features/session/core.rs`
// and `git-vista-protocol/tests/dto_golden.rs` were the three named holdouts,
// and all three were migrated before the constants went.
//
// The `#[serde(alias = "restricted")]` / `#[serde(alias = "allow")]` on the
// enum above **stay**, deliberately, and their deletion is not a follow-up.
// The constants were a *source-compatibility* shim with no wire meaning; the
// aliases are a *wire-compatibility* property — an `allow`/`restricted` string
// written by an M1.13a build still parses instead of hard-erroring — pinned by
// `adr_0025_wire_strings_still_deserialize_and_never_serialize` below. Dropping
// them would narrow what this versioned contract accepts, which is a protocol
// change and not something a cleanup task gets to do as a side effect. Nothing
// re-emits those strings; the test proves that half too.

/// Response of `GET`/`POST /api/session` (M1.04): whether the caller now has a
/// live session and, when it does, the CSRF token to echo in the
/// [`CSRF_HEADER`](crate::CSRF_HEADER) on every state-changing request. `csrf` is
/// `None` exactly when `authenticated` is false, so the SPA can branch on either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub authenticated: bool,
    #[serde(default)]
    pub csrf: Option<String>,
    /// Whether this session was established through the LAN listener (ADR
    /// 0005). Additive field (M1.02 rule: new fields are `#[serde(default)]`,
    /// no protocol bump) — an older client ignores it. Purely a UI signal: the
    /// LAN listener's write routes are structurally absent regardless of what
    /// a client does with this flag.
    #[serde(default)]
    pub via_lan: bool,
    /// The current hook policy (M1.13a, #66, ADR 0025) — see [`HookPolicy`]'s
    /// own doc comment for what this does and does not mean today. Additive
    /// field, same `#[serde(default)]` convention as `via_lan` above.
    #[serde(default)]
    pub hook_policy: HookPolicy,
}

/// Response of `GET /api/rebase-status`: whether "Rebase onto main" would do
/// anything right now, resolved live server-side — the same freshness posture
/// as `/api/head-branch`, so the menu can disable the item instead of offering
/// a rebase that no-ops (or the nonsense "rebase ‘main’ onto main").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseStatus {
    /// The checked-out branch; `None` => detached HEAD (nothing to rebase).
    pub branch: Option<String>,
    /// What the server would rebase onto: `origin/main` when that
    /// remote-tracking ref exists, else the local `main`.
    pub base: String,
    /// False when `base` doesn't resolve at all (a repo with no `main`).
    pub base_exists: bool,
    /// True when HEAD already contains the base tip — the branch is already
    /// based on the latest `base`, so a rebase would change nothing.
    pub up_to_date: bool,
    /// Whether `branch` has a recorded upstream (`<branch>@{upstream}`), as
    /// git resolves it — not merely whether a same-named remote-tracking ref
    /// exists, which is a different and weaker question (#233). `None` when
    /// there is no `branch` to ask (detached HEAD); `Some(false)` is a real,
    /// reportable absence, not "unknown" — an unreadable repository fails the
    /// whole response, same as `base_exists`/`up_to_date` above, rather than
    /// reporting a silent `false` here. Additive field: this DTO carries no
    /// `deny_unknown_fields` (response DTOs never do), and `#[serde(default)]`
    /// keeps a newer frontend working against an older server that doesn't
    /// send it yet.
    #[serde(default)]
    pub has_upstream: Option<bool>,
}

/// How a servable repository entry relates to git's on-disk layout (M1.03). The
/// catalog classifies every registered repository so the API — and eventually the
/// UI — can treat a bare repository or a linked worktree explicitly instead of
/// assuming one working tree per clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryKind {
    /// A bare repository: a git directory with no working tree. Reads work; the
    /// working-tree/status reads and every mutation are meaningless and refused.
    Bare,
    /// The main working tree of a repository (its git dir *is* the common dir).
    MainWorktree,
    /// A linked worktree (`git worktree add`): its own working tree and git dir
    /// under `…/worktrees/<name>`, sharing the repository's common dir — so it
    /// shares the [`repository`](RepositoryDescriptor::repository) id but carries
    /// a distinct [`worktree`](RepositoryDescriptor::worktree) id.
    LinkedWorktree,
}

/// One entry in the server-owned repository catalog (M1.03), as reported to the
/// client. This is the *capability* view: it addresses a repository by opaque
/// ids, never by a filesystem path, so the browser selects what to act on with a
/// [`worktree`](Self::worktree) id it cannot forge into a path.
///
/// The id fields are the opaque string forms of `git-vista-core`'s
/// `RepositoryId`/`WorktreeId`. They are kept as plain strings here to hold the
/// transport/domain boundary this crate exists to enforce: the client treats them
/// as meaningless handles and echoes them back, and only the native backend maps
/// an id to a path — through the catalog, which fails closed on anything it did
/// not itself register.
///
/// `path` is `None` by default. Absolute paths are server-filesystem detail and
/// are omitted unless the operator opts in (the `GIT_VISTA_EXPOSE_PATHS`
/// diagnostic), so the capability report never leaks the layout of the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDescriptor {
    /// Opaque id of the shared repository (its common git directory). Every
    /// worktree of one clone reports the same value here.
    pub repository: String,
    /// Opaque id of this specific worktree — the handle the client sends back to
    /// address this entry. Distinct per worktree even within one repository.
    pub worktree: String,
    /// A short, non-path display label (the directory's base name), safe to show
    /// in the UI without revealing where on disk the repository lives.
    pub name: String,
    /// Whether this entry is a bare repo, the main worktree, or a linked worktree.
    pub kind: RepositoryKind,
    /// True when the entry is view-only (e.g. a clone opened from a URL): every
    /// mutation is refused, mirroring the graph's `read_only`.
    pub read_only: bool,
    /// The absolute filesystem path — omitted (`None`) unless the operator opted
    /// into path exposure. Never sent by default; the client must not depend on it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// The repo's `origin` remote normalized to a browsable https base URL
    /// (ADR 0010), e.g. `"https://github.com/owner/repo"`. `None` when there is
    /// no usable remote. Optional on the wire (M1.02 contract rule).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_web_url: Option<String>,
    /// INV-15's **per-repository** hook-policy disclosure (#66 M1.13b, #202):
    /// the sandbox tier a *local* operation on this repository actually runs
    /// under, as computed by the server's own tier dispatch. Additive optional
    /// field (M1.02 rule, same `skip_serializing_if`/`default` shape as
    /// [`path`](Self::path) and [`remote_web_url`](Self::remote_web_url)), so an
    /// older client that has never heard of it keeps parsing this object.
    ///
    /// # `None` means "not disclosed", and it is never a guarantee
    ///
    /// Three different situations all arrive as `None`, and a client must treat
    /// every one of them the same way — as *unknown*, which
    /// [`hook_policy_requires_banner`](Self::hook_policy_requires_banner) folds
    /// to "fly the banner":
    ///
    /// * the response came from a server build predating this field;
    /// * the server refuses operations on this repository altogether, because
    ///   the host cannot supply the tier they require (INV-13 / ADR 0029 —
    ///   there is deliberately **no** [`HookPolicy`] variant meaning "refused",
    ///   since inventing one would be the degrade-and-block-hooks posture that
    ///   ADR rejects by name);
    /// * the server had not yet established a sandbox verdict when it built
    ///   this descriptor.
    ///
    /// Note what this value is scoped to: a **local** operation. A `push` on the
    /// same repository transiently runs under [`HookPolicy::Network`], which a
    /// per-repository field cannot express — see the server's
    /// `sandbox::hook_policy` module docs for why per-operation disclosure needs
    /// a call site that knows the operation.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub hook_policy: Option<HookPolicy>,
}

impl RepositoryDescriptor {
    /// Whether INV-15's persistent banner must be shown for this repository.
    ///
    /// Fail-safe on both axes: an *absent* policy over-warns (see
    /// [`hook_policy`](Self::hook_policy) — "not disclosed" is not a green
    /// light), and a *present* one defers to
    /// [`HookPolicy::requires_banner`], which itself over-warns for any variant
    /// that is not [`Strict`](HookPolicy::Strict). There is no input to this
    /// function that silences the banner without the server having positively
    /// disclosed the one tier that earns silence.
    pub fn hook_policy_requires_banner(&self) -> bool {
        self.hook_policy.is_none_or(HookPolicy::requires_banner)
    }
}

/// Which of git's two kinds of tag a [`TagDetail`] describes (M2.21a, #235,
/// ADR 0041). Shapes only in this slice — the read endpoint that produces
/// `TagDetail`s is a later M2.21 slice of #74.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagKind {
    /// A bare ref under `refs/tags/` pointing straight at a commit — no tag
    /// object, so no message, no tagger, and nothing to sign.
    Lightweight,
    /// A real tag *object* (message, tagger, date, optionally a GPG
    /// signature) that the ref points at, which in turn points at the
    /// tagged commit.
    Annotated,
}

/// What is known about an annotated tag's GPG signature (M2.21a, #235).
///
/// **Shapes only** — no verification logic exists in this crate or, yet, in
/// the server; M2.21c (#74) owns running the actual verification. The
/// vocabulary is closed and deliberately keeps "we could not check" apart
/// from "we checked and it failed": collapsing those two is how a UI ends up
/// calling an unverifiable signature invalid (alarming users over a missing
/// keyring) or, worse, valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureStatus {
    /// The tag carries no signature at all (every lightweight tag, and any
    /// unsigned annotated tag).
    Unsigned,
    /// A signature is present and verified against a known key.
    Valid,
    /// A signature is present and verification *ran* and *failed* — the
    /// bytes do not check out against the key that claims to have made them.
    Invalid,
    /// A signature is present but the key that made it is not in the
    /// verifying keyring — nothing can be said about the bytes either way.
    UnknownKey,
    /// A signature is present but verification could not run at all (no gpg
    /// on the host, config broken). Distinct from [`UnknownKey`]: there the
    /// machinery worked and lacked one key; here the machinery itself was
    /// unavailable — the same "could not be asked" honesty the server's
    /// `Obs::Unknown` keeps for observations.
    ///
    /// [`UnknownKey`]: Self::UnknownKey
    Unverifiable,
}

/// One tag, fully described — the read DTO every M2.21 tag-listing/detail
/// slice serves (M2.21a, #235, ADR 0041; shape modelled on #212's pattern of
/// landing the wire contract with a golden pin before its producer).
///
/// # `deny_unknown_fields` on a read DTO — deliberate, and cheap to relax
///
/// The crate's response DTOs usually omit `deny_unknown_fields` so an older
/// client keeps parsing when a newer server adds a field (M1.02 additive
/// rule; see [`AmendCommitError`]). `TagDetail` starts strict anyway because
/// it lands *before its producer exists*: until the M2.21 read slice ships,
/// every `TagDetail` a test or fixture constructs is hand-written, and
/// strictness turns a misspelled key in one of those into a loud error
/// instead of a silently-ignored field. Loosening later (dropping the
/// attribute when the additive rule first needs to apply) widens what the
/// contract accepts — a compatible change — while the reverse, tightening
/// after clients exist, would not be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagDetail {
    /// The tag's short name (`v1.0.0`).
    pub name: crate::plan::TagName,
    /// Lightweight or annotated — see [`TagKind`].
    pub kind: TagKind,
    /// The tagged **commit** — always the peeled target (`refs/tags/<name>^{}`),
    /// whichever kind the tag is, so a consumer never re-derives peeling.
    pub target: crate::plan::CommitOid,
    /// The tag *object's* own oid — `Some` exactly when
    /// [`kind`](Self::kind) is [`TagKind::Annotated`] (a lightweight tag has
    /// no object). This is the value [`RecoveryStrategy::RecreateTag`] wants
    /// as `at` should this tag be deleted.
    ///
    /// [`RecoveryStrategy::RecreateTag`]: crate::plan::RecoveryStrategy::RecreateTag
    pub tag_object: Option<crate::plan::CommitOid>,
    /// The tagger line's name/email/date as git renders it — annotated tags
    /// only, and plain display text (never parsed back).
    pub tagger: Option<String>,
    /// The annotation message — annotated tags only.
    pub message: Option<crate::plan::TagMessage>,
    /// What is known about the tag's signature — see [`SignatureStatus`].
    pub signature: SignatureStatus,
}

/// Validate a URL a user pasted to clone, before the server hands it to
/// `git clone` (Phase 12). This is a *gate*, not a parser: it accepts only the
/// public, read-oriented transports (`https://`, `http://`, `git://`) and rejects
/// everything else, so the pasted string can't be an SSH URL that would prompt for
/// keys, a local filesystem path, or an option smuggled in with a leading `-`.
/// git itself does the real URL parsing and reports a clear error if the host or
/// repo is wrong. Returns the trimmed URL on success, or a user-facing reason.
pub fn validate_clone_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Enter a repository URL.".to_string());
    }
    // Belt-and-braces even though the URL is passed as its own argv entry: a value
    // starting with '-' could still be read by git as an option.
    if url.starts_with('-') {
        return Err("URL can't start with '-'.".to_string());
    }
    const ALLOWED: [&str; 3] = ["https://", "http://", "git://"];
    if !ALLOWED.iter().any(|scheme| url.starts_with(scheme)) {
        return Err("Only https://, http:// or git:// URLs are supported.".to_string());
    }
    // Reject whitespace inside the URL — a single field should hold one URL, and it
    // keeps a space-separated second token from ever reaching git as an extra arg.
    if url.split_whitespace().count() != 1 {
        return Err("URL can't contain spaces.".to_string());
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_branch_request_roundtrips() {
        let req = CreateBranchRequest {
            name: "feature".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<CreateBranchRequest>(&json).unwrap(),
            req
        );
    }

    #[test]
    fn create_commit_request_roundtrips_with_and_without_branch() {
        let on_head = CreateCommitRequest {
            message: "msg".into(),
            allow_empty: false,
            branch: None,
        };
        let json = serde_json::to_string(&on_head).unwrap();
        assert_eq!(
            serde_json::from_str::<CreateCommitRequest>(&json).unwrap(),
            on_head
        );
        // `branch` defaults when absent from the wire.
        let back: CreateCommitRequest =
            serde_json::from_str(r#"{"message":"m","allow_empty":true}"#).unwrap();
        assert_eq!(back.branch, None);
    }

    #[test]
    fn amend_commit_request_roundtrips_and_rejects_unknown_fields() {
        let req = AmendCommitRequest {
            message: "msg".into(),
            allow_empty: false,
            expected_tip: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<AmendCommitRequest>(&json).unwrap(),
            req
        );
        assert!(serde_json::from_str::<AmendCommitRequest>(
            r#"{"message":"m","allow_empty":false,"expected_tip":"deadbeef","branch":"main"}"#
        )
        .is_err());
    }

    #[test]
    fn branch_and_clone_requests_roundtrip() {
        let b = BranchRequest {
            branch: "main".into(),
        };
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<BranchRequest>(&json).unwrap(), b);

        let c = CloneRequest {
            url: "https://example.com/r.git".into(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<CloneRequest>(&json).unwrap(), c);
    }

    #[test]
    fn worktree_paths_request_roundtrips_and_rejects_unknown_fields() {
        let req = WorktreePathsRequest {
            paths: vec!["a.txt".into(), "dir/b.txt".into()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<WorktreePathsRequest>(&json).unwrap(),
            req
        );
        assert!(serde_json::from_str::<WorktreePathsRequest>(
            r#"{"paths":["a.txt"],"repo":"/etc"}"#
        )
        .is_err());
    }

    /// M2.21d (#238). The load-bearing half is the last assertion: the wire
    /// has no way to say "annotated, but no message" — the shape that would
    /// make `git tag -a` open an editor on a headless server (ADR 0048).
    #[test]
    fn tag_requests_roundtrip_and_cannot_ask_for_an_annotation_with_no_message() {
        let lightweight = CreateTagRequest {
            name: "v1.0.0".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            message: None,
            sign: false,
        };
        let json = serde_json::to_string(&lightweight).unwrap();
        assert_eq!(
            serde_json::from_str::<CreateTagRequest>(&json).unwrap(),
            lightweight
        );

        let annotated = CreateTagRequest {
            message: Some("first stable release".into()),
            ..lightweight.clone()
        };
        let json = serde_json::to_string(&annotated).unwrap();
        assert_eq!(
            serde_json::from_str::<CreateTagRequest>(&json).unwrap(),
            annotated
        );

        // Both optional fields default when absent.
        let bare: CreateTagRequest =
            serde_json::from_str(r#"{"name":"v1","commit":"deadbeef"}"#).unwrap();
        assert_eq!(bare.message, None);
        assert!(!bare.sign);

        let del = DeleteTagRequest {
            tag: "v1.0.0".into(),
        };
        let json = serde_json::to_string(&del).unwrap();
        assert_eq!(
            serde_json::from_str::<DeleteTagRequest>(&json).unwrap(),
            del
        );

        // No path smuggling, and no separate annotate flag to disagree with
        // `message` — `annotated: true` is not a field, so a body carrying it
        // is a hard 400 rather than a request for an editor.
        assert!(serde_json::from_str::<CreateTagRequest>(
            r#"{"name":"v1","commit":"c","repo":"/etc"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<CreateTagRequest>(
            r#"{"name":"v1","commit":"c","annotated":true}"#
        )
        .is_err());
        assert!(serde_json::from_str::<DeleteTagRequest>(r#"{"tag":"v1","repo":"/etc"}"#).is_err());
    }

    #[test]
    fn request_bodies_reject_unknown_fields() {
        // The core of the "no path-based repository selection" guarantee at the
        // wire level: a stray `repo`/`path`/anything key is a hard error, not a
        // silently-dropped value that a future handler might start honouring.
        assert!(
            serde_json::from_str::<BranchRequest>(r#"{"branch":"main","repo":"/etc"}"#).is_err()
        );
        assert!(serde_json::from_str::<CloneRequest>(
            r#"{"url":"https://x/r.git","path":"/srv/secret"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<CreateBranchRequest>(
            r#"{"name":"b","commit":"c","dir":"/x"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<CreateCommitRequest>(
            r#"{"message":"m","allow_empty":false,"cwd":"/x"}"#
        )
        .is_err());
    }

    #[test]
    fn select_request_roundtrips_and_rejects_unknown_fields() {
        let req = SelectRequest {
            worktree: "22222222-2222-5222-8222-222222222222".into(),
            mode: RepoMode::Visualize,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<SelectRequest>(&json).unwrap(), req);
        // No path smuggling on the select body either.
        assert!(serde_json::from_str::<SelectRequest>(
            r#"{"worktree":"w","mode":"active","path":"/etc"}"#
        )
        .is_err());
    }

    #[test]
    fn delete_clone_request_round_trips_and_rejects_unknown_fields() {
        let req = DeleteCloneRequest {
            worktree: "11111111-2222-5333-8444-555555555555".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<DeleteCloneRequest>(&json).unwrap(),
            req
        );

        assert!(
            serde_json::from_str::<DeleteCloneRequest>(r#"{"worktree":"x","path":"/etc"}"#)
                .is_err()
        );
    }

    #[test]
    fn repo_mode_uses_stable_snake_case_wire_names() {
        // Wire names are contract (like RepositoryKind's): pin them so a rename
        // is a deliberate protocol change, never an accident.
        assert_eq!(
            serde_json::to_string(&RepoMode::Visualize).unwrap(),
            "\"visualize\""
        );
        assert_eq!(
            serde_json::to_string(&RepoMode::Active).unwrap(),
            "\"active\""
        );
    }

    #[test]
    fn session_dtos_roundtrip_and_reject_unknown_fields() {
        let req = SessionRequest {
            token: "deadbeef".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<SessionRequest>(&json).unwrap(), req);
        // A stray field on the bootstrap body is a hard error, like every other.
        assert!(serde_json::from_str::<SessionRequest>(r#"{"token":"x","extra":1}"#).is_err());

        let info = SessionInfo {
            authenticated: true,
            csrf: Some("csrf-token".into()),
            via_lan: false,
            hook_policy: HookPolicy::Unsandboxed,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<SessionInfo>(&json).unwrap(), info);
        // csrf defaults to None when absent (the unauthenticated response omits it);
        // hook_policy fails closed to `Blocked` per its own Default impl, the same
        // "an older/partial response still deserializes, safely" contract via_lan
        // already established.
        let back: SessionInfo = serde_json::from_str(r#"{"authenticated":false}"#).unwrap();
        assert_eq!(back.csrf, None);
        assert_eq!(back.hook_policy, HookPolicy::Blocked);
        assert!(
            back.hook_policy.requires_banner(),
            "an absent hook_policy must not silence the banner"
        );
    }

    /// INV-15's wire half. Every variant is pinned to the exact string the
    /// server's own `sandbox::Tier` names it by, in both directions, so a
    /// rename on either side of the wire is a failing test rather than a
    /// silently-misread policy.
    #[test]
    fn hook_policy_wire_names_match_the_sandbox_tier_names() {
        for (variant, wire) in [
            (HookPolicy::Strict, "strict"),
            (HookPolicy::Network, "network"),
            (HookPolicy::Unsandboxed, "unsandboxed"),
            (HookPolicy::Blocked, "blocked"),
        ] {
            assert_eq!(
                serde_json::to_string(&variant).unwrap(),
                format!("\"{wire}\""),
                "{variant:?} must serialize as {wire}"
            );
            assert_eq!(
                serde_json::from_str::<HookPolicy>(&format!("\"{wire}\"")).unwrap(),
                variant
            );
        }
        // An unknown policy name is a hard error, never a silent default —
        // `#[serde(default)]` governs an *absent* field, not a garbage one.
        assert!(serde_json::from_str::<HookPolicy>("\"contained\"").is_err());
    }

    /// ADR 0025's two shipped wire strings still deserialize (a stored older
    /// value must not be a hard error) and map to the tier that was actually
    /// in force when they were written — but they are never emitted again.
    #[test]
    fn adr_0025_wire_strings_still_deserialize_and_never_serialize() {
        assert_eq!(
            serde_json::from_str::<HookPolicy>("\"restricted\"").unwrap(),
            HookPolicy::Strict
        );
        assert_eq!(
            serde_json::from_str::<HookPolicy>("\"allow\"").unwrap(),
            HookPolicy::Unsandboxed
        );
        for v in [
            HookPolicy::Strict,
            HookPolicy::Network,
            HookPolicy::Unsandboxed,
            HookPolicy::Blocked,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            assert!(
                json != "\"allow\"" && json != "\"restricted\"",
                "{v:?} re-emitted a retired ADR 0025 wire string: {json}"
            );
        }
    }

    /// INV-15's banner half: the banner is silent for exactly one variant.
    /// Written as a full enumeration rather than by calling `requires_banner`'s
    /// own `!matches!`, so this would still fail if the implementation were
    /// inverted.
    #[test]
    fn only_strict_silences_the_banner() {
        assert!(!HookPolicy::Strict.requires_banner());
        for p in [
            HookPolicy::Network,
            HookPolicy::Unsandboxed,
            HookPolicy::Blocked,
        ] {
            assert!(p.requires_banner(), "{p:?} must fly the banner");
        }
    }

    #[test]
    fn rebase_status_roundtrips_through_json() {
        let status = RebaseStatus {
            branch: Some("feature".into()),
            base: "origin/main".into(),
            base_exists: true,
            up_to_date: false,
            has_upstream: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<RebaseStatus>(&json).unwrap(), status);
    }

    #[test]
    fn repository_descriptor_roundtrips_and_omits_path_by_default() {
        let d = RepositoryDescriptor {
            repository: "11111111-1111-5111-8111-111111111111".into(),
            worktree: "22222222-2222-5222-8222-222222222222".into(),
            name: "my-repo".into(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
            hook_policy: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        // A `None` path is skipped entirely — the wire form never carries the key,
        // so the capability report can't leak the server's filesystem by default.
        assert!(
            !json.contains("path"),
            "path must be omitted when None: {json}"
        );
        // Same for the optional forge base (ADR 0010 + M1.02 contract rule).
        assert!(
            !json.contains("remote_web_url"),
            "remote_web_url must be omitted when None: {json}"
        );
        // And for INV-15's per-repository disclosure: an undisclosed policy is
        // an *absent key*, never `"hook_policy": null` and — the part that
        // matters — never a fabricated value. An older client's `"hook_policy"
        // in obj` check must see exactly what a pre-#202 server sent.
        assert!(
            !json.contains("hook_policy"),
            "hook_policy must be omitted when None: {json}"
        );
        assert_eq!(
            serde_json::from_str::<RepositoryDescriptor>(&json).unwrap(),
            d
        );
    }

    /// INV-15's descriptor half: a disclosed policy reaches the wire under the
    /// tier name, and an *undisclosed* one is unknown rather than green.
    ///
    /// The banner assertions are the point. `None` is what an older server, a
    /// pre-verdict descriptor, and an ADR-0029 refusal all look like, and none
    /// of the three is permitted to silence the banner — that would be the
    /// "computed but never disclosed" failure INV-15 exists to prevent, wearing
    /// an absent field instead of a wrong one.
    #[test]
    fn descriptor_hook_policy_reaches_the_wire_and_absence_never_silences_the_banner() {
        let base = RepositoryDescriptor {
            repository: "11111111-1111-5111-8111-111111111111".into(),
            worktree: "22222222-2222-5222-8222-222222222222".into(),
            name: "my-repo".into(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
            hook_policy: None,
        };

        assert!(
            base.hook_policy_requires_banner(),
            "an undisclosed policy must fly the banner, not silence it"
        );

        for (policy, wire, banner) in [
            (HookPolicy::Strict, "strict", false),
            (HookPolicy::Network, "network", true),
            (HookPolicy::Unsandboxed, "unsandboxed", true),
            (HookPolicy::Blocked, "blocked", true),
        ] {
            let d = RepositoryDescriptor {
                hook_policy: Some(policy),
                ..base.clone()
            };
            let json = serde_json::to_string(&d).unwrap();
            assert!(
                json.contains(&format!("\"hook_policy\":\"{wire}\"")),
                "{policy:?} must reach the wire as {wire}: {json}"
            );
            assert_eq!(
                serde_json::from_str::<RepositoryDescriptor>(&json).unwrap(),
                d
            );
            assert_eq!(
                d.hook_policy_requires_banner(),
                banner,
                "{policy:?} banner polarity"
            );
        }

        // An older server's object — no key at all — still deserializes, and
        // lands on the unknown/banner-flying side rather than defaulting into a
        // policy nobody measured.
        let older: RepositoryDescriptor = serde_json::from_str(
            r#"{"repository":"r","worktree":"w","name":"n","kind":"main_worktree","read_only":false}"#,
        )
        .expect("a pre-#202 descriptor must still parse");
        assert_eq!(older.hook_policy, None);
        assert!(older.hook_policy_requires_banner());
    }

    #[test]
    fn repository_kind_uses_stable_snake_case_wire_names() {
        // The wire names are part of the contract; pin them so a rename is a
        // deliberate, visible protocol change rather than an accident.
        assert_eq!(
            serde_json::to_string(&RepositoryKind::Bare).unwrap(),
            "\"bare\""
        );
        assert_eq!(
            serde_json::to_string(&RepositoryKind::MainWorktree).unwrap(),
            "\"main_worktree\""
        );
        assert_eq!(
            serde_json::to_string(&RepositoryKind::LinkedWorktree).unwrap(),
            "\"linked_worktree\""
        );
    }

    #[test]
    fn clone_url_accepts_public_transports_and_trims() {
        assert_eq!(
            validate_clone_url("  https://github.com/rust-lang/rust.git "),
            Ok("https://github.com/rust-lang/rust.git".to_string())
        );
        assert!(validate_clone_url("http://example.com/r.git").is_ok());
        assert!(validate_clone_url("git://example.com/r.git").is_ok());
    }

    #[test]
    fn clone_url_rejects_unsafe_or_unsupported() {
        // SSH URL (would prompt for keys), local path, empty, option-like, spaces.
        assert!(validate_clone_url("git@github.com:owner/repo.git").is_err());
        assert!(validate_clone_url("/home/tom/secret").is_err());
        assert!(validate_clone_url("file:///etc").is_err());
        assert!(validate_clone_url("").is_err());
        assert!(validate_clone_url("   ").is_err());
        assert!(validate_clone_url("--upload-pack=evil").is_err());
        assert!(validate_clone_url("https://a.com/r.git --extra").is_err());
    }
}
