//! Layers 2, 2b and 3 (#144): no write DTO tolerates smuggled extras or
//! wrong shapes (serde level), the clone-URL gate refuses every hostile
//! transport/option-smuggling shape, and the same refusals are observed on
//! the wire through the real session/CSRF middleware. Split out of
//! `argv_boundary.rs` as its own seam — argv/DTO *validation* proofs,
//! distinct from the allowlist's proof about *which files* may spawn.
//!
//! **This file is scanned too, and is not exempt.** The parent's spawn-site
//! scan walks every `.rs` file under `src/`, including this one, and its
//! by-name exemption from the literal-`git` check names only
//! `src/argv_boundary.rs` — not this path. Nothing here constructs a
//! `Command`, and it should stay that way; never spell the bare pattern
//! (`Command` immediately followed by `::new(`) in a comment here even in
//! passing, or a prose mention reads as a new, unreviewed spawn site.

/// Layer 2: no write DTO tolerates smuggled extras or non-object shapes. The
/// interesting property is *where* these die: at deserialization, before any
/// handler code runs.
#[test]
fn write_dtos_reject_smuggled_args_and_wrong_shapes() {
    use git_vista_protocol::{
        BranchName, BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest,
        CreateTagRequest, DeleteCloneRequest, DeleteTagRequest, SelectRequest, TagName,
    };

    // An extra freeform-args field beside legitimate fields: refused.
    for (what, err) in [
        (
            "branch+args",
            serde_json::from_str::<CreateBranchRequest>(
                r#"{"name":"x","commit":"HEAD","args":["--force"]}"#,
            )
            .err(),
        ),
        (
            "commit+argv",
            serde_json::from_str::<CreateCommitRequest>(
                r#"{"message":"m","allow_empty":false,"argv":["push","--mirror"]}"#,
            )
            .err(),
        ),
        (
            "branch-op+flags",
            serde_json::from_str::<BranchRequest>(r#"{"branch":"b","flags":"--force"}"#).err(),
        ),
        (
            "clone+command",
            serde_json::from_str::<CloneRequest>(
                r#"{"url":"https://x.example/r","command":"rm -rf /"}"#,
            )
            .err(),
        ),
        (
            "select+path",
            serde_json::from_str::<SelectRequest>(
                r#"{"worktree":"w","mode":"active","path":"/etc"}"#,
            )
            .err(),
        ),
        (
            "delete-clone+recursive",
            serde_json::from_str::<DeleteCloneRequest>(r#"{"worktree":"w","recursive":true}"#)
                .err(),
        ),
        // M2.21d (#238): `-f` is the field a tag-create body would most like
        // to smuggle — it turns "create this tag" into "silently repoint an
        // existing one", past the plan's own `RefAbsent` precondition.
        (
            "tag+force",
            serde_json::from_str::<CreateTagRequest>(
                r#"{"name":"v1","commit":"HEAD","force":true}"#,
            )
            .err(),
        ),
        (
            "delete-tag+remote",
            serde_json::from_str::<DeleteTagRequest>(r#"{"tag":"v1","remote":"origin"}"#).err(),
        ),
    ] {
        assert!(err.is_some(), "{what}: unknown field was accepted");
    }

    // A raw argv array where an object is expected: refused.
    //
    // **What this does and does not say.** serde_json can also fill a struct
    // *positionally* from a JSON array, and a body whose element count and
    // types happen to line up with the fields does deserialize — the two
    // arrays above are refused on arity, not on being arrays. That affordance
    // is checked, deliberately and separately, in
    // [`a_positional_array_body_is_the_object_body_and_smuggles_nothing`]:
    // the point of this whole module is that no client string becomes an
    // argv element it was not declared to be, and a positional array cannot
    // reach a field the object form does not already expose, nor skip the
    // validation that field carries. Asserting "arrays are refused" as if it
    // were universal would have been a comfortable falsehood.
    assert!(serde_json::from_str::<CreateBranchRequest>(r#"["git","push","--force"]"#).is_err());
    assert!(serde_json::from_str::<BranchRequest>(r#"["--delete","main"]"#).is_err());

    // An undo body that names no known variant (a smuggled exec request) is
    // refused by the closed `UndoAction` enum.
    assert!(
        serde_json::from_str::<git_vista_core::activity::UndoAction>(r#"{"exec":"rm -rf /"}"#)
            .is_err()
    );

    // Option-shaped and empty ref names die in the typed `BranchName` gate —
    // the same gate the handlers apply before anything reaches the planner.
    assert!(BranchName::new("-force").is_err());
    assert!(BranchName::new("--exec=/bin/sh").is_err());
    assert!(BranchName::new("").is_err());
    // Same gate on the tag namespace (M2.21d, #238): `git tag -d <name>` puts
    // the name straight after a flag, so an option-shaped name is exactly the
    // shape that would turn a delete into something else.
    assert!(TagName::new("-d").is_err());
    assert!(TagName::new("--points-at=HEAD").is_err());
    assert!(TagName::new("").is_err());
}

/// The serde_json affordance the assertion above is careful *not* to claim
/// away: a write DTO can be filled positionally from a JSON array, and that is
/// harmless here — but only for a reason worth writing down and testing,
/// because "the body was an array" reads like an attack and is not one.
///
/// A positional array is fixed by the struct's own field order. It can name no
/// field the object form does not have, add none, reorder none, and skip none
/// of the validation each field carries downstream. So the two forms are the
/// *same request*, which is exactly what is asserted: array and object
/// deserialize to equal values, and the smuggling that would matter — an extra
/// key — is still refused in the object form (an array cannot express one at
/// all, since it has no keys).
///
/// Found while wiring M2.21d (#238): `["tag","-d","v1"]` deserializes into
/// [`CreateTagRequest`] as `name: "tag", commit: "-d", message: Some("v1")`,
/// with `sign` defaulted. It then dies at `resolve_commit_oid` ("-d" is not an
/// object), which is the ordinary path any bad `commit` takes.
#[test]
fn a_positional_array_body_is_the_object_body_and_smuggles_nothing() {
    use git_vista_protocol::{CreateTagRequest, DeleteTagRequest};

    let positional: CreateTagRequest = serde_json::from_str(r#"["v1","HEAD","notes",false]"#)
        .expect("serde_json fills a struct positionally from an array");
    let keyed: CreateTagRequest =
        serde_json::from_str(r#"{"name":"v1","commit":"HEAD","message":"notes","sign":false}"#)
            .unwrap();
    assert_eq!(
        positional, keyed,
        "the positional form must be the very same request, field for field"
    );
    assert_eq!(
        positional.name, "v1",
        "position 0 is `name` — an array cannot choose which field it fills"
    );

    // The one shape that would actually smuggle something is an extra key,
    // and that has no positional spelling: there are four fields, so a fifth
    // element is an arity error, and a key is only expressible in the object
    // form, where `deny_unknown_fields` refuses it.
    assert!(
        serde_json::from_str::<CreateTagRequest>(r#"["v1","HEAD","notes",false,"--force"]"#)
            .is_err(),
        "an array longer than the struct has fields is refused"
    );
    assert!(
        serde_json::from_str::<CreateTagRequest>(r#"{"name":"v1","commit":"HEAD","force":true}"#)
            .is_err(),
        "and the keyed spelling of the same extra is refused too"
    );
    assert!(
        serde_json::from_str::<DeleteTagRequest>(r#"["v1","origin"]"#).is_err(),
        "one field, two elements: refused"
    );
}

/// Layer 2b: the clone URL gate. The URL is the one client string that becomes
/// a git argument, so every smuggling shape must die in `validate_clone_url`.
#[test]
fn hostile_clone_urls_are_refused_by_the_gate() {
    use git_vista_protocol::validate_clone_url;

    for url in [
        "file:///etc/passwd",                           // local filesystem read
        "ssh://evil.example/repo",                      // key-prompting transport
        "git@github.com:owner/repo.git",                // scp-style ssh
        "-oProxyCommand=touch /tmp/pwned",              // option smuggled as the URL
        "--upload-pack=/tmp/evil",                      // ditto
        "ext::sh -c id",                                // git's ext transport = arbitrary exec
        "https://ok.example/r --upload-pack=/tmp/evil", // second token via whitespace
        "https://ok.example/r\tmore",                   // tab counts as whitespace too
        "",                                             // nothing
    ] {
        assert!(
            validate_clone_url(url).is_err(),
            "hostile clone URL was accepted: {url:?}"
        );
    }
}

/// Layer 3: the same refusals observed on the wire, through the real session/
/// CSRF middleware and the real extractors. Stub handler bodies mean a 2xx
/// with the marker text would prove a hostile body *reached* handler logic —
/// every assertion below is that it never does.
mod wire {
    use crate::handlers::session::{create_session, revoke_session, session_status, SessionState};
    use crate::security::{require_auth, AuthState, HostPolicy};
    use crate::session::SessionManager;
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use git_vista_core::activity::UndoAction;
    use git_vista_protocol::{
        validate_clone_url, BranchRequest, CloneRequest, CreateBranchRequest, CreateTagRequest,
        SessionInfo, CSRF_HEADER,
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    const REACHED: &str = "REACHED HANDLER";

    fn app() -> (Router, Arc<SessionManager>) {
        let sessions = Arc::new(SessionManager::new(None));
        let session_state = SessionState {
            manager: sessions.clone(),
            via_lan: false,
            rate_limiter: None,
        };
        let auth_state = AuthState {
            manager: sessions.clone(),
            hosts: HostPolicy::loopback(8080),
        };
        let router = Router::new()
            .route(
                "/api/session",
                get(session_status)
                    .post(create_session)
                    .delete(revoke_session),
            )
            .route(
                "/api/branch",
                post(|Json(_): Json<CreateBranchRequest>| async { REACHED }),
            )
            .route(
                "/api/checkout",
                post(|Json(_): Json<BranchRequest>| async { REACHED }),
            )
            .route(
                "/api/undo",
                post(|Json(_): Json<UndoAction>| async { REACHED }),
            )
            // M2.21d (#238): the tag-create body, whose extra fields are the
            // interesting attack surface (`force`, `annotated`).
            .route(
                "/api/tag",
                post(|Json(_): Json<CreateTagRequest>| async { REACHED }),
            )
            // Mirrors the real clone handler's order: the gate runs before any
            // spawn could (clone.rs validates, then passes the URL as its own
            // argv entry).
            .route(
                "/api/clone",
                post(|Json(req): Json<CloneRequest>| async move {
                    match validate_clone_url(&req.url) {
                        Ok(_) => (StatusCode::OK, REACHED.to_string()),
                        Err(reason) => (StatusCode::BAD_REQUEST, reason),
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                require_auth,
            ))
            .with_state(session_state);
        (router, sessions)
    }

    fn req(method: &str, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:8080")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                55001,
            ))))
    }

    async fn bootstrap(router: &Router, sessions: &SessionManager) -> (String, String) {
        let token = sessions.current_bootstrap();
        let resp = router
            .clone()
            .oneshot(
                req("POST", "/api/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bootstrap should succeed");
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let info: SessionInfo = serde_json::from_slice(&bytes).unwrap();
        (cookie, info.csrf.unwrap())
    }

    /// POST `body` to `path` with a valid session + CSRF, so the only thing
    /// standing between the payload and handler logic is the API boundary
    /// under test. Returns (status, body text).
    async fn post_json(
        router: &Router,
        cookie: &str,
        csrf: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, String) {
        let resp = router
            .clone()
            .oneshot(
                req("POST", path)
                    .header(header::COOKIE, cookie.to_string())
                    .header(CSRF_HEADER, csrf.to_string())
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn hostile_write_bodies_die_at_the_boundary() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;

        for (path, body) in [
            // Raw argv arrays instead of the typed object.
            ("/api/branch", r#"["git","push","--force"]"#),
            ("/api/checkout", r#"["--delete","main"]"#),
            // Freeform args smuggled beside legitimate fields.
            (
                "/api/branch",
                r#"{"name":"x","commit":"HEAD","args":["--force"]}"#,
            ),
            ("/api/checkout", r#"{"branch":"b","extra":"--force"}"#),
            // A smuggled exec request that matches no UndoAction variant.
            ("/api/undo", r#"{"exec":"rm -rf /"}"#),
            ("/api/undo", r#"["sh","-c","id"]"#),
            // M2.21d (#238). `force` would repoint an existing tag past the
            // plan's `RefAbsent` precondition; `annotated` without a message
            // is the request that makes `git tag -a` open an editor a
            // headless server has no way to finish (ADR 0048). Neither key
            // exists on the DTO, so both die here.
            ("/api/tag", r#"{"name":"v1","commit":"HEAD","force":true}"#),
            (
                "/api/tag",
                r#"{"name":"v1","commit":"HEAD","annotated":true}"#,
            ),
            // Not JSON at all.
            ("/api/branch", "name=x; git push --mirror"),
        ] {
            let (status, text) = post_json(&router, &cookie, &csrf, path, body).await;
            assert!(
                status.is_client_error(),
                "{path} accepted hostile body {body:?} (status {status})"
            );
            assert!(
                !text.contains(REACHED),
                "{path}: hostile body {body:?} reached handler logic"
            );
        }
    }

    #[tokio::test]
    async fn hostile_clone_urls_die_at_the_boundary() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;

        for body in [
            r#"{"url":"file:///etc/passwd"}"#,
            r#"{"url":"-oProxyCommand=touch /tmp/pwned"}"#,
            r#"{"url":"ext::sh -c id"}"#,
            r#"{"url":"https://ok.example/r --upload-pack=/tmp/evil"}"#,
            r#"{"url":"https://ok.example/r","depth":"--mirror"}"#,
        ] {
            let (status, text) = post_json(&router, &cookie, &csrf, "/api/clone", body).await;
            assert!(
                status.is_client_error(),
                "/api/clone accepted hostile body {body:?} (status {status})"
            );
            assert!(
                !text.contains(REACHED),
                "/api/clone: hostile body {body:?} got past the URL gate"
            );
        }
    }
}
