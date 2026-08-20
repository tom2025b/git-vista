//! The history graph's paging engine: exact remote reachability for one
//! commit (M1.10, #63); Frame/Page limits and their exact-body etag
//! validators; paged replay contiguity, edge-ownership and stub-ownership
//! across page boundaries; and cursor drift, tamper, scope, and error
//! precedence (Step 8, part B) — including the shallow-boundary deepen/
//! unshallow generation-change cases. All of it drives `frame`/`page_for_target`
//! against real throwaway repositories built to exercise a specific shape of
//! the commit graph.

use super::*;
use axum::routing::get;
use axum::Router;
use git_vista_core::identity::RepositoryId;
use git_vista_core::layout::stream::canonicalize_edges;
use git_vista_protocol::{ApiError, ErrorCode, PROTOCOL_HEADER, PROTOCOL_VERSION};
use tower::ServiceExt;

// --- duplicated cross-suite test helpers, verbatim from read.rs's original inline test module —
// private to their own modules and unreachable from here, same shape
// as the planner/*_suite.rs convention this crate already uses. ---

/// `git <args…>` in `repo`; asserts success. Same shape as the planner
/// suites' fixtures, duplicated because those helpers are private to their
/// own modules and unreachable from here.
fn run(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
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

// ---- exact remote reachability for one commit (M1.10, #63) ---------------

/// A **real** repository of `count` linear commits with
/// `refs/remotes/origin/main` at the chain tip and one further local-only
/// commit on top. Built through a single `git fast-import` so a fixture
/// deeper than the retained 5,000-commit cap costs a second, not minutes.
fn deep_remote_repo(count: usize) -> (tempfile::TempDir, PathBuf) {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);

    let mut stream = String::new();
    for n in 1..=count {
        let message = format!("commit {n}\n");
        stream.push_str("commit refs/heads/main\n");
        stream.push_str(&format!("mark :{n}\n"));
        stream.push_str(&format!(
            "committer t <t@example.invalid> {} +0000\n",
            1_000 + n
        ));
        stream.push_str(&format!("data {}\n{message}", message.len()));
        if n > 1 {
            stream.push_str(&format!("from :{}\n", n - 1));
        }
        stream.push('\n');
    }
    stream.push_str("reset refs/remotes/origin/main\n");
    stream.push_str(&format!("from :{count}\n\n"));
    stream.push_str("done\n");

    let mut child = std::process::Command::new("git")
        .args(["fast-import", "--quiet", "--done"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "git fast-import failed");

    // One local commit past the remote tip: "on a remote" stays a real question.
    run(&repo, &["commit", "-q", "--allow-empty", "-m", "local tip"]);
    (dir, repo)
}

/// The detail panel's remote flag is exact for an arbitrary commit, however
/// deep. A two-row page holds only the local tip and the remote tip; the deep
/// root and an arbitrary unloaded parent are both still reported as pushed,
/// which a `HISTORY_LIMIT`-capped remote walk could not manage.
#[test]
fn commit_detail_marks_unloaded_remote_parent() {
    let (_dir, repo) = deep_remote_repo(5_001);

    let local_tip = out(&repo, &["rev-parse", "HEAD"]);
    let remote_tip = out(&repo, &["rev-parse", "refs/remotes/origin/main"]);
    let arbitrary = out(&repo, &["rev-parse", "refs/remotes/origin/main~3"]);
    let root = out(
        &repo,
        &["rev-list", "--max-parents=0", "refs/remotes/origin/main"],
    );
    let depth: usize = out(&repo, &["rev-list", "--count", "refs/remotes/origin/main"])
        .parse()
        .unwrap();
    assert!(depth > 5_000, "fixture must exceed the cap, got {depth}");

    // The rows a two-row page would own; neither request below is among them.
    let page = [local_tip.as_str(), remote_tip.as_str()];
    assert!(!page.contains(&arbitrary.as_str()));
    assert!(!page.contains(&root.as_str()));

    for id in [&root, &arbitrary] {
        let detail = commit_detail_for_repo(&repo, id).expect("detail read");
        assert_eq!(&detail.id.0, id);
        assert!(detail.on_remote, "an unloaded parent is on the remote");
    }

    let unpushed = commit_detail_for_repo(&repo, &local_tip).expect("detail read");
    assert!(
        !unpushed.on_remote,
        "the local tip was never pushed anywhere"
    );
}

// ---- paged history: Frame, page limits, exact-body validators (M1.10, #63) --
//
// These drive the repo-parameterized `frame_for_target` / `page_for_target`
// seams directly, exactly as the bounded diff/file tests above drive
// `commit_diff_for_repo`. The axum handlers resolve their repository from the
// process-global `CURRENT` selection, shared by every test in this binary, so
// a handler-level test would race with `state::tests` and with its own
// siblings. The only production code skipped is `resolve_history_target`,
// whose selector arms are already pinned by the two tests at the top of this
// module.

/// `git <args…>` in `repo` with `envs` set; asserts success. Fixed
/// author/committer dates are what make two independently built repositories
/// share one history generation.
fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(repo);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let status = cmd.status().unwrap();
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// A repository named `name` under `parent`, on `main`, with `commits`
/// commits whose ids are a pure function of their content — two copies built
/// this way are byte-identical histories and share one generation.
fn deterministic_repo(parent: &Path, name: &str, commits: usize) -> PathBuf {
    assert!(commits >= 1, "a history fixture needs at least one commit");
    let repo = parent.join(name);
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    for i in 0..commits {
        std::fs::write(repo.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        run(&repo, &["add", "-A"]);
        let stamp = format!("{} +0000", 1_700_000_000 + i);
        let message = format!("c{i}");
        run_env(
            &repo,
            &["commit", "-q", "-m", &message],
            &[("GIT_AUTHOR_DATE", &stamp), ("GIT_COMMITTER_DATE", &stamp)],
        );
    }
    repo
}

/// A deterministic cursor codec, so nothing here depends on the per-process
/// random key.
fn history_codec() -> CursorCodec {
    CursorCodec::with_key([0x27; 32])
}

/// The degraded-mode target for `repo`: canonical path, no catalog ids, scope
/// bound through the codec's key — what `resolve_history_target` builds for a
/// selection the catalog never registered.
fn history_target(repo: &Path, codec: &CursorCodec) -> ResolvedHistoryTarget {
    let path = repo.canonicalize().expect("a temp repo path resolves");
    let scope = codec.scope_for_target(None, &path);
    ResolvedHistoryTarget {
        path,
        read_only: false,
        handle: None,
        scope,
    }
}

/// Split a history response into `(status, etag, body)`. Every 200 and 304
/// must carry its quoted representation tag.
async fn parts_of(response: Response) -> (StatusCode, HeaderValue, Vec<u8>) {
    let status = response.status();
    let etag = response
        .headers()
        .get(header::ETAG)
        .expect("every history response carries its representation tag")
        .clone();
    let body = axum::body::to_bytes(response.into_body(), 8 << 20)
        .await
        .expect("a bounded history body")
        .to_vec();
    (status, etag, body)
}

/// The loose-object path for `oid`. Deleting one is how these tests make a
/// commit traversal impossible while leaving refs and HEAD intact.
fn loose_object(repo: &Path, oid: &str) -> PathBuf {
    repo.join(".git")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..])
}

