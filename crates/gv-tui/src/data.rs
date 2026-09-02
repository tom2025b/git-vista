//! The shell's persistent authenticated data client and detached worker
//! (M10.02, #457 — phase 2a).
//!
//! [`Client`] owns the only live [`Session`]. It authenticates lazily and
//! delegates the server-restart case to [`git_vista_session::retry`]: one
//! `401` earns one fresh session and one retry with its new cookie. [`Worker`]
//! moves that client onto one named thread, so the event loop never waits on
//! a socket and no lock is needed around session state.
//!
//! Dropping [`Worker`] drops its request sender. That ends the worker's
//! receive loop and drops the client (and therefore the in-memory session)
//! on the worker thread. There is deliberately no join-on-drop: a request
//! already inside the bounded HTTP call may take its socket timeout to end,
//! and quitting the terminal must not wait for it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use git_vista_conflicts::core::ResultRead;
use git_vista_core::diff::WorktreeFileContent;
use git_vista_protocol::conflict::Resolution;
use git_vista_protocol::{
    CommitOid, GenerationToken, RepoMode, ResolveConflictContentRequest, ResolveConflictRequest,
    SelectRequest, WorktreePath,
};
use git_vista_session::auth::{self, Session};
use git_vista_session::http::{self, HttpResponse};
use git_vista_session::retry;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::app::{Data, Fetch};

pub const CATALOG_PATH: &str = "/api/catalog";
pub const HISTORY_LIMIT: usize = 250;

pub type FetchFn = Box<dyn FnMut(&str, &str) -> Result<HttpResponse, String> + Send>;
/// The injected POST seam: `(path, body, cookie, csrf, idempotency key)`.
///
/// The key is an `Option` because one of this client's three POSTs does not
/// take one. `/api/select` never reaches the planner — it moves this session's
/// own selection and runs no git — and the planner is where the key
/// requirement lives, deliberately at the chokepoint rather than in a route
/// list that drifts.
pub type PostFn =
    Box<dyn FnMut(&str, &[u8], &str, &str, Option<&str>) -> Result<HttpResponse, String> + Send>;
pub type AuthFn = Box<dyn FnMut() -> Result<Session, String> + Send>;

pub struct Client {
    session: Option<Session>,
    fetch: FetchFn,
    post: PostFn,
    auth: AuthFn,
}

impl Client {
    pub fn live() -> Client {
        Client::with(
            Box::new(|path, cookie| http::get(path, Some(cookie))),
            Box::new(|path, body, cookie, csrf, key| match key {
                Some(key) => {
                    http::post_json_idempotent(path, body, Some(cookie), Some(csrf), key)
                }
                None => http::post_json(path, body, Some(cookie), Some(csrf)),
            }),
            Box::new(auth::authenticate),
        )
    }

    pub fn with(fetch: FetchFn, post: PostFn, auth: AuthFn) -> Client {
        Client {
            session: None,
            fetch,
            post,
            auth,
        }
    }

    pub fn get_json<T: DeserializeOwned>(&mut self, path: &str) -> Result<T, String> {
        let body = retry::authed_fetch(path, &mut self.session, &mut *self.fetch, &mut *self.auth)?;
        serde_json::from_slice(&body)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
    }

    /// A read whose 404 is an answer rather than a failure.
    ///
    /// `Ok(None)` means the server said 404. Only `GET /api/worktree-file`
    /// uses this, and it is the reason `authed_fetch_response` exists: in a
    /// delete/modify conflict git legitimately leaves nothing on disk, so
    /// "there is no file at this path" is what the result pane must say. Read
    /// through the ordinary `get_json`, that fact would arrive as the sentence
    /// "content could not be loaded" — a fault reported where nothing went
    /// wrong, which is the collapse ADR 0063 exists to prevent.
    fn get_json_or_missing<T: DeserializeOwned>(
        &mut self,
        path: &str,
    ) -> Result<Option<T>, String> {
        let (response, reauthenticated) = retry::authed_fetch_response(
            path,
            &mut self.session,
            &mut *self.fetch,
            &mut *self.auth,
        )?;
        match response.status {
            404 => Ok(None),
            200 => serde_json::from_slice(&response.body)
                .map(Some)
                .map_err(|error| format!("{path} did not return valid JSON: {error}")),
            status => {
                let after = if reauthenticated {
                    " even after re-authenticating"
                } else {
                    ""
                };
                Err(format!(
                    "GET {path} answered {status}{after}: {}",
                    String::from_utf8_lossy(&response.body)
                ))
            }
        }
    }

