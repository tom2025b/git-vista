"""Failure-atlas entry point for #836's TLS and redaction mutations.

Failure Atlas accepts ``python3 -m unittest`` as a top-level runner, while the
repository requires every nested Cargo command to use ``buildlock``.  The atlas
holds the outer host-wide lock; this driver uses a separate inner lock file so
the child does not wait on its own parent.

#846: the redaction cases must catch removal of the refusal pass (4d134f84)
and running it after the general URL pass (c7548512). Mutate the production
``redact_bytes`` composition, leaving the assertions intact in both arms.
"""

from pathlib import Path
import os
import subprocess
import sys
import unittest


class TlsBoundaryMutations(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[4]
        cls.env = os.environ.copy()
        cls.env["GV_BUILD_SLOTS"] = "2"
        cls.env.setdefault("CARGO_TARGET_DIR", "/mnt/cargo-targets/gv-836-target")
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
        result = subprocess.run(
            ["buildlock", "cargo", command, *args],
            cwd=cls.repo,
            env=cls.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        sys.stdout.write(result.stdout)
        result.check_returncode()
        if command == "test" and "--exact" in args:
            # Cargo exits successfully even when a stale exact filter runs
            # zero tests. That cannot count as a green mutation baseline.
            if "test result: ok. 1 passed; 0 failed;" not in result.stdout:
                raise AssertionError("exact Cargo filter did not run one passing test")

    def test_path_embedded_lfs_refusal_credentials_are_redacted(self) -> None:
        self.run_cargo(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::network_exec::tests::redact_bytes_closes_the_path_embedded_credential_in_a_plaintext_lfs_refusal",
            "--",
            "--exact",
        )

    def test_embedded_scheme_separator_refusal_credentials_are_redacted(self) -> None:
        self.run_cargo(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::network_exec::tests::redact_bytes_closes_a_plaintext_lfs_refusal_credential_containing_an_embedded_scheme_separator",
            "--",
            "--exact",
        )

    def test_argv_redacts_both_lfs_refusal_credential_shapes(self) -> None:
        self.run_cargo(
            "test",
            "-p",
            "git-vista-server",
            "--bin",
            "git-vista-server",
            "sandbox::network_exec::tests::redact_args_strips_path_embedded_lfs_refusal_credentials",
            "--",
            "--exact",
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
