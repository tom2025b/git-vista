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

/// Does a **private** HTTPS clone survive the production clone policy?
///
/// The public leg above proves the policy does not break HTTPS. It cannot
/// answer the separate question this test exists for: git authenticates to a
/// private remote by *executing a credential helper* — an arbitrary program
/// named by `credential.helper`, living wherever the operator installed it,
/// reading a token store somewhere under `$HOME`. A sandbox that grants the
/// network but not that program, or not its token store, produces a clone
/// that fails for a reason no amount of token plumbing in this server would
/// fix.
///
/// `network_exec`'s module doc names the credential helper as "the
/// *sanctioned* HTTPS-auth mechanism this server is meant to work with", and
/// force-disables `core.askpass` precisely so that the helper is the only
/// door. Whether that door is actually open under the sandbox is a
/// measurement, and until this test was written nobody had taken it.
///
/// # No repository name in source
///
/// The URL comes from `GIT_VISTA_LIVE_PRIVATE_URL` and the test skips when it
/// is unset. A private repository's name is the operator's business, and a
/// literal here would also pin the test to one account — the same reason the
/// feature it measures must not carry a repo list.
#[tokio::test]
#[ignore = "reaches the network and needs a private repo; run with --ignored"]
async fn a_private_https_fetch_completes_through_the_production_clone_policy() {
    let Ok(url) = std::env::var("GIT_VISTA_LIVE_PRIVATE_URL") else {
        eprintln!(
            "SKIP: set GIT_VISTA_LIVE_PRIVATE_URL to a private repo's HTTPS \
             URL to run this"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Baseline leg: unsandboxed, through whatever credential helper the
    // operator has. If this fails, the host cannot demonstrate the premise
    // and the sandboxed leg would be blaming the wrong thing.
    let base = std::process::Command::new("git")
        .current_dir(root)
        .args(["ls-remote", "--heads", &url])
        .output()
        .expect("could not spawn git at all");
    if !base.status.success() {
        panic!(
            "premise not established: the UNSANDBOXED ls-remote failed, so \
             this host has no working credential path to that repository at \
             all — status {:?}, stderr:\n{}",
            base.status.code(),
            crate::sandbox::network_exec::redact_output(base.clone())
                .stderr
                .iter()
                .map(|b| *b as char)
                .collect::<String>()
        );
    }

    // Inside leg: the same operation under the policy `handlers/clone.rs`
    // uses.
    let policy = crate::sandbox::policy_for_clone(root)
        .unwrap_or_else(|e| panic!("policy_for_clone refused to build a policy: {e}"));
    let out = crate::sandbox::spawn::command_async(&policy, root, &["ls-remote", "--heads", &url])
        .output()
        .await
        .unwrap_or_else(|e| panic!("the sandboxed ls-remote could not be spawned at all: {e}"));

    let redacted = crate::sandbox::network_exec::redact_output(out.clone());
    assert!(
        out.status.success(),
        "a PRIVATE HTTPS fetch FAILED through policy_for_clone while the \
         identical unsandboxed fetch SUCCEEDED moments earlier. The sandbox, \
         not the network and not the credentials, is what refused it — so \
         private-repo support is a sandbox-policy question before it is a \
         token-storage one.\n\
         exit: {:?}\n\
         stderr (redacted):\n{}",
        out.status.code(),
        String::from_utf8_lossy(&redacted.stderr)
    );
}
