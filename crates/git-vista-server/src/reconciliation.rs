//! The repository change feed's driver (M12.03–M12.06, #553–#556; ADR 0094).
//!
//! # One sentence
//!
//! A watcher says *when it is worth looking*; a sweep looks, and a sweep is the
//! only thing here that ever says what is true.
//!
//! # Why that asymmetry, rather than believing the watcher
//!
//! An inotify event carries a path and a bitmask — never a value. Any design
//! that acts on the path alone has invented a claim, and the moment it is wrong
//! it is wrong in the direction that *loses* a change rather than delaying one.
//! With the sweep as the sole authority, three defects the obvious design has
//! simply do not exist here:
//!
//! - a **missed** hint costs latency and nothing else — the next sweep reads the
//!   world regardless, and there is no class of change the system can be taught
//!   to ignore because it is never taught about changes at all;
//! - a **spurious** hint costs one cheap read;
//! - inotify **queue overflow** is harmless — "you missed some events" is what a
//!   hint already is.
//!
//! It is also why the sweep's debounce is safe, and why the watcher can be
//! bounded (#556) or lost entirely without any statement on this feed becoming
//! untrue. Only *later*.
//!
//! # The self-write mechanism, which is a value and not a flag (#554)
//!
//! There is no deduplicator here. No "ignore the next event", no suppression
//! window, no matching of an event against a pending write. There is one value
//! per feed — the generation it last **published** — and a sweep publishes when
//! what it read differs from it.
//!
//! > `published` is written by, and only by, the act of publishing. It never
//! > records "what I wrote"; it records "what I last told every open stream".
//!
//! That invariant is what makes the mechanism unstuckable, and the four reasons
//! are independent of each other:
//!
//! 1. it is a **value, not a mode** — there is no state in which the feed is
//!    "suppressing", and every sweep does the same comparison;
//! 2. a write that **panics** never reaches the publish, so `published` keeps an
//!    older value and the next sweep publishes — the failure mode is one extra
//!    read, never a swallowed change;
//! 3. there is **no write window to be inside of**, so it does not matter how
//!    long a write took or whether an external change overlapped it;
//! 4. the one way to swallow a real change is for that change to produce the
//!    state already on screen — which ADR 0001 settled is the right answer.
//!
//! `refuse_if_git_busy`'s own doc in `coordinator.rs` records what the
//! alternative costs: a flag-shaped claim that, once true, "could never become
//! false again: every following request against the repository was refused,
//! **forever**". That defect shipped in this codebase, in a neighbouring file.
//!
//! # Nothing runs with nobody watching
//!
//! A feed exists only while at least one client stream holds it (spec open
//! question 3). No stream, no watcher, no sweep, no inotify watches consumed —
//! which is also the cheapest possible answer to #556. The cost of that choice
//! is stated rather than hidden: a tab that reconnects after an hour learns the
//! current state from its first snapshot, and the server has no history of what
//! it missed in between, so "three things changed while you were away" is not
//! offerable.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use git_vista_protocol::change_feed::{ChangeFeedSnapshot, WatchBudget, WatcherLoss};
use git_vista_protocol::UnixSeconds;
use tokio::sync::{mpsc, oneshot, watch};

use crate::watcher::{RepositoryWatcher, WatcherHealth, WatcherNotice};

#[path = "reconciliation/policy.rs"]
pub(crate) mod policy;

use policy::{FeedPolicy, SweepOutcome, SweepReading, SweepTrigger, WatcherState};

/// How long the driver waits for the watcher's first health notice before
/// stating its absence.
///
/// The watcher reports as soon as it has installed its watch set, so this is
/// generous. What it must not do is default to `Watching`: "I have not heard
/// from the watcher" is not "the watcher is fine", and a feed that guesses here
/// is the failure this milestone exists to prevent, aimed at itself.
const WATCHER_START_WINDOW: Duration = Duration::from_secs(2);

/// How long a write will wait for the feed to catch up before answering anyway.
///
/// The wait exists so a client that has seen a write's response is not then
/// told about that write as though it were news. It is bounded because the
/// alternative — a wedged driver holding every write's response open — is worse
/// than a feed that is briefly behind.
const WRITE_SWEEP_TIMEOUT: Duration = Duration::from_secs(5);

