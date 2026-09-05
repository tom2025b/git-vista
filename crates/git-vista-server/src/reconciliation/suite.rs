//! Integration tests for the change feed's driver (M12.03–M12.06).
//!
//! The decisions themselves are host-tested in `policy_suite.rs`; what these
//! prove is that the driver actually asks the policy those questions against a
//! real repository, a real watcher, and the real planner generation. A pure core
//! that nothing wires up is a core that answers questions nobody asks (#612's
//! own origin), so both halves exist deliberately.

use super::*;
use std::process::Command;

use git_vista_protocol::change_feed::{ChangeFeedHealth, RefDelta};

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "user.name", "Feed Test"]);
    git(
        temp.path(),
        &["config", "user.email", "feed@example.invalid"],
    );
    std::fs::write(temp.path().join("file"), "one\n").unwrap();
    git(temp.path(), &["add", "file"]);
    git(temp.path(), &["commit", "-qm", "base"]);
    temp
}

/// The next snapshot published after the one already seen.
async fn next_snapshot(
    rx: &mut watch::Receiver<Option<ChangeFeedSnapshot>>,
    within: Duration,
) -> ChangeFeedSnapshot {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(snapshot) = rx.borrow_and_update().clone() {
            return snapshot;
        }
        tokio::time::timeout_at(deadline, rx.changed())
            .await
            .expect("the feed published nothing before the deadline")
            .expect("the feed's driver stopped");
    }
}

/// Wait for a snapshot satisfying `wanted`, ignoring ones that do not.
async fn snapshot_where(
    rx: &mut watch::Receiver<Option<ChangeFeedSnapshot>>,
    within: Duration,
    wanted: impl Fn(&ChangeFeedSnapshot) -> bool,
) -> ChangeFeedSnapshot {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let snapshot = {
            let held = rx.borrow_and_update();
            held.clone()
        };
        if let Some(snapshot) = snapshot {
            if wanted(&snapshot) {
                return snapshot;
            }
        }
        tokio::time::timeout_at(deadline, rx.changed())
            .await
            .expect("no snapshot matched before the deadline")
            .expect("the feed's driver stopped");
    }
}

// --- #553: the sweep is the authority --------------------------------------

#[tokio::test]
async fn a_change_no_watcher_reported_is_caught_by_the_sweep_alone() {
    // #553 acceptance 2, and the reason it says "suppresses the watcher rather
    // than by waiting": with no hint source at all, every publication below was
    // produced by a sweep and by nothing else. Waiting for a hint-driven feed to
    // publish would prove the same result with the sweep contributing nothing.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    let first = next_snapshot(&mut snapshots, Duration::from_secs(5)).await;

    git(repo.path(), &["branch", "landed-behind-the-watchers-back"]);

    let after = snapshot_where(&mut snapshots, Duration::from_secs(15), |s| {
        s.generation != first.generation
    })
    .await;
    match after.changed {
        RefDelta::Named { refs, .. } => assert!(
            refs.iter()
                .any(|r| r.as_str() == "refs/heads/landed-behind-the-watchers-back"),
            "the sweep names the ref it found: {refs:?}"
        ),
        RefDelta::Unknown => panic!("there was a previous reading to difference against"),
    }
}

#[tokio::test]
async fn a_suppressed_watcher_is_a_named_condition_not_a_healthy_looking_feed() {
    // The other half of the same criterion. A feed with no hints still works —
    // and must not report that it is watching anything.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    let degraded = snapshot_where(&mut snapshots, Duration::from_secs(15), |s| {
        matches!(s.health, ChangeFeedHealth::SweepOnly { .. })
    })
    .await;
    assert!(
        degraded.generation.is_some(),
        "SweepOnly costs latency, never truth — the reading is still a reading"
    );
}

