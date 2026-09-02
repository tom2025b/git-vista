//! The shell's persistent authenticated data client and detached worker
//! (M10.02/#457, extended with #459's typed status/staging requests).
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
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use git_vista_protocol::{OperationByKeyResponse, RepoMode, SelectRequest};
use git_vista_session::auth::{self, Session};
use git_vista_session::http::{self, HttpResponse};
use serde::de::DeserializeOwned;
use serde::Serialize;
use uuid::Uuid;

use crate::app::{Data, Request};
use crate::panes::plan_review::{PlanApproval, SubmissionOutcome};

pub const CATALOG_PATH: &str = "/api/catalog";
pub const HISTORY_LIMIT: usize = 250;
pub const EXECUTE_PLAN_PATH: &str = "/api/execute-plan";
pub const PLAN_PATH: &str = "/api/plan";
pub const SELECT_PATH: &str = "/api/select";

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

    pub fn serve(&self, request: Request) -> Data {
        match request {
            Request::Catalog => Data::Catalog(self.get_json(CATALOG_PATH)),
            Request::Select { repo } => {
                let body = serde_json::to_vec(&SelectRequest {
                    worktree: repo.clone(),
                    mode: RepoMode::Active,
                })
                .map_err(|error| error.to_string());
                let result = body.and_then(|body| self.post_json(SELECT_PATH, &body).map(|_| ()));
                Data::Selected { repo, result }
            }
            Request::History { repo } => {
                let path = format!("/api/commits?repo={repo}&limit={HISTORY_LIMIT}");
                let result = self.get_json(&path);
                Data::History { repo, result }
            }
            Request::Commit { repo, id } => {
                let path = format!("/api/commit/{id}?repo={repo}");
                let result = self.get_json(&path);
                Data::Commit { repo, id, result }
            }
            Request::Diff { repo, id } => {
                // Deliberately no `full=1`: the terminal consumes the server's
                // bounded panel representation and windows it again on draw.
                let path = format!("/api/diff/{id}?repo={repo}");
                let result = self.get_json(&path);
                Data::Diff { repo, id, result }
            }
            Request::Status { repo } => {
                let path = format!("/api/status/v2?repo={repo}");
                let result = self.get_json(&path);
                Data::Status { repo, result }
            }
            Request::StagingDiff { repo, direction } => {
                let direction_query = match direction {
                    git_vista_protocol::StageDirection::Stage => "stage",
                    git_vista_protocol::StageDirection::Unstage => "unstage",
                };
                let path = format!("/api/staging/diff?direction={direction_query}");
                let result = self.get_json(&path);
                Data::StagingDiff {
                    repo,
                    direction,
                    result,
                }
            }
            Request::BuildPlan { repo, operation } => Data::Plan {
                repo,
                result: self.post_value(PLAN_PATH, &operation),
            },
            Request::BuildPlanWire(operation) => {
                let body = serde_json::to_vec(&operation).map_err(|error| error.to_string());
                Data::PlanReady(body.and_then(|body| self.post_json(PLAN_PATH, &body)))
            }
            Request::PreviewPatch { repo, plan } => Data::PatchPreview {
                repo,
                result: self.post_value("/api/staging/preview", &plan),
                plan,
            },
            Request::ExecutePlan { repo, plan } => {
                let key = format!("tui-{}-{}", plan.operation_hash.as_str(), plan.issued_at.0);
                let result = self.post_idempotent_value(EXECUTE_PLAN_PATH, &plan, &key);
                Data::Written { repo, result }
            }
            Request::ExecuteReviewedPlan(approval) => {
                Data::PlanSubmitted(self.submit_plan(&approval))
            }
            Request::ApplyPatch { repo, plan } => {
                let body =
                    serde_json::to_vec(&plan).expect("PatchPlan serialization is infallible");
                let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, &body);
                let key = format!("tui-patch-{id}");
                let result = self.post_idempotent_value("/api/staging/apply", &plan, &key);
                Data::Written { repo, result }
            }
            Request::Tags { repo } => {
                let path = format!("/api/tags?repo={repo}");
                let result = self.get_json(&path);
                Data::Tags { repo, result }
            }
            Request::OperationByKey { key } => {
                let path = format!("/api/operations/by-key/{key}");
                let result = self.get_optional_json::<OperationByKeyResponse>(&path);
                Data::OperationByKey {
                    key,
                    result: result.map(|found| found.map(|response| response.id)),
                }
            }
            Request::OperationStatus { id } => {
                let path = format!("/api/operations/{}", id.as_str());
                Data::OperationStatus(self.get_json(&path))
            }
            Request::CancelOperation { id } => {
                let path = format!("/api/operations/{}/cancel", id.as_str());
                let result = self
                    .post_json(&path, b"{}")
                    .map(|body| String::from_utf8_lossy(&body).trim().to_string());
                Data::OperationCancelled { id, result }
            }
        }
    }

    fn post_value<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        value: &T,
    ) -> Result<R, String> {
        let body = serde_json::to_vec(value)
            .map_err(|error| format!("could not encode the request for {path}: {error}"))?;
        let answer = self.post_json(path, &body)?;
        serde_json::from_slice(&answer)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
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

    fn post_idempotent_value<T: Serialize>(
        &self,
        path: &str,
        value: &T,
        key: &str,
    ) -> Result<String, String> {
        let body = serde_json::to_vec(value)
            .map_err(|error| format!("could not encode the request for {path}: {error}"))?;
        let session = self.authenticated()?;
        let post = |session: &Session| {
            (self.idempotent_post)(path, &body, &session.cookie, &session.csrf, key)
        };
        let response = post(&session)?;
        let response = if response.status == 401 {
            post(&self.reauthenticate_after(&session)?)?
        } else {
            response
        };
        if (200..=299).contains(&response.status) {
            Ok(String::from_utf8_lossy(&response.body).into_owned())
        } else {
            Err(format!(
                "POST {path} answered {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ))
        }
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

pub trait DataPort {
    fn request(&mut self, request: Request);
    fn poll(&mut self) -> Option<Data>;
}

pub struct Worker {
    requests: Sender<Request>,
    answers: Receiver<Data>,
    pending: VecDeque<Data>,
}

pub fn spawn(client: Client) -> Worker {
    let (request_tx, request_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    thread::Builder::new()
        .name(String::from("gv-tui-data"))
        .spawn(move || {
            for request in request_rx {
                match request {
                    Request::ExecuteReviewedPlan(approval) => {
                        let execute_client = client.clone();
                        let execute_answers = answer_tx.clone();
                        let started = thread::Builder::new()
                            .name(String::from("gv-tui-execute"))
                            .spawn(move || {
                                let answer =
                                    execute_client.serve(Request::ExecuteReviewedPlan(approval));
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
                    }
                    request => {
                        if answer_tx.send(client.serve(request)).is_err() {
                            break;
                        }
                    }
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
    fn request(&mut self, request: Request) {
        if let Err(stopped) = self.requests.send(request) {
            self.pending.push_back(stopped_answer(stopped.0));
        }
    }

    fn poll(&mut self) -> Option<Data> {
        self.pending
            .pop_front()
            .or_else(|| self.answers.try_recv().ok())
    }
}

fn stopped_answer(request: Request) -> Data {
    let message = || String::from("the data thread has stopped; restart gv-tui");
    match request {
        Request::Catalog => Data::Catalog(Err(message())),
        Request::Select { repo } => Data::Selected {
            repo,
            result: Err(message()),
        },
        Request::History { repo } => Data::History {
            repo,
            result: Err(message()),
        },
        Request::Commit { repo, id } => Data::Commit {
            repo,
            id,
            result: Err(message()),
        },
        Request::Diff { repo, id } => Data::Diff {
            repo,
            id,
            result: Err(message()),
        },
        Request::Status { repo } => Data::Status {
            repo,
            result: Err(message()),
        },
        Request::StagingDiff { repo, direction } => Data::StagingDiff {
            repo,
            direction,
            result: Err(message()),
        },
        Request::BuildPlan { repo, .. } => Data::Plan {
            repo,
            result: Err(message()),
        },
        Request::PreviewPatch { repo, plan } => Data::PatchPreview {
            repo,
            plan,
            result: Err(message()),
        },
        Request::ExecutePlan { repo, .. } | Request::ApplyPatch { repo, .. } => Data::Written {
            repo,
            result: Err(message()),
        },
        Request::ExecuteReviewedPlan(_) => {
            Data::PlanSubmitted(SubmissionOutcome::TransportFailed(message()))
        }
        Request::BuildPlanWire(_) => Data::PlanReady(Err(message())),
        Request::Tags { repo } => Data::Tags {
            repo,
            result: Err(message()),
        },
        Request::OperationByKey { key } => Data::OperationByKey {
            key,
            result: Err(message()),
        },
        Request::OperationStatus { .. } => Data::OperationStatus(Err(message())),
        Request::CancelOperation { id } => Data::OperationCancelled {
            id,
            result: Err(message()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use git_vista_protocol::{
        BranchName, FileSelection, GenerationToken, GitOperation, OperationHash, OperationId,
        PatchPlan, PatchPreview, Plan, Precondition, RecoveryStrategy, RepositoryDescriptor,
        RepositoryToken, RiskLevel, SelectionShape, StageDirection, StagingDiff, UnixSeconds,
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

    fn response(status: u16, body: impl AsRef<[u8]>) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.as_ref().to_vec(),
        }
    }

    fn plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("r1").unwrap(),
            worktree: WorktreeToken::new("w1").unwrap(),
            generation: GenerationToken::new("status-v1:test").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(100),
            expires_at: UnixSeconds(200),
            risk: RiskLevel::Safe,
            preconditions: vec![Precondition::CleanWorktree],
            expected_ref_changes: Vec::new(),
            advisories: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
        }
    }

    fn reviewed_approval() -> (Vec<u8>, PlanApproval) {
        let wire = format!(" {}\n", serde_json::to_string(&plan()).unwrap()).into_bytes();
        let mut pane = PlanReviewPane::from_wire(wire.clone()).unwrap();
        (wire, pane.approve().unwrap())
    }

    fn patch_plan() -> PatchPlan {
        PatchPlan {
            repository: RepositoryToken::new("r1").unwrap(),
            worktree: WorktreeToken::new("w1").unwrap(),
            generation: GenerationToken::new("diff-v1:test").unwrap(),
            direction: StageDirection::Stage,
            files: vec![FileSelection {
                path: "a.txt".to_string(),
                selection: SelectionShape::EntireFile,
            }],
        }
    }

    #[test]
    fn a_401_mid_session_re_authenticates_once_and_the_read_still_succeeds() {
        let auths = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auths);
        let fetch_count = Arc::clone(&requests);
        let client = Client::with(
            Box::new(move |path, cookie| {
                assert_eq!(path, CATALOG_PATH);
                let call = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                match (call, cookie) {
                    (1, "gv_session=gen1") => Ok(response(200, ALPHA)),
                    (2, "gv_session=gen1") => Ok(response(401, "expired")),
                    (3 | 4, "gv_session=gen2") => Ok(response(200, BETA)),
                    _ => panic!("unexpected request {call} with {cookie}"),
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
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        let second: Vec<RepositoryDescriptor> = client.get_json(CATALOG_PATH).unwrap();
        assert_eq!(second[0].name, "beta");
        assert_eq!(auths.load(Ordering::SeqCst), 2);
        assert_eq!(requests.load(Ordering::SeqCst), 3);

        let third: Vec<RepositoryDescriptor> = client.get_json(CATALOG_PATH).unwrap();
        assert_eq!(third[0].name, "beta");
        assert_eq!(auths.load(Ordering::SeqCst), 2, "the fresh session is kept");
        assert_eq!(requests.load(Ordering::SeqCst), 4);
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
        match client.serve(Request::Catalog) {
            Data::Catalog(Err(message)) => {
                assert!(message.contains(CATALOG_PATH), "{message}");
                assert!(message.contains("503"), "{message}");
            }
            Data::Catalog(Ok(_)) => panic!("a 503 became catalog rows"),
            _ => panic!("a catalog request became a different answer kind"),
        }
    }

    /// INVARIANT: repository activation and every status/staging read use the
    /// shared authenticated server endpoints; the terminal performs no local
    /// repository operation to answer them.
    ///
    /// MUTATION 1 (remove): skip `/api/select` before scoped reads.
    /// MUTATION 2 (weaken): route status or staging diff through a legacy path.
    #[test]
    fn serve_uses_active_selection_and_shared_status_and_staging_diff_routes() {
        let seen_gets = Arc::new(Mutex::new(Vec::new()));
        let seen_posts = Arc::new(Mutex::new(Vec::new()));
        let get_log = Arc::clone(&seen_gets);
        let post_log = Arc::clone(&seen_posts);
        let diff = StagingDiff {
            generation: GenerationToken::new("diff-v1:test").unwrap(),
            patch: "diff --git a/a.txt b/a.txt\n".to_string(),
            truncated: false,
        };
        let diff_wire = serde_json::to_vec(&diff).unwrap();
        let status_wire = br#"{
          "generation":"status-v1:test","branch":"main","upstream":null,
          "ahead":0,"behind":0,"entries":[]
        }"#
        .to_vec();
        let client = Client::with_transport(
            Box::new(move |path, cookie| {
                get_log
                    .lock()
                    .unwrap()
                    .push((path.to_string(), cookie.to_string()));
                match path {
                    "/api/status/v2?repo=w1" => Ok(response(200, &status_wire)),
                    "/api/staging/diff?direction=stage" => Ok(response(200, &diff_wire)),
                    _ => panic!("unexpected GET {path}"),
                }
            }),
            Box::new(move |path, body, cookie, csrf| {
                post_log.lock().unwrap().push((
                    path.to_string(),
                    body.to_vec(),
                    cookie.to_string(),
                    csrf.to_string(),
                ));
                Ok(response(200, ""))
            }),
            Box::new(|path, _, _, _, _| panic!("unexpected idempotent POST {path}")),
            Box::new(|| Ok(session(1))),
        );

        assert!(matches!(
            client.serve(Request::Select {
                repo: "w1".to_string()
            }),
            Data::Selected { result: Ok(()), .. }
        ));
        assert!(matches!(
            client.serve(Request::Status {
                repo: "w1".to_string()
            }),
            Data::Status { result: Ok(_), .. }
        ));
        assert!(matches!(
            client.serve(Request::StagingDiff {
                repo: "w1".to_string(),
                direction: StageDirection::Stage,
            }),
            Data::StagingDiff { result: Ok(_), .. }
        ));

        let posts = seen_posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "/api/select");
        assert_eq!(posts[0].2, "gv_session=gen1");
        assert_eq!(posts[0].3, "csrf-gen1");
        let selection: git_vista_protocol::SelectRequest =
            serde_json::from_slice(&posts[0].1).unwrap();
        assert_eq!(selection.worktree, "w1");
        assert_eq!(selection.mode, git_vista_protocol::RepoMode::Active);
        assert_eq!(
            *seen_gets.lock().unwrap(),
            [
                (
                    "/api/status/v2?repo=w1".to_string(),
                    "gv_session=gen1".to_string()
                ),
                (
                    "/api/staging/diff?direction=stage".to_string(),
                    "gv_session=gen1".to_string()
                )
            ]
        );
    }

    /// INVARIANT: the only write transports carry the exact shared Plan or
    /// PatchPlan body to the shared endpoints with deterministic idempotency.
    ///
    /// MUTATION 1 (remove): send an operation or ad-hoc argv instead of Plan.
    /// MUTATION 2 (weaken): drop idempotency or reuse one key for both plans.
    #[test]
    fn serve_previews_and_executes_exact_typed_plans_with_distinct_idempotency() {
        let ordinary_posts = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let post_log = Arc::clone(&ordinary_posts);
        let write_log = Arc::clone(&writes);
        let server_plan = plan();
        let plan_wire = serde_json::to_vec(&server_plan).unwrap();
        let submitted_patch = patch_plan();
        let preview = PatchPreview {
            generation: submitted_patch.generation.clone(),
            patch: String::new(),
            whole_files: vec!["a.txt".to_string()],
        };
        let preview_wire = serde_json::to_vec(&preview).unwrap();
        let client = Client::with_transport(
            Box::new(|path, _| panic!("unexpected GET {path}")),
            Box::new(move |path, body, cookie, csrf| {
                post_log.lock().unwrap().push((
                    path.to_string(),
                    body.to_vec(),
                    cookie.to_string(),
                    csrf.to_string(),
                ));
                match path {
                    "/api/plan" => Ok(response(200, &plan_wire)),
                    "/api/staging/preview" => Ok(response(200, &preview_wire)),
                    _ => panic!("unexpected ordinary POST {path}"),
                }
            }),
            Box::new(move |path, body, cookie, csrf, key| {
                write_log.lock().unwrap().push((
                    path.to_string(),
                    body.to_vec(),
                    cookie.to_string(),
                    csrf.to_string(),
                    key.to_string(),
                ));
                Ok(response(200, "written"))
            }),
            Box::new(|| Ok(session(1))),
        );

        let built = match client.serve(Request::BuildPlan {
            repo: "w1".to_string(),
            operation: GitOperation::StageAll,
        }) {
            Data::Plan {
                result: Ok(plan), ..
            } => plan,
            other => panic!("plan endpoint returned {other:?}"),
        };
        assert_eq!(built, server_plan);
        let previewed = match client.serve(Request::PreviewPatch {
            repo: "w1".to_string(),
            plan: submitted_patch.clone(),
        }) {
            Data::PatchPreview {
                plan,
                result: Ok(preview),
                ..
            } => (plan, preview),
            other => panic!("preview endpoint returned {other:?}"),
        };
        assert_eq!(previewed, (submitted_patch.clone(), preview));
        assert!(matches!(
            client.serve(Request::ExecutePlan {
                repo: "w1".to_string(),
                plan: Box::new(server_plan.clone()),
            }),
            Data::Written { result: Ok(_), .. }
        ));
        assert!(matches!(
            client.serve(Request::ApplyPatch {
                repo: "w1".to_string(),
                plan: submitted_patch.clone(),
            }),
            Data::Written { result: Ok(_), .. }
        ));

        let posts = ordinary_posts.lock().unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].0, "/api/plan");
        assert_eq!(
            serde_json::from_slice::<GitOperation>(&posts[0].1).unwrap(),
            GitOperation::StageAll
        );
        assert_eq!(posts[1].0, "/api/staging/preview");
        assert_eq!(
            serde_json::from_slice::<PatchPlan>(&posts[1].1).unwrap(),
            submitted_patch
        );

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, "/api/execute-plan");
        assert_eq!(
            serde_json::from_slice::<Plan>(&writes[0].1).unwrap(),
            server_plan
        );
        assert_eq!(writes[0].2, "gv_session=gen1");
        assert_eq!(writes[0].3, "csrf-gen1");
        assert_eq!(writes[0].4, format!("tui-{}-100", "a".repeat(64)));
        assert_eq!(writes[1].0, "/api/staging/apply");
        assert_eq!(
            serde_json::from_slice::<PatchPlan>(&writes[1].1).unwrap(),
            submitted_patch
        );
        assert!(writes[1].4.starts_with("tui-patch-"));
        assert_ne!(writes[0].4, writes[1].4);
    }

    #[test]
    fn the_worker_answers_every_request_in_order_without_blocking_the_caller() {
        let auths = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auths);
        let fetch_count = Arc::clone(&requests);
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
                    _ => panic!("unexpected request {call} with {cookie}"),
                }
            }),
            Box::new(move || {
                let generation = auth_count.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(session(generation))
            }),
        );
        let mut worker = spawn(client);

        let sent_at = Instant::now();
        worker.request(Request::Catalog);
        worker.request(Request::Catalog);
        assert!(
            sent_at.elapsed() < Duration::from_millis(100),
            "request waited for the deliberately slow request"
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
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert_eq!(auths.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn operation_lookup_is_served_while_the_approved_execution_is_still_running() {
        let (_, approval) = reviewed_approval();
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
        worker.request(Request::ExecuteReviewedPlan(approval));
        worker.request(Request::OperationByKey { key: key.clone() });

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

        worker.request(Request::Catalog);
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
        let (plan_wire, _) = reviewed_approval();
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
            client.serve(Request::Select { repo: "w1".into() }),
            Data::Selected { result: Ok(()), .. }
        ));
        assert!(matches!(
            client.serve(Request::BuildPlanWire(GitOperation::DeleteBranch {
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
            client.serve(Request::Tags { repo: "w1".into() }),
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
            client.serve(Request::OperationByKey {
                key: "intent_1".into()
            }),
            Data::OperationByKey {
                result: Ok(None),
                ..
            }
        ));
        let id = OperationId::new("op_0123456789abcdef").unwrap();
        assert!(matches!(
            client.serve(Request::CancelOperation { id }),
            Data::OperationCancelled { result: Ok(message), .. }
                if message == "Cancellation requested."
        ));
    }

    #[test]
    fn a_409_is_returned_as_stale_after_one_post_with_the_exact_reviewed_bytes() {
        let (wire, approval) = reviewed_approval();
        let expected = wire.clone();
        let posts = Arc::new(AtomicUsize::new(0));
        let post_count = Arc::clone(&posts);
        let client = Client::with_transport(
            Box::new(|_, _| panic!("approval is not a GET")),
            Box::new(|_, _, _, _| panic!("approval is not a plain POST")),
            Box::new(move |path, body, cookie, csrf, key| {
                post_count.fetch_add(1, Ordering::SeqCst);
                assert_eq!(path, EXECUTE_PLAN_PATH);
                assert_eq!(body, expected);
                assert_eq!(cookie, "gv_session=gen1");
                assert_eq!(csrf, "csrf-gen1");
                assert!(key.starts_with("tui-"));
                Ok(response(409, "some untyped conflict prose"))
            }),
            Box::new(|| Ok(session(1))),
        );

        assert!(matches!(
            client.serve(Request::ExecuteReviewedPlan(approval)),
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
        let (wire, approval) = reviewed_approval();
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
            client.serve(Request::ExecuteReviewedPlan(approval)),
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