/// The Frame read for `repo` under `headers`. There is deliberately no walk
/// counter to pass: `frame_for_target` takes none because a Frame has
/// nothing to walk — the claim is proved below by breaking the object
/// database and watching a Frame answer anyway.
async fn frame_parts(repo: &Path, headers: &HeaderMap) -> (StatusCode, HeaderValue, Vec<u8>) {
    let codec = history_codec();
    let target = history_target(repo, &codec);
    let response = frame_for_target(&target, headers)
        .await
        .expect("frame read");
    parts_of(response).await
}

/// One page read for `repo` at `cursor`/`limit` under `headers`, plus its
/// walk count. `history_codec` is keyed deterministically, so a cursor minted
/// by one call opens on the next exactly as it would inside one process.
async fn page_parts(
    repo: &Path,
    cursor: Option<&str>,
    limit: usize,
    headers: &HeaderMap,
) -> (StatusCode, HeaderValue, Vec<u8>, usize) {
    let codec = history_codec();
    let target = history_target(repo, &codec);
    let walks = AtomicUsize::new(0);
    let response = page_for_target(&target, cursor, limit, &codec, headers, &walks)
        .await
        .expect("page read");
    let (status, etag, body) = parts_of(response).await;
    (status, etag, body, walks.load(Ordering::Relaxed))
}

/// The page-1 read for `repo` at `limit` under `headers`, plus its walk count.
async fn page_one_parts(
    repo: &Path,
    limit: usize,
    headers: &HeaderMap,
) -> (StatusCode, HeaderValue, Vec<u8>, usize) {
    page_parts(repo, None, limit, headers).await
}

/// Follow the cursor chain from page 1 to exhaustion at `limit`, decoding
/// every page. The last page a history yields is the one that carries no
/// cursor — which may legitimately be an empty page, when the previous walk
/// stopped exactly at the window's end.
async fn all_pages(repo: &Path, limit: usize) -> Vec<Page> {
    let headers = HeaderMap::new();
    let mut pages: Vec<Page> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (status, _, body, walks) = page_parts(repo, cursor.as_deref(), limit, &headers).await;
        assert_eq!(status, StatusCode::OK, "every page in a chain is a 200");
        assert_eq!(walks, 1, "one page, one Topo walk");
        let page: Page = serde_json::from_slice(&body).expect("Page decodes");
        cursor = page.cursor.clone();
        pages.push(page);
        assert!(
            pages.len() <= 64,
            "paging at limit {limit} must terminate on a fixture this small"
        );
        if cursor.is_none() {
            return pages;
        }
    }
}

/// An `If-None-Match:` header map carrying exactly `value`.
fn if_none_match_header(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
    headers
}

/// The history target resolves through the same fail-closed selector arms
/// the other read endpoints use: a malformed id never reaches path
/// resolution, and an id the catalog never registered resolves to nothing
/// rather than falling back to any path. (Not a plan-named test — it exists
/// so the new resolution seam's refusals are pinned, since the nine tests
/// below construct their targets directly.)
#[test]
fn resolve_history_target_fails_closed_on_a_bad_selector() {
    let codec = history_codec();

    // Matched rather than `expect_err`-ed on purpose: the Ok variant holds a
    // canonical filesystem path, and a `Debug` bound would put it in a panic
    // message.
    let Err((status, _)) = resolve_history_target(Some("not-an-id"), &codec) else {
        panic!("a malformed selector must be refused");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let unknown = WorktreeId::from_git_dir("/no/such/repo/.git").to_string();
    let Err((status, _)) = resolve_history_target(Some(&unknown), &codec) else {
        panic!("an unregistered id must be refused");
    };
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The default page is the plan's 250; a client may ask for less, may ask for
/// more and be clamped, and can never ask for a page that would fail to
/// advance the cursor. An unknown query key (the frontend's `?t=`) is
/// accepted, never a 400.
#[test]
fn page_limit_defaults_and_clamps() {
    assert_eq!(DEFAULT_PAGE_LIMIT, 250);
    assert_eq!(MAX_PAGE_LIMIT, 1_000);

    assert_eq!(
        page_limit(None),
        DEFAULT_PAGE_LIMIT,
        "an absent ?limit= is the default page"
    );
    assert_eq!(
        page_limit(Some(0)),
        1,
        "a zero-row page would never advance the cursor"
    );
    assert_eq!(page_limit(Some(1)), 1);
    assert_eq!(page_limit(Some(7)), 7);
    assert_eq!(page_limit(Some(DEFAULT_PAGE_LIMIT)), DEFAULT_PAGE_LIMIT);
    assert_eq!(page_limit(Some(MAX_PAGE_LIMIT)), MAX_PAGE_LIMIT);
    assert_eq!(
        page_limit(Some(MAX_PAGE_LIMIT + 1)),
        MAX_PAGE_LIMIT,
        "an oversized ?limit= clamps rather than failing the read"
    );
    assert_eq!(page_limit(Some(usize::MAX)), MAX_PAGE_LIMIT);

    // `PageQuery` must not deny unknown fields: the frontend appends its own
    // cache-buster and must not be answered with a 400.
    let parsed: PageQuery =
        serde_json::from_str(r#"{"repo":null,"cursor":"opaque","limit":7,"t":"1737000000000"}"#)
            .expect("PageQuery tolerates the frontend's ?t= cache-buster");
    assert!(parsed.repo.is_none());
    assert_eq!(parsed.cursor.as_deref(), Some("opaque"));
    assert_eq!(page_limit(parsed.limit), 7);
}

/// One snapshot, one generation — but two different resources, so two
/// different, type-prefixed, exact-body validators. The Frame is `O(refs)`:
/// it must not touch the walk counter at all.
#[tokio::test]
async fn frame_and_page_one_share_generation_but_have_distinct_etags() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 4);
    let headers = HeaderMap::new();

    let (frame_status, frame_tag, frame_body) = frame_parts(&repo, &headers).await;
    assert_eq!(frame_status, StatusCode::OK);

    let (page_status, page_tag, page_body, page_walks) =
        page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &headers).await;
    assert_eq!(page_status, StatusCode::OK);
    assert_eq!(page_walks, 1, "page 1 walks exactly once");

    let frame: Frame = serde_json::from_slice(&frame_body).expect("Frame decodes");
    let page: Page = serde_json::from_slice(&page_body).expect("Page decodes");
    assert_eq!(
        frame.generation, page.generation,
        "one combined snapshot, one generation"
    );
    assert_ne!(frame_tag, page_tag);
    assert!(
        frame_tag.to_str().unwrap().starts_with("\"gv4-frame:"),
        "{frame_tag:?}"
    );
    assert!(
        page_tag.to_str().unwrap().starts_with("\"gv4-page:"),
        "{page_tag:?}"
    );

    // The tags are hashes of the exact bytes that were sent, not of a
    // re-serialization and never of the generation.
    assert_eq!(
        representation_etag(RepresentationKind::Frame, &frame_body),
        frame_tag
    );
    assert_eq!(
        representation_etag(RepresentationKind::Page, &page_body),
        page_tag
    );

    // The Frame answers branch slots from refs alone and carries no stubs
    // (the envelope has no such field); the Page carries the rows.
    assert_eq!(
        frame.branch_colors,
        vec![("main".to_string(), 0)],
        "the trunk's stable slot comes from the refs, with no walk"
    );
    assert!(
        !frame_body.windows(7).any(|w| w == b"\"stubs\""),
        "a Frame never carries stubs"
    );
    assert_eq!(page.rows.len(), 4);
    assert_eq!(page.rows[0].row, 0);

    // The `O(refs)` claim, with teeth: remove one interior commit object, so
    // every commit traversal in this repository now fails, and the Frame
    // still answers the identical body. Nothing below the ref tips feeds it,
    // which is why it needs — and is given — no walk counter at all.
    let interior = out(&repo, &["rev-parse", "HEAD~2"]);
    std::fs::remove_file(loose_object(&repo, &interior)).expect("a loose interior commit");
    let walks = AtomicUsize::new(0);
    let codec = history_codec();
    let target = history_target(&repo, &codec);
    page_for_target(&target, None, DEFAULT_PAGE_LIMIT, &codec, &headers, &walks)
        .await
        .expect_err("a Page cannot be built without the commit objects");
    assert_eq!(
        walks.load(Ordering::Relaxed),
        1,
        "the Page read counted its one walk before failing in it"
    );

    let (status, revalidated, body) = frame_parts(&repo, &headers).await;
    assert_eq!(status, StatusCode::OK, "a Frame needs no commit object");
    assert_eq!(
        revalidated, frame_tag,
        "the Frame is a pure function of refs, HEAD and the shallow set"
    );
    assert_eq!(body, frame_body);
}

