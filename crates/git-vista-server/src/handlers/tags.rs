//! `GET /api/tags` — the tag listing (M2.21b, #236).
//!
//! The read half of M2.21: what `GET /api/frame`'s ref badges throw away.
//! `read_refs` peels every `refs/tags/*` straight to a commit, so a badge can
//! say *that* a tag exists but never whether it is lightweight or annotated,
//! nor who tagged it or why. This endpoint answers all of that, as the
//! [`TagDetail`] DTO M2.21a (#235, ADR 0041) landed ahead of its producer.
//!
//! # Two boundaries meet here
//!
//! [`git_vista_git::read_tags`] produces raw, git-shaped facts (a
//! [`TagRecord`]); this module is the *only* place they become wire DTOs.
//! Keeping the mapping here rather than in `git-vista-git` is what lets that
//! crate stay free of a `git-vista-protocol` dependency, and it puts every
//! validated-newtype decision — which tags can be represented at all, what
//! happens to an over-long message — in one reviewable function,
//! [`tag_detail`].
//!
//! # Absence is modelled as absence
//!
//! A lightweight tag has no tag object, no tagger and no message; all three
//! are `null` on the wire, never `""`. A UI that renders an empty string in a
//! "Tagger" field shows a blank tagger, which is a different (and false)
//! claim from "this kind of tag has no tagger".
//!
//! # No new spawn path
//!
//! There is no subprocess at all. `read_tags` opens the repository once with
//! `gix::open_opts(.., isolated())` — the same posture as every other
//! `git-vista-git` read — and decodes each tag object out of the mapped object
//! database. So there is nothing for the Tier::Strict `sandbox::spawn`
//! chokepoint to classify here, and in particular nothing that could become a
//! spawn *per tag*; #221's held-open `cat-file --batch` would be strictly more
//! expensive than reading the odb we already have open.

use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use git_vista_git::TagRecord;
use git_vista_protocol::dto::{SignatureStatus, TagDetail, TagKind};
use git_vista_protocol::plan::{CommitOid, TagMessage, TagName, MAX_TAG_MESSAGE_LEN};

use crate::handlers::read::{resolve_repo, RepoQuery};

/// What is appended to a tag message that had to be cut, so the cut is visible
/// to whoever reads it.
///
/// A prefix that silently *looks* like the whole message is the failure this
/// exists to prevent: [`TagDetail`] has no `truncated` flag (it is a
/// `deny_unknown_fields` contract that shipped in M2.21a), so the only place
/// left to be honest is inside the display text itself.
const TRUNCATION_NOTE: &str =
    "\n\n[git-vista: this tag's message is longer than 16 KiB; it was cut here]";

/// Every tag in the repository, sorted by name.
///
/// `no-store` like the other live reads: a tag can be created or deleted at
/// any moment by git outside the app, so a cached listing is a wrong listing.
/// Note where that guarantee actually comes from through the router:
/// `security::require_auth` **overwrites** `Cache-Control` with `no-store` on
/// every authenticated API response, so a router-level assertion cannot tell
/// this line from its absence. The header is still set here, matching every
/// sibling read handler, and `the_handler_itself_marks_the_listing_no_store`
/// tests it where it *is* observable — by calling the handler directly.
///
/// A `SessionRequired` **read** (see `route_authz.rs`): it is registered on
/// both listeners, because a tag listing discloses exactly what the ref badges
/// on `/api/frame` already do — committed, published history — and nothing
/// about the working tree. That is the same line ADR 0005 draws for
/// `/api/status` (worktree contents: loopback only) versus `/api/frame`.
pub(crate) async fn tag_list(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    // `read_tags` opens a repository and mmaps packs — blocking work, so it
    // goes off the async workers exactly as `read_head_branch` does in
    // `planner::current_branch`.
    let records = tokio::task::spawn_blocking(move || git_vista_git::read_tags(&repo))
        .await
        .map_err(|e| {
            eprintln!("git-vista: /api/tags panicked while reading tags: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't read the repository's tags.".to_string(),
            )
        })?
        .map_err(|e| {
            eprintln!("git-vista: /api/tags couldn't read tags: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't read the repository's tags: {e}"),
            )
        })?;

    let tags: Vec<TagDetail> = records.iter().filter_map(tag_detail).collect();
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(tags)))
}

