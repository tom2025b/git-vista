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

    // A SECOND change, and this one is the assertion that means something.
    //
    // The first is catchable by a one-off: the driver sweeps once when its
    // watcher start-up window expires, and with hints suppressed that expiry
    // lands a couple of seconds in — so a change made immediately after the
    // stream opened is caught by a sweep that never repeats. Disarming the
    // periodic timer entirely left this test green, which is how that was
    // found: a mutation proof reporting `survived` against a test whose name
    // claims the sweep is periodic.
    //
    // By the time this second branch is made, that one-off is spent. Only a
    // sweep that keeps running can announce it.
    git(repo.path(), &["branch", "and-again-much-later"]);
    let later = snapshot_where(&mut snapshots, Duration::from_secs(20), |s| {
        s.generation != after.generation
    })
    .await;
    match later.changed {
        RefDelta::Named { refs, .. } => assert!(
            refs.iter()
                .any(|r| r.as_str() == "refs/heads/and-again-much-later"),
            "a repeating sweep names the second change too: {refs:?}"
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

// --- #556: what the bound degrades to --------------------------------------
//
// The bound's own enforcement is proven in `watcher::suite`, beside the code
// that enforces it — a mutation proof filtered to `watcher` must be able to see
// it fail.

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

// --- the resource bounds, measured rather than reasoned about --------------
//
// Every test in this section asserts a RATE. None of them can be satisfied by a
// published value, which is why the defects they pin survived a green suite,
// two reviewers reading the code, and a spec that states both bounds: the feed
// was doing exactly the right thing far too often.

/// A repository whose `refs` is a symlink — the geometry `WatchRoots` rejects,
/// so the native watcher reports a loss and its driver exits. Codex's fixture.
fn repository_the_watcher_refuses() -> tempfile::TempDir {
    let temp = repository();
    let refs = temp.path().join(".git/refs");
    let elsewhere = temp.path().join("refs-moved-aside");
    std::fs::rename(&refs, &elsewhere).expect("move the refs tree aside");
    std::os::unix::fs::symlink(&elsewhere, &refs).expect("put a symlink in its place");
    temp
}

#[tokio::test]
async fn a_watcher_whose_channel_closed_does_not_become_a_read_storm() {
    // #664 review, finding 1, measured at **41 production reads in two
    // seconds**. `recv()` on a closed channel returns `None` immediately and
    // forever, so leaving the retired watcher in the `select!` made that arm
    // win every iteration and schedule a read every time. Degrading to
    // sweep-only is supposed to cost promptness; it was costing the machine.
    let repo = repository_the_watcher_refuses();
    let feed = attach(repo.path());
    let mut snapshots = feed.subscribe();
    next_snapshot(&mut snapshots, Duration::from_secs(10)).await;

    tokio::time::sleep(Duration::from_secs(2)).await;
    let reads = feed.reads();
    assert!(
        reads <= 6,
        "a feed that cannot watch must still sweep on its own cadence, not spin: \
         {reads} reads in two seconds"
    );
}

#[tokio::test]
async fn a_watcher_that_named_its_loss_keeps_that_reason_when_its_channel_closes() {
    // The other half of finding 1. The driver reports a real loss and *then*
    // exits, so the closed channel arrives after a reason that was already
    // given. Overwriting it with "the watcher stopped without reporting" is
    // false — it did report — and it throws away the only useful word.
    let repo = repository_the_watcher_refuses();
    let feed = attach(repo.path());
    let mut snapshots = feed.subscribe();
    // Wait for the watcher's OWN reason, not for the start-up placeholder that
    // also reports `SweepOnly` while nothing has been heard yet. Matching any
    // `SweepOnly` here would catch "the watcher has not reported yet" and prove
    // nothing about what happens when it does.
    let degraded = snapshot_where(&mut snapshots, Duration::from_secs(10), |s| {
        matches!(
            &s.health,
            ChangeFeedHealth::SweepOnly {
                reason: WatcherLoss::WatchLost { .. }
            }
        )
    })
    .await;
    let ChangeFeedHealth::SweepOnly { reason } = &degraded.health else {
        unreachable!("selected on this arm one line above")
    };
    assert!(
        !format!("{reason:?}").contains("stopped without reporting"),
        "the reason the watcher gave must survive its exit: {reason:?}"
    );
    assert_eq!(
        *reason,
        WatcherLoss::WatchLost {
            location: "refs".to_string()
        },
        "and it names WHERE, as a git-dir-relative label rather than a path"
    );

    // And it must still say so after the channel has been closed for a while,
    // rather than being overwritten a beat later.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let now = snapshots.borrow_and_update().clone().expect("a reading");
    assert_eq!(
        now.health, degraded.health,
        "the named loss is the standing reason, not a value that decayed"
    );
}

#[tokio::test]
async fn a_burst_of_hints_cannot_outrun_the_duty_floor() {
    // #664 review, finding 2, measured at **35.0 % read occupancy** — forty
    // real tag writes at ~120 ms spacing produced 39 full reads. The floor was
    // applied to the timer deadline only, and every hint replaced that deadline
    // with `now`, so the bound this milestone states in its own ADR was
    // bypassed by the ordinary case.
    //
    // The assertion is on **occupancy**, not on a read count, and that
    // distinction is the point: the bound is a share of wall-clock time. A
    // cheap read may legitimately happen far more often than an expensive one.
    // Counting reads would pass or fail on how fast this box happens to be.
    let repo = repository();
    let feed = attach(repo.path());
    let mut snapshots = feed.subscribe();
    next_snapshot(&mut snapshots, Duration::from_secs(10)).await;

    let began = Instant::now();
    let before = feed.read_time();
    for tag in 0..40 {
        git(repo.path(), &["tag", &format!("burst-{tag}")]);
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    let elapsed = began.elapsed();
    let spent = feed.read_time() - before;
    let occupancy = spent.as_secs_f64() / elapsed.as_secs_f64();
    assert!(
        occupancy < 0.25,
        "the duty floor must survive hints: {:.1} % of {:.3} s spent reading \
         ({:.3} s over {} reads)",
        occupancy * 100.0,
        elapsed.as_secs_f64(),
        spent.as_secs_f64(),
        feed.reads()
    );
}

#[tokio::test]
async fn a_write_whose_sweep_publishes_nothing_still_answers_promptly() {
    // #664 review, finding 4, measured at **5.0009 s added** to a successful
    // write. `publish_after_write` waited for a *publication*, but the sweep it
    // asked for correctly publishes nothing when the native watcher has already
    // announced that generation — so the write's own response sat out the whole
    // timeout. The acknowledgement has to be the sweep's, not a snapshot's.
    //
    // Forced deterministically rather than raced: the write happens, the feed
    // is allowed to publish it, and only *then* is the wrapper's publish asked
    // for. That is the state the race produces, without depending on winning it.
    let repo = repository();
    let feed = attach(repo.path());
    let mut snapshots = feed.subscribe();
    let first = next_snapshot(&mut snapshots, Duration::from_secs(10)).await;

    git(repo.path(), &["branch", "already-announced"]);
    snapshot_where(&mut snapshots, Duration::from_secs(15), |s| {
        s.generation != first.generation
    })
    .await;

    let began = Instant::now();
    publish_after_write(repo.path()).await;
    let waited = began.elapsed();
    assert!(
        waited < Duration::from_secs(3),
        "a sweep that publishes nothing has still finished, and the write must \
         not wait out the timeout for it: waited {waited:?}"
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

// --- no path crosses the wire, whatever the geometry -----------------------

#[test]
fn a_watch_label_is_relative_to_a_git_directory_or_it_says_nothing() {
    // Transport never learns filesystem paths, and a watcher's diagnostic is
    // not a reason to make an exception. The label is built only from the
    // segments BELOW a recognised git directory.
    assert_eq!(
        watch_label(Path::new(
            "/home/someone/secret-project/.git/refs/heads/team"
        )),
        "refs/heads/team"
    );
    assert_eq!(
        watch_label(Path::new("/srv/mirror/project.git/refs/heads")),
        "refs/heads"
    );
    // The git directory itself has nothing below it to name.
    assert_eq!(
        watch_label(Path::new("/home/someone/secret-project/.git")),
        "a watched directory"
    );
    // A linked worktree's private directory keeps the desk's own name, and that
    // is deliberate: without it, a loss at one desk is indistinguishable from a
    // loss at another. A worktree name is a name the drawer already shows
    // (#548), not a filesystem path.
    assert_eq!(
        watch_label(Path::new("/home/someone/p/.git/worktrees/desk-two/refs")),
        "desk-two/refs"
    );
    // And a `GIT_DIR` pointing somewhere with no recognisable boundary
    // discloses nothing at all, rather than a path with its front cut off.
    assert_eq!(
        watch_label(Path::new("/var/lib/someone/private/refs/heads")),
        "a watched directory"
    );
    assert_eq!(watch_label(Path::new("/")), "a watched directory");
}