/// A change the generation deliberately excludes — repository config, not a
/// ref, HEAD, or a shallow boundary — still changes the Frame's body, so it
/// must change the Frame's validator. Generation and ETag are separate
/// things.
#[tokio::test]
async fn frame_metadata_change_changes_etag_without_generation_change() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 2);
    let headers = HeaderMap::new();

    let (_, before_tag, before_body) = frame_parts(&repo, &headers).await;
    let before: Frame = serde_json::from_slice(&before_body).unwrap();
    assert!(
        before.remote_web_url.is_none(),
        "the fixture starts with no remote"
    );

    run(
        &repo,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );

    let (_, after_tag, after_body) = frame_parts(&repo, &headers).await;
    let after: Frame = serde_json::from_slice(&after_body).unwrap();
    assert!(
        after.remote_web_url.is_some(),
        "adding a remote gives the Frame a forge base"
    );
    assert_eq!(
        before.generation, after.generation,
        "config moves no ref, no HEAD half and no shallow boundary"
    );
    assert_ne!(
        before_tag, after_tag,
        "the validator is derived from the sent body, so metadata moves it"
    );
}

/// Two selections over byte-identical histories share a generation but are
/// different resources: the resolved-target metadata rides in the Frame body,
/// so switching the default selection must move the Frame's validator — and
/// the two targets must bind different cursor scopes.
#[tokio::test]
async fn default_selection_switch_same_history_changes_frame_etag() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = deterministic_repo(dir.path(), "alpha", 3);
    let beta = deterministic_repo(dir.path(), "beta", 3);
    let headers = HeaderMap::new();

    let (_, alpha_tag, alpha_body) = frame_parts(&alpha, &headers).await;
    let (_, beta_tag, beta_body) = frame_parts(&beta, &headers).await;
    let a: Frame = serde_json::from_slice(&alpha_body).unwrap();
    let b: Frame = serde_json::from_slice(&beta_body).unwrap();

    assert_eq!(
        a.generation, b.generation,
        "identical committed topology is one history generation"
    );
    assert!(a
        .repo_label
        .as_deref()
        .is_some_and(|label| label.ends_with("alpha")));
    assert!(b
        .repo_label
        .as_deref()
        .is_some_and(|label| label.ends_with("beta")));
    assert_ne!(
        alpha_tag, beta_tag,
        "one generation, two selections, two validators"
    );

    let codec = history_codec();
    assert_ne!(
        history_target(&alpha, &codec).scope,
        history_target(&beta, &codec).scope,
        "a cursor minted for one selection must not open on the other"
    );
}

/// Page 1 at two different limits is two different representations of one
/// generation, each with its own exact-body validator and its own cursor.
#[tokio::test]
async fn page_one_limits_one_and_seven_have_distinct_etags() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 8);
    let headers = HeaderMap::new();

    let (_, tag_one, body_one, _) = page_one_parts(&repo, 1, &headers).await;
    let (_, tag_seven, body_seven, _) = page_one_parts(&repo, 7, &headers).await;
    let one: Page = serde_json::from_slice(&body_one).unwrap();
    let seven: Page = serde_json::from_slice(&body_seven).unwrap();

    assert_eq!(one.rows.len(), 1);
    assert_eq!(seven.rows.len(), 7);
    assert_eq!(one.rows[0].row, 0, "both pages start at absolute row 0");
    assert_eq!(seven.rows[0].row, 0);
    assert_eq!(one.rows[0].commit.id, seven.rows[0].commit.id);
    assert_eq!(
        one.generation, seven.generation,
        "the page size is not part of the history generation"
    );
    assert_ne!(tag_one, tag_seven);
    assert!(one.cursor.is_some(), "seven more rows remain after limit 1");
    assert!(seven.cursor.is_some(), "one more row remains after limit 7");
    assert_ne!(
        one.cursor, seven.cursor,
        "the two cursors name different next rows"
    );
}

/// The two tag namespaces are sealed: a Frame validator can never satisfy a
/// Page's precondition, nor a Page validator a Frame's. Both requests are
/// answered 200 with their own tag and a real body.
#[tokio::test]
async fn frame_etag_cannot_304_page_one() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);
    let none = HeaderMap::new();

    let (_, frame_tag, _) = frame_parts(&repo, &none).await;
    let (_, page_tag, _, _) = page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &none).await;
    assert_ne!(frame_tag, page_tag);

    let presented = if_none_match_header(frame_tag.to_str().unwrap());
    let (status, tag, body, _) = page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &presented).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a Frame tag must not 304 a Page: they are different resources"
    );
    assert_eq!(tag, page_tag);
    assert!(!body.is_empty());

    let presented = if_none_match_header(page_tag.to_str().unwrap());
    let (status, tag, body) = frame_parts(&repo, &presented).await;
    assert_eq!(status, StatusCode::OK, "nor a Page tag a Frame");
    assert_eq!(tag, frame_tag);
    assert!(!body.is_empty());
}

