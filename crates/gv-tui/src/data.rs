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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use git_vista_conflicts::core::ResultRead;
use git_vista_core::diff::WorktreeFileContent;
use git_vista_protocol::conflict::Resolution;
use git_vista_protocol::{
    CommitOid, GenerationToken, OperationByKeyResponse, RepoMode, ResolveConflictContentRequest,
    ResolveConflictRequest, SelectRequest, WorktreePath,
};
use git_vista_session::auth::{self, Session};
use git_vista_session::http::{self, HttpResponse};
use serde::de::DeserializeOwned;

use crate::app::{Data, Fetch};
use crate::panes::plan_review::{PlanApproval, SubmissionOutcome};

pub const CATALOG_PATH: &str = "/api/catalog";
pub const HISTORY_LIMIT: usize = 250;
pub const EXECUTE_PLAN_PATH: &str = "/api/execute-plan";
pub const PLAN_PATH: &str = "/api/plan";
pub const SELECT_PATH: &str = "/api/select";
pub const RESOLVE_CONFLICT_PATH: &str = "/api/resolve-conflict";
pub const RESOLVE_CONFLICT_CONTENT_PATH: &str = "/api/resolve-conflict-content";

pub type FetchFn = Box<dyn Fn(&str, &str) -> Result<HttpResponse, String> + Send + Sync>;
pub type PostFn =
    Box<dyn Fn(&str, &[u8], &str, &str) -> Result<HttpResponse, String> + Send + Sync>;
pub type IdempotentPostFn =
    Box<dyn Fn(&str, &[u8], &str, &str, &str) -> Result<HttpResponse, String> + Send + Sync>;
pub type AuthFn = Box<dyn Fn() -> Result<Session, String> + Send + Sync>;
type SharedFetchFn = Arc<dyn Fn(&str, &str) -> Result<HttpResponse, String> + Send + Sync>;
type SharedPostFn =
    Arc<dyn Fn(&str, &[u8], &str, &str) -> Result<HttpResponse, String> + Send + Sync>;
type SharedIdempotentPostFn =
    Arc<dyn Fn(&str, &[u8], &str, &str, &str) -> Result<HttpResponse, String> + Send + Sync>;
type SharedAuthFn = Arc<dyn Fn() -> Result<Session, String> + Send + Sync>;

#[derive(Clone)]
pub struct Client {
    session: Arc<Mutex<Option<Session>>>,
    fetch: SharedFetchFn,
    post: SharedPostFn,
    idempotent_post: SharedIdempotentPostFn,
    auth: SharedAuthFn,
}

impl Client {
    pub fn live() -> Client {
        Client::with_transport(
            Box::new(|path, cookie| http::get(path, Some(cookie))),
            Box::new(|path, body, cookie, csrf| {
                http::post_json(path, body, Some(cookie), Some(csrf))
            }),
            Box::new(|path, body, cookie, csrf, key| {
                http::post_json_idempotent(path, body, Some(cookie), Some(csrf), key)
            }),
            Box::new(auth::authenticate),
        )
    }

    #[cfg(test)]
    pub fn with(fetch: FetchFn, auth: AuthFn) -> Client {
        Client::with_transport(
            fetch,
            Box::new(|path, body, cookie, csrf| {
                http::post_json(path, body, Some(cookie), Some(csrf))
            }),
            Box::new(|path, body, cookie, csrf, key| {
                http::post_json_idempotent(path, body, Some(cookie), Some(csrf), key)
            }),
            auth,
        )
    }

    pub fn with_transport(
        fetch: FetchFn,
        post: PostFn,
        idempotent_post: IdempotentPostFn,
        auth: AuthFn,
    ) -> Client {
        Client {
            session: Arc::new(Mutex::new(None)),
            fetch: Arc::from(fetch),
            post: Arc::from(post),
            idempotent_post: Arc::from(idempotent_post),
            auth: Arc::from(auth),
        }
    }

