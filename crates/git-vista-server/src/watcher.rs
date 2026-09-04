//! Native repository change hints (M12.02, #552).
//!
//! A watcher never says what changed. It only asks the later, authoritative
//! sweep to read the repository. That asymmetry is deliberate: an inotify
//! event contains a path, not Git state, and a missed event must cost latency
//! rather than correctness.
//!
//! Watches are non-recursive and directory-targeted because Git replaces
//! `HEAD`, `index`, and `packed-refs` by rename. The named set is the worktree
//! git directory, the common git directory when different, and every existing
//! directory at or below either `refs/`. The working tree is not watched in
//! this slice; timer sweeps cover it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc as tokio_mpsc;

use crate::sandbox::worktree::linked_worktree_dirs;

/// Wait this long after the last hint before requesting a sweep.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(100);
/// A continuous event burst must still request sweeps while it is running.
pub(crate) const MAX_DEBOUNCE_DELAY: Duration = Duration::from_millis(500);

/// The only claims emitted by this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatcherNotice {
    /// At least one hint arrived; the consumer must sweep to learn the truth.
    Sweep,
    /// Whether the hint source is still available. Silence is never health.
    Health(WatcherHealth),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatcherHealth {
    Watching { watches: usize },
    Lost(WatcherLoss),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatcherLoss {
    UnsupportedGeometry { reason: String },
    Backend { reason: String },
    WatchLost { path: PathBuf, reason: String },
}

/// A running native watcher and its single-consumer notice stream.
pub(crate) struct RepositoryWatcher {
    notices: tokio_mpsc::UnboundedReceiver<WatcherNotice>,
    stop: Option<mpsc::Sender<DriverInput>>,
    driver: Option<thread::JoinHandle<()>>,
}

impl RepositoryWatcher {
    /// Start watching one served worktree.
    ///
    /// Geometry and backend failures are returned through [`Self::recv`] as a
    /// `Lost` health value. Returning a quiet or already-closed stream here
    /// would make a dead watcher indistinguishable from a quiet repository.
    pub(crate) fn start(worktree: &Path) -> Self {
        let (notice_tx, notice_rx) = tokio_mpsc::unbounded_channel();
        let roots = match WatchRoots::resolve(worktree) {
            Ok(roots) => roots,
            Err(reason) => {
                let _ = notice_tx.send(WatcherNotice::Health(WatcherHealth::Lost(
                    WatcherLoss::UnsupportedGeometry { reason },
                )));
                return Self {
                    notices: notice_rx,
                    stop: None,
                    driver: None,
                };
            }
        };

        let (input_tx, input_rx) = mpsc::channel();
        let stop = input_tx.clone();
        let spawn_failure_notices = notice_tx.clone();
        let panic_notices = notice_tx.clone();
        let driver = match thread::Builder::new()
            .name("git-vista-repository-watcher".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_driver(roots, input_tx, input_rx, notice_tx);
                }));
                if result.is_err() {
                    report_loss(
                        &panic_notices,
                        WatcherLoss::Backend {
                            reason: "watcher driver panicked".into(),
                        },
                    );
                }
            }) {
            Ok(driver) => driver,
            Err(error) => {
                let _ = spawn_failure_notices.send(WatcherNotice::Health(WatcherHealth::Lost(
                    WatcherLoss::Backend {
                        reason: format!("could not start watcher driver: {error}"),
                    },
                )));
                return Self {
                    notices: notice_rx,
                    stop: None,
                    driver: None,
                };
            }
        };

        Self {
            notices: notice_rx,
            stop: Some(stop),
            driver: Some(driver),
        }
    }

    pub(crate) async fn recv(&mut self) -> Option<WatcherNotice> {
        self.notices.recv().await
    }
}

impl Drop for RepositoryWatcher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(DriverInput::Stop);
        }
        if let Some(driver) = self.driver.take() {
            let _ = driver.join();
        }
    }
}

#[derive(Debug, Clone)]
struct WatchRoots {
    git_dir: PathBuf,
    common_dir: PathBuf,
    ref_roots: BTreeSet<PathBuf>,
}

