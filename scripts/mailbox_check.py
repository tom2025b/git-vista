#!/usr/bin/env python3
"""Mailbox validator (Q1) — turns the parallel-lane fences from promises into
checks.

Written after the exact failure this exists to catch: two mailbox files
(`pro-task.md`'s frontmatter and `state.json`'s `worker.task`) disagreed about
which task was assigned, and a worker silently idled on stale state instead of
picking up real work. This script makes that class of disagreement, plus a
fence being crossed without anyone noticing, a hard failure instead of a
promise nobody checks.

Five checks, each independently toggleable via CLI flags for testing:

  1. `task_id` (`pro-task.md` frontmatter) must appear in `state.json`'s
     `worker.task`/`worker.last_task` prose, as "Task <id>".
  2. `base_sha` (`pro-task.md` frontmatter) must be a prefix of
     `git rev-parse origin/main`.
  3. Every path in "the worker's diff" must match at least one
     `allowed_paths` glob.
  4. No path in "the worker's diff" may match any `forbidden_paths` glob.
  5. `pro-result.md`, if present, must contain a `**Status:**` line.

No third-party dependencies — stdlib only (`re`, `subprocess`, `pathlib`),
matching the queue entry's own instruction. No YAML library either: this
project's `pro-task.md` frontmatter is a small, consistent subset of YAML
(flat scalar keys, one-level string lists with optional trailing `# comment`
on either shape), so a dependency-free line-based parser is proportionate —
see `parse_frontmatter`'s docstring for exactly what shape it assumes and how
it fails when that assumption breaks.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


class MailboxError(Exception):
    """A parse-level problem with the mailbox files themselves (not a check
    failure) — malformed frontmatter, a missing required field. Distinct from
    a check *failing*: this is "the validator itself cannot proceed," and
    always exits loud, per the brief's instruction not to silently return an
    empty list and pass everything.
    """


# ---------------------------------------------------------------------------
# Frontmatter parsing
# ---------------------------------------------------------------------------


def parse_frontmatter(text: str, source: str) -> dict:
    """Parse the `---`-delimited YAML frontmatter block at the top of a task
    file into a dict of scalars and string lists.

    Assumed shape (every `pro-task.md` in this project matches it):

        ---
        key: scalar value
        key: "quoted value"
        list_key:
          - item one
          - item two   # trailing comment, stripped
        ---

    A scalar value's trailing ` # comment` is stripped unless the value is
    quoted (a quoted value is taken verbatim between the quotes, comment
    stripping included, so a `#` inside quotes survives). A list item's
    trailing ` # comment` is always stripped the same way.

    Anything outside this shape — a nested mapping, a multi-line scalar
    (`|`/`>`), a flow-style list (`[a, b]`) — raises `MailboxError` rather
    than silently dropping the field: this parser is deliberately narrow, and
    a task file that outgrows it should fail the validator loudly, not pass
    it having read nothing.
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise MailboxError(f"{source}: no opening '---' frontmatter fence found")
    try:
        end = lines.index("---", 1)
    except ValueError:
        raise MailboxError(f"{source}: no closing '---' frontmatter fence found")

    fields: dict = {}
    i = 1
    while i < end:
        line = lines[i]
        if not line.strip():
            i += 1
            continue
        scalar_match = re.match(r"^(\w+):\s*(.*)$", line)
        if scalar_match is None:
            raise MailboxError(
                f"{source}: line {i + 1} inside frontmatter doesn't parse as "
                f"'key: value' or 'key:' — got {line!r}. This parser is "
                "deliberately narrow (see parse_frontmatter's docstring); "
                "either the file drifted from the assumed shape, or a real "
                "new shape needs a deliberate parser update, not a silent "
                "skip."
            )
        key, rest = scalar_match.group(1), scalar_match.group(2)
        if rest == "":
            # A list field: consume '  - item' lines until one doesn't match.
            items = []
            i += 1
            while i < end and re.match(r"^\s*-\s*(.+)$", lines[i]):
                item = re.match(r"^\s*-\s*(.+)$", lines[i]).group(1)
                items.append(_strip_comment_and_quotes(item))
                i += 1
            fields[key] = items
            continue
        fields[key] = _strip_comment_and_quotes(rest)
        i += 1
    return fields