#[tokio::test]
async fn the_watcher_and_the_sweep_together_publish_a_change_promptly() {
    let repo = repository();
    let feed = attach(repo.path());
    let mut snapshots = feed.subscribe();
    let first = snapshot_where(&mut snapshots, Duration::from_secs(10), |s| {
        matches!(s.health, ChangeFeedHealth::Watching { .. })
    })
    .await;

    git(repo.path(), &["branch", "seen"]);
    let after = snapshot_where(&mut snapshots, Duration::from_secs(10), |s| {
        s.generation != first.generation
    })
    .await;
    assert!(matches!(after.changed, RefDelta::Named { .. }));
}

#[tokio::test]
async fn a_sweep_that_cannot_read_the_repository_says_so_rather_than_going_quiet() {
    // #553 acceptance 4: the sweep's own failure is a stated condition, never a
    // quiet no-op. The repository is taken away underneath a running feed.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    next_snapshot(&mut snapshots, Duration::from_secs(5)).await;

    std::fs::rename(
        repo.path().join(".git"),
        repo.path().join(".git-moved-away"),
    )
    .unwrap();

    let blind = snapshot_where(&mut snapshots, Duration::from_secs(15), |s| {
        matches!(s.health, ChangeFeedHealth::Blind { .. })
    })
    .await;
    assert_eq!(
        blind.generation, None,
        "a feed that cannot look publishes no reading — a last-known value \
         would be indistinguishable from a fresh one"
    );
}

// --- #554: this process's own writes ---------------------------------------