/// A Frame whose own current validator is presented is answered with an
/// empty 304 that still carries that validator.
#[tokio::test]
async fn frame_matching_validator_returns_304_empty() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);

    let (status, tag, body) = frame_parts(&repo, &HeaderMap::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());

    let presented = if_none_match_header(tag.to_str().unwrap());
    let (status, revalidated, body) = frame_parts(&repo, &presented).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(revalidated, tag, "a 304 keeps the validator it matched");
    assert!(body.is_empty(), "a 304 carries no body");
}

/// Page 1 evaluates the precondition against its own current tag, and a
/// match is an empty 304 carrying that tag.
#[tokio::test]
async fn page_one_matching_validator_returns_304_empty() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);

    let (status, tag, body, _) = page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &HeaderMap::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());

    let presented = if_none_match_header(tag.to_str().unwrap());
    let (status, revalidated, body, _) =
        page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &presented).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(revalidated, tag);
    assert!(body.is_empty(), "a 304 carries no body");
}

/// RFC 9110 weak comparison, on both representations: a `W/`-prefixed tag, a
/// matching member of a comma-separated list, and `*` each revalidate to an
/// empty 304 carrying the representation's own tag.
#[tokio::test]
async fn frame_and_page_one_weak_list_and_star_validators_return_304_empty() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);

    let (_, frame_tag, _) = frame_parts(&repo, &HeaderMap::new()).await;
    let (_, page_tag, _, _) = page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &HeaderMap::new()).await;

    for (what, tag) in [("frame", &frame_tag), ("page", &page_tag)] {
        let quoted = tag.to_str().unwrap();
        let validators = [
                format!("W/{quoted}"),
                format!("\"gv4-page:0000000000000000000000000000000000000000000000000000000000000000\", {quoted}"),
                "*".to_string(),
            ];
        for validator in validators {
            let presented = if_none_match_header(&validator);
            let (status, revalidated, body) = if what == "frame" {
                frame_parts(&repo, &presented).await
            } else {
                let (status, tag, body, _) =
                    page_one_parts(&repo, DEFAULT_PAGE_LIMIT, &presented).await;
                (status, tag, body)
            };
            assert_eq!(
                status,
                StatusCode::NOT_MODIFIED,
                "{what} must revalidate on {validator}"
            );
            assert_eq!(&revalidated, tag, "{what}: 304 keeps its own validator");
            assert!(body.is_empty(), "{what}: a 304 carries no body");
        }
    }
}

// ---- paged replay: contiguity, edge ownership, stub ownership -------------

/// One commit in `repo` at a fixed author/committer timestamp, so the Topo
/// `DateOrder` these fixtures depend on is not a function of wall-clock time.
fn commit_at(repo: &Path, file: &str, message: &str, epoch: i64) {
    std::fs::write(repo.join(file), format!("{message}\n")).unwrap();
    run(repo, &["add", "-A"]);
    let stamp = format!("{epoch} +0000");
    run_env(
        repo,
        &["commit", "-q", "-m", message],
        &[("GIT_AUTHOR_DATE", &stamp), ("GIT_COMMITTER_DATE", &stamp)],
    );
}

/// The plan's adversarial edge fixture, and the reason it is adversarial:
///
/// ```text
///   row 0  M   merge, parents [A, B]
///   row 1  A   parent [R]
///   row 2  R   recorded shallow boundary — its parent Z is cut
///   row 3  B   unrelated root, older than R
/// ```
///
/// Topo `DateOrder` emits `M(0) -> [A(1), B(3)]` and `A(1) -> R(2)`, so the
/// `M -> B` edge resolves three rows below its own row: any page containing
/// row 3 owns an edge whose `from_row` is 0. That is the shape a page-local
/// row index cannot express, which is what `ResolvedEdge.parent_ordinal` and
/// the checkpointed `PendingEdge` list exist for.
///
/// Returns `(repo, z_oid)` — `Z` must never appear in any page.
fn adversarial_edge_repo(parent: &Path) -> (PathBuf, String) {
    let repo = parent.join("edges");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);

    commit_at(&repo, "z.txt", "z", 1_700_001_000);
    let z = out(&repo, &["rev-parse", "HEAD"]);
    commit_at(&repo, "r.txt", "r", 1_700_003_000);
    let r = out(&repo, &["rev-parse", "HEAD"]);
    commit_at(&repo, "a.txt", "a", 1_700_004_000);

    // B: an unrelated root, deliberately *older* than R so `DateOrder` puts
    // it last even though it is the merge's second parent.
    run(&repo, &["checkout", "-q", "--orphan", "bside"]);
    run(&repo, &["rm", "-r", "-f", "-q", "--cached", "."]);
    for stale in ["z.txt", "r.txt", "a.txt"] {
        std::fs::remove_file(repo.join(stale)).unwrap();
    }
    commit_at(&repo, "b.txt", "b", 1_700_002_000);

    run(&repo, &["checkout", "-q", "main"]);
    let stamp = format!("{} +0000", 1_700_005_000_i64);
    run_env(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "--allow-unrelated-histories",
            "-m",
            "m",
            "bside",
        ],
        &[("GIT_AUTHOR_DATE", &stamp), ("GIT_COMMITTER_DATE", &stamp)],
    );

    // Record R as a shallow boundary *after* every commit is written: from
    // here on Z is unreachable to the traversal, cut rather than missing.
    std::fs::write(repo.join(".git").join("shallow"), format!("{r}\n")).unwrap();
    (repo, z)
}

/// A linear history carrying two stub anchors: one local branch demoted at
/// row 1, and a three-branch cascade demoted at row 3. Local `main` outranks
/// every one of them, so each is a [`FrameStub`] rather than a badge.
fn stub_cascade_repo(parent: &Path) -> PathBuf {
    let repo = deterministic_repo(parent, "stubs", 6);
    run(&repo, &["branch", "zeta", "HEAD~1"]);
    run(&repo, &["branch", "alpha", "HEAD~3"]);
    run(&repo, &["branch", "beta", "HEAD~3"]);
    run(&repo, &["branch", "gamma", "HEAD~3"]);
    repo
}

