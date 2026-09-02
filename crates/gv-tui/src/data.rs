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
use std::thread;

use git_vista_session::auth::{self, Session};
use git_vista_session::http::{self, HttpResponse};
use git_vista_session::retry;
use serde::de::DeserializeOwned;
use serde::Serialize;
use uuid::Uuid;

use crate::app::{Data, Request};

pub const CATALOG_PATH: &str = "/api/catalog";
pub const HISTORY_LIMIT: usize = 250;

pub type FetchFn = Box<dyn FnMut(&str, &str) -> Result<HttpResponse, String> + Send>;
pub type PostFn = Box<dyn FnMut(&str, &[u8], &str, &str) -> Result<HttpResponse, String> + Send>;
pub type IdempotentPostFn =
    Box<dyn FnMut(&str, &[u8], &str, &str, &str) -> Result<HttpResponse, String> + Send>;
pub type AuthFn = Box<dyn FnMut() -> Result<Session, String> + Send>;

pub struct Client {
    session: Option<Session>,
    request: FetchFn,
    post: PostFn,
    post_idempotent: IdempotentPostFn,
    auth: AuthFn,
}

impl Client {
    pub fn live() -> Client {
        Client::with_http(
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
    pub fn with(request: FetchFn, auth: AuthFn) -> Client {
        Client::with_http(
            request,
            Box::new(|path, body, cookie, csrf| {
                http::post_json(path, body, Some(cookie), Some(csrf))
            }),
            Box::new(|path, body, cookie, csrf, key| {
                http::post_json_idempotent(path, body, Some(cookie), Some(csrf), key)
            }),
            auth,
        )
    }

    pub fn with_http(
        request: FetchFn,
        post: PostFn,
        post_idempotent: IdempotentPostFn,
        auth: AuthFn,
    ) -> Client {
        Client {
            session: None,
            request,
            post,
            post_idempotent,
            auth,
        }
    }

    pub fn get_json<T: DeserializeOwned>(&mut self, path: &str) -> Result<T, String> {
        let body =
            retry::authed_fetch(path, &mut self.session, &mut *self.request, &mut *self.auth)?;
        serde_json::from_slice(&body)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
    }

    fn post_bytes<T: Serialize>(&mut self, path: &str, value: &T) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(value)
            .map_err(|error| format!("could not encode the request for {path}: {error}"))?;
        retry::authed_post(
            path,
            &body,
            &mut self.session,
            &mut *self.post,
            &mut *self.auth,
        )
    }

    fn post_json<T: Serialize, R: DeserializeOwned>(
        &mut self,
        path: &str,
        value: &T,
    ) -> Result<R, String> {
        let body = self.post_bytes(path, value)?;
        serde_json::from_slice(&body)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
    }

    fn post_idempotent<T: Serialize>(
        &mut self,
        path: &str,
        value: &T,
        key: &str,
    ) -> Result<String, String> {
        let body = serde_json::to_vec(value)
            .map_err(|error| format!("could not encode the request for {path}: {error}"))?;
        let post = &mut self.post_idempotent;
        let mut adapter =
            |path: &str, body: &[u8], cookie: &str, csrf: &str| post(path, body, cookie, csrf, key);
        let answer = retry::authed_post(
            path,
            &body,
            &mut self.session,
            &mut adapter,
            &mut *self.auth,
        )?;
        Ok(String::from_utf8_lossy(&answer).into_owned())
    }

    pub fn serve(&mut self, request: Request) -> Data {
        match request {
            Request::Catalog => Data::Catalog(self.get_json(CATALOG_PATH)),
            Request::Select { repo } => {
                let body = git_vista_protocol::SelectRequest {
                    worktree: repo.clone(),
                    mode: git_vista_protocol::RepoMode::Active,
                };
                let result = self.post_bytes("/api/select", &body).map(|_| ());
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
                result: self.post_json("/api/plan", &operation),
            },
            Request::PreviewPatch { repo, plan } => Data::PatchPreview {
                repo,
                result: self.post_json("/api/staging/preview", &plan),
                plan,
            },
            Request::ExecutePlan { repo, plan } => {
                let key = format!("tui-{}-{}", plan.operation_hash.as_str(), plan.issued_at.0);
                let result = self.post_idempotent("/api/execute-plan", &plan, &key);
                Data::Written { repo, result }
            }
            Request::ApplyPatch { repo, plan } => {
                let body =
                    serde_json::to_vec(&plan).expect("PatchPlan serialization is infallible");
                let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, &body);
                let key = format!("tui-patch-{id}");
                let result = self.post_idempotent("/api/staging/apply", &plan, &key);
                Data::Written { repo, result }
            }
        }
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

pub fn spawn(mut client: Client) -> Worker {
    let (request_tx, request_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    thread::Builder::new()
        .name(String::from("gv-tui-data"))
        .spawn(move || {
            for request in request_rx {
                if answer_tx.send(client.serve(request)).is_err() {
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
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use git_vista_protocol::{
        FileSelection, GenerationToken, GitOperation, OperationHash, PatchPlan, PatchPreview, Plan,
        Precondition, RecoveryStrategy, RepositoryDescriptor, RepositoryToken, RiskLevel,
        SelectionShape, StageDirection, StagingDiff, UnixSeconds, WorktreeToken,
    };

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
        let mut client = Client::with(
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
        let mut client = Client::with(
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
        let mut client = Client::with(
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
        let mut client = Client::with_http(
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
        let mut client = Client::with_http(
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
}