    /// POST a JSON body through the session, with an optional idempotency key.
    fn post_json<T: Serialize>(
        &mut self,
        path: &str,
        body: &T,
        key: Option<&str>,
    ) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(body)
            .map_err(|error| format!("could not encode the request for {path}: {error}"))?;
        // Destructured so the session and the POST closure are two disjoint
        // borrows rather than one of `self`.
        let Client {
            session, post, ..
        } = self;
        let auth = &mut *self.auth;
        retry::authed_post(
            path,
            &body,
            session,
            &mut |path, body, cookie, csrf| post(path, body, cookie, csrf, key),
            auth,
        )
    }

    /// Point this session at `repo` in a mode that admits writes, immediately
    /// before the write itself.
    ///
    /// **Not once when the overlay opens — every time, paired with the
    /// write.** The reads in this client address a repository explicitly with
    /// `?repo=`, but `/api/resolve-conflict` carries no repository at all: it
    /// goes through the planner, which acts on *this session's* selection
    /// (ADR 0103 made that per-session; before #588 it was per-process). A
    /// client that listed one repository's conflicts and posted a resolution
    /// without selecting would write to whichever repository the server
    /// launched with — and if that one happened to have a conflict at the same
    /// path, the write would succeed, in the wrong repository, silently.
    ///
    /// Pairing the two makes "the write lands where the user was looking" true
    /// by construction rather than by an earlier call having been remembered.
    /// `Active` because it is the only mode a write is legal in
    /// (`reject_if_read_only` refuses `Visualize`), and the selection is this
    /// terminal session's own — the browser's session keeps whatever it had.
    fn select_for_write(&mut self, repo: &str) -> Result<(), String> {
        self.post_json(
            "/api/select",
            &SelectRequest {
                worktree: repo.to_string(),
                mode: RepoMode::Active,
            },
            None,
        )
        .map(|_| ())
        .map_err(|error| format!("could not select the repository to write to: {error}"))
    }

    pub fn serve(&mut self, fetch: Fetch) -> Data {
        match fetch {
            Fetch::Catalog => Data::Catalog(self.get_json(CATALOG_PATH)),
            Fetch::History { repo } => {
                let path = format!("/api/commits?repo={repo}&limit={HISTORY_LIMIT}");
                let result = self.get_json(&path);
                Data::History { repo, result }
            }
            Fetch::Commit { repo, id } => {
                let path = format!("/api/commit/{id}?repo={repo}");
                let result = self.get_json(&path);
                Data::Commit { repo, id, result }
            }
            Fetch::Diff { repo, id } => {
                // Deliberately no `full=1`: the terminal consumes the server's
                // bounded panel representation and windows it again on draw.
                let path = format!("/api/diff/{id}?repo={repo}");
                let result = self.get_json(&path);
                Data::Diff { repo, id, result }
            }
            Fetch::Conflicts { repo } => {
                let path = format!("/api/conflicts?repo={repo}");
                let result = self.get_json(&path);
                Data::Conflicts { repo, result }
            }
            Fetch::ConflictStage {
                repo,
                path,
                pane,
                oid,
            } => {
                // The oid goes into the URL unencoded on purpose: the server
                // admits only 40 or 64 lowercase hex characters and answers
                // 400 to anything else before it spawns git, so there is no
                // byte here that percent-encoding would protect. The path
                // below is arbitrary user text and is a different matter.
                let url = format!("/api/blob/{oid}?repo={repo}");
                let result = self.get_json(&url);
                Data::ConflictStage {
                    repo,
                    path,
                    pane,
                    result,
                }
            }
            Fetch::ConflictResult { repo, path } => {
                let url = format!("/api/worktree-file/{}?repo={repo}", encode_path(&path));
                let read = match self.get_json_or_missing::<WorktreeFileContent>(&url) {
                    Ok(Some(file)) => ResultRead::Wrote(file),
                    Ok(None) => ResultRead::NoFile,
                    Err(error) => ResultRead::Failed(error),
                };
                Data::ConflictResult { repo, path, read }
            }
            Fetch::ConflictSource { repo, path } => {
                let url = format!("/api/conflict-source/{}?repo={repo}", encode_path(&path));
                let result = self.get_json(&url);
                Data::ConflictSource { repo, path, result }
            }
            Fetch::ResolveWholeFile {
                repo,
                path,
                resolution,
            } => {
                let result = self.resolve_whole_file(&repo, &path, resolution);
                Data::Resolved { repo, path, result }
            }
            Fetch::ResolveContent {
                repo,
                path,
                expected_stages,
                expected_source,
                content,
            } => {
                let result =
                    self.resolve_content(&repo, &path, expected_stages, expected_source, content);
                Data::Resolved { repo, path, result }
            }
        }
    }

    /// `POST /api/resolve-conflict` — take a whole side, or the deletion.
    fn resolve_whole_file(
        &mut self,
        repo: &str,
        path: &str,
        resolution: Resolution,
    ) -> Result<(), String> {
        // The DTO's `path` is a `WorktreePath`, so a traversal cannot be built
        // into a request here at all — the same wire-boundary guarantee the
        // server relies on, enforced one process earlier. In practice this
        // never fails: the path came from `/api/conflicts`, which reports what
        // git itself listed. It is checked rather than unwrapped because "git
        // said so" is an assumption, and a panic in a program that has taken
        // over the terminal is worse than a sentence on the status line.
        let path = WorktreePath::new(path.to_string()).map_err(|e| e.to_string())?;
        self.select_for_write(repo)?;
        self.post_json(
            "/api/resolve-conflict",
            &ResolveConflictRequest { path, resolution },
            Some(&mint_idempotency_key()),
        )
        .map(|_| ())
    }

    /// `POST /api/resolve-conflict-content` — a block, line or hand-edited
    /// resolution (ADR 0069).
    ///
    /// `expected_stages` and `expected_source` travel back exactly as they
    /// were served. Nothing here recomputes either: the executor compares them
    /// against a fresh scan and a re-minted token inside its lock, and a client
    /// that computed its own would only ever agree with itself — gates 3 and 4
    /// would pass by construction and prove nothing.
    fn resolve_content(
        &mut self,
        repo: &str,
        path: &str,
        expected_stages: [Option<CommitOid>; 3],
        expected_source: GenerationToken,
        content: String,
    ) -> Result<(), String> {
        let path = WorktreePath::new(path.to_string()).map_err(|e| e.to_string())?;
        self.select_for_write(repo)?;
        self.post_json(
            "/api/resolve-conflict-content",
            &ResolveConflictContentRequest {
                path,
                expected_stages,
                expected_source,
                content,
            },
            Some(&mint_idempotency_key()),
        )
        .map(|_| ())
    }
}