impl WatchRoots {
    fn resolve(worktree: &Path) -> Result<Self, String> {
        let (git_dir, common_dir) = match linked_worktree_dirs(worktree)? {
            Some(dirs) => (dirs.gitdir, dirs.commondir),
            None => {
                let dot_git = worktree.join(".git");
                let git_dir = dot_git.canonicalize().map_err(|error| {
                    format!(
                        "plain worktree git directory {} does not canonicalise: {error}",
                        dot_git.display()
                    )
                })?;
                if !git_dir.is_dir() {
                    return Err(format!(
                        "plain worktree git directory {} is not a directory",
                        git_dir.display()
                    ));
                }
                (git_dir.clone(), git_dir)
            }
        };

        let ref_roots = [git_dir.join("refs"), common_dir.join("refs")]
            .into_iter()
            .collect();
        Ok(Self {
            git_dir,
            common_dir,
            ref_roots,
        })
    }

    fn required_roots(&self) -> BTreeSet<PathBuf> {
        [self.git_dir.clone(), self.common_dir.clone()]
            .into_iter()
            .collect()
    }

    fn wanted_directories(&self) -> Result<BTreeSet<PathBuf>, WatcherLoss> {
        let mut wanted = self.required_roots();
        for refs in &self.ref_roots {
            let metadata = match std::fs::symlink_metadata(refs) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(WatcherLoss::WatchLost {
                        path: refs.clone(),
                        reason: format!("could not inspect refs root: {error}"),
                    })
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(WatcherLoss::WatchLost {
                    path: refs.clone(),
                    reason: "refs root is not a real directory".into(),
                });
            }
            collect_real_directories(refs, &mut wanted).map_err(|reason| {
                WatcherLoss::WatchLost {
                    path: refs.clone(),
                    reason,
                }
            })?;
        }
        Ok(wanted)
    }

    fn relevant(&self, event: &Event) -> bool {
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        event.paths.iter().any(|path| {
            self.ref_roots.iter().any(|refs| path.starts_with(refs))
                || root_entry_is_relevant(path, &self.git_dir)
                || root_entry_is_relevant(path, &self.common_dir)
        })
    }
}

fn collect_real_directories(directory: &Path, found: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    found.insert(directory.to_path_buf());
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("could not read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not read an entry in {}: {error}",
                directory.display()
            )
        })?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", entry.path().display()))?;
        // Never follow a repository-controlled symlink out of the validated
        // git/common directories.
        if file_type.is_dir() && !file_type.is_symlink() {
            collect_real_directories(&entry.path(), found)?;
        }
    }
    Ok(())
}

fn root_entry_is_relevant(path: &Path, root: &Path) -> bool {
    if path == root {
        return true;
    }
    if path.parent() != Some(root) {
        return false;
    }
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            "HEAD"
                | "index"
                | "packed-refs"
                | "MERGE_HEAD"
                | "REBASE_HEAD"
                | "ORIG_HEAD"
                | "CHERRY_PICK_HEAD"
                | "refs"
        )
    )
}

enum DriverInput {
    Notify(Result<Event, notify::Error>),
    Stop,
}