/// Paging is a partition of the same replay, not a different one: at any page
/// size, the concatenated pages are the uninterrupted walk's rows, in order,
/// with absolute row numbers that never repeat and never skip.
#[tokio::test]
async fn pages_are_contiguous_at_limits_one_and_seven() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 8);

    let (_, _, oracle_body, _) = page_one_parts(&repo, MAX_PAGE_LIMIT, &HeaderMap::new()).await;
    let oracle: Page = serde_json::from_slice(&oracle_body).unwrap();
    assert_eq!(oracle.rows.len(), 8, "the uninterrupted page holds it all");
    assert!(
        oracle.cursor.is_none(),
        "a walk that ended before the window filled opens no next page"
    );

    for limit in [1_usize, 7] {
        let pages = all_pages(&repo, limit).await;

        let mut expected_start = 0_usize;
        let mut union: Vec<GraphRow> = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            assert!(
                page.rows.len() <= limit,
                "limit {limit}: page {index} overran the window"
            );
            for (offset, row) in page.rows.iter().enumerate() {
                assert_eq!(
                    row.row,
                    expected_start + offset,
                    "limit {limit}: page {index} row {offset} is not contiguous"
                );
            }
            expected_start += page.rows.len();
            union.extend(page.rows.iter().cloned());
        }

        assert_eq!(
            union.iter().map(|r| r.row).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>(),
            "limit {limit}: absolute rows are 0..8 exactly once, in order"
        );
        assert_eq!(
            union
                .iter()
                .map(|r| r.commit.id.clone())
                .collect::<Vec<_>>(),
            oracle
                .rows
                .iter()
                .map(|r| r.commit.id.clone())
                .collect::<Vec<_>>(),
            "limit {limit}: the pages replay the uninterrupted walk"
        );
        assert_eq!(
            union.iter().map(|r| r.lane).collect::<Vec<_>>(),
            oracle.rows.iter().map(|r| r.lane).collect::<Vec<_>>(),
            "limit {limit}: lanes survive the checkpoint/resume boundary"
        );
        assert_eq!(
            union.iter().map(|r| r.color).collect::<Vec<_>>(),
            oracle.rows.iter().map(|r| r.color).collect::<Vec<_>>(),
            "limit {limit}: the prefix replay rebuilt the same claims"
        );
        assert_eq!(
            union
                .iter()
                .map(|r| r.refs.iter().map(|x| x.name.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            oracle
                .rows
                .iter()
                .map(|r| r.refs.iter().map(|x| x.name.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "limit {limit}: badges land on their own row, once"
        );

        let generations: HashSet<_> = pages.iter().map(|p| p.generation.clone()).collect();
        assert_eq!(
            generations.len(),
            1,
            "limit {limit}: one stable history, one generation"
        );
        assert!(
            pages.last().unwrap().cursor.is_none(),
            "limit {limit}: the chain ends without a cursor"
        );
    }
}

/// Edge ownership at every page boundary, over the plan's adversarial graph.
///
/// Each edge is delivered exactly once, on the page that owns its *parent*
/// row, even when the child row is pages away. Raw concatenation is therefore
/// deliberately **not** canonical order — only a canonicalized clone of the
/// completed union is, and it must equal the uninterrupted walk's own edges.
#[tokio::test]
async fn paged_edge_union_canonicalizes_to_uninterrupted_oracle_at_every_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, z) = adversarial_edge_repo(dir.path());

    let (_, _, oracle_body, _) = page_one_parts(&repo, MAX_PAGE_LIMIT, &HeaderMap::new()).await;
    let oracle: Page = serde_json::from_slice(&oracle_body).unwrap();

    // The fixture really is `M(0) -> [A(1), B(3)]`, `A(1) -> R(2)`, cut at R.
    let summaries: Vec<&str> = oracle
        .rows
        .iter()
        .map(|r| r.commit.summary.as_str())
        .collect();
    assert_eq!(
        summaries,
        vec!["m", "a", "r", "b"],
        "the adversarial Topo DateOrder the plan specifies"
    );
    assert!(
        oracle.rows[2].commit.parents.is_empty(),
        "a recorded shallow boundary reaches the layout as a root"
    );
    assert!(
        !oracle.rows.iter().any(|r| r.commit.id.0 == z),
        "the commit below the boundary is cut, not paged"
    );
    assert_eq!(
        oracle.edges,
        vec![
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 1,
                to_lane: 0
            },
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 3,
                to_lane: 1
            },
            Edge {
                from_row: 1,
                from_lane: 0,
                to_row: 2,
                to_lane: 0
            },
        ],
        "the uninterrupted oracle is canonical (from_row, parent ordinal, …)"
    );

    let mut saw_edge_from_an_earlier_page = false;
    let mut saw_noncanonical_raw_union = false;

    for limit in 1..=oracle.rows.len() {
        let pages = all_pages(&repo, limit).await;

        let mut start = 0_usize;
        let mut union_rows: Vec<GraphRow> = Vec::new();
        let mut raw_union: Vec<Edge> = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            let end = start + page.rows.len();
            for edge in &page.edges {
                assert!(
                    (start..end).contains(&edge.to_row),
                    "limit {limit}: page {index} [{start},{end}) must own only \
                         edges whose destination row it holds, got {edge:?}"
                );
                if edge.from_row < start {
                    saw_edge_from_an_earlier_page = true;
                }
            }
            start = end;
            union_rows.extend(page.rows.iter().cloned());
            raw_union.extend(page.edges.iter().cloned());
        }

        assert_eq!(
            union_rows.len(),
            oracle.rows.len(),
            "limit {limit}: the union is the whole history"
        );
        assert_eq!(
            raw_union.len(),
            oracle.edges.len(),
            "limit {limit}: every edge exactly once — no duplicate, no drop"
        );
        let distinct: HashSet<_> = raw_union
            .iter()
            .map(|e| (e.from_row, e.from_lane, e.to_row, e.to_lane))
            .collect();
        assert_eq!(
            distinct.len(),
            oracle.edges.len(),
            "limit {limit}: the union holds no repeated edge"
        );

        if raw_union != oracle.edges {
            saw_noncanonical_raw_union = true;
        }

        // Only *this* — a canonicalized clone of the completed union, indexed
        // against absolute rows starting at zero — is required to equal the
        // oracle. `canonicalize_edges` is never called on page-local rows.
        let mut canonical = raw_union.clone();
        canonicalize_edges(&union_rows, &mut canonical);
        assert_eq!(
            canonical, oracle.edges,
            "limit {limit}: the completed union canonicalizes to the \
                 uninterrupted new-pipeline oracle"
        );
    }

    assert!(
        saw_edge_from_an_earlier_page,
        "the fixture must exercise a page owning an edge with from_row < n"
    );
    assert!(
        saw_noncanonical_raw_union,
        "raw concatenated page edge order is deliberately not required to be \
             canonical order; a fixture where it always happens to be proves nothing"
    );
}

