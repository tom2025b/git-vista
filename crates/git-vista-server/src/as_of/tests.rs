use super::*;
use crate::activity::{activity_page_for_target, ActivityParams};
use crate::journal;
use git_vista_core::activity::{ActivityKind, ActivitySource};
use git_vista_protocol::{ActivityObservation, ActivityPage};
use serde_json::{json, Value};
use std::path::PathBuf;

fn fixture() -> (tempfile::TempDir, PathBuf, CursorCodec, CursorScope) {
    let (dir, repo) = git_vista_fixtures::seeded_files(&[("f", "base\n")], "base");
    let codec = CursorCodec::with_key([0x36; 32]);
    let scope = codec.scope_for_target(None, &repo.canonicalize().unwrap());
    (dir, repo, codec, scope)
}

fn event(repo: &Path, summary: &str) -> ActivityEvent {
    ActivityEvent {
        time: 4_000_000_000,
        kind: ActivityKind::Other,
        ref_name: None,
        summary: summary.into(),
        old_oid: None,
        new_oid: None,
        source: ActivitySource::App,
        undo: None,
        refs: Some(journal::capture_refs(repo)),
    }
}

fn write_events(repo: &Path, events: &[ActivityEvent]) {
    let dir = journal::state_dir(repo).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let mut text = String::new();
    for event in events {
        text.push_str(&serde_json::to_string(event).unwrap());
        text.push('\n');
    }
    std::fs::write(dir.join("journal.jsonl"), text).unwrap();
}

fn page(
    repo: &Path,
    codec: &CursorCodec,
    scope: CursorScope,
) -> ActivityPage<ActivityObservation<ActivityEvent>> {
    activity_page_for_target(
        repo,
        false,
        scope,
        &ActivityParams {
            limit: Some(100),
            cursor: None,
        },
        codec,
    )
    .unwrap()
}

fn token(row: &ActivityObservation<ActivityEvent>) -> &str {
    match &row.as_of {
        AsOfAvailability::Available { token } => token,
        other => panic!("{} is unavailable: {other:?}", row.summary),
    }
}

fn changed_capture(original: &ActivityEvent, mutate: impl FnOnce(&mut Value)) -> ActivityEvent {
    let mut value = serde_json::to_value(original).unwrap();
    mutate(&mut value["refs"]);
    serde_json::from_value(value).unwrap()
}