fn run_driver(
    roots: WatchRoots,
    input_tx: mpsc::Sender<DriverInput>,
    input_rx: mpsc::Receiver<DriverInput>,
    notices: tokio_mpsc::UnboundedSender<WatcherNotice>,
) {
    let callback_tx = input_tx.clone();
    let mut watcher = match notify::recommended_watcher(move |event| {
        let _ = callback_tx.send(DriverInput::Notify(event));
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            report_loss(
                &notices,
                WatcherLoss::Backend {
                    reason: error.to_string(),
                },
            );
            return;
        }
    };

    let mut installed = BTreeSet::new();
    if let Err(loss) = reconcile_watches(&roots, &mut watcher, &mut installed) {
        report_loss(&notices, loss);
        return;
    }
    if notices
        .send(WatcherNotice::Health(WatcherHealth::Watching {
            watches: installed.len(),
        }))
        .is_err()
    {
        return;
    }

    let mut debounce = DebounceWindow::default();
    loop {
        // Check before receiving. Once the cap is due, an already-backlogged
        // channel must not keep winning `recv_timeout(Duration::ZERO)` and
        // starve the sweep for as long as producers can fill the queue.
        if debounce.take_if_due(Instant::now()) {
            if notices.send(WatcherNotice::Sweep).is_err() {
                return;
            }
            continue;
        }

        let input = match debounce.due_at() {
            Some(due) => match input_rx.recv_timeout(due.saturating_duration_since(Instant::now()))
            {
                Ok(input) => Some(input),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    report_loss(
                        &notices,
                        WatcherLoss::Backend {
                            reason: "watcher input channel closed".into(),
                        },
                    );
                    return;
                }
            },
            None => match input_rx.recv() {
                Ok(input) => Some(input),
                Err(_) => {
                    report_loss(
                        &notices,
                        WatcherLoss::Backend {
                            reason: "watcher input channel closed".into(),
                        },
                    );
                    return;
                }
            },
        };

        let Some(input) = input else {
            if debounce.take_if_due(Instant::now()) && notices.send(WatcherNotice::Sweep).is_err() {
                return;
            }
            continue;
        };

        match input {
            DriverInput::Stop => return,
            DriverInput::Notify(Err(error)) => {
                // Queue overflow and backend failures are never silence. The
                // pending sweep preserves correctness; the explicit health
                // transition tells the later sweep layer not to trust hints.
                let _ = notices.send(WatcherNotice::Sweep);
                report_loss(
                    &notices,
                    WatcherLoss::Backend {
                        reason: error.to_string(),
                    },
                );
                return;
            }
            DriverInput::Notify(Ok(event)) if roots.relevant(&event) => {
                let invalidated =
                    forget_invalidated_directory_watches(&event, &mut watcher, &mut installed);
                let before = installed.len();
                if let Err(loss) = reconcile_watches(&roots, &mut watcher, &mut installed) {
                    let _ = notices.send(WatcherNotice::Sweep);
                    report_loss(&notices, loss);
                    return;
                }
                debounce.hint(Instant::now());
                if invalidated || installed.len() > before {
                    // A ref file may have been created between mkdir and this
                    // watch being installed. Sweep immediately after closing
                    // that unclosable race, as D2 requires. The same immediate
                    // sweep follows a directory watch invalidated by rename.
                    debounce.clear();
                    if notices.send(WatcherNotice::Sweep).is_err() {
                        return;
                    }
                }
            }
            DriverInput::Notify(Ok(_)) => {}
        }
    }
}

fn forget_invalidated_directory_watches(
    event: &Event,
    watcher: &mut RecommendedWatcher,
    installed: &mut BTreeSet<PathBuf>,
) -> bool {
    if !matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) {
        return false;
    }

    let invalidated: Vec<_> = event
        .paths
        .iter()
        .filter(|path| installed.contains(*path))
        .cloned()
        .collect();
    for path in &invalidated {
        // Git commonly removes now-empty namespace directories. The parent is
        // still watched, so forgetting this dead inode preserves coverage and
        // lets a later CREATE install a watch on the replacement.
        let _ = watcher.unwatch(path);
        installed.remove(path);
    }
    !invalidated.is_empty()
}

fn reconcile_watches(
    roots: &WatchRoots,
    watcher: &mut RecommendedWatcher,
    installed: &mut BTreeSet<PathBuf>,
) -> Result<(), WatcherLoss> {
    let wanted = roots.wanted_directories()?;
    for required in roots.required_roots() {
        if !wanted.contains(&required) || !required.is_dir() {
            return Err(WatcherLoss::WatchLost {
                path: required,
                reason: "required watched directory no longer exists".into(),
            });
        }
    }

    // A vanished optional refs namespace is still covered by its watched
    // parent. Forget its old inode so a later CREATE installs a fresh watch.
    let removals: Vec<_> = installed.difference(&wanted).cloned().collect();
    for path in removals {
        let _ = watcher.unwatch(&path);
        installed.remove(&path);
    }
    let additions: Vec<_> = wanted.difference(installed).cloned().collect();
    for path in additions {
        watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(|error| WatcherLoss::WatchLost {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        installed.insert(path);
    }
    Ok(())
}

fn report_loss(notices: &tokio_mpsc::UnboundedSender<WatcherNotice>, loss: WatcherLoss) {
    let _ = notices.send(WatcherNotice::Health(WatcherHealth::Lost(loss)));
}

/// Pure debounce policy: trailing edge, capped from the first hint.
#[derive(Debug, Default)]
struct DebounceWindow {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl DebounceWindow {
    fn hint(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn due_at(&self) -> Option<Instant> {
        Some(std::cmp::min(
            self.last?.checked_add(DEBOUNCE)?,
            self.first?.checked_add(MAX_DEBOUNCE_DELAY)?,
        ))
    }

    fn take_if_due(&mut self, now: Instant) -> bool {
        if self.due_at().is_some_and(|due| now >= due) {
            self.clear();
            true
        } else {
            false
        }
    }

    fn clear(&mut self) {
        self.first = None;
        self.last = None;
    }
}

#[cfg(test)]
#[path = "watcher/suite.rs"]
mod suite;
