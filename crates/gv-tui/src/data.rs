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
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use git_vista_protocol::{RepoMode, SelectRequest};
use git_vista_session::auth::{self, Session};
use git_vista_session::http::{self, HttpResponse};
use git_vista_session::retry;
use serde::de::DeserializeOwned;

use crate::app::{Data, Fetch};
use crate::panes::plan_review::{PlanApproval, SubmissionOutcome};

pub const CATALOG_PATH: &str = "/api/catalog";
pub const HISTORY_LIMIT: usize = 250;
pub const EXECUTE_PLAN_PATH: &str = "/api/execute-plan";
pub const PLAN_PATH: &str = "/api/plan";
pub const SELECT_PATH: &str = "/api/select";

pub type FetchFn = Box<dyn FnMut(&str, &str) -> Result<HttpResponse, String> + Send>;
pub type PostFn = Box<dyn FnMut(&str, &[u8], &str, &str) -> Result<HttpResponse, String> + Send>;
pub type IdempotentPostFn =
    Box<dyn FnMut(&str, &[u8], &str, &str, &str) -> Result<HttpResponse, String> + Send>;
pub type AuthFn = Box<dyn FnMut() -> Result<Session, String> + Send>;

pub struct Client {
    session: Option<Session>,
    fetch: FetchFn,
    post: PostFn,
    idempotent_post: IdempotentPostFn,
    auth: AuthFn,
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
            session: None,
            fetch,
            post,
            idempotent_post,
            auth,
        }
    }

    pub fn get_json<T: DeserializeOwned>(&mut self, path: &str) -> Result<T, String> {
        let body = retry::authed_fetch(path, &mut self.session, &mut *self.fetch, &mut *self.auth)?;
        serde_json::from_slice(&body)
            .map_err(|error| format!("{path} did not return valid JSON: {error}"))
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
            Fetch::Select { repo } => {
                let body = serde_json::to_vec(&SelectRequest {
                    worktree: repo.clone(),
                    mode: RepoMode::Active,
                })
                .map_err(|error| error.to_string());
                let result = body.and_then(|body| self.post_json(SELECT_PATH, &body).map(|_| ()));
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
            Fetch::ExecutePlan(approval) => Data::PlanSubmitted(self.submit_plan(&approval)),
        }
    }

    fn post_json(&mut self, path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        if self.session.is_none() {
            self.session = Some((self.auth)()?);
        }
        let response = self.post_once(path, body)?;
        let response = if response.status == 401 {
            self.session = Some((self.auth)()?);
            self.post_once(path, body)?
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

    fn post_once(&mut self, path: &str, body: &[u8]) -> Result<HttpResponse, String> {
        let session = self.session.as_ref().expect("authenticated above");
        (self.post)(path, body, &session.cookie, &session.csrf)
    }

    /// Submit exactly one reviewed plan. Only a 401 earns a retry, and that
    /// retry reuses the same body and idempotency key. A 409 is an operation
    /// refusal, never a prompt to rebuild or resubmit behind the user's back.
    fn submit_plan(&mut self, approval: &PlanApproval) -> SubmissionOutcome {
        if self.session.is_none() {
            match (self.auth)() {
                Ok(session) => self.session = Some(session),
                Err(message) => return SubmissionOutcome::TransportFailed(message),
            }
        }

        let response = self.post_approval(approval);
        let response = match response {
            Ok(response) if response.status == 401 => {
                match (self.auth)() {
                    Ok(session) => self.session = Some(session),
                    Err(message) => return SubmissionOutcome::TransportFailed(message),
                }
                self.post_approval(approval)
            }
            other => other,
        };

        match response {
            Ok(response) => SubmissionOutcome::from_response(response.status, &response.body),
            Err(message) => SubmissionOutcome::TransportFailed(message),
        }
    }

    fn post_approval(&mut self, approval: &PlanApproval) -> Result<HttpResponse, String> {
        let session = self.session.as_ref().expect("authenticated above");
        (self.idempotent_post)(
            EXECUTE_PLAN_PATH,
            approval.body(),
            &session.cookie,
            &session.csrf,
            approval.key(),
        )
    }
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
        Fetch::Select { repo } => Data::Selected {
            repo,
            result: Err(message()),
        },
        Fetch::BuildPlan(_) => Data::PlanReady(Err(message())),
        Fetch::Tags { repo } => Data::Tags {
            repo,
            result: Err(message()),
        },
        Fetch::ExecutePlan(_) => Data::PlanSubmitted(SubmissionOutcome::TransportFailed(message())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    use git_vista_protocol::{
        GenerationToken, GitOperation, OperationHash, Plan, RecoveryStrategy, RepositoryDescriptor,
        RepositoryToken, RiskLevel, UnixSeconds, WorktreeToken,
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
        let mut client = Client::with_transport(
            Box::new(|_, _| panic!("selection and planning are not GETs")),
            Box::new(move |path, body, cookie, csrf| {
                captured
                    .lock()
                    .unwrap()
                    .push((path.to_string(), body.to_vec()));
                assert_eq!(cookie, "gv_session=gen1");
                assert_eq!(csrf, "csrf-gen1");
                match path {
                    SELECT_PATH => Ok(response(200, "selected")),
                    PLAN_PATH => Ok(response(200, &answer)),
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
            client.serve(Fetch::BuildPlan(GitOperation::StageAll)),
            Data::PlanReady(Ok(body)) if body == plan_wire
        ));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, SELECT_PATH);
        assert_eq!(
            serde_json::from_slice::<SelectRequest>(&calls[0].1).unwrap(),
            SelectRequest {
                worktree: "w1".into(),
                mode: RepoMode::Active,
            }
        );
        assert_eq!(calls[1].0, PLAN_PATH);
        assert_eq!(
            serde_json::from_slice::<GitOperation>(&calls[1].1).unwrap(),
            GitOperation::StageAll
        );
    }

    #[test]
    fn tag_listing_is_a_scoped_read_not_a_write_disguised_as_a_plan() {
        let mut client = Client::with(
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
    fn a_409_is_returned_as_stale_after_one_post_with_the_exact_reviewed_bytes() {
        let (wire, approval) = approval();
        let posts = Arc::new(AtomicUsize::new(0));
        let post_count = Arc::clone(&posts);
        let mut client = Client::with_transport(
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
        let mut client = Client::with_transport(
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
