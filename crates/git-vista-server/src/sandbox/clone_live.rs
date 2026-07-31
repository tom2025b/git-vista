//! A real `git clone` over HTTPS, through the real production
//! `policy_for_clone` launcher.
//!
//! # Why this file exists
//!
//! `documented_gaps.rs` records that `policy_for_clone` — the policy behind
//! `POST /api/clone` — is "an ordinary, production-reachable, attacker-facing
//! spawn site that nobody has written an `EscapeCase` for yet". This is not
//! that `EscapeCase`; containment is a separate question. This is the more
//! basic one that gap left unasked: **does a clone through that policy
//! succeed at all?**
//!
//! It was written after a human drive of the app on 2026-07-31 reported that
//! `POST /api/clone` hangs on "Cloning…" for any public repository, with no
//! `[/api/clone] cloning …` line ever reaching the server log. Every existing
//! test of this path is structural — `argv.rs` proves `policy_for_clone`
//! *populates* the right grants, `hook_mode_suite.rs` proves it spells its
//! hook mode. None of them ever ran a clone. A policy can be structurally
//! perfect and still deny something git needs.
//!
//! # Network, and what that means for CI
//!
//! This test reaches the public internet, so it is `#[ignore]`d by default and
//! run explicitly. That is a deliberate trade and it is the *weak* part of this
//! file: an ignored test proves nothing on a run that skips it. It is still
//! worth having, because the failure it exists to catch is invisible to every
//! offline test in the crate — and because "run this one command to reproduce
//! the user's bug" is worth more than nothing, which is what the path has now.

use std::path::Path;

/// The paired baseline: the same clone, same URL, same destination shape,
/// **unsandboxed**. Without it a failure below is unattributable — a network
/// outage, a moved repository and a broken sandbox policy all look identical.
fn baseline_clone_succeeds(url: &str, dest: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(dest)
        .output()
        .map_err(|e| format!("could not spawn git at all: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "baseline (UNSANDBOXED) clone failed, so this host cannot demonstrate \
         the premise — status {:?}, stderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    ))
}

#[tokio::test]
#[ignore = "reaches the public internet; run with --ignored"]
async fn a_public_https_clone_completes_through_the_production_clone_policy() {
    const URL: &str = "https://github.com/tom2025b/remind";

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Baseline leg first. If this fails the test says so and stops, rather
    // than blaming the sandbox for a network problem.
    let base_dest = root.join("baseline");
    if let Err(why) = baseline_clone_succeeds(URL, &base_dest) {
        panic!("premise not established: {why}");
    }

    // Inside leg: the exact call `handlers/clone.rs::clone_repo` makes.
    let policy = crate::sandbox::policy_for_clone(root)
        .unwrap_or_else(|e| panic!("policy_for_clone refused to build a policy: {e}"));

    let dest = root.join("sandboxed");
    let dest_str = dest.to_string_lossy().to_string();
    let out = crate::sandbox::spawn::command_async(
        &policy,
        root,
        &["clone", "--depth", "1", URL, &dest_str],
    )
    .output()
    .await
    .unwrap_or_else(|e| panic!("the sandboxed clone could not be spawned at all: {e}"));

    assert!(
        out.status.success(),
        "a public HTTPS clone FAILED through policy_for_clone while the identical \
         unsandboxed clone SUCCEEDED moments earlier — so this is the sandbox policy, \
         not the network or the repository.\n\
         exit: {:?}\n\
         stdout:\n{}\n\
         stderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        dest.join(".git").is_dir(),
        "the sandboxed clone reported success but produced no .git at {} — a clone \
         that exits 0 having written nothing is the silent-no-op shape this project \
         keeps finding, so it is asserted rather than assumed",
        dest.display()
    );
}
