#!/usr/bin/env python3
"""Self-test for `mailbox_check.py` — stdlib `unittest` only, no pytest
dependency. Proves each of the five checks actually fires on a deliberately
broken fixture and stays silent on a clean one; a validator with no proof it
validates anything is exactly the kind of unchecked promise Q1 exists to
replace.

Run directly: `python3 scripts/mailbox-check-fixtures/test_mailbox_check.py`
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import mailbox_check as mc  # noqa: E402


def git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    )


def init_repo(repo: Path) -> str:
    """A minimal real git repo with one commit, returning its full sha —
    every check that touches git needs a real repository, not a mock, since
    the point is proving the validator's git plumbing works, not just its
    string logic.
    """
    repo.mkdir(parents=True, exist_ok=True)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "t@example.invalid")
    git(repo, "config", "user.name", "t")
    (repo / "seed.txt").write_text("seed\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", "seed")
    # Fake an "origin/main" ref pointing at the same commit — no real remote
    # needed for `git rev-parse origin/main` to resolve.
    sha = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    git(repo, "update-ref", "refs/remotes/origin/main", sha)
    return sha


class FrontmatterParsing(unittest.TestCase):
    def test_scalars_and_lists_with_comments(self):
        text = (
            '---\n'
            'task_id: 7\n'
            'base_sha: abc1234\n'
            'title: "a quoted # value"\n'
            'allowed_paths:\n'
            '  - crates/a/b.rs   # a comment\n'
            "  - crates/c/**\n"
            "forbidden_paths:\n"
            "  - docs/adr/**\n"
            "---\n"
            "# body, ignored\n"
        )
        fields = mc.parse_frontmatter(text, "test")
        self.assertEqual(fields["task_id"], "7")
        self.assertEqual(fields["base_sha"], "abc1234")
        self.assertEqual(fields["title"], "a quoted # value")
        self.assertEqual(fields["allowed_paths"], ["crates/a/b.rs", "crates/c/**"])
        self.assertEqual(fields["forbidden_paths"], ["docs/adr/**"])

    def test_missing_closing_fence_raises(self):
        with self.assertRaises(mc.MailboxError):
            mc.parse_frontmatter("---\ntask_id: 1\n", "test")

    def test_unparseable_line_raises_rather_than_silently_dropping(self):
        with self.assertRaises(mc.MailboxError):
            mc.parse_frontmatter("---\nnested:\n  deeper: value\n---\n", "test")


class GlobMatching(unittest.TestCase):
    def test_literal_path(self):
        self.assertTrue(mc.path_matches_any("a/b.rs", ["a/b.rs"]))
        self.assertFalse(mc.path_matches_any("a/c.rs", ["a/b.rs"]))

    def test_tree_glob(self):
        patterns = ["crates/git-vista-git/**"]
        self.assertTrue(mc.path_matches_any("crates/git-vista-git/src/lib.rs", patterns))
        self.assertTrue(
            mc.path_matches_any("crates/git-vista-git/src/deep/nested.rs", patterns)
        )
        self.assertFalse(mc.path_matches_any("crates/git-vista-server/src/lib.rs", patterns))

    def test_single_star_does_not_cross_slash(self):
        self.assertTrue(mc.path_matches_any("docs/RELEASE_GATES.md", ["docs/*.md"]))
        self.assertFalse(mc.path_matches_any("docs/adr/0001.md", ["docs/*.md"]))


class TaskIdCheck(unittest.TestCase):
    def test_mismatch_is_reported(self):
        msg = mc.check_task_id({"task_id": "9"}, '{"worker": {"task": "Task 8 — thing"}}')
        self.assertIsNotNone(msg)
        self.assertIn("9", msg)

    def test_match_passes(self):
        msg = mc.check_task_id(
            {"task_id": "9"}, '{"worker": {"task": "Task 9 — the real thing"}}'
        )
        self.assertIsNone(msg)

    def test_missing_task_id_field_raises(self):
        with self.assertRaises(mc.MailboxError):
            mc.check_task_id({}, "anything")


class BaseShaCheck(unittest.TestCase):
    def test_mismatch_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            init_repo(repo)
            msg = mc.check_base_sha({"base_sha": "0000000"}, repo)
            self.assertIsNotNone(msg)

    def test_correct_prefix_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            sha = init_repo(repo)
            msg = mc.check_base_sha({"base_sha": sha[:7]}, repo)
            self.assertIsNone(msg)


class PathFenceChecks(unittest.TestCase):
    def test_allowed_paths_violation(self):
        fields = {"allowed_paths": ["scripts/mailbox-check.sh"]}
        violations = mc.check_allowed_paths(fields, ["crates/git-vista-git/src/lib.rs"])
        self.assertEqual(len(violations), 1)

    def test_allowed_paths_clean(self):
        fields = {"allowed_paths": ["scripts/mailbox-check.sh"]}
        violations = mc.check_allowed_paths(fields, ["scripts/mailbox-check.sh"])
        self.assertEqual(violations, [])

    def test_forbidden_paths_violation(self):
        fields = {"forbidden_paths": ["crates/git-vista-server/src/git_cmd.rs"]}
        violations = mc.check_forbidden_paths(
            fields, ["crates/git-vista-server/src/git_cmd.rs"]
        )
        self.assertEqual(len(violations), 1)

    def test_forbidden_paths_clean(self):
        fields = {"forbidden_paths": ["crates/git-vista-server/src/git_cmd.rs"]}
        violations = mc.check_forbidden_paths(fields, ["scripts/mailbox-check.sh"])
        self.assertEqual(violations, [])


class ResultStatusLineCheck(unittest.TestCase):
    def test_missing_status_line_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "pro-result.md"
            p.write_text("# result\n\nno status line here\n")
            self.assertIsNotNone(mc.check_result_status_line(p))

    def test_present_status_line_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "pro-result.md"
            p.write_text("# result\n\n**Status:** done\n")
            self.assertIsNone(mc.check_result_status_line(p))

    def test_absent_file_is_not_a_violation(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "pro-result.md"  # never created
            self.assertIsNone(mc.check_result_status_line(p))


class EndToEndViaMain(unittest.TestCase):
    """Drives `main()` itself against a fully constructed fixture mailbox +
    repo — the same seam a real invocation uses, not just the individual
    check functions in isolation.
    """

    def _write_mailbox(
        self, mailbox: Path, task_id: str, base_sha: str, worker_task_line: str
    ) -> None:
        mailbox.mkdir(parents=True, exist_ok=True)
        (mailbox / "pro-task.md").write_text(
            "---\n"
            f"task_id: {task_id}\n"
            f"base_sha: {base_sha}\n"
            "allowed_paths:\n"
            "  - allowed.txt\n"
            "forbidden_paths:\n"
            "  - forbidden.txt\n"
            "---\n"
        )
        (mailbox / "state.json").write_text(f'{{"worker": {{"task": "{worker_task_line}"}}}}')

    def test_clean_mailbox_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            sha = init_repo(repo)
            mailbox = Path(tmp) / "mailbox"
            self._write_mailbox(mailbox, "3", sha[:7], "Task 3 — a clean thing")
            (repo / "allowed.txt").write_text("ok\n")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "allowed change")

            code = mc.main(["--mailbox-dir", str(mailbox), "--repo", str(repo)])
            self.assertEqual(code, 0)

    def test_dirty_mailbox_exits_nonzero(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            sha = init_repo(repo)
            mailbox = Path(tmp) / "mailbox"
            # task_id mismatch AND a forbidden path touched.
            self._write_mailbox(mailbox, "5", sha[:7], "Task 6 — the wrong task")
            (repo / "forbidden.txt").write_text("nope\n")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "forbidden change")

            code = mc.main(["--mailbox-dir", str(mailbox), "--repo", str(repo)])
            self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