/// One repository's live change feed.
///
/// Held by every open stream. When the last one drops, [`Drop`] stops the
/// driver — which stops the watcher, which releases its inotify watches.
pub(crate) struct Feed {
    repo: PathBuf,
    snapshots: watch::Sender<Option<ChangeFeedSnapshot>>,
    /// Asks the driver to sweep now and **say when it has**, without saying
    /// anything about the repository. Used by this process's own writes (#554)
    /// — a nudge, exactly like a watcher hint, because a write is not evidence
    /// either.
    ///
    /// It carries a one-shot rather than being a bare notify because "the sweep
    /// you asked for has finished" and "something was published" are different
    /// facts, and the caller needs the first (#664 review, finding 4). A sweep
    /// that correctly publishes nothing — because the watcher already announced
    /// this write's generation — must still complete the write's response.
    wake: mpsc::UnboundedSender<oneshot::Sender<()>>,
    /// Every read this feed's driver has made, and how long they took in total.
    ///
    /// Two of this milestone's properties are about how often the repository is
    /// read, and neither is visible in any published value: a retired watcher
    /// must not become a read storm, and a burst of hints must not outrun the
    /// duty floor. Both were reproduced by measuring reads, so both are pinned
    /// by measuring reads.
    ///
    /// The *count* is the right instrument for the first and the wrong one for
    /// the second. The duty bound is a share of wall-clock time, not a number
    /// of reads — a cheap read may happen far more often than an expensive one
    /// and still be within budget, which is the whole point of a
    /// self-calibrating floor. So the second is asserted on occupancy.
    reads: Arc<ReadMeter>,
    driver: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Feed {
    /// A receiver of every published snapshot, starting with whatever has been
    /// published so far (`None` until the first sweep completes).
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<ChangeFeedSnapshot>> {
        self.snapshots.subscribe()
    }

    pub(crate) fn repo(&self) -> &Path {
        &self.repo
    }

    /// How many times this feed has read the repository.
    pub(crate) fn reads(&self) -> u64 {
        self.reads.count.load(Ordering::Relaxed)
    }

    /// How long those reads took in total — the numerator of the duty cycle.
    pub(crate) fn read_time(&self) -> Duration {
        Duration::from_nanos(self.reads.nanos.load(Ordering::Relaxed))
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.lock().expect("feed driver slot").take() {
            driver.abort();
        }
    }
}

/// What one feed has spent reading the repository.
#[derive(Default)]
pub(crate) struct ReadMeter {
    count: AtomicU64,
    nanos: AtomicU64,
}

type Registry = StdMutex<HashMap<PathBuf, Weak<Feed>>>;

fn registry() -> &'static Registry {
    static FEEDS: OnceLock<Registry> = OnceLock::new();
    FEEDS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Join this repository's feed, starting it if nobody is on it yet.
///
/// The returned `Arc` is what keeps the feed alive: hold it for as long as the
/// stream is open and drop it when the stream closes.
pub(crate) fn attach(repo: &Path) -> Arc<Feed> {
    attach_with_hints(repo, Hints::Native)
}

/// Where the driver's hints come from.
///
/// [`Hints::Suppressed`] exists for #553's acceptance criterion, which requires
/// that a change the watcher missed be **caught by the sweep, proven by a test
/// that suppresses the watcher rather than by waiting**. A test that merely
/// waits proves nothing: a hint arriving a millisecond earlier would have
/// produced the same result, so the sweep's contribution is invisible.
pub(crate) enum Hints {
    Native,
    /// No watcher at all — every publication in this configuration was produced
    /// by a sweep and by nothing else.
    #[cfg_attr(not(test), allow(dead_code))]
    Suppressed,
}

pub(crate) fn attach_with_hints(repo: &Path, hints: Hints) -> Arc<Feed> {
    let mut feeds = registry().lock().expect("change feed registry");
    // Prune while we are here: a `Weak` whose feed is gone is the only garbage
    // this map can accumulate, and the map is otherwise bounded by the catalog.
    feeds.retain(|_, feed| feed.strong_count() > 0);
    if let Some(existing) = feeds.get(repo).and_then(Weak::upgrade) {
        return existing;
    }
    let (snapshots, _) = watch::channel(None);
    let (wake, wake_rx) = mpsc::unbounded_channel();
    let reads = Arc::new(ReadMeter::default());
    let feed = Arc::new(Feed {
        repo: repo.to_path_buf(),
        snapshots: snapshots.clone(),
        wake,
        reads: Arc::clone(&reads),
        driver: StdMutex::new(None),
    });
    // The driver holds no `Arc<Feed>` — only the sender and the notify — so the
    // feed's own strong count is exactly the number of open streams.
    let driver = tokio::spawn(crate::state::inherit_selection(drive(
        repo.to_path_buf(),
        snapshots,
        wake_rx,
        reads,
        hints,
    )));
    *feed.driver.lock().expect("feed driver slot") = Some(driver);
    feeds.insert(repo.to_path_buf(), Arc::downgrade(&feed));
    feed
}

/// This repository's feed, if any stream is currently holding one.
fn existing(repo: &Path) -> Option<Arc<Feed>> {
    registry()
        .lock()
        .expect("change feed registry")
        .get(repo)
        .and_then(Weak::upgrade)
}

/// Run one of this process's own writes, then publish what it left behind
/// (#554).
///
/// # This wrapper is the mechanism, and its shape is the whole point
///
/// The publish is **after** the write and cannot be reached any other way. So:
///
/// - a write that completes publishes the state it produced — including any
///   external change that landed alongside it, because the sweep reads the
///   world rather than replaying what the app intended;
/// - a write that **panics** never reaches the publish at all, records nothing,
///   and leaves the next ordinary sweep free to publish. That is the safe
///   direction, and `a_panicking_write_leaves_the_feed_free_to_publish` in the
///   suite panics through this very function rather than reasoning about it.
///
/// With no feed running (nobody is watching) this is the write, unchanged.
pub(crate) async fn with_publish<F, T>(repo: &Path, write: F) -> T
where
    F: Future<Output = T>,
{
    let outcome = write.await;
    publish_after_write(repo).await;
    outcome
}

/// Ask this repository's feed to read and publish the state a write just left.
///
/// A nudge, not a claim: the driver sweeps and decides, exactly as it does for
/// a watcher hint. Nothing here tells the feed what changed, so there is no
/// "what I wrote" for a later reading to be compared against.
pub(crate) async fn publish_after_write(repo: &Path) {
    let Some(feed) = existing(repo) else {
        return;
    };
    let (done, wait) = oneshot::channel();
    if feed.wake.send(done).is_err() {
        // The driver is gone; there is nothing left to wait for.
        return;
    }
    // Wait for the sweep this nudge asked for to **complete**, so a write whose
    // response the client has already seen cannot be followed by a feed that
    // has not caught up yet.
    //
    // The acknowledgement is the sweep's own, not "a snapshot was published"
    // (#664 review, finding 4). Those are different facts and the difference is
    // measurable: if the native watcher already announced this write's
    // generation, the requested sweep correctly publishes nothing — and waiting
    // on a publication then held a successful write's response for the whole
    // five-second timeout. It also cut the other way, since an unrelated
    // publication could satisfy a wait it never answered.
    //
    // Still bounded: a driver that is wedged must not hold a write open.
    let _ = tokio::time::timeout(WRITE_SWEEP_TIMEOUT, wait).await;
}

/// The driver: one task per live feed.
async fn drive(
    repo: PathBuf,
    snapshots: watch::Sender<Option<ChangeFeedSnapshot>>,
    mut wake: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    reads: Arc<ReadMeter>,
    hints: Hints,
) {
    let origin = Instant::now();
    let mut policy = FeedPolicy::new();
    let mut watcher = match hints {
        Hints::Native => Some(RepositoryWatcher::start(&repo)),
        Hints::Suppressed => None,
    };
    let mut watcher_reported = false;
    let watcher_deadline = origin + WATCHER_START_WINDOW;
    // The first sweep runs immediately: a client that has just connected gets
    // an answer rather than waiting for a transition that already happened.
    let mut next_sweep = origin;
    let mut trigger = SweepTrigger::StreamOpen;
    // The earliest instant a read is permitted, whatever asks for it.
    //
    // This is the duty floor, and it lives here rather than only in the timer
    // deadline because the timer is not the only thing that can ask (#664
    // review, finding 2). Every watcher hint and every app write used to
    // schedule a read for *now*, so an ordinary burst of events bypassed the
    // "never sooner than ten times the previous read's cost" bound this
    // milestone claims — measured at 35 % read occupancy under forty tag writes
    // on a tiny repository. Hints inside the window are not dropped: they are
    // coalesced, and the read they asked for happens the moment the floor lifts.
    let mut floor_until = origin;
    // Writers waiting to be told their sweep finished. A `Vec` because two
    // writes can land inside one floor window and one read answers both.
    let mut pending_acks: Vec<oneshot::Sender<()>> = Vec::new();

    loop {
        let now = Instant::now();
        if now >= next_sweep && now >= floor_until {
            let began = Instant::now();
            let outcome = read(&repo).await;
            let cost = began.elapsed();
            reads.count.fetch_add(1, Ordering::Relaxed);
            reads
                .nanos
                .fetch_add(cost.as_nanos() as u64, Ordering::Relaxed);
            if let Some(snapshot) = policy.observe(
                millis(origin),
                UnixSeconds(crate::activity::now_secs()),
                trigger,
                outcome,
            ) {
                if snapshots.send(Some(snapshot)).is_err() {
                    // Every receiver is gone, which means every stream closed.
                    return;
                }
            }
            let finished = Instant::now();
            floor_until = finished + policy.duty_floor(cost);
            next_sweep = finished + policy.next_sweep_delay(cost);
            trigger = SweepTrigger::Timer;
            // The sweep is complete, so every writer waiting on it is answered
            // — whether or not it published anything. That distinction is the
            // whole of finding 4.
            for ack in pending_acks.drain(..) {
                let _ = ack.send(());
            }
            continue;
        }

        if !watcher_reported && Instant::now() >= watcher_deadline {
            // Stated, not assumed. The watcher may still report later, and when
            // it does the health moves again and a snapshot goes out.
            watcher_reported = true;
            policy.note_watcher(WatcherState::Lost {
                reason: WatcherLoss::Backend {
                    detail: "the watcher did not report within its start-up window".to_string(),
                },
                budget: WatchBudget::Undetermined { watches: 0 },
            });
            next_sweep = Instant::now();
            continue;
        }

        let until_sweep = next_sweep
            .max(floor_until)
            .saturating_duration_since(Instant::now());
        let until_watcher_deadline = if watcher_reported {
            Duration::MAX
        } else {
            watcher_deadline.saturating_duration_since(Instant::now())
        };
        let wait = until_sweep.min(until_watcher_deadline);

        tokio::select! {
            notice = next_notice(watcher.as_mut()) => match notice {
                Some(WatcherNotice::Sweep) => {
                    policy.note_hint(millis(origin));
                    next_sweep = Instant::now();
                    trigger = SweepTrigger::Hint;
                }
                Some(WatcherNotice::Health(health)) => {
                    watcher_reported = true;
                    policy.note_watcher(watcher_state(health));
                    // A health transition is a change every open stream must be
                    // told about, and the snapshot that carries it must carry a
                    // generation somebody just read — so it goes out through a
                    // sweep, never on its own.
                    next_sweep = Instant::now();
                }
                None => {
                    // The notice stream closed. **Retire it** (#664 review,
                    // finding 1): `recv()` on a closed channel returns `None`
                    // immediately and forever, so leaving it in the `select!`
                    // makes this arm win every iteration and schedule a read
                    // every time — measured at 41 production reads in two
                    // seconds, turning a degraded feed into a read storm.
                    watcher = None;
                    // And do not overwrite a reason the watcher already gave.
                    // A driver that reported `WatchLost` or `LimitReached` and
                    // then exited *did* report; replacing that with "stopped
                    // without reporting" loses the only useful word in it.
                    if !policy.watcher_is_lost() {
                        watcher_reported = true;
                        policy.note_watcher(WatcherState::Lost {
                            reason: WatcherLoss::Backend {
                                detail: "the watcher stopped without reporting".to_string(),
                            },
                            budget: WatchBudget::Undetermined { watches: 0 },
                        });
                        next_sweep = Instant::now();
                    }
                }
            },
            requested = wake.recv() => match requested {
                Some(ack) => {
                    pending_acks.push(ack);
                    next_sweep = Instant::now();
                    trigger = SweepTrigger::AppWrite;
                }
                // Every `Feed` holding the sender is gone, so no write can ask
                // again. The feed itself is about to be dropped with it.
                None => return,
            },
            () = tokio::time::sleep(wait) => {
                policy.settle_due(millis(origin));
            }
        }
    }
}

/// The next watcher notice, or a future that never resolves when there is no
/// watcher to hear from.
///
/// `select!` needs a branch either way; a `None` watcher must park rather than
/// resolve, or the loop would spin at the speed of the scheduler.
async fn next_notice(watcher: Option<&mut RepositoryWatcher>) -> Option<WatcherNotice> {
    match watcher {
        Some(watcher) => watcher.recv().await,
        None => std::future::pending().await,
    }
}

/// Millis since this driver started — the monotonic clock the policy reasons
/// on. Wall time never enters those decisions; `UnixSeconds` is only ever
/// carried on the wire for a human to read.
fn millis(origin: Instant) -> u64 {
    origin.elapsed().as_millis() as u64
}

/// One sweep: the authoritative read.
async fn read(repo: &Path) -> SweepOutcome {
    let reading = crate::planner::live_reading(repo).await;
    match reading.blind {
        Some(reason) => SweepOutcome::Blind { reason },
        None => SweepOutcome::Read(SweepReading {
            generation: reading.token,
            refs: reading.refs.into_iter().collect::<BTreeMap<_, _>>(),
            other: reading.other,
        }),
    }
}

/// The watcher's own vocabulary, mapped to the policy's.
///
/// The one place a `PathBuf` is turned into a wire label: see
/// [`watch_label`].
fn watcher_state(health: WatcherHealth) -> WatcherState {
    match health {
        WatcherHealth::Watching {
            installed,
            wanted,
            budget,
        } => WatcherState::Watching {
            installed,
            wanted,
            budget,
        },
        WatcherHealth::Lost(loss) => WatcherState::Lost {
            reason: wire_loss(loss),
            budget: WatchBudget::Undetermined { watches: 0 },
        },
    }
}

fn wire_loss(loss: crate::watcher::WatcherLoss) -> WatcherLoss {
    match loss {
        crate::watcher::WatcherLoss::UnsupportedGeometry { reason } => {
            WatcherLoss::Unsupported { detail: reason }
        }
        crate::watcher::WatcherLoss::Backend { reason } => WatcherLoss::Backend { detail: reason },
        crate::watcher::WatcherLoss::WatchLost { path, .. } => WatcherLoss::WatchLost {
            location: watch_label(&path),
        },
        crate::watcher::WatcherLoss::LimitReached { at } => WatcherLoss::LimitReached { at },
    }
}

/// A lost watch's directory, as something a client may be told.
///
/// Transport never learns filesystem paths (the posture `RepositoryToken` takes
/// and #657 is currently correcting elsewhere), and a watcher's diagnostic is
/// not a reason to make an exception. So the label is the tail of the path from
/// the git directory it lives under — `refs/heads/team` — and a path with no
/// recognisable git segment collapses to a shape that names nothing.
pub(crate) fn watch_label(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut found_the_boundary = false;
    for part in path.iter().rev() {
        let part = part.to_string_lossy().to_string();
        if part == ".git" || part == "worktrees" || part.ends_with(".git") {
            found_the_boundary = true;
            break;
        }
        parts.push(part);
        if parts.len() > 4 {
            break;
        }
    }
    // No recognisable git directory above it, so nothing here is known to be
    // *inside* a repository — and a "relative" label built from segments of an
    // unknown path is just a path with the front cut off. `GIT_DIR` can point
    // anywhere, so this arm is reachable, and it says nothing rather than
    // guessing how much of a filesystem path is safe to disclose.
    if !found_the_boundary || parts.is_empty() || parts.len() > 4 {
        return "a watched directory".to_string();
    }
    parts.reverse();
    parts.join("/")
}

#[cfg(test)]
#[path = "reconciliation/suite.rs"]
mod suite;