#[tokio::test]
async fn an_app_write_publishes_the_state_it_left_and_the_next_sweeps_add_nothing() {
    // #554 acceptance 2, counted rather than inspected. The count is of
    // *publications*, which is what a client actually pays for: a redundant
    // re-read that publishes nothing is invisible to every open stream.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    let first = next_snapshot(&mut snapshots, Duration::from_secs(5)).await;

    let (status, body) = crate::planner::plan_and_execute_in(
        repo.path(),
        None,
        (
            git_vista_protocol::RepositoryToken::new("11111111-1111-5111-8111-111111111111")
                .unwrap(),
            git_vista_protocol::WorktreeToken::new("22222222-2222-5222-8222-222222222222").unwrap(),
        ),
        git_vista_protocol::GitOperation::CreateBranch {
            name: git_vista_protocol::BranchName::new("mine").unwrap(),
            at: git_vista_protocol::CommitOid::new(head_oid(repo.path())).unwrap(),
        },
        crate::planner::DropProof::Nothing,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");

    let after_write = snapshot_where(&mut snapshots, Duration::from_secs(10), |s| {
        s.generation != first.generation
    })
    .await;

    // Now let several sweeps run over the state the write produced. Every one
    // of them re-reads; not one of them may publish.
    let mut extra = 0;
    let watch_for = tokio::time::Instant::now() + Duration::from_secs(7);
    while tokio::time::timeout_at(watch_for, snapshots.changed())
        .await
        .is_ok()
    {
        let seen = snapshots.borrow_and_update().clone();
        if seen.as_ref().map(|s| &s.generation) != Some(&after_write.generation) {
            extra += 1;
        }
    }
    assert_eq!(
        extra, 0,
        "the state the write published is the state every later sweep read"
    );
}

#[tokio::test]
async fn an_external_change_landing_inside_a_write_is_announced_not_swallowed() {
    // #554 acceptance 3 — the direction that loses data, and the one the issue
    // says gets the stronger test. Something else moves the repository *during*
    // this process's own write, so the post-write reading observes the combined
    // state. Publishing whatever was read is what makes the window stop
    // mattering.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    let first = next_snapshot(&mut snapshots, Duration::from_secs(5)).await;

    let path = repo.path().to_path_buf();
    let combined = with_publish(repo.path(), async {
        // This process's own write ...
        git(&path, &["branch", "ours"]);
        // ... and, before the publish, somebody else's.
        git(&path, &["branch", "theirs"]);
    })
    .await;
    let () = combined;

    let announced = snapshot_where(&mut snapshots, Duration::from_secs(10), |s| {
        s.generation != first.generation
    })
    .await;
    match announced.changed {
        RefDelta::Named { refs, .. } => {
            let named: Vec<_> = refs.iter().map(|r| r.as_str()).collect();
            assert!(
                named.contains(&"refs/heads/theirs"),
                "the external ref must be announced, not absorbed into the \
                 app's own write: {named:?}"
            );
        }
        RefDelta::Unknown => panic!("there was a previous reading"),
    }
}

#[tokio::test]
async fn a_panicking_write_leaves_the_feed_free_to_publish() {
    // #554 acceptance 4: "test by panicking mid-write, not by reasoning about
    // it". The panic goes through the real `with_publish`, so the publish it
    // wraps genuinely never runs — nothing is recorded, and the ordinary sweep
    // that follows announces the change the dead write left behind.
    //
    // This is the test a flag-based deduplicator fails: a flag set before the
    // write would still be set here, and every later change would be ignored
    // with nothing able to say so.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    let first = next_snapshot(&mut snapshots, Duration::from_secs(5)).await;

    let path = repo.path().to_path_buf();
    let died = tokio::spawn(async move {
        let inner = path.clone();
        with_publish(&path, async move {
            git(&inner, &["branch", "written-then-died"]);
            panic!("the write died before it could publish");
        })
        .await
    })
    .await;
    assert!(died.is_err(), "the panic must propagate, not be swallowed");

    let announced = snapshot_where(&mut snapshots, Duration::from_secs(15), |s| {
        s.generation != first.generation
    })
    .await;
    match announced.changed {
        RefDelta::Named { refs, .. } => assert!(
            refs.iter()
                .any(|r| r.as_str() == "refs/heads/written-then-died"),
            "the sweep announces what the dead write left: {refs:?}"
        ),
        RefDelta::Unknown => panic!("there was a previous reading"),
    }
}

// --- #556: the bound, and what it degrades to ------------------------------

#[tokio::test]
async fn a_budget_smaller_than_the_watch_set_reports_bounded_rather_than_watching() {
    // #556 acceptance 2 and 5: the bound is enforced in code, and hitting it is
    // an observable state. A watcher that quietly covered less while still
    // reporting `Watching` is the failure this milestone exists to prevent,
    // aimed at itself.
    let repo = repository();
    git(repo.path(), &["branch", "one"]);
    git(repo.path(), &["branch", "team/two"]);
    git(repo.path(), &["branch", "team/sub/three"]);

    let mut watcher = crate::watcher::RepositoryWatcher::start_with_budget(
        repo.path(),
        WatchBudget::Undetermined { watches: 2 },
    );
    let health = tokio::time::timeout(Duration::from_secs(5), watcher.recv())
        .await
        .expect("the watcher reported within five seconds")
        .expect("the watcher's notice stream stayed open");
    match health {
        WatcherNotice::Health(WatcherHealth::Watching {
            installed, wanted, ..
        }) => {
            assert_eq!(installed, 2, "the budget bound the installs");
            assert!(
                wanted > installed,
                "and the watcher says how much it wanted: {wanted} > {installed}"
            );
        }
        other => panic!("expected a bounded watching report, got {other:?}"),
    }
}

#[tokio::test]
async fn what_the_bound_gives_up_is_latency_and_the_sweep_still_covers_it() {
    // #556 acceptance 3: the degraded mode is defined and works. The most
    // degraded mode available — no watcher at all — still catches every change,
    // because the sweep was the only thing making a claim in the first place.
    // This is the same experiment as the first test in this file, stated as the
    // property #556 asks for.
    let repo = repository();
    let feed = attach_with_hints(repo.path(), Hints::Suppressed);
    let mut snapshots = feed.subscribe();
    let first = next_snapshot(&mut snapshots, Duration::from_secs(5)).await;

    git(repo.path(), &["branch", "team/deep/namespace"]);
    let after = snapshot_where(&mut snapshots, Duration::from_secs(15), |s| {
        s.generation != first.generation
    })
    .await;
    assert!(
        matches!(after.health, ChangeFeedHealth::SweepOnly { .. }),
        "the coverage it is running at is named on the same snapshot"
    );
}

// --- the feed's own lifetime -----------------------------------------------

#[tokio::test]
async fn two_streams_on_one_repository_share_a_single_feed() {
    let repo = repository();
    let first = attach(repo.path());
    let second = attach(repo.path());
    assert!(
        Arc::ptr_eq(&first, &second),
        "one repository, one watcher, one sweep — never one per tab"
    );
    assert_eq!(first.repo(), repo.path());
}

#[tokio::test]
async fn the_feed_stops_when_its_last_stream_closes() {
    // Spec open question 3: nothing runs with nobody watching — no watcher, no
    // sweep, no inotify watches consumed.
    let repo = repository();
    let feed = attach(repo.path());
    let mut snapshots = feed.subscribe();
    next_snapshot(&mut snapshots, Duration::from_secs(5)).await;
    drop(feed);
    // The driver holds no strong reference of its own, so dropping the last
    // stream's handle aborts it and closes every receiver.
    let closed = tokio::time::timeout(Duration::from_secs(5), snapshots.changed()).await;
    assert!(
        matches!(closed, Ok(Err(_))),
        "the driver stopped and its senders dropped: {closed:?}"
    );
    assert!(
        existing(repo.path()).is_none(),
        "and nothing in the registry keeps it alive"
    );
}

fn head_oid(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("run git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// --- #556's measurement ----------------------------------------------------

/// Measure one sweep, and the watch set, against a repository named by
/// `GV_MEASURE_REPO`. `#[ignore]`d: it needs a repository this suite does not
/// build, and it asserts nothing about a machine it cannot see.
///
/// It exists as a **test rather than a script** so the numbers in #556's PR
/// come from the production read path — `planner::live_reading` and
/// `WatchRoots::wanted_directories` — rather than from a `git for-each-ref`
/// standing in for them. A proxy measurement is how the previous bound came to
/// be justified against a constant that was wrong by a factor of 63.
///
/// ```text
/// GV_MEASURE_REPO=/path/to/repo cargo test -p git-vista-server \
///     measure_one_sweep -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs GV_MEASURE_REPO; reports numbers rather than asserting them"]
async fn measure_one_sweep_and_the_watch_set() {
    let Ok(repo) = std::env::var("GV_MEASURE_REPO") else {
        panic!("set GV_MEASURE_REPO to the repository to measure");
    };
    let repo = PathBuf::from(repo);

    let budget = crate::watcher::budget::derive(crate::watcher::budget::InotifyLimits::read());
    println!("repository        {}", repo.display());
    println!("watch budget      {budget:?}");

    // Ten sweeps: the first is cold, the rest are what a running feed pays.
    let mut costs = Vec::new();
    for _ in 0..10 {
        let began = Instant::now();
        let reading = crate::planner::live_reading(&repo).await;
        costs.push(began.elapsed());
        assert!(
            reading.blind.is_none(),
            "the measured repository must be readable: {:?}",
            reading.blind
        );
        println!(
            "sweep             {:>8.1} ms   refs={} generation={}",
            costs.last().unwrap().as_secs_f64() * 1000.0,
            reading.refs.len(),
            reading.token.as_str()
        );
    }
    let warm: Duration = costs[1..].iter().sum::<Duration>() / (costs.len() as u32 - 1);
    let floor = warm.saturating_mul(policy::DUTY_FACTOR);
    println!(
        "warm mean         {:>8.1} ms  → duty-cycle floor {:.1} ms; the binding \
         constraint is {}",
        warm.as_secs_f64() * 1000.0,
        floor.as_secs_f64() * 1000.0,
        if floor > policy::SWEEP_BASE {
            "the sweep's own measured cost"
        } else {
            "the base interval"
        }
    );

    let mut watcher = crate::watcher::RepositoryWatcher::start(&repo);
    let health = tokio::time::timeout(Duration::from_secs(20), watcher.recv())
        .await
        .expect("the watcher reported")
        .expect("the watcher's stream stayed open");
    println!("watcher           {health:?}");
}