/// Stub ownership at every page boundary: a stub rides the page that owns its
/// anchor row and no other, a suppressed prefix emits none, and the cumulative
/// column numbering survives the prefix replay.
///
/// Per accepted decision D18, paged `lane_offset` is **row**-order numbering:
/// the streaming classifier emits each stub on its anchor's page and cannot
/// see later rows, so it cannot reproduce the whole-graph pass's
/// priority-sorted seed order. The oracle here is the uninterrupted *new*
/// pipeline, which is exactly what the frontend will render.
#[tokio::test]
async fn page_stubs_emit_once_on_anchor_page_with_stable_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let repo = stub_cascade_repo(dir.path());

    let (_, _, oracle_body, _) = page_one_parts(&repo, MAX_PAGE_LIMIT, &HeaderMap::new()).await;
    let oracle: Page = serde_json::from_slice(&oracle_body).unwrap();
    assert_eq!(oracle.rows.len(), 6);

    let anchor_one = oracle.rows[1].commit.id.clone();
    let anchor_three = oracle.rows[3].commit.id.clone();
    assert_eq!(
        oracle
            .stubs
            .iter()
            .map(|s| (
                s.name.as_str(),
                s.anchor_commit.clone(),
                s.lane_offset,
                s.depth
            ))
            .collect::<Vec<_>>(),
        vec![
            ("zeta", anchor_one.clone(), 0, 0),
            ("alpha", anchor_three.clone(), 1, 0),
            ("beta", anchor_three.clone(), 2, 1),
            ("gamma", anchor_three.clone(), 3, 2),
        ],
        "row-order cumulative offsets, name-sorted within one anchor (D18)"
    );
    for name in ["zeta", "alpha", "beta", "gamma"] {
        assert!(
            !oracle
                .rows
                .iter()
                .any(|r| r.refs.iter().any(|x| x.name == name)),
            "{name} is drawn as a stub line, never as a second badge"
        );
    }

    for limit in 1..=oracle.rows.len() {
        let pages = all_pages(&repo, limit).await;

        let mut start = 0_usize;
        let mut union: Vec<FrameStub> = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            let end = start + page.rows.len();
            let owned: HashSet<Oid> = page.rows.iter().map(|r| r.commit.id.clone()).collect();
            for stub in &page.stubs {
                assert!(
                    owned.contains(&stub.anchor_commit),
                    "limit {limit}: page {index} [{start},{end}) carries a stub \
                         whose anchor row it does not own: {stub:?}"
                );
            }
            start = end;
            union.extend(page.stubs.iter().cloned());
        }

        assert_eq!(
            union, oracle.stubs,
            "limit {limit}: each stub once, on its anchor page, with the \
                 cumulative offsets the uninterrupted classification hands out"
        );
    }
}

// ---- cursor drift, tamper, scope, and error precedence (Step 8, part B) ---

/// A `count`-commit linear history built via `git fast-import`, carrying no
/// `M` (modify) commands — every commit shares one empty tree, so the batch
/// is small enough that fast-import writes it as individually addressable
/// **loose** objects rather than one pack. The two walk-error fixtures below
/// need that: they force a traversal failure by deleting one specific
/// commit's object file, and a duplicate copy sitting in a pack would defeat
/// the deletion.
fn deep_linear_repo(parent: &Path, count: usize) -> (PathBuf, String, String) {
    use std::io::Write;
    assert!(count >= 2, "a walk-error fixture needs a root and a child");

    let repo = parent.join(format!("deep-{count}"));
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    // `git fast-import` only unpacks a batch this size automatically up to
    // `transfer.unpackLimit` (default 100); above that it always writes one
    // pack regardless of how few "M" commands the stream carries. Raise the
    // limit so a fixture of any size the tests below choose stays loose.
    run(&repo, &["config", "transfer.unpackLimit", "1000000"]);

    let mut stream = String::new();
    for n in 1..=count {
        let message = format!("commit {n}\n");
        stream.push_str("commit refs/heads/main\n");
        stream.push_str(&format!("mark :{n}\n"));
        stream.push_str(&format!(
            "committer t <t@example.invalid> {} +0000\n",
            1_000 + n
        ));
        stream.push_str(&format!("data {}\n{message}", message.len()));
        if n > 1 {
            stream.push_str(&format!("from :{}\n", n - 1));
        }
        stream.push('\n');
    }
    stream.push_str("done\n");

    let mut child = std::process::Command::new("git")
        .args(["fast-import", "--quiet", "--done"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "git fast-import failed");
    assert!(
        std::fs::read_dir(repo.join(".git/objects/pack"))
            .unwrap()
            .next()
            .is_none(),
        "no \"M\" commands means fast-import must never pack this fixture"
    );

    let tip = out(&repo, &["rev-parse", "refs/heads/main"]);
    let root = out(&repo, &["rev-list", "--max-parents=0", "refs/heads/main"]);
    (repo, tip, root)
}

/// Move `repo`'s `ref_name` to `new_oid` from a background thread, after a
/// short fixed delay. The delay is comfortably longer than the calling
/// test's already-in-flight snapshot read (a handful of small file reads,
/// microseconds) and comfortably shorter than the multi-hundred/thousand
/// commit walk these tests give it to race against, so the mutation lands
/// strictly between the two.
fn race_ref_move(
    repo: &Path,
    ref_name: &'static str,
    new_oid: &str,
) -> std::thread::JoinHandle<()> {
    let repo = repo.to_path_buf();
    let new_oid = new_oid.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        run(&repo, &["update-ref", ref_name, &new_oid]);
    })
}

/// A cursor page never revalidates against `If-None-Match`, even when the
/// client presents that exact page's own current tag: only a Frame and page
/// 1 are stable, addressable representations.
#[tokio::test]
async fn cursor_page_ignores_if_none_match() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);

    let (status_one, _, body_one, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
    assert_eq!(status_one, StatusCode::OK);
    let page_one: Page = serde_json::from_slice(&body_one).unwrap();
    let cursor = page_one.cursor.clone().expect("more rows remain");

    let (status_two, tag_two, body_two, walks_two) =
        page_parts(&repo, Some(&cursor), 1, &HeaderMap::new()).await;
    assert_eq!(status_two, StatusCode::OK);
    assert_eq!(walks_two, 1);

    // Presenting that exact, freshly computed tag back must still 200.
    let presented = if_none_match_header(tag_two.to_str().unwrap());
    let (status_three, tag_three, body_three, walks_three) =
        page_parts(&repo, Some(&cursor), 1, &presented).await;
    assert_eq!(
        status_three,
        StatusCode::OK,
        "a cursor page always 200s despite a matching If-None-Match"
    );
    assert_eq!(tag_three, tag_two);
    assert_eq!(body_three, body_two);
    assert_eq!(walks_three, 1);
}

/// A ref moving between the page that mints a cursor and the page that
/// consumes it is refused as a 409 — caught by the cursor's own generation
/// comparison, strictly before any traversal.
#[tokio::test]
async fn ref_move_between_pages_returns_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 4);

    let (status_one, _, body_one, walks_one) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
    assert_eq!(status_one, StatusCode::OK);
    assert_eq!(walks_one, 1);
    let page_one: Page = serde_json::from_slice(&body_one).unwrap();
    let cursor = page_one.cursor.clone().expect("more rows remain");

    // The branch this cursor was minted against moves: a new commit lands.
    commit_at(&repo, "extra.txt", "extra", 1_700_009_000);

    let codec = history_codec();
    let target = history_target(&repo, &codec);
    let walks = AtomicUsize::new(0);
    let error = page_for_target(&target, Some(&cursor), 1, &codec, &HeaderMap::new(), &walks)
        .await
        .expect_err("a cursor pinned to a generation the repository has left must be refused");
    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(error.1, "history moved");
    assert_eq!(
        walks.load(Ordering::Relaxed),
        0,
        "generation drift is caught before any walk"
    );
}

