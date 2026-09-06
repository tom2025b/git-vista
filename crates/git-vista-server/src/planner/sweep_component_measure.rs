//! #661 — a component breakdown of one reconciliation sweep's read path.
//!
//! `reconciliation::suite::measure_one_sweep_and_the_watch_set` measures the
//! whole of [`live_reading`] as one number. #661 asks for a breakdown by
//! component — spawn setup vs git execution vs `gix` read — and that split
//! only exists inside `planner.rs`'s private functions, so this lives here
//! (`use super::*`) rather than in `reconciliation::suite`.
//!
//! `#[ignore]`d for the same reason as its sibling: it needs a real
//! repository named by `GV_MEASURE_REPO` and reports numbers rather than
//! asserting them. Delete this file once #661 is closed — it exists to
//! produce the measurement in the issue's report, not as a standing
//! regression guard (the guard, if the fix warrants one, is a separate,
//! narrower assertion).
//!
//! ```text
//! GV_MEASURE_REPO=/path/to/repo cargo test -p git-vista-server \
//!     measure_sweep_components -- --ignored --nocapture
//! ```

use super::*;
use std::time::Instant;

async fn timed<F, Fut, T>(label: &str, warm_runs: u32, f: F) -> std::time::Duration
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    // One cold run, discarded, then the warm mean — same shape as the sibling
    // measurement's "ten sweeps, first is cold".
    let _ = f().await;
    let mut total = std::time::Duration::ZERO;
    for _ in 0..warm_runs {
        let began = Instant::now();
        let _ = f().await;
        total += began.elapsed();
    }
    let mean = total / warm_runs;
    println!("{label:<45} {:>8.2} ms", mean.as_secs_f64() * 1000.0);
    mean
}

#[tokio::test]
#[ignore = "needs GV_MEASURE_REPO; reports numbers rather than asserting them"]
async fn measure_sweep_components() {
    let Ok(repo) = std::env::var("GV_MEASURE_REPO") else {
        panic!("set GV_MEASURE_REPO to the repository to measure");
    };
    let repo = std::path::PathBuf::from(repo);
    const N: u32 = 20;

    println!(
        "repository {}                                    ({N} warm runs each)",
        repo.display()
    );
    println!();
    println!("--- observe_live_for_generation's two reads ---");
    let head_rev_parse = timed("rev_parse(HEAD)          (git spawn, sandboxed)", N, || {
        crate::git_cmd::rev_parse(&repo, "HEAD")
    })
    .await;
    let status = timed(
        "worktree_status           (git spawn, sandboxed)",
        N,
        || worktree_status(&repo),
    )
    .await;

    println!();
    println!("--- read_generation_parts's three reads ---");
    let refs = timed(
        "refs_reading              (gix open, N refs + HEAD)",
        N,
        || refs_reading(&repo),
    )
    .await;
    let stash = timed(
        "stash_digest_input        (git spawn, sandboxed)",
        N,
        || stash_digest_input(&repo),
    )
    .await;
    let merge_ff = timed(
        "merge_ff_digest_input     (git spawn, sandboxed)",
        N,
        || merge_ff_digest_input(&repo),
    )
    .await;

    let sum_of_parts = head_rev_parse + status + refs + stash + merge_ff;
    println!();
    println!(
        "sum of the five components (sequential)    {:>8.2} ms",
        sum_of_parts.as_secs_f64() * 1000.0
    );

    let whole = timed("live_reading (the whole read path)", N, || {
        crate::planner::live_reading(&repo)
    })
    .await;
    println!();
    println!(
        "concurrency saved: {:>6.2} ms ({:.0}% of the sum) — \
         observe_live_for_generation's two reads and read_generation_parts's \
         three reads run sequentially WITHIN each group via `.await` chaining, \
         but the two groups and the gix opens go through separate spawn_blocking \
         tasks, so some overlap is real, not measurement noise",
        (sum_of_parts.as_secs_f64() - whole.as_secs_f64()) * 1000.0,
        (1.0 - whole.as_secs_f64() / sum_of_parts.as_secs_f64()) * 100.0
    );

    println!();
    println!("--- for comparison: git's own commands, unsandboxed, no server involved ---");
    let raw_status = timed("git status --porcelain=v2 (raw std::process)", N, || {
        raw_git(&repo, &["status", "--porcelain=v2"])
    })
    .await;
    let raw_for_each_ref = timed("git for-each-ref          (raw std::process)", N, || {
        raw_git(&repo, &["for-each-ref"])
    })
    .await;
    println!();
    println!(
        "sandbox tax on status:      {:>6.2} ms ({:.1}x raw)",
        (status.as_secs_f64() - raw_status.as_secs_f64()) * 1000.0,
        status.as_secs_f64() / raw_status.as_secs_f64().max(0.000_001)
    );
    println!(
        "raw for-each-ref, for scale against refs_reading's {:.2} ms: {:.2} ms",
        refs.as_secs_f64() * 1000.0,
        raw_for_each_ref.as_secs_f64() * 1000.0
    );

    println!();
    println!("--- isolating refs_reading: is the cost the gix OPEN or the ref WALK? ---");
    let repo_for_open = repo.clone();
    let open_only = timed("gix::open_opts alone, no ref read", N, move || {
        let repo = repo_for_open.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                // `gix::Repository` is not `Send` (an `Rc` in its config
                // cache), so it cannot cross the `spawn_blocking` boundary —
                // drop it before returning; the `timed()` wrapper measures
                // wall time around this whole call, not what comes back.
                let _ = gix::open_opts(&repo, gix::open::Options::isolated())
                    .expect("gix open must succeed on a readable repo");
            })
            .await
            .expect("join gix open")
        }
    })
    .await;
    println!(
        "open is {:.0}% of refs_reading's {:.2} ms; the ref walk itself is the remaining {:.2} ms",
        open_only.as_secs_f64() / refs.as_secs_f64() * 100.0,
        refs.as_secs_f64() * 1000.0,
        (refs.as_secs_f64() - open_only.as_secs_f64()) * 1000.0
    );
}

/// A raw, unsandboxed spawn — `std::process::Command`, not `git_cmd`'s
/// launcher — so the sandbox's own overhead (bwrap's namespace/mount setup,
/// per #036) can be isolated from git's own execution time. Blocking, run on
/// a blocking thread so it does not stall the runtime the way a bare
/// `Command::output()` would inside an async fn.
async fn raw_git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    let repo = repo.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(&args)
            .output()
            .expect("raw git spawn")
    })
    .await
    .expect("join raw git spawn")
}
