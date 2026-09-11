"""Failure-atlas entry point for #836's compiled TLS-boundary mutations.

Failure Atlas accepts ``python3 -m unittest`` as a top-level runner, while the
repository requires every nested Cargo command to use ``buildlock``.  The atlas
holds the outer host-wide lock; this driver uses a separate inner lock file so
the child does not wait on its own parent.
"""

from pathlib import Path
import os
import subprocess
import unittest


class TlsBoundaryMutations(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[4]
        cls.env = os.environ.copy()
        cls.env["GV_BUILD_SLOTS"] = "2"
        cls.env["CARGO_TARGET_DIR"] = "/mnt/cargo-targets/gv-836-target"
        cls.env["BUILDLOCK_FILE"] = "/tmp/git-vista-failure-atlas-836-inner-buildlock"
        cls.run_cargo(
            "build",
            "-p",
            "git-vista-server",
            "--bin",
            "gv-sandbox",
            "--bin",
            "gv-sandbox-reaper",
        )

    @classmethod
    def run_cargo(cls, command: str, *args: str) -> None:
        subprocess.run(
            ["buildlock", "cargo", command, *args],
            cwd=cls.repo,
            env=cls.env,
            check=True,
        )

    def test_original_plaintext_clone_endpoint_is_rejected(self) -> None:
        self.run_cargo(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "handlers::clone::tests::clone_endpoint_rejects_plaintext_http_even_on_port_443",
            "--",
            "--exact",
        )

    def test_direct_plaintext_lfs_object_action_is_rejected(self) -> None:
        self.run_cargo(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::lfs::tests::an_https_lfs_batch_cannot_select_a_direct_plaintext_object_action",
            "--",
            "--exact",
            "--test-threads=1",
        )


if __name__ == "__main__":
    unittest.main()