/// The generation can move *during* a page that never presented a cursor at
/// all: the walk itself completes (against the seeds the initial snapshot
/// captured), but the repository has moved by the time the success-path
/// combined re-read runs, and that re-read still refuses the page. Driven
/// through the real `api_contract` middleware, so the wire JSON — not just
/// the handler's own tuple — is proved to carry `error.code == "conflict"`.
#[tokio::test]
async fn generation_move_during_page_returns_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, tip, _root) = deep_linear_repo(dir.path(), 1_500);

    // A new commit, built but referenced by nothing yet: the racer below
    // moves `main` onto it only after this request is already under way.
    let tree = out(&repo, &["rev-parse", &format!("{tip}^{{tree}}")]);
    let extra = out(&repo, &["commit-tree", &tree, "-p", &tip, "-m", "extra"]);
    let racer = race_ref_move(&repo, "refs/heads/main", &extra);

    let walks = Arc::new(AtomicUsize::new(0));
    let repo_for_route = repo.clone();
    let walks_for_route = Arc::clone(&walks);
    let app = Router::new()
        .route(
            "/api/commits",
            get(move || {
                let repo_for_route = repo_for_route.clone();
                let walks_for_route = Arc::clone(&walks_for_route);
                async move {
                    let codec = history_codec();
                    let target = history_target(&repo_for_route, &codec);
                    page_for_target(
                        &target,
                        None,
                        1_500,
                        &codec,
                        &HeaderMap::new(),
                        walks_for_route.as_ref(),
                    )
                    .await
                }
            }),
        )
        .layer(axum::middleware::from_fn(crate::middleware::api_contract));

    let req = axum::http::Request::get("/api/commits")
        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    racer.join().unwrap();

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "the walk ran against the old seeds; the repository moved before the re-read"
    );
    assert_eq!(
        walks.load(Ordering::Relaxed),
        1,
        "the walk itself ran exactly once, unlike a rejected cursor"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let err: ApiError = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        err.error.code,
        ErrorCode::Conflict,
        "the real middleware envelope, not just the handler's own tuple"
    );
    assert_eq!(err.error.message, "history moved");
}

/// A cursor whose signature no longer verifies — one flipped character — is
/// the same generic 400 as every other codec failure, and costs nothing but
/// the failed HMAC check.
#[tokio::test]
async fn tampered_cursor_is_bad_request_before_walk() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);

    let (_, _, body, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
    let page: Page = serde_json::from_slice(&body).unwrap();
    let cursor = page.cursor.clone().expect("more rows remain");

    let mut chars: Vec<char> = cursor.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    let tampered: String = chars.into_iter().collect();
    assert_ne!(tampered, cursor);

    let codec = history_codec();
    let target = history_target(&repo, &codec);
    let walks = AtomicUsize::new(0);
    let error = page_for_target(
        &target,
        Some(&tampered),
        1,
        &codec,
        &HeaderMap::new(),
        &walks,
    )
    .await
    .expect_err("a tampered cursor must be refused");
    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(error.1, "invalid history cursor");
    assert_eq!(walks.load(Ordering::Relaxed), 0);
}

/// A cursor minted for one repository must not open on a different one, even
/// when the two happen to share a generation (byte-identical committed
/// topology): the codec's own signature still verifies, so only the scope
/// comparison — the same generic 400 — catches it.
#[tokio::test]
async fn same_generation_other_repository_cursor_is_rejected_before_walk() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = deterministic_repo(dir.path(), "alpha", 3);
    let beta = deterministic_repo(dir.path(), "beta", 3);

    let (_, _, alpha_body, _) = page_one_parts(&alpha, 1, &HeaderMap::new()).await;
    let alpha_page: Page = serde_json::from_slice(&alpha_body).unwrap();
    let cursor = alpha_page.cursor.clone().expect("more rows remain");

    let (_, _, beta_body, _) = page_one_parts(&beta, 1, &HeaderMap::new()).await;
    let beta_page: Page = serde_json::from_slice(&beta_body).unwrap();
    assert_eq!(
        alpha_page.generation, beta_page.generation,
        "identical committed topology shares one generation"
    );

    let codec = history_codec();
    let target = history_target(&beta, &codec);
    let walks = AtomicUsize::new(0);
    let error = page_for_target(&target, Some(&cursor), 1, &codec, &HeaderMap::new(), &walks)
        .await
        .expect_err(
            "a cursor minted for one repository must not open on another, \
                 even at the same generation",
        );
    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(error.1, "invalid history cursor");
    assert_eq!(walks.load(Ordering::Relaxed), 0);
}

/// A registered target's scope binds both halves of its `RepositoryHandle`:
/// a cursor minted for one worktree of a repository must not open on a
/// sibling worktree of that same repository, even though both share the
/// same generation (they are the same committed history).
#[tokio::test]
async fn same_repository_sibling_worktree_cursor_is_rejected_before_walk() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 3);
    let path = repo.canonicalize().expect("a temp repo path resolves");
    let common = path.join(".git");
    let common_str = common.to_str().expect("a temp path is valid utf-8");

    let repository = RepositoryId::from_common_dir(common_str);
    let worktree_main = WorktreeId::from_git_dir(common_str);
    let worktree_other = WorktreeId::from_git_dir(&format!("{common_str}/worktrees/other"));
    let handle_main = RepositoryHandle::new(repository, worktree_main);
    let handle_other = RepositoryHandle::new(repository, worktree_other);
    assert_ne!(handle_main.worktree, handle_other.worktree);

    let codec = history_codec();
    let scope_main = codec.scope_for_target(Some(&handle_main), &path);
    let target_other = ResolvedHistoryTarget {
        path: path.clone(),
        read_only: false,
        handle: Some(handle_other),
        scope: codec.scope_for_target(Some(&handle_other), &path),
    };
    assert_ne!(
        scope_main, target_other.scope,
        "sibling worktrees of one repository bind different scopes"
    );

    let snapshot = read_history_snapshot(&path).await.expect("snapshot read");
    let cursor = codec
        .encode(
            scope_main,
            &snapshot.generation,
            &HistoryCursor { next_row: 1 },
        )
        .expect("signing a cursor for the main worktree's scope");

    let walks = AtomicUsize::new(0);
    let error = page_for_target(
        &target_other,
        Some(&cursor),
        1,
        &codec,
        &HeaderMap::new(),
        &walks,
    )
    .await
    .expect_err("a cursor scoped to a sibling worktree must not open on this one");
    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(error.1, "invalid history cursor");
    assert_eq!(walks.load(Ordering::Relaxed), 0);
}