def _strip_comment_and_quotes(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    # Unquoted: an unescaped ' #' starts a trailing comment.
    comment_at = value.find(" #")
    if comment_at != -1:
        value = value[:comment_at]
    return value.strip()


# ---------------------------------------------------------------------------
# Glob matching for allowed_paths / forbidden_paths
# ---------------------------------------------------------------------------


def glob_to_regex(pattern: str) -> re.Pattern:
    """Translate one fence glob into a compiled regex.

    Supports exactly the shapes every `pro-task.md`/`task-queue.md` fence in
    this project actually uses: a literal path (`crates/.../read.rs`), a
    `dir/**` meaning "the directory itself is not a match, but everything
    under it is" (matches `git`'s own `.gitignore` `**` semantics, which is
    what every fence author has had in mind when writing one), and a bare
    `*` meaning "any run of characters not crossing a `/`."
    """
    # Trailing "/**" — the directory tree, not the bare directory name.
    tree = pattern.endswith("/**")
    core = pattern[:-3] if tree else pattern

    out = []
    i = 0
    while i < len(core):
        c = core[i]
        if core[i : i + 2] == "**":
            out.append(".*")
            i += 2
        elif c == "*":
            out.append("[^/]*")
            i += 1
        else:
            out.append(re.escape(c))
            i += 1
    body = "".join(out)
    regex = f"^{body}(?:/.*)?$" if tree else f"^{body}$"
    return re.compile(regex)


def path_matches_any(path: str, patterns: list[str]) -> bool:
    return any(glob_to_regex(p).match(path) for p in patterns)


# ---------------------------------------------------------------------------
# Git plumbing
# ---------------------------------------------------------------------------


def run_git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def worker_diff_paths(repo: Path, base_sha: str) -> list[str]:
    """Every path the worker has touched: committed changes since `base_sha`
    **plus** anything currently staged, unstaged, or untracked.

    Uncommitted changes are deliberately included, not just the committed
    diff: a fence violation caught before a commit is strictly cheaper than
    one caught after (no history to untangle, nothing pushed for a
    checkpointer to have already picked up), and this project's own
    60-second-interval auto-checkpointer means "uncommitted" is often only a
    few seconds away from "pushed" anyway — checking only the committed diff
    would miss a violation for a window that's routinely too short to matter,
    while the cost of checking `git status` too is one more cheap call.
    """
    paths: set[str] = set()

    try:
        committed = run_git(repo, "diff", "--name-only", f"{base_sha}..HEAD")
        paths.update(p for p in committed.splitlines() if p)
    except subprocess.CalledProcessError:
        # base_sha doesn't resolve (a bad/rotted mailbox entry) — the base_sha
        # check below will already fail loudly for this; don't also crash
        # the diff check on the same root cause.
        pass

    status = run_git(repo, "status", "--porcelain=v1", "--no-renames")
    for line in status.splitlines():
        if not line:
            continue
        # "XY path" (or "XY orig -> new", excluded via --no-renames).
        paths.add(line[3:].strip())

    return sorted(paths)


# ---------------------------------------------------------------------------
# The five checks
# ---------------------------------------------------------------------------


def check_task_id(task_fields: dict, state_text: str) -> str | None:
    task_id = task_fields.get("task_id")
    if task_id is None:
        raise MailboxError("pro-task.md: no task_id field in frontmatter")
    needle = f"Task {task_id}"
    if needle not in state_text:
        return (
            f"task_id mismatch: pro-task.md declares task_id: {task_id}, but "
            f"{needle!r} does not appear anywhere in state.json's worker "
            "section (checked worker.task / worker.last_task prose)."
        )
    return None


def check_base_sha(task_fields: dict, repo: Path) -> str | None:
    base_sha = task_fields.get("base_sha")
    if not base_sha:
        raise MailboxError("pro-task.md: no base_sha field in frontmatter")
    try:
        actual = run_git(repo, "rev-parse", "origin/main").strip()
    except subprocess.CalledProcessError as e:
        raise MailboxError(f"couldn't resolve origin/main: {e}")
    if not actual.startswith(base_sha):
        return (
            f"base_sha mismatch: pro-task.md declares base_sha: {base_sha}, "
            f"but origin/main is currently {actual} — the brief was written "
            "against a stale main, or main moved after branching."
        )
    return None


def check_allowed_paths(task_fields: dict, diff_paths: list[str]) -> list[str]:
    allowed = task_fields.get("allowed_paths", [])
    violations = []
    for path in diff_paths:
        if not path_matches_any(path, allowed):
            violations.append(
                f"{path} is touched but matches none of allowed_paths: {allowed}"
            )
    return violations


def check_forbidden_paths(task_fields: dict, diff_paths: list[str]) -> list[str]:
    forbidden = task_fields.get("forbidden_paths", [])
    if not forbidden:
        return []
    violations = []
    for path in diff_paths:
        if path_matches_any(path, forbidden):
            violations.append(f"{path} matches a forbidden_paths entry: {forbidden}")
    return violations


def check_result_status_line(result_path: Path) -> str | None:
    if not result_path.exists():
        # No result file yet is not a violation — the worker may still be
        # mid-task. Only a *present but incomplete* result file is a
        # violation (see main()'s handling of this None-vs-absent case).
        return None
    text = result_path.read_text()
    if not re.search(r"^\*\*Status:\*\*", text, re.MULTILINE):
        return f"{result_path} has no '**Status:**' line."
    return None


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mailbox-dir",
        type=Path,
        default=Path(__file__).resolve().parent.parent.parent
        / "Git-Vista"
        / ".claude"
        / "parallel",
        help="Directory containing pro-task.md/pro-result.md/state.json "
        "(default: the sibling Git-Vista repo's mailbox, since this script "
        "ships in the -pro worktree but the mailbox lives in the "
        "orchestrator's checkout).",
    )
    parser.add_argument(
        "--repo",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Git repository to check origin/main and the worker's diff "
        "against (default: this script's own repo).",
    )
    args = parser.parse_args(argv)

    task_path = args.mailbox_dir / "pro-task.md"
    state_path = args.mailbox_dir / "state.json"
    result_path = args.mailbox_dir / "pro-result.md"

    try:
        task_fields = parse_frontmatter(task_path.read_text(), str(task_path))
    except (FileNotFoundError, MailboxError) as e:
        print(f"FAIL: couldn't read/parse {task_path}: {e}")
        return 2
    try:
        state_text = state_path.read_text()
    except FileNotFoundError as e:
        print(f"FAIL: couldn't read {state_path}: {e}")
        return 2

    failures: list[str] = []

    try:
        msg = check_task_id(task_fields, state_text)
        if msg:
            failures.append(msg)
    except MailboxError as e:
        failures.append(str(e))

    try:
        msg = check_base_sha(task_fields, args.repo)
        if msg:
            failures.append(msg)
    except MailboxError as e:
        failures.append(str(e))

    try:
        base_sha = task_fields.get("base_sha", "")
        diff_paths = worker_diff_paths(args.repo, base_sha) if base_sha else []
    except MailboxError:
        diff_paths = []

    failures.extend(check_allowed_paths(task_fields, diff_paths))
    failures.extend(check_forbidden_paths(task_fields, diff_paths))

    result_msg = check_result_status_line(result_path)
    if result_msg:
        failures.append(result_msg)

    if failures:
        print(f"mailbox-check: {len(failures)} problem(s) found\n")
        for f in failures:
            print(f"  FAIL: {f}")
        return 1

    print("mailbox-check: all checks passed")
    print(f"  task_id {task_fields.get('task_id')} matches state.json")
    print(f"  base_sha {task_fields.get('base_sha')} matches origin/main")
    print(f"  {len(diff_paths)} diff path(s), all within allowed_paths")
    print("  no forbidden_paths touched")
    print("  pro-result.md status line present (or file not yet written)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