/// Percent-encode one worktree path for the server's wildcard routes.
///
/// Slashes stay literal — the route is `/{*path}` and those separators are the
/// path's own. Everything outside RFC 3986's unreserved set is escaped, which
/// is stricter than the minimum and deliberately so: a `#` in a filename would
/// otherwise cut the request short at a fragment, a `?` would start a query
/// string, and a space would end the request line early. The browser client
/// reaches the same set by calling `encodeURIComponent` per segment; this is
/// that rule written out where there is no JS engine to call.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One key press that asks for a write is one intent, and gets one key.
///
/// Unique per press rather than derived from the request's content. An
/// idempotency key names *one user action*, and the server replays the
/// recorded outcome for a key it has already seen — so a key derived from the
/// resolution itself would make a second, deliberate attempt at the same
/// resolution look like a retry of the first and replay its answer instead of
/// running. For a refusal that means being told again about a repository state
/// that has since changed.
///
/// The wall-clock nanosecond is in there because the operation registry is
/// durable across server restarts (#62): a bare counter would restart at 1 in
/// a fresh `gv-tui` and collide with the previous run's keys, and a collision
/// here is a write that silently does not happen. The counter covers two
/// presses inside one clock tick.
///
/// The 401 retry inside [`retry::authed_post`] reuses this call's key, which
/// is the case the mechanism exists for: same intent, second attempt.
fn mint_idempotency_key() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("gvtui-{nanos}-{seq}")
}

pub trait DataPort {
    fn request(&mut self, fetch: Fetch);
    fn poll(&mut self) -> Option<Data>;
}

pub struct Worker {
    requests: Sender<Fetch>,
    answers: Receiver<Data>,
    pending: VecDeque<Data>,
}

