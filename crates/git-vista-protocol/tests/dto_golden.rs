//! Golden-fixture test for the request/response DTOs in `dto.rs` (#67 M1.14,
//! "fixture compatibility" — see the module docs on `history_golden.rs` and
//! `plan_golden.rs` for the two other wire families this same pattern already
//! covers; `dto.rs`'s own `#[cfg(test)] mod tests` already round-trips each
//! type in isolation, which catches a rename within one test run but not a
//! shape that drifted while staying internally consistent — only a
//! byte-for-byte comparison against a *committed* file catches that).
//!
//! `tests/fixtures/dto_v1.json` is the **committed** wire form of one
//! representative instance of every public DTO `dto.rs` puts on the wire,
//! bundled into one [`DtoGoldenSet`] so the whole family regenerates and
//! compares as a single file rather than sprawling into one tiny file per
//! type. Covers the awkward cases deliberately, not just the happy path:
//! `CreateCommitRequest`/`SessionInfo`/`RepositoryDescriptor` each appear
//! twice, once with their optional fields present and once with them
//! absent, since an optional field silently becoming required (or the
//! reverse) is exactly the kind of change a plain round-trip test cannot
//! catch but a byte-for-byte fixture does.
//!
//! Same two-directions proof as the other golden tests: the fixture
//! deserializes into exactly the values built here, and re-serializing
//! reproduces the fixture byte for byte.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test dto_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).

use git_vista_protocol::{
    AmendCommitError, AmendCommitRequest, AmendCommitSuccess, AmendFailureKind, BranchRequest,
    CloneRequest, CommitOid, CreateBranchRequest, CreateCommitRequest, DeleteCloneRequest,
    FetchError, FetchFailureKind, FetchRequest, FetchSuccess, ForcePublish, HookPolicy,
    MergeStrategy, PullError, PullFailureKind, PullRequest, PullSuccess, PushRequest, RebaseStatus,
    RemoteRefUpdate, RepoMode, RepositoryDescriptor, RepositoryKind, SelectRequest, SessionInfo,
    SessionRequest,
};
use serde::{Deserialize, Serialize};

const FIXTURE: &str = include_str!("fixtures/dto_v1.json");
const FIXTURE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dto_v1.json");

/// Every public DTO in `dto.rs`, bundled so the whole family is one fixture
/// file. Field order here is the field order on the wire.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct DtoGoldenSet {
    create_branch_request: CreateBranchRequest,
    create_commit_request_on_head: CreateCommitRequest,
    create_commit_request_with_branch: CreateCommitRequest,
    amend_commit_request: AmendCommitRequest,
    amend_commit_success_published: AmendCommitSuccess,
    amend_commit_success_unknown_reach: AmendCommitSuccess,
    amend_commit_error_hook_rejected: AmendCommitError,
    branch_request: BranchRequest,
    fetch_request: FetchRequest,
    fetch_success_with_updates: FetchSuccess,
    fetch_success_already_up_to_date: FetchSuccess,
    fetch_error_auth: FetchError,
    fetch_error_cancelled_after_a_ref_moved: FetchError,
    pull_request_merge: PullRequest,
    pull_request_rebase: PullRequest,
    pull_success_advanced: PullSuccess,
    pull_success_already_up_to_date: PullSuccess,
    pull_error_strategy_required: PullError,
    pull_error_conflict_restored: PullError,
    pull_error_conflict_left_in_progress: PullError,
    push_request_plain: PushRequest,
    push_request_upstream_and_lease: PushRequest,
    clone_request: CloneRequest,
    select_request: SelectRequest,
    delete_clone_request: DeleteCloneRequest,
    session_request: SessionRequest,
    session_info_authenticated: SessionInfo,
    session_info_unauthenticated: SessionInfo,
    rebase_status: RebaseStatus,
    repository_descriptor_minimal: RepositoryDescriptor,
    repository_descriptor_with_path_and_remote: RepositoryDescriptor,
}

