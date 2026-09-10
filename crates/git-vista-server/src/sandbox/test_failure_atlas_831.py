"""Failure-atlas entry point for #831's compiled boundary mutations.

The atlas accepts ``python3 -m unittest`` but does not accept ``buildlock`` as
its top-level runner. Keeping this driver beside the Rust boundary lets the
atlas use its containment while every nested Cargo command still follows the
repository's build-lock and target-directory rules.
"""

from pathlib import Path
import os
import subprocess
import unittest


class CheckoutBoundaryMutations(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[4]
        cls.env = os.environ.copy()
        cls.env["GV_BUILD_SLOTS"] = "1"
        cls.env["CARGO_TARGET_DIR"] = "/home/tom/.cargo-targets/gv-831"
        # failure-atlas already holds the host-wide buildlock around this
        # top-level unittest command. Its child closes the inherited lock fd,
        # so a nested buildlock on the same file would wait on its own parent
        # until the atlas timeout. Keep the required buildlock prefix on every
        # Cargo command, but give that re-entrant inner layer its own one-slot
        # file; the atlas parent remains the outer host-wide exclusion.
        cls.env["BUILDLOCK_FILE"] = "/tmp/git-vista-failure-atlas-inner-buildlock"
        # These stay present in the cargo-test process after the Rust test's
        # short composition guard restores its environment. Removing
        # env_clear/allowlist construction therefore produces an observable
        # leak at the actual child spawn, not an incidental config failure.
        cls.env["GV_CLONE_ENVIRONMENT_CANARY"] = "atlas-ambient-canary"
        cls.env["SSH_AUTH_SOCK"] = "/tmp/gv831-atlas-agent.sock"
        # The focused Rust tests compile the server test binary themselves.
        # Build only the two sibling executables they launch; compiling the
        # non-test server binary here as well wastes enough cold-clone time to
        # exceed failure-atlas's transport deadline.
        cls.run_buildlocked(
            "build",
            "-p",
            "git-vista-server",
            "--bin",
            "gv-sandbox",
            "--bin",
            "gv-sandbox-reaper",
        )

    @classmethod
    def run_buildlocked(cls, command: str, *args: str) -> None:
        subprocess.run(
            ["buildlock", "cargo", command, *args],
            cwd=cls.repo,
            env=cls.env,
            check=True,
        )

    def test_checkout_filter_receives_only_the_allowlisted_environment(self) -> None:
        self.run_buildlocked(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "handlers::clone::tests::clone_checkout_runs_a_filter_with_only_an_allowlisted_environment",
            "--",
            "--exact",
        )

    def test_checkout_filter_cannot_connect_to_an_agent_socket(self) -> None:
        self.run_buildlocked(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::checkout_security::a_checkout_filter_cannot_reach_an_agent_and_a_fetched_hook_does_not_run",
            "--",
            "--exact",
        )

    def test_tracked_lfsconfig_cannot_fake_a_successful_checkout(self) -> None:
        self.run_buildlocked(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::lfs::tests::tracked_lfsconfig_",
            "--",
            "--test-threads=1",
        )

    def test_checkout_output_redacts_lfs_action_queries(self) -> None:
        self.run_buildlocked(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::network_exec::tests::untrusted_checkout_output_never_exposes_an_lfs_action_query",
            "--",
            "--exact",
            "--test-threads=1",
        )


if __name__ == "__main__":
    unittest.main()