#[test]
fn as_of_incomplete_observations_never_receive_a_token() {
    let (_dir, repo, codec, scope) = fixture();
    let complete = event(&repo, "complete");
    let cases = [
        (
            "no_capture",
            Value::Null,
            AsOfUnavailable::CaptureNotRecorded,
        ),
        (
            "failed",
            json!({"status":"capture_failed","reason":"IO"}),
            AsOfUnavailable::CaptureFailed,
        ),
        (
            "future",
            json!({"status":"future_shape"}),
            AsOfUnavailable::CaptureNotRecorded,
        ),
        (
            "orphan",
            json!({"status":"in_batch","batch":"missing"}),
            AsOfUnavailable::CaptureNotRecorded,
        ),
    ];
    let mut events = vec![complete.clone()];
    let mut expected = HashMap::new();
    for (name, capture, reason) in cases {
        let mut e = changed_capture(&complete, |v| *v = capture);
        e.summary = name.into();
        expected.insert(e.summary.clone(), reason);
        events.push(e);
    }
    for (field, reason) in [
        ("head", AsOfUnavailable::IncompleteCapture),
        ("tags", AsOfUnavailable::IncompleteCapture),
        ("remotes", AsOfUnavailable::IncompleteCapture),
        ("shallow", AsOfUnavailable::ShallowStateNotRecorded),
    ] {
        let mut e = changed_capture(&complete, |v| {
            v.as_object_mut().unwrap().remove(field);
        });
        e.summary = format!("missing-{field}");
        expected.insert(e.summary.clone(), reason);
        events.push(e);
    }
    for path in [
        vec!["truncated_at"],
        vec!["tags", "truncated_at"],
        vec!["remotes", "truncated_at"],
    ] {
        let mut e = changed_capture(&complete, |v| {
            let mut slot = v;
            for key in &path {
                slot = &mut slot[*key];
            }
            *slot = json!(501);
        });
        e.summary = format!("truncated-{}", path.join("-"));
        expected.insert(e.summary.clone(), AsOfUnavailable::TruncatedCapture);
        events.push(e);
    }
    for (name, head, reason) in [
        (
            "unreadable",
            json!({"at":"unreadable","reason":"IO"}),
            AsOfUnavailable::UnreadableHead,
        ),
        (
            "unresolvable",
            json!({"at":"unresolvable"}),
            AsOfUnavailable::UnresolvableHead,
        ),
        (
            "missing-object",
            json!({"at":"detached","oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}),
            AsOfUnavailable::MissingCommitObjects,
        ),
        (
            "malformed-oid",
            json!({"at":"detached","oid":"bad"}),
            AsOfUnavailable::InvalidCapture,
        ),
        (
            "inconsistent-head",
            json!({"at":"on_branch","symbolic":"refs/heads/absent","oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}),
            AsOfUnavailable::InvalidCapture,
        ),
    ] {
        let mut e = changed_capture(&complete, |v| v["head"] = head);
        e.summary = name.into();
        expected.insert(e.summary.clone(), reason);
        events.push(e);
    }
    write_events(&repo, &events);
    let rows = page(&repo, &codec, scope).events;
    assert!(!token(rows.iter().find(|r| r.summary == "complete").unwrap()).is_empty());
    for (name, reason) in expected {
        let row = rows.iter().find(|r| r.summary == name).unwrap();
        assert_eq!(
            row.as_of,
            AsOfAvailability::Unavailable { reason },
            "{name}"
        );
        assert!(
            serde_json::to_value(&row.as_of)
                .unwrap()
                .get("token")
                .is_none(),
            "{name}"
        );
    }
}

/// Both insertion and an in-place capture change must invalidate a signed
/// index. The second case keeps feed length, timestamp, and summary identical.
#[test]
fn as_of_stale_fold_returns_409_for_head_and_in_place_changes() {
    for change_at_head in [true, false] {
        let (_dir, repo, codec, scope) = fixture();
        let first = event(&repo, "selected");
        let mut tail = event(&repo, "unchanged tail");
        tail.time -= 1;
        write_events(&repo, &[first.clone(), tail.clone()]);
        let rows = page(&repo, &codec, scope).events;
        let selected = token(rows.iter().find(|r| r.summary == "selected").unwrap());
        let earlier = token(rows.iter().find(|r| r.summary == "unchanged tail").unwrap());
        assert!(redeem(&repo, false, scope, selected, &codec).is_ok());
        if change_at_head {
            let mut head = event(&repo, "inserted");
            head.time += 1;
            write_events(&repo, &[head, first, tail]);
        } else {
            let changed = changed_capture(&first, |v| {
                v["tags"]["entries"]["recorded-later"] = v["branches"]["main"].clone()
            });
            write_events(&repo, &[changed, tail]);
        }
        for encoded in [selected, earlier] {
            let error = redeem(&repo, false, scope, encoded, &codec).unwrap_err();
            assert_eq!(
                error.0,
                StatusCode::CONFLICT,
                "stale selector was served: {error:?}"
            );
        }
    }
}

#[test]
fn as_of_identical_rows_have_distinct_absolute_selectors_across_pages() {
    let (_dir, repo, codec, scope) = fixture();
    let observation = event(&repo, "identical");
    write_events(&repo, &[observation.clone(), observation]);
    let first = activity_page_for_target(
        &repo,
        false,
        scope,
        &ActivityParams {
            limit: Some(1),
            cursor: None,
        },
        &codec,
    )
    .unwrap();
    let second = activity_page_for_target(
        &repo,
        false,
        scope,
        &ActivityParams {
            limit: Some(1),
            cursor: first.cursor.clone(),
        },
        &codec,
    )
    .unwrap();
    assert_eq!(first.events[0].event, second.events[0].event);
    let a = token(&first.events[0]);
    let b = token(&second.events[0]);
    assert_ne!(a, b);
    assert_eq!(
        codec.decode::<AsOfPosition>(a).unwrap().state.event_index,
        0
    );
    assert_eq!(
        codec.decode::<AsOfPosition>(b).unwrap().state.event_index,
        1
    );
    let a = redeem(&repo, false, scope, a, &codec).unwrap();
    let b = redeem(&repo, false, scope, b, &codec).unwrap();
    assert_eq!(a.refs, b.refs);
    assert_ne!(
        a.generation, b.generation,
        "page cursors must not cross observations"
    );
    assert!(
        !std::fs::read_to_string(journal::state_dir(&repo).unwrap().join("journal.jsonl"))
            .unwrap()
            .contains("as_of")
    );
}

#[test]
fn as_of_tokens_refuse_forgery_other_scopes_kinds_positions_and_processes() {
    let (_dir, repo, codec, scope) = fixture();
    write_events(&repo, &[event(&repo, "selected")]);
    let rows = page(&repo, &codec, scope).events;
    let good = token(&rows[0]);
    let opened = codec.decode::<AsOfPosition>(good).unwrap();
    let other_scope = codec.scope_for_target(None, &repo.join("sibling-worktree"));
    assert_eq!(
        redeem(&repo, false, other_scope, good, &codec)
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        redeem(&repo, false, scope, good, &CursorCodec::with_key([99; 32]))
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut forged = good.to_string();
    forged.replace_range(0..1, if &good[0..1] == "A" { "B" } else { "A" });
    assert_eq!(
        redeem(
            Path::new("/not-a-repository"),
            false,
            scope,
            &forged,
            &codec
        )
        .unwrap_err()
        .0,
        StatusCode::BAD_REQUEST
    );
    for state in [
        json!({"next_event":0}),
        json!({"next_row":0}),
        json!({"event_index":usize::MAX}),
    ] {
        let other = codec.encode(scope, &opened.generation, &state).unwrap();
        assert_eq!(
            redeem(&repo, false, scope, &other, &codec).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[test]
fn as_of_shallow_status_is_recorded_never_inferred_retroactively() {
    let (_dir, repo, codec, scope) = fixture();
    let observed = event(&repo, "unshallow");
    assert_eq!(
        serde_json::to_value(&observed).unwrap()["refs"]["shallow"],
        false
    );
    let legacy = changed_capture(&observed, |v| {
        v.as_object_mut().unwrap().remove("shallow");
    });
    assert!(matches!(
        captured_snapshot(&legacy),
        Err(AsOfUnavailable::ShallowStateNotRecorded)
    ));
    let tip = serde_json::to_value(&observed).unwrap()["refs"]["branches"]["main"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(repo.join(".git/shallow"), format!("{tip}\n")).unwrap();
    let shallow = event(&repo, "was shallow");
    assert_eq!(
        serde_json::to_value(&shallow).unwrap()["refs"]["shallow"],
        true
    );
    write_events(&repo, &[observed]);
    assert_eq!(
        page(&repo, &codec, scope).events[0].as_of,
        AsOfAvailability::Unavailable {
            reason: AsOfUnavailable::ShallowRepository
        }
    );
    std::fs::remove_file(repo.join(".git/shallow")).unwrap();
    write_events(&repo, &[shallow]);
    assert_eq!(
        page(&repo, &codec, scope).events[0].as_of,
        AsOfAvailability::Unavailable {
            reason: AsOfUnavailable::ShallowRepository
        }
    );
    std::fs::create_dir(repo.join(".git/shallow")).unwrap();
    assert_eq!(
        serde_json::to_value(event(&repo, "unreadable metadata")).unwrap()["refs"].get("shallow"),
        None
    );
}

#[test]
fn as_of_folded_bursts_and_external_deletions_are_not_observations() {
    let (_dir, repo, codec, scope) = fixture();
    let mut burst = Vec::new();
    for name in ["refs/remotes/origin/one", "refs/remotes/origin/two"] {
        let mut e = event(&repo, "fetched ref");
        e.kind = ActivityKind::Fetch;
        e.ref_name = Some(name.into());
        e.new_oid = Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
        burst.push(e);
    }
    write_events(&repo, &burst);
    let rows = page(&repo, &codec, scope).events;
    let fetched: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == ActivityKind::Fetch)
        .collect();
    assert_eq!(
        fetched.len(),
        1,
        "fixture must actually exercise the lossy burst fold"
    );
    assert_eq!(
        fetched[0].as_of,
        AsOfAvailability::Unavailable {
            reason: AsOfUnavailable::CaptureNotRecorded
        }
    );
    journal::write_snapshot(
        &repo,
        &HashMap::from([(
            "vanished".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        )]),
    );
    let rows = page(&repo, &codec, scope).events;
    let deletion = rows
        .iter()
        .find(|r| r.kind == ActivityKind::BranchDeleted)
        .unwrap();
    assert_eq!(
        deletion.as_of,
        AsOfAvailability::Unavailable {
            reason: AsOfUnavailable::IncompleteCapture
        }
    );
}

#[test]
fn as_of_empty_unborn_capture_is_a_real_empty_history_and_batches_keep_the_fact() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let snapshot = captured_snapshot(&event(dir.path(), "empty")).unwrap();
    assert!(snapshot.tips.is_empty());
    assert_eq!(snapshot.head_state, git_vista_protocol::HeadState::Unborn);
    let mut batch = vec![event(dir.path(), "a"), event(dir.path(), "b")];
    for e in &mut batch {
        e.refs = None;
    }
    journal::append_all(dir.path(), &batch);
    let feed = collect_activity_feed(dir.path(), false).unwrap();
    for row in feed {
        assert!(
            captured_snapshot(&row).is_ok(),
            "resolved batch lost shallow status"
        );
    }
}