fn golden_set() -> DtoGoldenSet {
    DtoGoldenSet {
        create_branch_request: CreateBranchRequest {
            name: "feature/idea".to_string(),
            commit: "1111111111111111111111111111111111111111".to_string(),
        },
        // No `branch` on the wire — the common case, a plain commit on HEAD.
        create_commit_request_on_head: CreateCommitRequest {
            message: "feat: land the thing".to_string(),
            allow_empty: false,
            branch: None,
        },
        // `branch` present — the empty-commit-onto-a-stub-branch case.
        create_commit_request_with_branch: CreateCommitRequest {
            message: "chore: start the branch".to_string(),
            allow_empty: true,
            branch: Some("feature/idea".to_string()),
        },
        // M2.19a (#222): the DTO the issue's own acceptance criteria asked
        // for, added contract-only alongside `GitOperation::AmendCommit`;
        // M2.19b (#223) wired the handler that builds it.
        amend_commit_request: AmendCommitRequest {
            message: "fix: correct the typo".to_string(),
            allow_empty: false,
            expected_tip: "5555555555555555555555555555555555555555".to_string(),
        },
        // M2.19b (#223): the amend response contract, both bodies. The
        // published flag is deliberately three-state — `Some(true)` here
        // (the case the flag exists for), `None` below (the walk failed;
        // "unknown" must stay distinct from "not published" on the wire, so
        // this fixture pins that `None` serializes as an explicit null and
        // not as an absent-therefore-false key).
        amend_commit_success_published: AmendCommitSuccess {
            message: "Amended commit.".to_string(),
            old_tip: "5555555555555555555555555555555555555555".to_string(),
            new_tip: Some("6666666666666666666666666666666666666666".to_string()),
            amended_published_commit: Some(true),
        },
        amend_commit_success_unknown_reach: AmendCommitSuccess {
            message: "Amended commit.".to_string(),
            old_tip: "5555555555555555555555555555555555555555".to_string(),
            new_tip: None,
            amended_published_commit: None,
        },
        // The typed failure body: `kind` is the tag M2.19d branches on
        // instead of regex-sniffing stderr; `message` stays git's (or the
        // hook's) own words. `hook_rejected` is the variant pinned because
        // it is the one whose wire spelling a client is most likely to
        // hard-code a match on.
        amend_commit_error_hook_rejected: AmendCommitError {
            kind: AmendFailureKind::HookRejected,
            message: "pre-commit: trailing whitespace on line 3".to_string(),
        },
        branch_request: BranchRequest {
            branch: "main".to_string(),
        },
        // M2.20c (#229). A remote *name*, never a URL — see `FetchRequest`.
        fetch_request: FetchRequest {
            remote: "origin".to_string(),
        },
        // A ref that moved and a ref that is new on the remote, so both
        // `old_oid` shapes (`Some`/`None`) are pinned on the wire.
        fetch_success_with_updates: FetchSuccess {
            remote: "origin".to_string(),
            message: "Fetched from ‘origin’: 2 remote-tracking refs updated.".to_string(),
            updated_refs: vec![
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/main".to_string(),
                    old_oid: Some("1111111111111111111111111111111111111111".to_string()),
                    new_oid: Some("2222222222222222222222222222222222222222".to_string()),
                },
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/feature".to_string(),
                    old_oid: None,
                    new_oid: Some("3333333333333333333333333333333333333333".to_string()),
                },
            ],
        },
        // The no-op success. `updated_refs` must reach the wire as `[]`, not
        // be omitted: "the fetch ran and nothing moved" is the answer, and a
        // missing key would read as "the server didn't say".
        fetch_success_already_up_to_date: FetchSuccess {
            remote: "origin".to_string(),
            message: "Fetched from ‘origin’: already up to date.".to_string(),
            updated_refs: Vec::new(),
        },
        fetch_error_auth: FetchError {
            kind: FetchFailureKind::AuthenticationFailed,
            message: "fatal: Authentication failed for 'https://example.invalid/repo.git/'"
                .to_string(),
            updated_refs: Vec::new(),
        },
        // The case the taxonomy exists for: cancelled *after* something had
        // already landed locally. A client that renders "nothing changed" for
        // every cancel would be lying here, which is why the ref list is a
        // typed field on the error and not a sentence in `message`.
        fetch_error_cancelled_after_a_ref_moved: FetchError {
            kind: FetchFailureKind::Cancelled,
            message: "The fetch from ‘origin’ was cancelled after 1 remote-tracking ref \
                      had already been updated."
                .to_string(),
            updated_refs: vec![RemoteRefUpdate {
                ref_name: "refs/remotes/origin/main".to_string(),
                old_oid: Some("1111111111111111111111111111111111111111".to_string()),
                new_oid: Some("2222222222222222222222222222222222222222".to_string()),
            }],
        },
        // M2.20d (#230). Both strategies are pinned as *request* fixtures on
        // purpose: `strategy` is the one field on this wire family with no
        // default and no third value, so its two spellings ("merge" /
        // "rebase") are exactly the bytes a client hard-codes. A rename here
        // would silently turn every pull into a 400.
        pull_request_merge: PullRequest {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            strategy: MergeStrategy::Merge,
        },
        pull_request_rebase: PullRequest {
            remote: "upstream".to_string(),
            branch: "release/2026-08".to_string(),
            strategy: MergeStrategy::Rebase,
        },
        pull_success_advanced: PullSuccess {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            strategy: MergeStrategy::Rebase,
            message: "Pulled ‘main’ from ‘origin’ into the checked-out branch \
                      (rebase strategy)."
                .to_string(),
            updated_refs: vec![RemoteRefUpdate {
                ref_name: "refs/remotes/origin/main".to_string(),
                old_oid: Some("1111111111111111111111111111111111111111".to_string()),
                new_oid: Some("2222222222222222222222222222222222222222".to_string()),
            }],
            advanced: true,
        },
        // The no-op success. `advanced: false` must reach the wire as an
        // explicit `false`, not be omitted: "the pull ran and the branch did
        // not move" is the answer, and a missing key would read as "the server
        // didn't say".
        pull_success_already_up_to_date: PullSuccess {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            strategy: MergeStrategy::Merge,
            message: "Already up to date — ‘main’ on ‘origin’ has nothing the \
                      checked-out branch doesn’t already have."
                .to_string(),
            updated_refs: Vec::new(),
            advanced: false,
        },
        // #230's headline refusal, pinned because the frontend branches on it
        // to prompt for a strategy rather than showing a generic error.
        pull_error_strategy_required: PullError {
            kind: PullFailureKind::StrategyRequired,
            message: "A pull must say how to integrate what it fetches: send \
                      \"strategy\": \"merge\" or \"strategy\": \"rebase\"."
                .to_string(),
            updated_refs: Vec::new(),
            worktree_restored: true,
        },
        // The two conflict outcomes, both pinned: they demand opposite things
        // of the user (choose again vs. your working tree needs attention), so
        // a client that collapsed them would give the wrong advice half the
        // time. `worktree_restored` is what tells them apart.
        pull_error_conflict_restored: PullError {
            kind: PullFailureKind::Conflict,
            message: "Pulling ‘main’ from ‘origin’ (merge strategy) failed: \
                      CONFLICT (content): Merge conflict in a.txt The merge was \
                      aborted and the checked-out branch is back where it started."
                .to_string(),
            updated_refs: vec![RemoteRefUpdate {
                ref_name: "refs/remotes/origin/main".to_string(),
                old_oid: Some("1111111111111111111111111111111111111111".to_string()),
                new_oid: Some("2222222222222222222222222222222222222222".to_string()),
            }],
            worktree_restored: true,
        },
        pull_error_conflict_left_in_progress: PullError {
            kind: PullFailureKind::ConflictLeftInProgress,
            message: "Pulling ‘main’ from ‘origin’ (rebase strategy) failed: \
                      error: could not apply 1a2b3c4 The rebase could not be \
                      aborted cleanly."
                .to_string(),
            updated_refs: Vec::new(),
            worktree_restored: false,
        },
        // M2.20e (#231). Both ends of `PushRequest`'s range, because the two
        // pin different things. The plain one is what every client written
        // before this slice sends — including the live frontend — and its
        // fixture bytes are what a reviewer checks when asking "did the
        // defaults stay the *safe* end?". The full one pins the lease's wire
        // shape reaching this endpoint: `{"mode": "with_lease",
        // "expected_remote_tip": …}`, the same internally-tagged encoding
        // `plan_golden` pins inside the operation, so a client cannot be
        // correct against one and wrong against the other.
        push_request_plain: PushRequest {
            branch: "main".to_string(),
            set_upstream: false,
            force: None,
        },
        push_request_upstream_and_lease: PushRequest {
            branch: "feature/x".to_string(),
            set_upstream: true,
            force: Some(ForcePublish::WithLease {
                expected_remote_tip: CommitOid::new("4".repeat(40)).unwrap(),
            }),
        },
        clone_request: CloneRequest {
            url: "https://github.com/owner/repo.git".to_string(),
        },
        select_request: SelectRequest {
            worktree: "22222222-2222-5222-8222-222222222222".to_string(),
            mode: RepoMode::Visualize,
        },
        delete_clone_request: DeleteCloneRequest {
            worktree: "33333333-3333-5333-8333-333333333333".to_string(),
        },
        session_request: SessionRequest {
            token: "bootstrap-token-deadbeef".to_string(),
        },
        // `csrf` present — the authenticated shape a client actually acts on.
        // `hook_policy: Strict` here: the one variant that silences INV-15's
        // banner, so it is the value most worth pinning on the wire.
        //
        // The committed fixture used to spell these `restricted`/`allow`
        // (ADR 0025's two-variant vocabulary). #202 widened `HookPolicy` to
        // the server's own `sandbox::Tier` names, which is a **value-domain**
        // change, not an additive field — hence the regeneration. The old
        // strings still deserialize via `#[serde(alias)]`, and the fact that
        // the *deserialize* half of this test kept passing against the older
        // committed fixture is what proved those aliases work on real stored
        // data; only the re-serialize half needed updating.
        session_info_authenticated: SessionInfo {
            authenticated: true,
            csrf: Some("csrf-token-abc123".to_string()),
            via_lan: false,
            hook_policy: HookPolicy::Strict,
        },
        // `csrf` absent — the unauthenticated shape; `via_lan` also exercises
        // its `#[serde(default)]` additive-field posture at `false`.
        // `hook_policy: Unsandboxed` covers a banner-flying variant.
        session_info_unauthenticated: SessionInfo {
            authenticated: false,
            csrf: None,
            via_lan: false,
            hook_policy: HookPolicy::Unsandboxed,
        },
        rebase_status: RebaseStatus {
            branch: Some("feature/idea".to_string()),
            base: "origin/main".to_string(),
            base_exists: true,
            up_to_date: false,
        },
        // `path`, `remote_web_url` and `hook_policy` all absent — the default
        // capability report shape, which must never leak the server's
        // filesystem, and the shape a pre-#202 server emitted.
        repository_descriptor_minimal: RepositoryDescriptor {
            repository: "11111111-1111-5111-8111-111111111111".to_string(),
            worktree: "22222222-2222-5222-8222-222222222222".to_string(),
            name: "git-vista".to_string(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
            hook_policy: None,
        },
        // Every optional field present — the `GIT_VISTA_EXPOSE_PATHS` shape,
        // and a `LinkedWorktree` kind so all three `RepositoryKind` variants
        // appear somewhere in this fixture set (`Bare` on its own doesn't get
        // a full descriptor here since `dto.rs`'s own
        // `repository_kind_uses_stable_snake_case_wire_names` unit test
        // already pins all three tag strings directly; this fixture's job is
        // whole-object shape, not re-pinning tags a simpler test already
        // covers).
        repository_descriptor_with_path_and_remote: RepositoryDescriptor {
            repository: "11111111-1111-5111-8111-111111111111".to_string(),
            worktree: "44444444-4444-5444-8444-444444444444".to_string(),
            name: "git-vista-linked".to_string(),
            kind: RepositoryKind::LinkedWorktree,
            read_only: true,
            path: Some("/home/tom/repos/git-vista/.worktrees/linked".to_string()),
            remote_web_url: Some("https://github.com/owner/repo".to_string()),
            // INV-15's per-repository disclosure, present. `Unsandboxed` on
            // purpose rather than `Strict`: it is the value an operator-trusted
            // repository discloses, the one this field exists to surface, and a
            // banner-flying variant — so the fixture pins the interesting case
            // rather than the quiet one. The minimal descriptor above pins the
            // absent case.
            hook_policy: Some(HookPolicy::Unsandboxed),
        },
    }
}

