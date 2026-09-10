"""Failure-atlas entry point for #831's two compiled boundary mutations.

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
        cls.env["GV_BUILD_SLOTS"] = "3"
        cls.env["CARGO_TARGET_DIR"] = str(cls.repo / "target/gv831-mutation")
        # These stay present in the cargo-test process after the Rust test's
        # short composition guard restores its environment. Removing
        # env_clear/allowlist construction therefore produces an observable
        # leak at the actual child spawn, not an incidental config failure.
        cls.env["GV_CLONE_ENVIRONMENT_CANARY"] = "atlas-ambient-canary"
        cls.env["SSH_AUTH_SOCK"] = "/tmp/gv831-atlas-agent.sock"
        cls.run_buildlocked("build", "-p", "git-vista-server", "--bins")

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


if __name__ == "__main__":
    unittest.main()