/// A shallow boundary set changing — deepening, then unshallowing — moves
/// the generation without moving a single ref or either HEAD half. A cursor
/// pinned before either move is a stale, rejected-before-walk 409 in both
/// directions, and every fresh Frame/Page tag moves with it.
#[tokio::test]
async fn deepen_without_ref_move_rejects_stale_cursor_with_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 5);
    let path = repo.canonicalize().unwrap();
    let head_path = path.join(".git").join("HEAD");
    let ref_path = path.join(".git").join("refs").join("heads").join("main");
    let head_before = std::fs::read(&head_path).unwrap();
    let ref_before = std::fs::read(&ref_path).unwrap();

    let (_, tag_frame_before, _) = frame_parts(&repo, &HeaderMap::new()).await;
    let (_, tag_page_before, body_before, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
    let page_before: Page = serde_json::from_slice(&body_before).unwrap();
    let cursor_before = page_before.cursor.clone().expect("more rows remain");
    let generation_before = page_before.generation.clone();

    let codec = history_codec();
    let target = history_target(&repo, &codec);

    // --- deepen: record a shallow boundary. Only `.git/shallow` changes.
    let boundary = out(&repo, &["rev-parse", "HEAD~2"]);
    std::fs::write(path.join(".git").join("shallow"), format!("{boundary}\n")).unwrap();
    assert_eq!(
        std::fs::read(&head_path).unwrap(),
        head_before,
        "HEAD is untouched by a deepen"
    );
    assert_eq!(
        std::fs::read(&ref_path).unwrap(),
        ref_before,
        "the branch ref is untouched by a deepen"
    );

    let walks_deepen = AtomicUsize::new(0);
    let error_deepen = page_for_target(
        &target,
        Some(&cursor_before),
        1,
        &codec,
        &HeaderMap::new(),
        &walks_deepen,
    )
    .await
    .expect_err("a cursor pinned before a deepen must be refused");
    assert_eq!(error_deepen.0, StatusCode::CONFLICT);
    assert_eq!(walks_deepen.load(Ordering::Relaxed), 0);

    let (_, tag_frame_deepened, _) = frame_parts(&repo, &HeaderMap::new()).await;
    let (_, tag_page_deepened, body_deepened, _) =
        page_one_parts(&repo, 1, &HeaderMap::new()).await;
    let page_deepened: Page = serde_json::from_slice(&body_deepened).unwrap();
    assert_ne!(
        generation_before, page_deepened.generation,
        "the shallow boundary is part of the history generation"
    );
    assert_ne!(tag_frame_before, tag_frame_deepened);
    assert_ne!(tag_page_before, tag_page_deepened);
    let cursor_deepened = page_deepened.cursor.clone().expect("more rows remain");

    // --- unshallow: clear the boundary. Again, only `.git/shallow` moves.
    std::fs::remove_file(path.join(".git").join("shallow")).unwrap();
    assert_eq!(
        std::fs::read(&head_path).unwrap(),
        head_before,
        "HEAD is untouched by an unshallow"
    );
    assert_eq!(
        std::fs::read(&ref_path).unwrap(),
        ref_before,
        "the branch ref is untouched by an unshallow"
    );

    let walks_unshallow = AtomicUsize::new(0);
    let error_unshallow = page_for_target(
        &target,
        Some(&cursor_deepened),
        1,
        &codec,
        &HeaderMap::new(),
        &walks_unshallow,
    )
    .await
    .expect_err("a cursor pinned before an unshallow must be refused");
    assert_eq!(error_unshallow.0, StatusCode::CONFLICT);
    assert_eq!(walks_unshallow.load(Ordering::Relaxed), 0);

    let (_, tag_frame_final, _) = frame_parts(&repo, &HeaderMap::new()).await;
    let (_, tag_page_final, body_final, _) = page_one_parts(&repo, 1, &HeaderMap::new()).await;
    let page_final: Page = serde_json::from_slice(&body_final).unwrap();
    assert_ne!(
        page_deepened.generation, page_final.generation,
        "unshallowing moves the generation again"
    );
    assert_ne!(tag_frame_deepened, tag_frame_final);
    assert_ne!(tag_page_deepened, tag_page_final);
}

/// Malformed `.git/shallow` content fails the very first combined snapshot
/// read `page_for_target` performs — before any cursor is even looked at —
/// so it is the handler-level twin of
/// `history::tests::malformed_shallow_metadata_is_snapshot_error`: an
/// explicit read error, never a silent "unshallow".
#[tokio::test]
async fn malformed_shallow_metadata_is_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = deterministic_repo(dir.path(), "alpha", 2);
    std::fs::write(repo.join(".git").join("shallow"), "not-hex\n").unwrap();

    let codec = history_codec();
    let target = history_target(&repo, &codec);
    let walks = AtomicUsize::new(0);
    let error = page_for_target(
        &target,
        None,
        DEFAULT_PAGE_LIMIT,
        &codec,
        &HeaderMap::new(),
        &walks,
    )
    .await
    .expect_err("malformed shallow metadata must be an explicit error, not a silent unshallow");
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(error.1.contains("shallow"), "{}", error.1);
    assert_eq!(
        walks.load(Ordering::Relaxed),
        0,
        "the snapshot read fails before the walk counter ever moves"
    );
}

/// A traversal failure and a concurrent repository move can happen
/// together; the combined re-read this triggers must report the move, not
/// the walk's own error — a 409 always outranks a simultaneous read error.
#[tokio::test]
async fn walk_error_after_snapshot_move_returns_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, tip, root) = deep_linear_repo(dir.path(), 1_500);
    // The root is visited last under `DateOrder`, so the walk must process
    // nearly the entire history before failing — the racer's whole window.
    std::fs::remove_file(loose_object(&repo, &root)).expect("the root is a real loose object");

    let tree = out(&repo, &["rev-parse", &format!("{tip}^{{tree}}")]);
    let extra = out(&repo, &["commit-tree", &tree, "-p", &tip, "-m", "extra"]);
    let racer = race_ref_move(&repo, "refs/heads/main", &extra);

    let codec = history_codec();
    let target = history_target(&repo, &codec);
    let walks = AtomicUsize::new(0);
    let error = page_for_target(&target, None, 1_500, &codec, &HeaderMap::new(), &walks)
        .await
        .expect_err("a walk that fails while the repository has moved must report the move");
    racer.join().unwrap();

    assert_eq!(
        error.0,
        StatusCode::CONFLICT,
        "drift takes precedence over the walk's own error: {error:?}"
    );
    assert_eq!(error.1, "history moved");
    assert_eq!(
        walks.load(Ordering::Relaxed),
        1,
        "the walk ran once before failing"
    );
}

/// The same missing-object failure, but nothing else moves: the combined
/// re-read finds the identical generation, so the explicit read error is
/// surfaced rather than an invented conflict.
#[tokio::test]
async fn walk_error_with_stable_snapshot_returns_explicit_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _tip, root) = deep_linear_repo(dir.path(), 30);
    std::fs::remove_file(loose_object(&repo, &root)).expect("the root is a real loose object");

    let codec = history_codec();
    let target = history_target(&repo, &codec);
    let walks = AtomicUsize::new(0);
    let error = page_for_target(
        &target,
        None,
        MAX_PAGE_LIMIT,
        &codec,
        &HeaderMap::new(),
        &walks,
    )
    .await
    .expect_err("a missing commit object must surface as an explicit read error");
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_ne!(
        error.0,
        StatusCode::CONFLICT,
        "nothing moved, so this must never be reported as drift"
    );
    assert_eq!(
        walks.load(Ordering::Relaxed),
        1,
        "the walk counted its one attempt before failing"
    );
}
