//! Host tests for M12.02's native watcher.
//!
//! Kept outside `watcher.rs` so the production module contains no process
//! construction at all. The only `Command` here builds throwaway Git fixtures
//! and is reviewed by `argv_boundary`'s test-only allowlist.

use super::*;
use std::process::Command;

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
    git(temp.path(), &["config", "user.name", "Watcher Test"]);
    git(
        temp.path(),
        &["config", "user.email", "watcher@example.invalid"],
    );
    std::fs::write(temp.path().join("file"), "one\n").unwrap();
    git(temp.path(), &["add", "file"]);
    git(temp.path(), &["commit", "-qm", "base"]);
    git(temp.path(), &["branch", "packed-only"]);
    temp
}

async fn next_notice(watcher: &mut RepositoryWatcher) -> WatcherNotice {
    tokio::time::timeout(Duration::from_secs(5), watcher.recv())
        .await
        .expect("watcher did not report within five seconds")
        .expect("watcher notice stream closed silently")
}

#[tokio::test]
async fn git_pack_refs_rewrite_requests_an_authoritative_sweep() {
    let repo = repository();
    git(repo.path(), &["pack-refs", "--all"]);
    let loose_ref_files = std::fs::read_dir(repo.path().join(".git/refs/heads"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .count();
    assert_eq!(
        loose_ref_files, 0,
        "the watched refs tree must be quiet so only packed-refs can satisfy the test"
    );

    let mut watcher = RepositoryWatcher::start(repo.path());
    assert!(matches!(
        next_notice(&mut watcher).await,
        WatcherNotice::Health(WatcherHealth::Watching { watches }) if watches >= 2
    ));

    // Acceptance requires the real Git operation: touching packed-refs would
    // not prove that the watch set observes Git's rename protocol.
    // A second pack has no loose ref to remove. Git still replaces the packed
    // file, so this goes red if the root-level basename is removed from the
    // watch set even though all refs remain resolvable.
    git(repo.path(), &["pack-refs", "--all"]);

    loop {
        match next_notice(&mut watcher).await {
            WatcherNotice::Sweep => break,
            WatcherNotice::Health(WatcherHealth::Lost(loss)) => {
                panic!("watcher degraded instead of noticing pack-refs: {loss:?}")
            }
            WatcherNotice::Health(WatcherHealth::Watching { .. }) => {}
        }
    }
}

#[tokio::test]
async fn a_new_ref_namespace_is_watched_before_its_next_ref_arrives() {
    let repo = repository();
    let mut watcher = RepositoryWatcher::start(repo.path());
    assert!(matches!(
        next_notice(&mut watcher).await,
        WatcherNotice::Health(WatcherHealth::Watching { .. })
    ));

    git(repo.path(), &["branch", "nested/one"]);
    assert_eq!(next_notice(&mut watcher).await, WatcherNotice::Sweep);

    // Let any sibling events from the mkdir+write burst settle, then discard
    // them. The next ref lives below the newly-installed non-recursive watch;
    // its parent watch cannot see that file.
    tokio::time::sleep(DEBOUNCE + Duration::from_millis(50)).await;
    while watcher.notices.try_recv().is_ok() {}
    git(repo.path(), &["branch", "nested/two"]);
    assert_eq!(next_notice(&mut watcher).await, WatcherNotice::Sweep);
}

#[tokio::test]
async fn replacing_the_required_git_directory_reports_watch_loss() {
    let repo = repository();
    let mut watcher = RepositoryWatcher::start(repo.path());
    assert!(matches!(
        next_notice(&mut watcher).await,
        WatcherNotice::Health(WatcherHealth::Watching { .. })
    ));

    let watched_git_dir = repo.path().join(".git").canonicalize().unwrap();
    std::fs::rename(&watched_git_dir, repo.path().join(".git-moved"))
        .expect("replace the directory inode the backend watches");

    loop {
        match next_notice(&mut watcher).await {
            WatcherNotice::Health(WatcherHealth::Lost(WatcherLoss::WatchLost { path, .. })) => {
                assert_eq!(path, watched_git_dir);
                break;
            }
            WatcherNotice::Sweep => {}
            other => panic!("directory replacement reported the wrong state: {other:?}"),
        }
    }
}

#[test]
fn the_last_hint_moves_the_trailing_edge_without_starving_the_cap() {
    let origin = Instant::now();
    let mut debounce = DebounceWindow::default();
    debounce.hint(origin);
    debounce.hint(origin + Duration::from_millis(90));

    // Mutation proof B: stop updating `last` after the first hint. That
    // mutation fires at 100 ms and this assertion fails; the real policy waits
    // 100 ms after the final hint.
    assert!(!debounce.take_if_due(origin + Duration::from_millis(100)));
    assert!(debounce.take_if_due(origin + Duration::from_millis(190)));

    for millis in [0, 90, 180, 270, 360, 450] {
        debounce.hint(origin + Duration::from_millis(1_000 + millis));
    }
    assert!(!debounce.take_if_due(origin + Duration::from_millis(1_499)));
    assert!(debounce.take_if_due(origin + Duration::from_millis(1_500)));
}

#[test]
fn backend_death_is_loss_and_never_a_healthy_or_absent_notice() {
    let (notice_tx, mut notice_rx) = tokio_mpsc::unbounded_channel();
    report_loss(
        &notice_tx,
        WatcherLoss::Backend {
            reason: "injected backend death".into(),
        },
    );

    // Mutation proof A: report `Watching` (or report nothing) when the backend
    // dies. The former fails equality here; the latter makes `try_recv` fail,
    // differently from the debounce mutation above.
    assert_eq!(
        notice_rx.try_recv().unwrap(),
        WatcherNotice::Health(WatcherHealth::Lost(WatcherLoss::Backend {
            reason: "injected backend death".into(),
        }))
    );
}

#[tokio::test]
async fn linked_worktree_watches_private_and_common_git_directories() {
    let main = repository();
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("side");
    git(
        main.path(),
        &["worktree", "add", "-qb", "side", linked.to_str().unwrap()],
    );

    let roots = WatchRoots::resolve(&linked).unwrap();
    assert_ne!(roots.git_dir, roots.common_dir);
    let wanted = roots.wanted_directories().unwrap();
    assert!(wanted.contains(&roots.git_dir));
    assert!(wanted.contains(&roots.common_dir));
    assert!(wanted.contains(&roots.common_dir.join("refs")));

    let mut watcher = RepositoryWatcher::start(&linked);
    assert!(matches!(
        next_notice(&mut watcher).await,
        WatcherNotice::Health(WatcherHealth::Watching { .. })
    ));
    git(&linked, &["branch", "shared-hint"]);
    assert_eq!(next_notice(&mut watcher).await, WatcherNotice::Sweep);
}

#[test]
fn root_filter_includes_packed_refs_but_not_object_noise() {
    let root = Path::new("/repo/.git");
    assert!(root_entry_is_relevant(&root.join("packed-refs"), root));
    assert!(root_entry_is_relevant(&root.join("HEAD"), root));
    assert!(!root_entry_is_relevant(&root.join("objects"), root));
    assert!(!root_entry_is_relevant(
        &root.join("objects/ab/object"),
        root
    ));
}

#[cfg(unix)]
#[test]
fn refs_walk_does_not_follow_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let refs = temp.path().join("refs");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::create_dir_all(outside.join("nested")).unwrap();
    symlink(&outside, refs.join("escape")).unwrap();
    let mut found = BTreeSet::new();
    collect_real_directories(&refs, &mut found).unwrap();
    assert_eq!(found, BTreeSet::from([refs]));
}

#[cfg(unix)]
#[test]
fn a_symlinked_refs_root_is_named_as_watch_loss_not_followed() {
    use std::os::unix::fs::symlink;

    let repo = repository();
    let refs = repo.path().join(".git/refs");
    let moved = repo.path().join(".git/refs-real");
    std::fs::rename(&refs, &moved).unwrap();
    symlink(&moved, &refs).unwrap();

    let roots = WatchRoots::resolve(repo.path()).unwrap();
    assert!(matches!(
        roots.wanted_directories(),
        Err(WatcherLoss::WatchLost { path, reason })
            if path == refs && reason == "refs root is not a real directory"
    ));
}

/// PROBE (review only, not part of #655): a watched ref namespace replaced
/// **in place** by a fresh inode. This is the miss mode #552 names — "inotify
/// watches are lost on directory replacement" — and it is the one case
/// `reconcile_watches` alone cannot repair, because the path is in both
/// `wanted` and `installed` before and after, so the diff is empty and the
/// watch is left pointing at the dead inode.
#[tokio::test]
async fn a_namespace_replaced_in_place_keeps_a_live_watch() {
    let repo = repository();
    git(repo.path(), &["branch", "team/one"]);
    let heads = repo.path().join(".git/refs/heads");
    let team = heads.join("team");
    let spare = heads.join("spare");

    let mut watcher = RepositoryWatcher::start(repo.path());
    assert!(matches!(
        next_notice(&mut watcher).await,
        WatcherNotice::Health(WatcherHealth::Watching { .. })
    ));

    // One atomic syscall: `team`'s inode is unlinked and a different empty
    // directory takes its name. Nothing before or after distinguishes the two
    // by path, which is exactly why the diff-based reconcile cannot see it.
    std::fs::create_dir(&spare).unwrap();
    std::fs::remove_file(team.join("one")).unwrap();
    std::fs::rename(&spare, &team).expect("replace the namespace inode in place");

    tokio::time::sleep(MAX_DEBOUNCE_DELAY + Duration::from_millis(100)).await;
    while watcher.notices.try_recv().is_ok() {}

    // A ref inside the replaced namespace. The parent watch is non-recursive,
    // so only a live watch on `team` itself can hint this.
    std::fs::write(team.join("two"), "0000000000000000000000000000000000000000\n").unwrap();
    assert_eq!(next_notice(&mut watcher).await, WatcherNotice::Sweep);
}