#[test]
fn dto_v1_golden() {
    let set = golden_set();

    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&set).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    let parsed: DtoGoldenSet =
        serde_json::from_str(&fixture).expect("fixture must deserialize into DtoGoldenSet");
    assert_eq!(
        parsed, set,
        "fixture and in-code golden DTO set diverged — if this is deliberate, \
         regenerate with REGEN_GOLDEN=1 and review the diff"
    );

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized DTO set no longer matches the committed fixture at \
         tests/fixtures/dto_v1.json — if this wire change is intentional and \
         the protocol version was bumped where the change warrants it, \
         regenerate with `REGEN_GOLDEN=1 cargo test -p git-vista-protocol \
         --test dto_golden`, review the diff, and record the protocol \
         implications; if it was not intentional, you have just broken \
         whatever older client depended on this shape"
    );

    // The optional-field awkward cases, checked directly against the raw
    // JSON rather than only through the Rust types — a field that silently
    // changed between "always present, possibly null" and "omitted when
    // absent" would round-trip fine through Rust but is still a real
    // wire-shape change an older client's `JSON.parse` sees differently
    // (`"x" in obj` vs `obj.x === null` are not the same check).
    //
    // This codebase's optional response/request fields are **not** uniform
    // on this point, confirmed against `dto.rs`'s actual `#[serde(...)]`
    // attributes rather than assumed: `CreateCommitRequest::branch` and
    // `SessionInfo::csrf` carry only `#[serde(default)]` (accepted absent on
    // deserialize, but serialize as an explicit `null` when `None` — present
    // key, null value), while `RepositoryDescriptor::path`/`remote_web_url`
    // additionally carry `skip_serializing_if = "Option::is_none"` (key
    // genuinely absent when `None`). Both are legitimate, deliberate choices
    // already in the code; this fixture pins which fields do which rather
    // than assuming they're all the same.
    let value: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    let obj = value.as_object().unwrap();
    assert_eq!(
        obj["create_commit_request_on_head"]
            .as_object()
            .unwrap()
            .get("branch"),
        Some(&serde_json::Value::Null),
        "create_commit_request_on_head's `branch` must be present-but-null \
         when absent (only #[serde(default)], no skip_serializing_if) — if \
         this now omits the key instead, that's a real wire-shape change"
    );
    assert_eq!(
        obj["session_info_unauthenticated"]
            .as_object()
            .unwrap()
            .get("csrf"),
        Some(&serde_json::Value::Null),
        "session_info_unauthenticated's `csrf` must be present-but-null when \
         absent, same reasoning as branch above"
    );
    assert!(
        obj["repository_descriptor_minimal"]
            .as_object()
            .unwrap()
            .get("path")
            .is_none(),
        "repository_descriptor_minimal must omit `path` entirely, not null \
         it — this is the guarantee that the capability report never leaks \
         the server's filesystem by default"
    );
    assert!(
        obj["repository_descriptor_minimal"]
            .as_object()
            .unwrap()
            .get("remote_web_url")
            .is_none(),
        "repository_descriptor_minimal must omit `remote_web_url` entirely, not null it"
    );
    // INV-15's per-repository field (#202). Same omit-don't-null posture as the
    // two above, and for a sharper reason: an absent `hook_policy` is what a
    // pre-#202 server sends and what an ADR-0029 refusal sends, and the client
    // must be able to tell "the server said nothing" from "the server said
    // something" with `"hook_policy" in obj` — which a null would defeat.
    assert!(
        obj["repository_descriptor_minimal"]
            .as_object()
            .unwrap()
            .get("hook_policy")
            .is_none(),
        "repository_descriptor_minimal must omit `hook_policy` entirely, not null it"
    );
    assert_eq!(
        obj["repository_descriptor_with_path_and_remote"]
            .as_object()
            .unwrap()
            .get("hook_policy")
            .and_then(|v| v.as_str()),
        Some("unsandboxed"),
        "a disclosed policy must reach the wire under the server's own tier name"
    );
    // M2.19b (#223): the amend success body's two optionals are
    // present-but-null when absent (only #[serde(default)], no
    // skip_serializing_if) — deliberately, because for
    // `amended_published_commit` the null IS the payload: "the walk failed,
    // reachability unknown" must stay distinguishable from `false` ("the
    // walk ran and found nothing") for the client's warning to be honest.
    // Omitting the key would make an old-fixture-shaped parse read unknown
    // as not-published, the exact fail-open collapse `Obs` exists to
    // prevent server-side.
    let unknown_reach = obj["amend_commit_success_unknown_reach"]
        .as_object()
        .unwrap();
    assert_eq!(
        unknown_reach.get("amended_published_commit"),
        Some(&serde_json::Value::Null),
        "amended_published_commit must be present-but-null when unknown — \
         null is the honest 'walk failed' answer, distinct from false"
    );
    assert_eq!(
        unknown_reach.get("new_tip"),
        Some(&serde_json::Value::Null),
        "new_tip must be present-but-null when the post-amend re-read failed"
    );
    assert_eq!(
        obj["amend_commit_error_hook_rejected"]
            .as_object()
            .unwrap()
            .get("kind")
            .and_then(|v| v.as_str()),
        Some("hook_rejected"),
        "AmendFailureKind must reach the wire as snake_case strings — \
         M2.19d branches on these exact spellings"
    );
}