pub fn spawn(mut client: Client) -> Worker {
    let (request_tx, request_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    thread::Builder::new()
        .name(String::from("gv-tui-data"))
        .spawn(move || {
            for fetch in request_rx {
                if answer_tx.send(client.serve(fetch)).is_err() {
                    break;
                }
            }
        })
        .expect("gv-tui could not start its data thread");
    Worker {
        requests: request_tx,
        answers: answer_rx,
        pending: VecDeque::new(),
    }
}

impl DataPort for Worker {
    fn request(&mut self, fetch: Fetch) {
        if let Err(stopped) = self.requests.send(fetch) {
            self.pending.push_back(stopped_answer(stopped.0));
        }
    }

    fn poll(&mut self) -> Option<Data> {
        self.pending
            .pop_front()
            .or_else(|| self.answers.try_recv().ok())
    }
}

fn stopped_answer(fetch: Fetch) -> Data {
    let message = || String::from("the data thread has stopped; restart gv-tui");
    match fetch {
        Fetch::Catalog => Data::Catalog(Err(message())),
        Fetch::History { repo } => Data::History {
            repo,
            result: Err(message()),
        },
        Fetch::Commit { repo, id } => Data::Commit {
            repo,
            id,
            result: Err(message()),
        },
        Fetch::Diff { repo, id } => Data::Diff {
            repo,
            id,
            result: Err(message()),
        },
        Fetch::Conflicts { repo } => Data::Conflicts {
            repo,
            result: Err(message()),
        },
        Fetch::ConflictStage {
            repo, path, pane, ..
        } => Data::ConflictStage {
            repo,
            path,
            pane,
            result: Err(message()),
        },
        // `Failed`, never `NoFile`. A request that never left this process
        // observed nothing about what is on disk, and answering "there is no
        // file at that path" would assert a fact nobody looked for — the exact
        // collapse `Stage::Absent` versus `Stage::Unreadable` exists to stop.
        Fetch::ConflictResult { repo, path } => Data::ConflictResult {
            repo,
            path,
            read: ResultRead::Failed(message()),
        },
        Fetch::ConflictSource { repo, path } => Data::ConflictSource {
            repo,
            path,
            result: Err(message()),
        },
        Fetch::ResolveWholeFile { repo, path, .. } | Fetch::ResolveContent { repo, path, .. } => {
            Data::Resolved {
                repo,
                path,
                result: Err(message()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    use git_vista_protocol::RepositoryDescriptor;

    use super::*;

    const ALPHA: &str = r#"[
      {"repository":"r1","worktree":"w1","name":"alpha","kind":"main_worktree","read_only":false}
    ]"#;
    const BETA: &str = r#"[
      {"repository":"r2","worktree":"w2","name":"beta","kind":"bare","read_only":true}
    ]"#;

    fn session(generation: usize) -> Session {
        Session {
            cookie: format!("gv_session=gen{generation}"),
            csrf: format!("csrf-gen{generation}"),
        }
    }

    /// The POST seam for a test that only reads. It panics rather than
    /// returning a benign answer: a read-only test that starts posting has
    /// changed what it is testing, and should say so loudly.
    fn never_posts() -> PostFn {
        Box::new(|path, _, _, _, _| panic!("this test never posts, but something posted to {path}"))
    }

    fn response(status: u16, body: impl AsRef<[u8]>) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.as_ref().to_vec(),
        }
    }

    #[test]
    fn a_401_mid_session_re_authenticates_once_and_the_read_still_succeeds() {
        let auths = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auths);
        let fetch_count = Arc::clone(&fetches);
        let mut client = Client::with(
            Box::new(move |path, cookie| {
                assert_eq!(path, CATALOG_PATH);
                let call = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                match (call, cookie) {
                    (1, "gv_session=gen1") => Ok(response(200, ALPHA)),
                    (2, "gv_session=gen1") => Ok(response(401, "expired")),
                    (3 | 4, "gv_session=gen2") => Ok(response(200, BETA)),
                    _ => panic!("unexpected fetch {call} with {cookie}"),
                }
            }),
            never_posts(),
            Box::new(move || {
                let generation = auth_count.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(session(generation))
            }),
        );

        let first: Vec<RepositoryDescriptor> = client.get_json(CATALOG_PATH).unwrap();
        assert_eq!(first[0].name, "alpha");
        assert_eq!(auths.load(Ordering::SeqCst), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);

        let second: Vec<RepositoryDescriptor> = client.get_json(CATALOG_PATH).unwrap();
        assert_eq!(second[0].name, "beta");
        assert_eq!(auths.load(Ordering::SeqCst), 2);
        assert_eq!(fetches.load(Ordering::SeqCst), 3);

        let third: Vec<RepositoryDescriptor> = client.get_json(CATALOG_PATH).unwrap();
        assert_eq!(third[0].name, "beta");
        assert_eq!(auths.load(Ordering::SeqCst), 2, "the fresh session is kept");
        assert_eq!(fetches.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_non_200_answer_and_a_malformed_body_are_errors_that_name_the_path() {
        let mut client = Client::with(
            Box::new(|path, _| match path {
                "/api/down" => Ok(response(503, "catalog rebuilding")),
                "/api/broken" => Ok(response(200, "<html>not json</html>")),
                _ => panic!("unexpected path {path}"),
            }),
            never_posts(),
            Box::new(|| Ok(session(1))),
        );

        let down = client
            .get_json::<Vec<RepositoryDescriptor>>("/api/down")
            .unwrap_err();
        assert!(down.contains("/api/down"), "{down}");
        assert!(down.contains("503"), "{down}");
        assert!(down.contains("catalog rebuilding"), "{down}");

        let broken = client
            .get_json::<Vec<RepositoryDescriptor>>("/api/broken")
            .unwrap_err();
        assert!(broken.contains("/api/broken"), "{broken}");
        assert!(broken.contains("valid JSON"), "{broken}");
    }

    #[test]
    fn serve_maps_a_catalog_fetch_to_a_catalog_answer_carrying_the_error() {
        let mut client = Client::with(
            Box::new(|_, _| Ok(response(503, "catalog rebuilding"))),
            never_posts(),
            Box::new(|| Ok(session(1))),
        );
        match client.serve(Fetch::Catalog) {
            Data::Catalog(Err(message)) => {
                assert!(message.contains(CATALOG_PATH), "{message}");
                assert!(message.contains("503"), "{message}");
            }
            Data::Catalog(Ok(_)) => panic!("a 503 became catalog rows"),
            _ => panic!("a catalog request became a different answer kind"),
        }
    }

    #[test]
    fn the_worker_answers_every_request_in_order_without_blocking_the_caller() {
        let auths = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auths);
        let fetch_count = Arc::clone(&fetches);
        let client = Client::with(
            Box::new(move |_, cookie| {
                let call = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    thread::sleep(Duration::from_millis(150));
                }
                match (call, cookie) {
                    (1, "gv_session=gen1") => Ok(response(200, ALPHA)),
                    (2, "gv_session=gen1") => Ok(response(401, "expired")),
                    (3, "gv_session=gen2") => Ok(response(200, BETA)),
                    _ => panic!("unexpected fetch {call} with {cookie}"),
                }
            }),
            never_posts(),
            Box::new(move || {
                let generation = auth_count.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(session(generation))
            }),
        );
        let mut worker = spawn(client);

        let sent_at = Instant::now();
        worker.request(Fetch::Catalog);
        worker.request(Fetch::Catalog);
        assert!(
            sent_at.elapsed() < Duration::from_millis(100),
            "request waited for the deliberately slow fetch"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut names = Vec::new();
        while names.len() < 2 && Instant::now() < deadline {
            if let Some(Data::Catalog(answer)) = worker.poll() {
                names.push(answer.unwrap()[0].name.clone());
            } else {
                thread::yield_now();
            }
        }
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(fetches.load(Ordering::SeqCst), 3);
        assert_eq!(auths.load(Ordering::SeqCst), 2);
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_the_worker_ends_its_thread_and_drops_the_client_holding_the_session() {
        let dropped = Arc::new(AtomicBool::new(false));
        let held = DropFlag(Arc::clone(&dropped));
        let client = Client::with(
            Box::new(move |_, _| {
                let _held = &held;
                Ok(response(200, ALPHA))
            }),
            never_posts(),
            Box::new(|| Ok(session(1))),
        );
        let worker = spawn(client);
        drop(worker);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "the detached thread kept the client and its session alive"
        );
    }

    #[test]
    fn a_request_the_thread_can_no_longer_take_becomes_an_error_answer_not_silence() {
        let (request_tx, request_rx) = mpsc::channel();
        drop(request_rx);
        let (_answer_tx, answer_rx) = mpsc::channel();
        let mut worker = Worker {
            requests: request_tx,
            answers: answer_rx,
            pending: VecDeque::new(),
        };

        worker.request(Fetch::Catalog);
        match worker.poll() {
            Some(Data::Catalog(Err(message))) => {
                assert_eq!(message, "the data thread has stopped; restart gv-tui");
            }
            Some(Data::Catalog(Ok(_))) => panic!("a stopped worker returned rows"),
            None => panic!("a stopped worker silently lost the request"),
            Some(_) => panic!("a catalog request became a different answer kind"),
        }
    }
}