    pub fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let session = self.authenticated()?;
        let response = (self.fetch)(path, &session.cookie)?;
        let response = if response.status == 401 {
            let fresh = self.reauthenticate_after(&session)?;
            (self.fetch)(path, &fresh.cookie)?
        } else {
            response
        };
        if !(200..=299).contains(&response.status) {
            return Err(format!(
                "GET {path} answered {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ));
        }
        let body = response.body;
        serde_json::from_slice(&body)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
    }

    pub fn serve(&self, fetch: Fetch) -> Data {
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
            Fetch::Select { repo } => {
                let result = self.select_active(&repo);
                Data::Selected { repo, result }
            }
            Fetch::BuildPlan(operation) => {
                let body = serde_json::to_vec(&operation).map_err(|error| error.to_string());
                Data::PlanReady(body.and_then(|body| self.post_json(PLAN_PATH, &body)))
            }
            Fetch::Tags { repo } => {
                let path = format!("/api/tags?repo={repo}");
                let result = self.get_json(&path);
                Data::Tags { repo, result }
            }
            Fetch::OperationByKey { key } => {
                let path = format!("/api/operations/by-key/{key}");
                let result = self.get_optional_json::<OperationByKeyResponse>(&path);
                Data::OperationByKey {
                    key,
                    result: result.map(|found| found.map(|response| response.id)),
                }
            }
            Fetch::OperationStatus { id } => {
                let path = format!("/api/operations/{}", id.as_str());
                Data::OperationStatus(self.get_json(&path))
            }
            Fetch::CancelOperation { id } => {
                let path = format!("/api/operations/{}/cancel", id.as_str());
                let result = self
                    .post_json(&path, b"{}")
                    .map(|body| String::from_utf8_lossy(&body).trim().to_string());
                Data::OperationCancelled { id, result }
            }
            Fetch::ExecutePlan(approval) => Data::PlanSubmitted(self.submit_plan(&approval)),
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
                // `get_optional_json`, not `get_json`, and that is the whole
                // point of this arm. The server answers 404 when there is no
                // file at the path, and in a delete/modify conflict that is
                // exactly what git left behind — information, not a fault.
                // Through `get_json` it would arrive as a failed read, which
                // reports something broke when nothing did.
                let url = format!("/api/worktree-file/{}?repo={repo}", encode_path(&path));
                let read = match self.get_optional_json::<WorktreeFileContent>(&url) {
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

    fn get_optional_json<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, String> {
        let session = self.authenticated()?;
        let response = (self.fetch)(path, &session.cookie)?;
        let response = if response.status == 401 {
            let fresh = self.reauthenticate_after(&session)?;
            (self.fetch)(path, &fresh.cookie)?
        } else {
            response
        };
        if response.status == 404 {
            return Ok(None);
        }
        if !(200..=299).contains(&response.status) {
            return Err(format!(
                "GET {path} answered {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ));
        }
        serde_json::from_slice(&response.body)
            .map(Some)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
    }

    /// Point this session at `repo` in a mode that admits writes.
    ///
    /// One definition, used by `Fetch::Select` and by every conflict write.
    fn select_active(&self, repo: &str) -> Result<(), String> {
        let body = serde_json::to_vec(&SelectRequest {
            worktree: repo.to_string(),
            mode: RepoMode::Active,
        })
        .map_err(|error| error.to_string())?;
        self.post_json(SELECT_PATH, &body).map(|_| ())
    }

    /// `POST` a planner write, carrying an idempotency key.
    ///
    /// [`Self::post_json`]'s keyed sibling. `submit_plan` reaches
    /// `idempotent_post` through its own `post_approval` because an approval
    /// already carries its key and body together; a conflict resolution has
    /// neither, so it needs the general form.
    fn post_json_keyed(&self, path: &str, body: &[u8], key: &str) -> Result<Vec<u8>, String> {
        let session = self.authenticated()?;
        let response = (self.idempotent_post)(path, body, &session.cookie, &session.csrf, key)?;
        let response = if response.status == 401 {
            let fresh = self.reauthenticate_after(&session)?;
            // The SAME key on the retry. That is what the mechanism is for:
            // one intent, a second attempt, recognised as a retry rather than
            // run twice.
            (self.idempotent_post)(path, body, &fresh.cookie, &fresh.csrf, key)?
        } else {
            response
        };
        if (200..=299).contains(&response.status) {
            Ok(response.body)
        } else {
            Err(format!(
                "POST {path} answered {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ))
        }
    }

    /// `POST /api/resolve-conflict` — take a whole side, or the deletion.
    ///
    /// **The select is paired with the write, every time, and is not an
    /// artefact of the merge with #461.** The shell does select a repository
    /// when one is activated, so in the ordinary flow this is a second
    /// select — but "the resolution lands where the user was looking" then
    /// depends on that earlier call having run and nothing having changed the
    /// selection since. `/api/resolve-conflict` carries no repository at all:
    /// it goes through the planner, which acts on this session's selection
    /// (ADR 0103). Pairing makes the guarantee structural rather than
    /// remembered, and the failure it prevents is silent — a conflict at the
    /// same path in another repository resolves successfully, in the wrong
    /// one. ADR 0105 decision 5 records that the real fix is for the endpoint
    /// to carry the repository, the way every conflict READ already does;
    /// that is issue #621, and this pairing goes away when it lands.
    fn resolve_whole_file(
        &self,
        repo: &str,
        path: &str,
        resolution: Resolution,
    ) -> Result<(), String> {
        // The DTO's `path` is a `WorktreePath`, so a traversal cannot be built
        // into a request here at all — the same wire-boundary guarantee the
        // server relies on, enforced one process earlier. It came from
        // `/api/conflicts`, so in practice this never fails; it is checked
        // rather than unwrapped because "git said so" is an assumption, and a
        // panic in a program that has taken over the terminal is worse than a
        // sentence on the status line.
        let path = WorktreePath::new(path.to_string()).map_err(|e| e.to_string())?;
        self.select_for_write(repo)?;
        let body = serde_json::to_vec(&ResolveConflictRequest { path, resolution })
            .map_err(|error| error.to_string())?;
        self.post_json_keyed(RESOLVE_CONFLICT_PATH, &body, &mint_idempotency_key())
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
        &self,
        repo: &str,
        path: &str,
        expected_stages: [Option<CommitOid>; 3],
        expected_source: GenerationToken,
        content: String,
    ) -> Result<(), String> {
        let path = WorktreePath::new(path.to_string()).map_err(|e| e.to_string())?;
        self.select_for_write(repo)?;
        let body = serde_json::to_vec(&ResolveConflictContentRequest {
            path,
            expected_stages,
            expected_source,
            content,
        })
        .map_err(|error| error.to_string())?;
        self.post_json_keyed(
            RESOLVE_CONFLICT_CONTENT_PATH,
            &body,
            &mint_idempotency_key(),
        )
        .map(|_| ())
    }

    fn select_for_write(&self, repo: &str) -> Result<(), String> {
        self.select_active(repo)
            .map_err(|error| format!("could not select the repository to write to: {error}"))
    }

    fn post_json(&self, path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        let session = self.authenticated()?;
        let response = self.post_once(path, body, &session)?;
        let response = if response.status == 401 {
            let fresh = self.reauthenticate_after(&session)?;
            self.post_once(path, body, &fresh)?
        } else {
            response
        };
        if (200..=299).contains(&response.status) {
            Ok(response.body)
        } else {
            Err(format!(
                "POST {path} answered {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ))
        }
    }

    fn post_once(
        &self,
        path: &str,
        body: &[u8],
        session: &Session,
    ) -> Result<HttpResponse, String> {
        (self.post)(path, body, &session.cookie, &session.csrf)
    }

    /// Submit exactly one reviewed plan. Only a 401 earns a retry, and that
    /// retry reuses the same body and idempotency key. A 409 is an operation
    /// refusal, never a prompt to rebuild or resubmit behind the user's back.
    fn submit_plan(&self, approval: &PlanApproval) -> SubmissionOutcome {
        let session = match self.authenticated() {
            Ok(session) => session,
            Err(message) => return SubmissionOutcome::TransportFailed(message),
        };
        let response = self.post_approval(approval, &session);
        let response = match response {
            Ok(response) if response.status == 401 => match self.reauthenticate_after(&session) {
                Ok(fresh) => self.post_approval(approval, &fresh),
                Err(message) => return SubmissionOutcome::TransportFailed(message),
            },
            other => other,
        };

        match response {
            Ok(response) => SubmissionOutcome::from_response(response.status, &response.body),
            Err(message) => SubmissionOutcome::TransportFailed(message),
        }
    }

    fn post_approval(
        &self,
        approval: &PlanApproval,
        session: &Session,
    ) -> Result<HttpResponse, String> {
        (self.idempotent_post)(
            EXECUTE_PLAN_PATH,
            approval.body(),
            &session.cookie,
            &session.csrf,
            approval.key(),
        )
    }

    fn authenticated(&self) -> Result<Session, String> {
        let mut slot = self
            .session
            .lock()
            .map_err(|_| String::from("the in-memory session lock was poisoned"))?;
        if let Some(session) = slot.as_ref() {
            return Ok(session.clone());
        }
        let session = (self.auth)()?;
        *slot = Some(session.clone());
        Ok(session)
    }

    /// Refresh only if this request still owns the stale generation. A
    /// concurrent request may already have replaced it; in that case reuse
    /// the fresh in-memory session instead of racing for another token.
    fn reauthenticate_after(&self, attempted: &Session) -> Result<Session, String> {
        let mut slot = self
            .session
            .lock()
            .map_err(|_| String::from("the in-memory session lock was poisoned"))?;
        if let Some(current) = slot
            .as_ref()
            .filter(|current| current.cookie != attempted.cookie)
        {
            return Ok(current.clone());
        }
        let session = (self.auth)()?;
        *slot = Some(session.clone());
        Ok(session)
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

/// One key press that asks for a conflict write is one intent, and gets one
/// key.
///
/// Unique per press rather than derived from the request's content. An
/// idempotency key names *one user action*, and the server replays the
/// recorded outcome for a key it has already seen — so a key derived from the
/// resolution itself would make a second, deliberate attempt at the same
/// resolution look like a retry of the first and replay its answer instead of
/// running. For a refusal that means being told again about a repository state
/// that has since changed.
///
/// Minted here rather than reusing `PlanApproval::key` because a conflict
/// resolution never becomes a reviewed plan: it posts straight to its own
/// endpoint, so there is no approval to take a key from.
///
/// The wall-clock nanosecond is in the key because the operation registry is
/// durable across server restarts (#62): a bare counter would restart at 1 in
/// a fresh `gv-tui` and collide with the previous run's keys, and a collision
/// here is a write that silently does not happen. The counter covers two
/// presses inside one clock tick.
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

pub fn spawn(client: Client) -> Worker {
    let (request_tx, request_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    thread::Builder::new()
        .name(String::from("gv-tui-data"))
        .spawn(move || {
            for fetch in request_rx {
                if let Fetch::ExecutePlan(approval) = fetch {
                    let execute_client = client.clone();
                    let execute_answers = answer_tx.clone();
                    let started = thread::Builder::new()
                        .name(String::from("gv-tui-execute"))
                        .spawn(move || {
                            let answer = execute_client.serve(Fetch::ExecutePlan(approval));
                            let _ = execute_answers.send(answer);
                        });
                    if started.is_err()
                        && answer_tx
                            .send(Data::PlanSubmitted(SubmissionOutcome::TransportFailed(
                                String::from("gv-tui could not start its execution thread"),
                            )))
                            .is_err()
                    {
                        break;
                    }
                    continue;
                }
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
        Fetch::Select { repo } => Data::Selected {
            repo,
            result: Err(message()),
        },
        Fetch::BuildPlan(_) => Data::PlanReady(Err(message())),
        Fetch::Tags { repo } => Data::Tags {
            repo,
            result: Err(message()),
        },
        Fetch::OperationByKey { key } => Data::OperationByKey {
            key,
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
        Fetch::OperationStatus { .. } => Data::OperationStatus(Err(message())),
        Fetch::CancelOperation { id } => Data::OperationCancelled {
            id,
            result: Err(message()),
        },
        Fetch::ExecutePlan(_) => Data::PlanSubmitted(SubmissionOutcome::TransportFailed(message())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use git_vista_protocol::{
        BranchName, GenerationToken, GitOperation, OperationHash, OperationId, Plan,
        RecoveryStrategy, RepositoryDescriptor, RepositoryToken, RiskLevel, UnixSeconds,
        WorktreeToken,
    };

    use super::*;
    use crate::panes::plan_review::PlanReviewPane;

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

    // ---- conflict reads and writes (M10.07, #462) ----------------------

    /// `(path, body, idempotency key)` for every POST a test provoked.
    type Posted = Arc<std::sync::Mutex<Vec<(String, Vec<u8>, Option<String>)>>>;

    /// A client whose POSTs are recorded and answered by `reply`.
    ///
    /// Built through `with_transport` rather than the two-argument `with`,
    /// because these tests are about what gets posted and in what order.
    fn recording_client(
        reply: impl Fn(&str) -> HttpResponse + Send + Sync + 'static,
        get: impl Fn(&str) -> HttpResponse + Send + Sync + 'static,
    ) -> (Client, Posted) {
        let posted: Posted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plain = Arc::clone(&posted);
        let keyed = Arc::clone(&posted);
        let plain_reply = Arc::new(reply);
        let keyed_reply = Arc::clone(&plain_reply);
        let client = Client::with_transport(
            Box::new(move |path, _| Ok(get(path))),
            Box::new(move |path, body, _, _| {
                plain
                    .lock()
                    .unwrap()
                    .push((path.to_string(), body.to_vec(), None));
                Ok(plain_reply(path))
            }),
            Box::new(move |path, body, _, _, key| {
                keyed.lock().unwrap().push((
                    path.to_string(),
                    body.to_vec(),
                    Some(key.to_string()),
                ));
                Ok(keyed_reply(path))
            }),
            Box::new(|| Ok(session(1))),
        );
        (client, posted)
    }

    fn ok_body(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn oid_hex(seed: char) -> CommitOid {
        CommitOid::new(std::iter::repeat_n(seed, 40).collect::<String>()).unwrap()
    }

    #[test]
    fn a_write_selects_the_repository_it_is_about_to_write_to_first() {
        // `/api/resolve-conflict` carries no repository — it goes through the
        // planner, which acts on this session's selection (ADR 0103). Without
        // the select, a resolution lands in whichever repository the session
        // last pointed at, and if that one happens to have a conflict at the
        // same path it SUCCEEDS, in the wrong repository, silently.
        //
        // MUTATION A: delete the `select_for_write` call. MUTATION B: move it
        // after the resolution post. Both leave the write working against
        // whatever was already selected, so only the ORDER assertion catches
        // them.
        let (client, posted) = recording_client(
            |_| ok_body("Resolved."),
            |path| panic!("a whole-side resolution should not GET {path}"),
        );

        match client.serve(Fetch::ResolveWholeFile {
            repo: "worktree-77".to_string(),
            path: "src/a.txt".to_string(),
            resolution: Resolution::TakeTheirs,
        }) {
            Data::Resolved {
                result: Ok(()),
                path,
                repo,
            } => {
                assert_eq!(path, "src/a.txt");
                assert_eq!(repo, "worktree-77");
            }
            other => panic!("expected a resolved answer, got {other:?}"),
        }

        let posted = posted.lock().unwrap();
        let paths: Vec<&str> = posted.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [SELECT_PATH, RESOLVE_CONFLICT_PATH],
            "the write did not select the repository it wrote to, first"
        );

        let select: serde_json::Value = serde_json::from_slice(&posted[0].1).unwrap();
        assert_eq!(select["worktree"], "worktree-77");
        assert_eq!(
            select["mode"], "active",
            "a write selected a mode that refuses writes"
        );
        assert!(
            posted[0].2.is_none(),
            "the select carried an idempotency key it does not need"
        );

        let write: serde_json::Value = serde_json::from_slice(&posted[1].1).unwrap();
        assert_eq!(write["path"], "src/a.txt");
        assert_eq!(write["resolution"]["choice"], "take_theirs");
        assert!(
            posted[1]
                .2
                .as_deref()
                .is_some_and(|key| key.starts_with("gvtui-")),
            "the planner write carried no idempotency key: {:?}",
            posted[1].2
        );
    }

    #[test]
    fn a_failed_select_fails_the_write_and_never_posts_the_resolution() {
        // If selecting is refused and the write goes out anyway, it goes out
        // against the previous selection.
        let (client, posted) = recording_client(
            |path| HttpResponse {
                status: if path == SELECT_PATH { 404 } else { 200 },
                headers: Vec::new(),
                body: b"No such repository.".to_vec(),
            },
            |path| panic!("unexpected GET {path}"),
        );

        match client.serve(Fetch::ResolveWholeFile {
            repo: "gone".to_string(),
            path: "a.txt".to_string(),
            resolution: Resolution::TakeOurs,
        }) {
            Data::Resolved {
                result: Err(message),
                ..
            } => {
                assert!(
                    message.contains("select the repository"),
                    "the failure did not say what went wrong: {message}"
                );
                assert!(message.contains("No such repository"), "{message}");
            }
            other => panic!("a refused select still produced {other:?}"),
        }
        let posted = posted.lock().unwrap();
        assert_eq!(
            posted.len(),
            1,
            "the resolution was posted after the select was refused"
        );
        assert_eq!(posted[0].0, SELECT_PATH);
    }

    #[test]
    fn a_content_resolution_posts_the_stages_and_token_it_was_handed() {
        // ADR 0069 gates 3 and 4 compare these against a fresh scan and a
        // re-minted token. Anything this client computed itself would agree
        // with itself by construction.
        let (client, posted) = recording_client(
            |_| ok_body("Resolved."),
            |path| panic!("unexpected GET {path}"),
        );
        client.serve(Fetch::ResolveContent {
            repo: "w1".to_string(),
            path: "dir/a b.txt".to_string(),
            expected_stages: [None, Some(oid_hex('a')), Some(oid_hex('b'))],
            expected_source: GenerationToken::new("conflict-v1:deadbeef").unwrap(),
            content: "resolved\n".to_string(),
        });
        let posted = posted.lock().unwrap();
        assert_eq!(posted[1].0, RESOLVE_CONFLICT_CONTENT_PATH);
        let body: serde_json::Value = serde_json::from_slice(&posted[1].1).unwrap();
        assert_eq!(body["path"], "dir/a b.txt");
        assert_eq!(body["content"], "resolved\n");
        assert_eq!(body["expected_source"], "conflict-v1:deadbeef");
        assert_eq!(body["expected_stages"][0], serde_json::Value::Null);
        assert_eq!(body["expected_stages"][1], oid_hex('a').as_str());
        assert_eq!(body["expected_stages"][2], oid_hex('b').as_str());
    }

    #[test]
    fn the_result_read_turns_a_404_into_no_file_and_anything_else_into_a_failure() {
        // "There is no file at this path" is what git leaves behind in a
        // delete/modify conflict — information, not a fault. Read through
        // `get_json` it would arrive as "content could not be loaded",
        // reporting a failure where nothing went wrong.
        //
        // MUTATION: use `get_json` here instead of `get_optional_json`. The
        // result pane then says a read failed for every conflict resolved
        // toward deletion.
        let (client, _) = recording_client(
            |_| panic!("no POST here"),
            |path| {
                let status = if path.contains("missing") { 404 } else { 500 };
                HttpResponse {
                    status,
                    headers: Vec::new(),
                    body: b"nope".to_vec(),
                }
            },
        );
        match client.serve(Fetch::ConflictResult {
            repo: "w1".to_string(),
            path: "missing.txt".to_string(),
        }) {
            Data::ConflictResult {
                read: ResultRead::NoFile,
                ..
            } => {}
            other => panic!("a 404 was not read as an absent file: {other:?}"),
        }
        match client.serve(Fetch::ConflictResult {
            repo: "w1".to_string(),
            path: "broken.txt".to_string(),
        }) {
            Data::ConflictResult {
                read: ResultRead::Failed(message),
                ..
            } => {
                assert!(message.contains("500"), "{message}");
            }
            other => panic!("a 500 was not read as a failure: {other:?}"),
        }
    }

    #[test]
    fn a_path_is_encoded_so_it_cannot_cut_the_request_short() {
        // Every one of these is a real filename, and every one of them ends
        // the request line or starts a query string if it goes through raw.
        assert_eq!(
            encode_path("src/a.txt"),
            "src/a.txt",
            "slashes are the route's"
        );
        assert_eq!(encode_path("a b.txt"), "a%20b.txt");
        assert_eq!(encode_path("what?.txt"), "what%3F.txt");
        assert_eq!(encode_path("a#b.txt"), "a%23b.txt");
        assert_eq!(
            encode_path("a%b.txt"),
            "a%25b.txt",
            "an existing % is escaped"
        );
        assert_eq!(
            encode_path("naïve.txt"),
            "na%C3%AFve.txt",
            "per byte, not per char"
        );
        assert_eq!(encode_path("keep-._~"), "keep-._~", "unreserved bytes stay");
    }

    #[test]
    fn two_presses_carry_two_different_idempotency_keys() {
        // A key names one user action. Deriving it from the request's content
        // would make a second, deliberate attempt at the same resolution look
        // like a retry of the first and replay its answer instead of running.
        let (client, posted) = recording_client(
            |_| ok_body("Resolved."),
            |path| panic!("unexpected GET {path}"),
        );
        let request = || Fetch::ResolveWholeFile {
            repo: "w1".to_string(),
            path: "a.txt".to_string(),
            resolution: Resolution::TakeOurs,
        };
        client.serve(request());
        client.serve(request());
        let posted = posted.lock().unwrap();
        let keys: Vec<&str> = posted
            .iter()
            .filter_map(|(path, _, key)| {
                (path == RESOLVE_CONFLICT_PATH)
                    .then_some(key.as_deref())
                    .flatten()
            })
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(
            keys[0], keys[1],
            "two identical presses shared a key, so the second would replay the first"
        );
    }

    #[test]
    fn a_stopped_worker_never_claims_a_file_is_absent() {
        // A request that never left the process observed nothing about what is
        // on disk. `NoFile` here would assert a fact nobody looked for.
        match stopped_answer(Fetch::ConflictResult {
            repo: "w1".to_string(),
            path: "a.txt".to_string(),
        }) {
            Data::ConflictResult {
                read: ResultRead::Failed(_),
                ..
            } => {}
            other => panic!("a dead worker reported {other:?} about a file it never read"),
        }
    }

    fn response(status: u16, body: impl AsRef<[u8]>) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.as_ref().to_vec(),
        }
    }

    fn approval() -> (Vec<u8>, PlanApproval) {
        let wire = format!(
            " {}\n",
            serde_json::to_string(&Plan {
                repository: RepositoryToken::new("repo-1").unwrap(),
                worktree: WorktreeToken::new("worktree-1").unwrap(),
                generation: GenerationToken::new("generation-reviewed").unwrap(),
                operation: GitOperation::StageAll,
                operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
                issued_at: UnixSeconds(1_788_365_000),
                expires_at: UnixSeconds(1_788_365_300),
                risk: RiskLevel::Safe,
                preconditions: Vec::new(),
                expected_ref_changes: Vec::new(),
                advisories: Vec::new(),
                recovery: RecoveryStrategy::NotNeeded,
            })
            .unwrap()
        )
        .into_bytes();
        let mut pane = PlanReviewPane::from_wire(wire.clone()).unwrap();
        let approval = pane.approve().unwrap();
        (wire, approval)
    }

    #[test]
    fn a_401_mid_session_re_authenticates_once_and_the_read_still_succeeds() {
        let auths = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auths);
        let fetch_count = Arc::clone(&fetches);
        let client = Client::with(
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
        let client = Client::with(
            Box::new(|path, _| match path {
                "/api/down" => Ok(response(503, "catalog rebuilding")),
                "/api/broken" => Ok(response(200, "<html>not json</html>")),
                _ => panic!("unexpected path {path}"),
            }),
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
        let client = Client::with(
            Box::new(|_, _| Ok(response(503, "catalog rebuilding"))),
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

    #[test]
    fn operation_lookup_is_served_while_the_approved_execution_is_still_running() {
        let (_, approval) = approval();
        let gate = Arc::new(Barrier::new(2));
        let execute_gate = Arc::clone(&gate);
        let client = Client::with_transport(
            Box::new(|path, _| {
                assert!(path.starts_with("/api/operations/by-key/"), "{path}");
                Ok(response(200, r#"{"id":"op_0123456789abcdef"}"#))
            }),
            Box::new(|_, _, _, _| panic!("lookup is not a POST")),
            Box::new(move |path, _, _, _, _| {
                assert_eq!(path, EXECUTE_PLAN_PATH);
                execute_gate.wait();
                Ok(response(200, "done"))
            }),
            Box::new(|| Ok(session(1))),
        );
        let mut worker = spawn(client);
        let key = approval.key().to_string();
        worker.request(Fetch::ExecutePlan(approval));
        worker.request(Fetch::OperationByKey { key: key.clone() });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while !found && Instant::now() < deadline {
            if let Some(Data::OperationByKey {
                key: answer,
                result,
            }) = worker.poll()
            {
                assert_eq!(answer, key);
                assert_eq!(result.unwrap().unwrap().as_str(), "op_0123456789abcdef");
                found = true;
            } else {
                thread::yield_now();
            }
        }
        assert!(found, "lookup was blocked behind the running execution");
        gate.wait();
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

    #[test]
    fn selection_and_planning_use_plain_posts_with_typed_exact_bodies() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&calls);
        let (plan_wire, _) = approval();
        let answer = plan_wire.clone();
        let client = Client::with_transport(
            Box::new(|_, _| panic!("selection and planning are not GETs")),
            Box::new(move |path, body, cookie, csrf| {
                captured
                    .lock()
                    .unwrap()
                    .push((path.to_string(), body.to_vec()));
                assert_eq!(cookie, "gv_session=gen1");
                assert_eq!(csrf, "csrf-gen1");
                match path {
                    "/api/select" => Ok(response(200, "selected")),
                    "/api/plan" => Ok(response(200, &answer)),
                    _ => panic!("unexpected POST {path}"),
                }
            }),
            Box::new(|_, _, _, _, _| panic!("build-only requests are not idempotent writes")),
            Box::new(|| Ok(session(1))),
        );

        assert!(matches!(
            client.serve(Fetch::Select { repo: "w1".into() }),
            Data::Selected { result: Ok(()), .. }
        ));
        assert!(matches!(
            client.serve(Fetch::BuildPlan(GitOperation::DeleteBranch {
                branch: BranchName::new("topic").unwrap(),
            })),
            Data::PlanReady(Ok(body)) if body == plan_wire
        ));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "/api/select");
        assert_eq!(
            serde_json::from_slice::<SelectRequest>(&calls[0].1).unwrap(),
            SelectRequest {
                worktree: "w1".into(),
                mode: RepoMode::Active,
            }
        );
        assert_eq!(calls[1].0, "/api/plan");
        assert_eq!(
            serde_json::from_slice::<GitOperation>(&calls[1].1).unwrap(),
            GitOperation::DeleteBranch {
                branch: BranchName::new("topic").unwrap(),
            }
        );
    }

    #[test]
    fn tag_listing_is_a_scoped_read_not_a_write_disguised_as_a_plan() {
        let client = Client::with(
            Box::new(|path, cookie| {
                assert_eq!(path, "/api/tags?repo=w1");
                assert_eq!(cookie, "gv_session=gen1");
                Ok(response(200, "[]"))
            }),
            Box::new(|| Ok(session(1))),
        );
        assert!(matches!(
            client.serve(Fetch::Tags { repo: "w1".into() }),
            Data::Tags { result: Ok(tags), .. } if tags.is_empty()
        ));
    }

    #[test]
    fn missing_operation_identity_is_pending_and_cancel_uses_the_typed_id_path() {
        let client = Client::with_transport(
            Box::new(|path, _| {
                assert!(path.starts_with("/api/operations/by-key/"));
                Ok(response(404, "not admitted yet"))
            }),
            Box::new(|path, body, cookie, csrf| {
                assert_eq!(path, "/api/operations/op_0123456789abcdef/cancel");
                assert_eq!(body, b"{}");
                assert_eq!(cookie, "gv_session=gen1");
                assert_eq!(csrf, "csrf-gen1");
                Ok(response(202, "Cancellation requested."))
            }),
            Box::new(|_, _, _, _, _| panic!("cancel is not execute-plan")),
            Box::new(|| Ok(session(1))),
        );

        assert!(matches!(
            client.serve(Fetch::OperationByKey {
                key: "intent_1".into()
            }),
            Data::OperationByKey {
                result: Ok(None),
                ..
            }
        ));
        let id = OperationId::new("op_0123456789abcdef").unwrap();
        assert!(matches!(
            client.serve(Fetch::CancelOperation { id }),
            Data::OperationCancelled { result: Ok(message), .. }
                if message == "Cancellation requested."
        ));
    }

    #[test]
    fn a_409_is_returned_as_stale_after_one_post_with_the_exact_reviewed_bytes() {
        let (wire, approval) = approval();
        let posts = Arc::new(AtomicUsize::new(0));
        let post_count = Arc::clone(&posts);
        let client = Client::with_transport(
            Box::new(|_, _| panic!("approval is not a GET")),
            Box::new(|_, _, _, _| panic!("approval is not a plain POST")),
            Box::new(move |path, body, cookie, csrf, key| {
                post_count.fetch_add(1, Ordering::SeqCst);
                assert_eq!(path, EXECUTE_PLAN_PATH);
                assert_eq!(body, wire);
                assert_eq!(cookie, "gv_session=gen1");
                assert_eq!(csrf, "csrf-gen1");
                assert!(key.starts_with("tui-"));
                Ok(response(
                    409,
                    "The repository changed while this plan was pending — refresh and try again.",
                ))
            }),
            Box::new(|| Ok(session(1))),
        );

        assert!(matches!(
            client.serve(Fetch::ExecutePlan(approval)),
            Data::PlanSubmitted(SubmissionOutcome::Stale)
        ));
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "a staleness refusal was silently retried"
        );
    }

    #[test]
    fn only_a_401_reauthenticates_and_it_reuses_the_body_and_idempotency_key() {
        let (wire, approval) = approval();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let auths = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auths);
        let client = Client::with_transport(
            Box::new(|_, _| panic!("approval is not a GET")),
            Box::new(|_, _, _, _| panic!("approval is not a plain POST")),
            Box::new(move |_, body, cookie, _csrf, key| {
                captured
                    .lock()
                    .unwrap()
                    .push((body.to_vec(), cookie.to_string(), key.to_string()));
                if cookie == "gv_session=gen1" {
                    Ok(response(401, "session ended"))
                } else {
                    Ok(response(200, "Staged all changes."))
                }
            }),
            Box::new(move || {
                let generation = auth_count.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(session(generation))
            }),
        );

        assert!(matches!(
            client.serve(Fetch::ExecutePlan(approval)),
            Data::PlanSubmitted(SubmissionOutcome::Executed(_))
        ));
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, wire);
        assert_eq!(calls[1].0, calls[0].0);
        assert_eq!(calls[0].1, "gv_session=gen1");
        assert_eq!(calls[1].1, "gv_session=gen2");
        assert_eq!(calls[1].2, calls[0].2, "retry changed its intent key");
        assert_eq!(auths.load(Ordering::SeqCst), 2);
    }
}