/// **The backward-compatibility guarantee `/api/push` makes** (M2.20e, #231):
/// a body that says only `{"branch": …}` still parses, and parses to the *safe*
/// end of both new axes.
///
/// This is the one property in this file that protects a live client rather
/// than a future one. The frontend on Tom's iPad sends exactly that body; if
/// `set_upstream` or `force` ever lost its `#[serde(default)]`, every push from
/// it would become a `422` about serde — a whole-app regression whose cause
/// would be invisible from the UI.
///
/// The golden fixture pins the *serialized* shape; this pins the **parse** of a
/// body the fixture does not contain, which is a different question. Both legs
/// are here: absent means safe, and present means honoured — so a
/// `PushRequest` that ignored `force` entirely (defaulting it away on the way
/// in) would fail the second leg rather than quietly downgrading every
/// force-publish to a fast-forward.
#[test]
fn a_push_body_from_before_this_slice_still_parses_and_defaults_to_the_safe_end() {
    for raw in [
        r#"{"branch":"main"}"#,
        r#"{"branch":"main","set_upstream":false}"#,
        r#"{"branch":"main","force":null}"#,
    ] {
        let parsed: PushRequest =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw} must still parse: {e}"));
        assert_eq!(parsed.branch, "main");
        assert!(
            !parsed.set_upstream,
            "an omitted set_upstream must mean *no* upstream write: {raw}"
        );
        assert_eq!(
            parsed.force, None,
            "an omitted force must mean *no* force — a default may only ever \
             point at less capability: {raw}"
        );
    }

    // The paired positive: a body that *does* say force is honoured, so the
    // legs above are proving a default rather than a field being ignored.
    let leased: PushRequest = serde_json::from_str(
        r#"{"branch":"main","set_upstream":true,"force":{"mode":"with_lease",
            "expected_remote_tip":"4444444444444444444444444444444444444444"}}"#,
    )
    .expect("a full push body must parse");
    assert!(leased.set_upstream);
    assert!(matches!(leased.force, Some(ForcePublish::WithLease { .. })));

    // And `deny_unknown_fields` still holds: a stray key is a hard error, not a
    // silently-ignored value that a client could believe the server acted on.
    assert!(
        serde_json::from_str::<PushRequest>(r#"{"branch":"main","remote":"evil"}"#).is_err(),
        "a push body may not smuggle a field this endpoint does not have — \
         above all not a remote"
    );
}