/// Map one [`TagRecord`] onto the wire DTO, or `None` when it cannot be
/// represented at all.
///
/// # Why this returns an `Option`
///
/// [`TagName`] and [`CommitOid`] are *validated* newtypes: a name that is
/// option-shaped (`-d`) or an id that is not hex cannot exist as a value.
/// Those shapes are not reachable through `git tag`, but they are reachable
/// through `git update-ref` and a hand-written ref file, and a repository is
/// allowed to be strange. The alternatives were both worse: `unwrap` turns a
/// strange repository into a 500 for the whole listing, and inventing a
/// substitute name would put a tag on screen that does not exist under that
/// name. Skipping one tag, loudly on stderr, keeps the other tags readable —
/// the same call `read_refs` makes for a ref that will not peel.
///
/// # The signature field, and why it is not always `Unsigned`
///
/// #236's scope note says to hardcode [`SignatureStatus::Unsigned`] as a
/// placeholder until M2.21c runs real verification. That is right for a tag
/// with **no signature block** and wrong for one that has it: `Unsigned` is
/// documented as "carries no signature at all", so reporting it for a signed
/// tag is a false negative in exactly the direction
/// [`SignatureStatus`]'s own doc warns about. A tag whose object *does* carry
/// armour gets [`SignatureStatus::Unverifiable`] — "a signature is present but
/// verification could not run at all" — which is a true statement in a slice
/// where the verifier does not exist yet. No gpg is invoked either way; the
/// presence bit comes from the object parser splitting the armour off the
/// message. M2.21c narrows `Unverifiable` to valid / invalid / unknown-key and
/// never has to widen `Unsigned` back out.
fn tag_detail(record: &TagRecord) -> Option<TagDetail> {
    let name = match TagName::new(&record.name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!(
                "git-vista: tag {:?} cannot be named on the wire ({e}); not listed",
                record.name
            );
            return None;
        }
    };
    let target = match CommitOid::new(&record.target.0) {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "git-vista: tag {:?} has an unusable target {:?} ({e}); not listed",
                record.name, record.target.0
            );
            return None;
        }
    };
    let tag_object = match &record.tag_object {
        None => None,
        Some(oid) => match CommitOid::new(&oid.0) {
            Ok(o) => Some(o),
            Err(e) => {
                eprintln!(
                    "git-vista: tag {:?} has an unusable tag object {:?} ({e}); not listed",
                    record.name, oid.0
                );
                return None;
            }
        },
    };
    Some(TagDetail {
        name,
        kind: if record.annotated {
            TagKind::Annotated
        } else {
            TagKind::Lightweight
        },
        target,
        tag_object,
        tagger: record.tagger.clone(),
        message: record
            .message
            .as_deref()
            .and_then(|m| fit_message(m, record.message_truncated)),
        signature: if record.signed {
            SignatureStatus::Unverifiable
        } else {
            SignatureStatus::Unsigned
        },
    })
}

