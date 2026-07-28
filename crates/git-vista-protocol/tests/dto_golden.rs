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
    BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest, DeleteCloneRequest,
    HookPolicy, RebaseStatus, RepoMode, RepositoryDescriptor, RepositoryKind, SelectRequest,
    SessionInfo, SessionRequest,
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
    branch_request: BranchRequest,
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
        branch_request: BranchRequest {
            branch: "main".to_string(),
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
        // `hook_policy: Restricted` here (a LAN-style session) so both
        // HookPolicy variants appear somewhere in this fixture set, the
        // authenticated one carrying the "something to disclose" value.
        session_info_authenticated: SessionInfo {
            authenticated: true,
            csrf: Some("csrf-token-abc123".to_string()),
            via_lan: false,
            hook_policy: HookPolicy::Restricted,
        },
        // `csrf` absent — the unauthenticated shape; `via_lan` also exercises
        // its `#[serde(default)]` additive-field posture at `false`.
        // `hook_policy: Allow` covers the other variant.
        session_info_unauthenticated: SessionInfo {
            authenticated: false,
            csrf: None,
            via_lan: false,
            hook_policy: HookPolicy::Allow,
        },
        rebase_status: RebaseStatus {
            branch: Some("feature/idea".to_string()),
            base: "origin/main".to_string(),
            base_exists: true,
            up_to_date: false,
        },
        // `path` and `remote_web_url` both absent — the default capability
        // report shape, which must never leak the server's filesystem.
        repository_descriptor_minimal: RepositoryDescriptor {
            repository: "11111111-1111-5111-8111-111111111111".to_string(),
            worktree: "22222222-2222-5222-8222-222222222222".to_string(),
            name: "git-vista".to_string(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
        },
        // Both optional fields present — the `GIT_VISTA_EXPOSE_PATHS` shape,
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
}