/// Fit a tag's annotation into a [`TagMessage`], appending [`TRUNCATION_NOTE`]
/// whenever bytes were dropped.
///
/// `already_truncated` is the reader's own byte-level fact from
/// [`TagRecord::message_truncated`] — never re-derived from this string's
/// length, which has already been trimmed. Both inputs matter: the message can
/// be short enough to fit here and still be a prefix of a much longer one.
fn fit_message(message: &str, already_truncated: bool) -> Option<TagMessage> {
    if !already_truncated && message.len() <= MAX_TAG_MESSAGE_LEN {
        return TagMessage::new(message).ok();
    }
    // Leave room for the note, and cut on a character boundary so a
    // multi-byte character is dropped whole rather than mangled.
    let mut end = MAX_TAG_MESSAGE_LEN
        .saturating_sub(TRUNCATION_NOTE.len())
        .min(message.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    TagMessage::new(format!("{}{TRUNCATION_NOTE}", &message[..end])).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use git_vista_core::model::Oid;

    /// What [`build_tagged_fixture`] planted, read back **out of git** so the
    /// router test can compare the response against git's own answers rather
    /// than against anything git-vista computed.
    pub(crate) struct TaggedFixture {
        /// The opaque worktree id the repository is addressable by (`?repo=`).
        pub repo_id: String,
        /// `HEAD` — what the lightweight tag marks.
        pub tip: String,
        /// `HEAD~1` — what the annotated tag's *peeled target* must be.
        pub root: String,
        /// The annotated tag's own object id (`rev-parse refs/tags/v1.0`).
        pub tag_object: String,
        /// The exact `tagger` header line git wrote into that object.
        pub tagger: String,
    }

    /// Build a repository with one lightweight and one annotated tag, register
    /// it in the catalog, and report both the addressable id and git's own
    /// view of what was planted.
    ///
    /// # Why this lives here and not in `main.rs`
    ///
    /// `main.rs` is the deliberate negative control for
    /// `argv_boundary::every_allowlist_entry_names_a_live_spawn_site` ("if
    /// `main.rs` ever starts spawning, this line will fail") and it is the one
    /// file `sandbox::escape_contract`'s R7 scans for the server's `GIT_*`
    /// surface. A `#[cfg(test)]` git fixture there would trip both guards for
    /// no reason. So the router test in `main.rs` calls this, and the
    /// `Command` + `GIT_*` env stay in a file the argv allowlist covers as
    /// test-fixture setup.
    ///
    /// The repository is addressed by `?repo=<id>` rather than by the default
    /// selection on purpose: id resolution reads the *catalog*, which only
    /// grows, so the test cannot be perturbed by another test in this binary
    /// moving the process-global `CURRENT` selection out from under it.
    pub(crate) fn build_tagged_fixture(dir: &std::path::Path) -> TaggedFixture {
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Ada Lovelace")
                .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada Lovelace")
                .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .env("GIT_AUTHOR_DATE", "@1753300000 +0000")
                .env("GIT_COMMITTER_DATE", "@1753300000 +0000")
                .status()
                .expect("git should be runnable")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        let git_out = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git should be runnable");
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        git(&["commit", "-q", "--allow-empty", "-m", "second"]);
        // One of each kind, in one repository, so a response cannot pass by
        // getting a single kind right.
        git(&["tag", "tip-marker", "HEAD"]);
        git(&["tag", "-a", "-m", "one\n\nrelease notes", "v1.0", "HEAD~1"]);

        crate::state::allow_repo_root(dir);
        let handle = crate::state::set_current(dir, git_vista_protocol::RepoMode::Active)
            .expect("the fixture registers in the catalog");

        TaggedFixture {
            repo_id: handle.worktree.to_string(),
            tip: git_out(&["rev-parse", "HEAD"]),
            root: git_out(&["rev-parse", "HEAD~1"]),
            tag_object: git_out(&["rev-parse", "refs/tags/v1.0"]),
            tagger: git_out(&["cat-file", "-p", "refs/tags/v1.0"])
                .lines()
                .find_map(|l| l.strip_prefix("tagger ").map(str::to_string))
                .expect("git wrote a tagger header"),
        }
    }

    /// The handler's own response, without the auth middleware in the way.
    ///
    /// Through the router, `security::require_auth` overwrites `Cache-Control`
    /// on every authenticated API response, so the router test cannot fail if
    /// this handler stops setting it — that mutation was tried and survived
    /// there. Calling the handler directly is where the header is the
    /// handler's own fact, and where breaking it goes red.
    #[tokio::test]
    async fn the_handler_itself_marks_the_listing_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = build_tagged_fixture(dir.path());
        let response = tag_list(axum::extract::Query(RepoQuery {
            repo: Some(fixture.repo_id),
        }))
        .await
        .expect("the fixture repository reads")
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    /// The same direct call, proving the handler really composes
    /// `read_tags` → `tag_detail` — not only that the router reaches it.
    #[tokio::test]
    async fn the_handler_composes_the_reader_and_the_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = build_tagged_fixture(dir.path());
        let response = tag_list(axum::extract::Query(RepoQuery {
            repo: Some(fixture.repo_id),
        }))
        .await
        .expect("the fixture repository reads")
        .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let body: Vec<TagDetail> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].kind, TagKind::Lightweight);
        assert_eq!(body[0].target.as_str(), fixture.tip);
        assert_eq!(body[1].kind, TagKind::Annotated);
        assert_eq!(body[1].target.as_str(), fixture.root);
        assert_eq!(
            body[1].tag_object.as_ref().map(|o| o.as_str()),
            Some(fixture.tag_object.as_str())
        );
    }

    /// A lightweight record, as `read_tags` would produce one.
    fn lightweight(name: &str) -> TagRecord {
        TagRecord {
            name: name.to_string(),
            annotated: false,
            target: Oid("2".repeat(40)),
            tag_object: None,
            tagger: None,
            message: None,
            message_truncated: false,
            signed: false,
        }
    }

    /// An annotated record with a tagger and a message.
    fn annotated(name: &str) -> TagRecord {
        TagRecord {
            name: name.to_string(),
            annotated: true,
            target: Oid("2".repeat(40)),
            tag_object: Some(Oid("8".repeat(40))),
            tagger: Some("Ada Lovelace <ada@example.com> 1753300000 +0000".to_string()),
            message: Some("first stable release".to_string()),
            message_truncated: false,
            signed: false,
        }
    }

    /// The mapping is asserted against **hand-written wire JSON**, not against
    /// `tag_detail`'s own output re-serialised — a round trip through the
    /// function that defines the mapping would bless any mapping at all.
    #[test]
    fn a_lightweight_tag_serialises_with_nulls_not_empty_strings() {
        let wire = serde_json::to_value(tag_detail(&lightweight("tip-marker")).unwrap()).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "name": "tip-marker",
                "kind": "lightweight",
                "target": "2".repeat(40),
                "tag_object": null,
                "tagger": null,
                "message": null,
                "signature": "unsigned",
            })
        );
    }

    #[test]
    fn an_annotated_tag_carries_its_object_tagger_and_message() {
        let wire = serde_json::to_value(tag_detail(&annotated("v1.0.0")).unwrap()).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "name": "v1.0.0",
                "kind": "annotated",
                "target": "2".repeat(40),
                "tag_object": "8".repeat(40),
                "tagger": "Ada Lovelace <ada@example.com> 1753300000 +0000",
                "message": "first stable release",
                "signature": "unsigned",
            })
        );
    }

    /// The deviation from #236's literal wording, pinned so it cannot be
    /// "fixed" back to a false negative without this test going red.
    #[test]
    fn a_signed_tag_is_unverifiable_not_unsigned() {
        let mut record = annotated("v-signed");
        record.signed = true;
        assert_eq!(
            tag_detail(&record).unwrap().signature,
            SignatureStatus::Unverifiable
        );
        // …and the unsigned twin still says `Unsigned`, so the test above
        // cannot pass by making every tag unverifiable.
        assert_eq!(
            tag_detail(&annotated("v-plain")).unwrap().signature,
            SignatureStatus::Unsigned
        );
    }

    #[test]
    fn a_tag_whose_name_cannot_ride_the_wire_is_skipped_not_renamed() {
        // `-d` is option-shaped, so `TagName` refuses it. Reachable via
        // `git update-ref refs/tags/-d <oid>`.
        assert!(tag_detail(&lightweight("-d")).is_none());
        // A name that only *contains* a dash is fine — the skip is narrow.
        assert!(tag_detail(&lightweight("v1.0-rc1")).is_some());
    }

    #[test]
    fn a_tag_with_an_unusable_target_is_skipped() {
        let mut record = lightweight("v1.0");
        record.target = Oid("not-a-hex-object-id".to_string());
        assert!(tag_detail(&record).is_none());

        let mut record = annotated("v2.0");
        record.tag_object = Some(Oid("zz".repeat(20)));
        assert!(tag_detail(&record).is_none());
    }

    #[test]
    fn an_over_long_message_is_cut_visibly_and_still_fits_the_newtype() {
        let mut record = annotated("v-long");
        record.message = Some("x".repeat(MAX_TAG_MESSAGE_LEN));
        record.message_truncated = true;

        let detail = tag_detail(&record).unwrap();
        let message = detail.message.expect("an over-long message is not dropped");
        assert!(
            message.as_str().ends_with(TRUNCATION_NOTE),
            "the cut has to be visible to whoever reads the message"
        );
        assert!(
            message.as_str().len() <= MAX_TAG_MESSAGE_LEN,
            "the result must satisfy TagMessage's own cap, or the newtype \
             would have refused it and the message would have vanished"
        );
        assert!(message.as_str().starts_with("xxxx"));
    }

    /// The flag is load-bearing on its own: a message short enough to fit can
    /// still be a prefix of a much longer one that the reader cut.
    #[test]
    fn the_truncation_flag_alone_triggers_the_note() {
        let mut record = annotated("v-short-but-cut");
        record.message = Some("only the first line".to_string());
        record.message_truncated = true;
        let message = tag_detail(&record).unwrap().message.unwrap();
        assert!(message.as_str().ends_with(TRUNCATION_NOTE));
        assert!(message.as_str().starts_with("only the first line"));

        // …and an untruncated message of the same text gets no note at all.
        let mut clean = annotated("v-whole");
        clean.message = Some("only the first line".to_string());
        assert_eq!(
            tag_detail(&clean).unwrap().message.unwrap().as_str(),
            "only the first line"
        );
    }

    /// A multi-byte character straddling the cut point is dropped whole.
    #[test]
    fn a_cut_never_splits_a_character() {
        let room = MAX_TAG_MESSAGE_LEN - TRUNCATION_NOTE.len();
        // One byte of a two-byte 'é' would land past the boundary.
        let mut text = "x".repeat(room - 1);
        text.push('é');
        text.push_str("tail");
        let fitted = fit_message(&text, true).unwrap();
        assert!(!fitted.as_str().contains('\u{FFFD}'));
        assert_eq!(fitted.as_str().len(), room - 1 + TRUNCATION_NOTE.len());
    }
}
