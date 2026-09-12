#!/usr/bin/env python3
"""Deterministic before/after regressions. All git writes use disposable repos;
gh is always a stub. No real PR, issue, request, or build is touched.

Run: python3 test_tooling.py [--case SUBSTRING]
The baseline lane's hardcoded discovery/body-temp paths and the baseline batch's
body-temp path are redirected into the fixture. No baseline logic is changed.
"""
import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
REAL_GIT = "/usr/bin/git"
REAL_FLOCK = "/usr/bin/flock"
REAL_JQ = "/usr/bin/jq"
REPO = "tom2025b/Git-Vista"


def call(args, **kwargs):
    return subprocess.run(args, text=True, capture_output=True, check=True, **kwargs).stdout.strip()


def event(root, value):
    with (root / "events").open("a") as stream:
        stream.write(json.dumps(value) + "\n")


def wait_file(path):
    deadline = time.monotonic() + 10
    while not path.exists():
        if time.monotonic() > deadline:
            raise RuntimeError(f"barrier timed out: {path}")
        time.sleep(0.01)


def stub():
    root = Path(os.environ["TOOLING_FIXTURE"])
    name = Path(sys.argv[0]).name
    args = sys.argv[1:]
    state = json.loads((root / "state.json").read_text())
    scenario = state.get("scenario", "")
    event(root, [name, args])
    if scenario == "check_helper_fds" and name in {"git", "gh"}:
        fds = [os.readlink(path) for path in Path("/proc/self/fd").iterdir() if path.exists()]
        assert not any(path.endswith((".batch-land.lock", ".lane-pr-request.lock")) for path in fds), fds
    if name == "git":
        if "remote" in args and "get-url" in args:
            print("https://github.com/" + REPO + ".git")
            return 0
        if "fetch" in args and os.environ.get("BLOCK_FETCH") == "1":
            (root / "fetch-entered").touch()
            wait_file(root / "fetch-release")
        if "push" in args and os.environ.get("BLOCK_PUSH") == "1":
            (root / "push-entered").touch()
            wait_file(root / "push-release")
        if "push" in args and scenario == "body_after_scan":
            state["prs"]["101"]["body"] = "Fixes:#999"
            (root / "state.json").write_text(json.dumps(state))
        if "push" in args and scenario == "head_tamper":
            wt = args[args.index("-C") + 1]
            call([REAL_GIT, "-C", wt, "checkout", "--detach", state["alternate"]])
        return subprocess.run([REAL_GIT, *args]).returncode
    if name == "jq":
        if scenario == "body_write_fail" and "-rs" in args:
            print("PARTIAL")
            return 1
        return subprocess.run([REAL_JQ, *args]).returncode
    if name == "gv-thermal":
        count_file = root / "thermal-count"
        count = int(count_file.read_text()) + 1 if count_file.exists() else 1
        count_file.write_text(str(count))
        if count == 1:
            (root / "precheck").touch()
        return 1 if (root / "hot").exists() else 0
    if name == "flock":
        if "-w" in args:
            (root / "waiting").touch()
        # exec preserves the descriptor supplied by Bash; Popen closes it.
        os.execv(REAL_FLOCK, [REAL_FLOCK, *args])
    if name != "gh":
        raise RuntimeError(name)

    def option(flag, default=None):
        return args[args.index(flag) + 1] if flag in args else default

    if args[:2] == ["pr", "view"]:
        pr = args[2]
        if pr.startswith("integration/"):
            pr = "900"
        fields = option("--json").split(",")
        if scenario == "meta_failure" and "body" in fields:
            print("simulated metadata outage", file=sys.stderr)
            return 1
        if scenario == "invalid_meta" and "body" in fields:
            print('{"title":"safe"}')
            return 0
        if scenario == "head_race" and not (root / "moved").exists():
            # A different lane pushes: do not update this checkout's tracking ref.
            call([REAL_GIT, "--git-dir", str(root / "remote.git"), "update-ref", "refs/heads/lane/101", state["alternate"]])
            state["prs"]["101"]["headRefOid"] = state["alternate"]
            (root / "state.json").write_text(json.dumps(state))
            (root / "moved").touch()
        meta = dict(state["prs"][pr])
        if scenario == "title_race" and pr == "101":
            meta["title"] = "Fixes:#999" if "headRefOid" in fields or fields == ["title"] else "safe now"
        if scenario == "count_race" and fields == ["changedFiles"]:
            meta["changedFiles"] = 1
        if scenario == "files_failure" and fields == ["files"] and option("--jq", "").startswith(".files[].path") and pr == "101":
            print("simulated files outage", file=sys.stderr)
            return 1
        if fields == ["files"]:
            meta["files"] = meta["files"][:100]
        selected = {key: meta[key] for key in fields}
        if "--jq" in args:
            result = subprocess.run([REAL_JQ, "-r", option("--jq")], input=json.dumps(selected), text=True, capture_output=True)
            print(result.stdout, end="")
            return result.returncode
        print(json.dumps(selected))
        return 0
    if args[:2] == ["pr", "list"]:
        print("[]")
        return 0
    if args[:2] in (["pr", "edit"], ["pr", "create"]):
        if scenario == "body_write_fail_old":
            print("https://github.com/" + REPO + "/pull/900")
            return 0
        if scenario == "edit_failure" or scenario == "create_failure":
            print("simulated GitHub write failure", file=sys.stderr)
            return 1
        body = Path(option("--body-file")).read_text()
        event(root, ["github-write", {"args": args, "body": body}])
        if args[:2] == ["pr", "create"] and option("--head", "").startswith("integration/"):
            branch = option("--head")
            oid = call([REAL_GIT, "--git-dir", str(root / "remote.git"), "rev-parse", branch])
            state["prs"]["900"] = {
                "number": 900, "headRefName": branch, "headRefOid": oid,
                "baseRefName": "main", "baseRefOid": state["base"],
                "title": option("--title"), "body": body, "state": "OPEN",
                "isDraft": "--draft" in args, "autoMergeRequest": None,
            }
            (root / "state.json").write_text(json.dumps(state))
        print("https://github.com/" + REPO + "/pull/900")
        return 0
    if args[:2] == ["pr", "ready"]:
        state["prs"][args[2]]["isDraft"] = False
        if scenario == "ready_race":
            state["prs"]["101"]["body"] = "Fixes:#999"
        (root / "state.json").write_text(json.dumps(state))
        return 0
    if args[:2] == ["api", "--method"] and args[2] == "PUT":
        values = [args[i + 1] for i, arg in enumerate(args[:-1]) if arg == "-f"]
        assert "sha=" + state["prs"]["900"]["headRefOid"] in values, values
        event(root, ["github-merge", args])
        print(json.dumps({"merged": scenario != "merge_denied"}))
        return 0
    raise RuntimeError(f"unimplemented gh command: {args}")


class Fixture:
    def __init__(self, scenario="", layout="normal", lightweight=False):
        self.temp = tempfile.TemporaryDirectory(prefix="daybreak-tooling-")
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.remote = self.root / "remote.git"
        self.wt = self.root / "batch-wt"
        self.lane = self.root / "gv-test"
        self.lane.mkdir()
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        for name in ("git", "gh", "gv-thermal", "flock", "jq"):
            (bin_dir / name).symlink_to(Path(__file__).resolve())
        self.env = dict(os.environ, PATH=str(bin_dir) + ":" + os.environ["PATH"],
                        TOOLING_FIXTURE=str(self.root), REPO_DIR=str(self.repo),
                        BATCH_WT=str(self.wt), TMPDIR=str(self.root),
                        BUILDLOCK_FILE=str(self.root / "slot"), BUILDLOCK_TIMEOUT="3",
                        GV_THERMAL_WAIT="0", GV_BUILD_SLOTS="1", GV_THERMAL_OFF="0")
        for name in ("DRY", "DRY_RUN", "ALLOW_CLOSING", "BATCH_ALLOW_OVERLAP", "BATCH_FORCE", "BLOCK_FETCH", "BLOCK_PUSH"):
            self.env.pop(name, None)
        if lightweight:
            self.state = {"scenario": scenario, "prs": {}}
            self.save()
            return
        call([REAL_GIT, "init", "-q", "--bare", str(self.remote)])
        call([REAL_GIT, "init", "-q", "-b", "main", str(self.repo)])
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        (self.repo / "old path.txt").write_text("\n".join(f"line {i}" for i in range(40)) + "\n")
        (self.repo / "shared.txt").write_text("\n".join(f"base {i}" for i in range(40)) + "\n")
        self.git("add", ".")
        self.git("commit", "-qm", "Initial fixture")
        base = self.git("rev-parse", "HEAD")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "-q", "origin", "main")
        prs = {}
        for p in (101, 102):
            self.git("checkout", "-qb", f"lane/{p}", base)
            if layout == "rename":
                if p == 101:
                    self.git("mv", "old path.txt", "new path.txt")
                else:
                    with (self.repo / "old path.txt").open("a") as stream:
                        stream.write("additional line\n")
            elif layout == "overlap":
                lines = (self.repo / "shared.txt").read_text().splitlines()
                lines[0 if p == 101 else -1] = f"changed {p}"
                (self.repo / "shared.txt").write_text("\n".join(lines) + "\n")
            elif layout == "many":
                if p == 101:
                    for i in range(110):
                        (self.repo / f"a{i:03d}.txt").write_text("new\n")
                lines = (self.repo / "shared.txt").read_text().splitlines()
                lines[0 if p == 101 else -1] = f"changed {p}"
                (self.repo / "shared.txt").write_text("\n".join(lines) + "\n")
            else:
                (self.repo / f"file-{p}.txt").write_text(f"source {p}\n")
            self.git("add", ".")
            self.git("commit", "-qm", f"Source {p}")
            oid = self.git("rev-parse", "HEAD")
            paths = self.git("diff", "--name-only", base + "..." + oid).splitlines()
            prs[str(p)] = {"number": p, "headRefName": f"lane/{p}", "headRefOid": oid,
                           "baseRefName": "main", "baseRefOid": base, "title": f"Source {p}",
                           "body": "Reviewed work.", "state": "OPEN", "isDraft": False,
                           "autoMergeRequest": None, "changedFiles": len(paths),
                           "files": [{"path": path} for path in paths]}
            self.git("push", "-q", "origin", f"lane/{p}")
        self.git("checkout", "-q", "lane/101")
        (self.repo / "alternate.txt").write_text("later revision\n")
        self.git("add", ".")
        self.git("commit", "-qm", "Later source revision")
        alternate = self.git("rev-parse", "HEAD")
        self.git("push", "-q", "origin", alternate + ":refs/heads/fixture-alternate")
        self.git("checkout", "-q", "main")
        self.state = {"scenario": scenario, "prs": prs, "base": base, "alternate": alternate}
        self.save()
        # Lane worktree is an independent clone; caller cwd intentionally differs.
        call([REAL_GIT, "-C", str(self.lane), "init", "-q", "-b", "lane/test"])
        call([REAL_GIT, "-C", str(self.lane), "remote", "add", "origin", str(self.remote)])

    def git(self, *args):
        return call([REAL_GIT, "-C", str(self.repo), *args])

    def save(self):
        (self.root / "state.json").write_text(json.dumps(self.state))

    def load(self):
        self.state = json.loads((self.root / "state.json").read_text())

    def events(self, name):
        path = self.root / "events"
        return [item for line in (path.read_text().splitlines() if path.exists() else [])
                if (item := json.loads(line))[0] == name]

    def request(self, text="pr: 101\ntitle: Reviewed title\nallow_closing: none\n---BODY---\nReviewed body.\n"):
        (self.lane / "PR-REQUEST.md").write_text(text)

    def command(self, tool, old, args):
        if old:
            source = (HERE / (tool + ".before")).read_text()
            source = source.replace("/home/tom/projects/gv-*", shlex.quote(str(self.root)) + "/gv-*")
            source = source.replace("/tmp/.lane-pr-body.$$", str(self.root) + "/.lane-pr-body.$$")
            source = source.replace("/tmp/batch-body-$NAME.md", str(self.root) + "/batch-body-$NAME.md")
            return ["bash", "-c", source, tool, *args]
        if tool == "lane-pr-requests":
            args = [str(self.lane), *args]
        return ["bash", str(HERE / tool), *args]

    def run(self, tool, old=False, args=(), env=None, start=False):
        cmd = self.command(tool, old, list(args))
        settings = dict(env=self.env | (env or {}), cwd=self.root, text=True,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if start:
            return subprocess.Popen(cmd, **settings)
        return subprocess.run(cmd, timeout=15, **settings)

    def close(self):
        self.temp.cleanup()


@contextlib.contextmanager
def fixture(*args, **kwargs):
    f = Fixture(*args, **kwargs)
    try:
        yield f
    finally:
        f.close()


def require(condition, detail):
    if not condition:
        raise AssertionError(detail)


def success(result):
    require(result.returncode == 0, result.stdout + result.stderr)


def refused(result):
    require(result.returncode != 0, result.stdout + result.stderr)


CASES = []


def case(name):
    def decorate(fn):
        CASES.append((name, fn))
        return fn
    return decorate


@case("04_source_body_after_scan")
def body_race(old):
    with fixture("body_after_scan") as f:
        result = f.run("batch-land", old, ["test", "101", "102"])
        if old:
            success(result)
            require(f.events("github-write"), "baseline did not publish")
        else:
            refused(result)
            require(not f.events("github-write"), "unsafe draft published")


@case("04_batch_body_at_merge")
def merge_body_race(old):
    with fixture() as f:
        success(f.run("batch-land", old, ["test", "101", "102"]))
        f.load()
        f.state["prs"]["900"]["body"] += "\nFixes:#999"
        f.save()
        if old:
            require("--merge" not in (HERE / "batch-land.before").read_text(), "baseline unexpectedly has merge gate")
        else:
            refused(f.run("batch-land", args=["--merge", "test"]))
            require(not f.events("github-merge"), "unsafe merge was sent")


@case("04_change_during_ready")
def merge_ready_race(old):
    if old:
        return "N/A: original has no merge entry point (covered by 04_batch_body_at_merge)"
    with fixture("ready_race") as f:
        success(f.run("batch-land", args=["test", "101", "102"]))
        refused(f.run("batch-land", args=["--merge", "test"]))
        require(not f.events("github-merge"), "merge after source edit during ready")


@case("05_concurrent_shared_worktree")
def concurrent(old):
    with fixture() as f:
        f.git("checkout", "-qb", "lane/103", f.state["base"])
        (f.repo / "third.txt").write_text("third source\n")
        f.git("add", ".")
        f.git("commit", "-qm", "Third source; Fixes:#991")
        third = f.git("rev-parse", "HEAD")
        f.git("push", "-q", "origin", "lane/103")
        f.git("checkout", "-q", "main")
        f.state["prs"]["103"] = f.state["prs"]["102"] | {
            "number": 103, "headRefName": "lane/103", "headRefOid": third,
            "title": "Third source", "files": [{"path": "third.txt"}], "changedFiles": 1,
        }
        f.save()
        first = f.run("batch-land", old, ["one", "101", "102"], {"BLOCK_PUSH": "1"}, start=True)
        try:
            wait_file(f.root / "push-entered")
            second = f.run("batch-land", old, ["two", "101", "103"], {"DRY": "1", "ALLOW_CLOSING": "991"})
            if old:
                success(second)
            else:
                refused(second)
                require("another batch-land run" in second.stderr, second.stderr)
        finally:
            (f.root / "push-release").touch()
            out, err = first.communicate(timeout=15)
        require(first.returncode == 0, out + err)
        pushed = call([REAL_GIT, "--git-dir", str(f.remote), "rev-parse", "integration/one"])
        has_third = subprocess.run([REAL_GIT, "-C", str(f.repo), "merge-base", "--is-ancestor", third, pushed]).returncode == 0
        require(has_third == old, "first run pushed the wrong batch")


@case("05_push_scanned_oid_after_head_tamper")
def push_oid(old):
    with fixture("head_tamper") as f:
        success(f.run("batch-land", old, ["test", "101", "102"]))
        pushed = call([REAL_GIT, "--git-dir", str(f.remote), "rev-parse", "integration/test"])
        contains_second = subprocess.run([REAL_GIT, "-C", str(f.repo), "merge-base", "--is-ancestor",
                                          f.state["prs"]["102"]["headRefOid"], pushed]).returncode == 0
        require(contains_second != old, f"wrong ancestry after tamper: {pushed}")


@case("06_head_changes_after_fetch")
def head_race(old):
    with fixture("head_race") as f:
        success(f.run("batch-land", old, ["test", "101", "102"], {"DRY": "1"}))
        built = call([REAL_GIT, "-C", str(f.wt), "rev-parse", "HEAD"])
        includes_b = subprocess.run([REAL_GIT, "-C", str(f.repo), "merge-base", "--is-ancestor",
                                    f.state["alternate"], built]).returncode == 0
        require(includes_b != old, f"wrong head pinning: {built}")


@case("07_files_api_failure")
def files_failure(old):
    with fixture("files_failure", "overlap") as f:
        result = f.run("batch-land", old, ["test", "101", "102"], {"DRY": "1"})
        if old:
            success(result)
        else:
            refused(result)
            require("SHARED:" in result.stdout, result.stdout + result.stderr)


@case("07_rename_source_overlap")
def rename(old):
    with fixture(layout="rename") as f:
        result = f.run("batch-land", old, ["test", "101", "102"], {"DRY": "1"})
        if old:
            success(result)
        else:
            refused(result)
            require("SHARED:" in result.stdout, result.stdout + result.stderr)


@case("07_new_count_snapshot_race")
def count_race(old):
    with fixture("count_race", "many") as f:
        result = f.run("batch-land", old, ["test", "101", "102"], {"DRY": "1"})
        if old:
            success(result)
        else:
            refused(result)
            require("SHARED:" in result.stdout, result.stdout + result.stderr)


@case("08_body_write_failure")
def body_failure(old):
    with fixture("body_write_fail") as f:
        # Original output redirects onto /dev/full; the new generator's jq stub
        # emits partial bytes then fails, modelling a short write/ENOSPC.
        (f.root / "batch-body-test.md").symlink_to("/dev/full")
        # Prevent the stub from reading /dev/full forever on the old create path:
        # break at gh create itself, recording that it was reached on failed body.
        f.state["scenario"] = "body_write_fail_old" if old else "body_write_fail"
        f.save()
        result = f.run("batch-land", old, ["test", "101", "102"])
        if old:
            success(result)
            require(any(e[1][:2] == ["pr", "create"] for e in f.events("gh")), "baseline never reached gh create")
        else:
            refused(result)
            require(not any(e[1][:2] == ["pr", "create"] for e in f.events("gh")), "create called after short write")
            require(not any("push" in e[1] for e in f.events("git")), "push occurred before body verification")


@case("02_new_title_snapshot_race")
def title_race(old):
    with fixture("title_race") as f:
        result = f.run("batch-land", old, ["test", "101", "102"])
        if old:
            success(result)
            require("Fixes:#999" in f.events("github-write")[0][1]["body"], "baseline did not emit unsafe old title")
        else:
            refused(result)
            require(not f.events("github-write"), "unsafe title emitted")


@case("09_failed_edit_retains_request")
def edit_failure(old):
    with fixture("edit_failure") as f:
        f.request()
        result = f.run("lane-pr-requests", old)
        if old:
            success(result)
            require((f.lane / "PR-REQUEST.md.done").exists(), "baseline did not consume request")
        else:
            refused(result)
            require((f.lane / "PR-REQUEST.md").exists(), "request lost after failed edit")
            require(not (f.lane / "PR-REQUEST.md.done").exists(), "failure receipt created")


@case("10_repo_independent_of_cwd")
def repo_scoping(old):
    with fixture() as f:
        f.request()
        success(f.run("lane-pr-requests", old))
        args = f.events("github-write")[0][1]["args"]
        require(("--repo" in args) != old, str(args))
        if not old:
            require(args[args.index("--repo") + 1] == REPO, str(args))


@case("11_omit_pr_creates")
def create(old):
    with fixture() as f:
        f.request("title: New lane work\nallow_closing: none\n---BODY---\nReviewed work.\n")
        success(f.run("lane-pr-requests", old))
        writes = f.events("github-write")
        require(bool(writes) != old, str(writes))
        if not old:
            require(writes[0][1]["args"][:2] == ["pr", "create"], str(writes))
            require((f.lane / "PR-REQUEST.md.done").exists(), "missing receipt")


def lane_gate(old, body, allow="none", title="Reviewed title"):
    with fixture() as f:
        f.request(f"pr: 101\ntitle: {title}\nallow_closing: {allow}\n---BODY---\n{body}\n")
        result = f.run("lane-pr-requests", old)
        writes = f.events("github-write")
        if old:
            require(writes, result.stdout + result.stderr)
        else:
            refused(result)
            require(not writes, result.stdout + result.stderr)


@case("12_colon_bypass")
def colon(old):
    lane_gate(old, "Fixes:#999")


@case("12_qualified_bypass")
def qualified(old):
    lane_gate(old, "Resolves owner/repo#107")


@case("12_each_match_on_same_line")
def each_match(old):
    lane_gate(old, "Fixes #206 and fixes #999", "206")


@case("12_literal_allowlist")
def literal(old):
    lane_gate(old, "Fixes #999", ".*")


@case("12_title_gate")
def lane_title(old):
    lane_gate(old, "Reviewed body.", "none", "Fixes:#999")


@case("13_missing_body_delimiter")
def delimiter(old):
    with fixture() as f:
        f.request("pr: 101\ntitle: Reviewed title\nallow_closing: none\n")
        result = f.run("lane-pr-requests", old)
        if old:
            success(result)
            require(f.events("github-write")[0][1]["body"] == "", "baseline body not erased")
        else:
            refused(result)
            require(not f.events("github-write"), "malformed request edited GitHub")


@case("14_dry_alias")
def dry_alias(old):
    with fixture() as f:
        f.request()
        success(f.run("lane-pr-requests", old, env={"DRY": "1", "DRY_RUN": "0"}))
        require(bool(f.events("github-write")) == old, "DRY did not suppress live write")
        require((f.lane / "PR-REQUEST.md").exists() != old, "DRY consumed request")


@case("15_thermal_after_slot_wait")
def thermal_after_wait(old):
    with fixture(lightweight=True) as f:
        with (f.root / "slot.1").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            process = f.run("buildlock", old, ["bash", "-c", "printf started"], start=True)
            wait_file(f.root / "waiting")
            require((f.root / "precheck").exists(), "no initial thermal check")
            (f.root / "hot").touch()
            fcntl.flock(lock, fcntl.LOCK_UN)
            out, err = process.communicate(timeout=10)
        if old:
            require(process.returncode == 0 and out == "started", out + err)
        else:
            require(process.returncode == 75 and out == "", out + err)


@case("verified_03_metadata_failure")
def metadata_failure(old):
    with fixture("meta_failure") as f:
        refused(f.run("batch-land", old, ["test", "101", "102"]))
        require(not f.events("github-write"), "published after failed metadata read")


@case("verified_child_streams_status_arguments")
def streams(old):
    with fixture(lightweight=True) as f:
        result = f.run("buildlock", old, ["bash", "-c", 'printf "%s" "$1"; printf diagnostic >&2; exit 37', "_", "one argument"])
        require(result.returncode == 37 and result.stdout == "one argument" and result.stderr == "diagnostic", str(result))


@case("verified_initial_thermal_gate")
def initial_thermal(old):
    with fixture(lightweight=True) as f:
        (f.root / "hot").touch()
        result = f.run("buildlock", old, ["bash", "-c", "printf started"])
        require(result.returncode == 75 and not result.stdout, str(result))
        require(not f.events("flock"), "queued while initially hot")


@case("verified_lock_held_and_not_inherited")
def fd_lifetime(old):
    with fixture(lightweight=True) as f:
        probe = ('import fcntl, os, pathlib, subprocess, sys; '
                 'p=pathlib.Path(os.environ["TOOLING_FIXTURE"]); '
                 'fds=[os.readlink(x) for x in pathlib.Path("/proc/self/fd").iterdir() if x.exists()]; '
                 'assert str(p/"slot.1") not in fds, fds; '
                 'subprocess.Popen(["sleep","0.5"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); '
                 '(p/"child-running").touch(); '
                 'import time; time.sleep(0.15)')
        process = f.run("buildlock", old, [sys.executable, "-c", probe], start=True)
        wait_file(f.root / "child-running")
        with (f.root / "slot.1").open("w") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError("parent released slot while child running")
            out, err = process.communicate(timeout=10)
            require(process.returncode == 0, out + err)
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)


@case("new_merge_positive_and_sha_condition")
def positive_merge(old):
    if old:
        return "N/A: original has no merge entry point"
    with fixture() as f:
        success(f.run("batch-land", args=["test", "101", "102"]))
        success(f.run("batch-land", args=["--merge", "test"], env={"DRY": "1"}))
        require(not f.events("github-merge"), "dry merge wrote GitHub")
        success(f.run("batch-land", args=["--merge", "test"]))
        require(len(f.events("github-merge")) == 1, "missing synchronous merge")


@case("new_invalid_metadata_fails_closed")
def invalid_metadata(old):
    if old:
        return "N/A: added strict metadata validation"
    with fixture("invalid_meta") as f:
        refused(f.run("batch-land", args=["test", "101", "102"]))
        require(not f.events("github-write"), "invalid metadata allowed a write")


@case("new_positive_literal_multiple_allowlist")
def allowed_positive(old):
    if old:
        return "N/A: original allowlist is regex-based"
    with fixture() as f:
        f.request("pr: 101\nallow_closing: 206 999\n---BODY---\nFixes #206 and fixes #999\n")
        success(f.run("lane-pr-requests"))
        require(f.events("github-write"), "declared references refused")


@case("verified_queued_child_streams_and_status")
def queued_streams(old):
    with fixture(lightweight=True) as f:
        with (f.root / "slot.1").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            process = f.run("buildlock", old, ["bash", "-c", 'printf "%s" "$1"; printf diagnostic >&2; exit 37', "_", "one argument"], start=True)
            wait_file(f.root / "waiting")
            fcntl.flock(lock, fcntl.LOCK_UN)
            out, err = process.communicate(timeout=10)
        require(process.returncode == 37 and out == "one argument" and err == "diagnostic", out + err)


@case("new_lock_descriptors_excluded_from_helpers")
def helper_fds(old):
    if old:
        return "N/A: original consumers do not hold these locks"
    with fixture("check_helper_fds") as f:
        success(f.run("batch-land", args=["test", "101", "102"]))
        f.request()
        success(f.run("lane-pr-requests"))


@case("new_merge_head_change_and_denial")
def changed_merge(old):
    if old:
        return "N/A: original has no merge entry point"
    with fixture() as f:
        success(f.run("batch-land", args=["test", "101", "102"]))
        f.load()
        saved = f.state["prs"]["900"]["headRefOid"]
        f.state["prs"]["900"]["headRefOid"] = f.state["alternate"]
        f.save()
        refused(f.run("batch-land", args=["--merge", "test"]))
        require(not f.events("github-merge"), "changed head was submitted")
        f.state["prs"]["900"]["headRefOid"] = saved
        f.state["scenario"] = "merge_denied"
        f.save()
        refused(f.run("batch-land", args=["--merge", "test"]))


@case("new_lane_create_failure_and_dry")
def create_controls(old):
    if old:
        return "N/A: original cannot create"
    with fixture("create_failure") as f:
        f.request("title: New work\nallow_closing: none\n---BODY---\nReviewed body.\n")
        success(f.run("lane-pr-requests", env={"DRY": "1"}))
        require(not f.events("github-write"), "dry create wrote to GitHub")
        refused(f.run("lane-pr-requests"))
        require((f.lane / "PR-REQUEST.md").exists(), "failed create consumed request")


@case("new_batch_create_failure")
def batch_create_failure(old):
    with fixture("create_failure") as f:
        refused(f.run("batch-land", old, ["test", "101", "102"]))
        require(not (Path(str(f.wt) + ".batch-land-state") / "test.json").exists(), "failure saved a successful manifest")


@case("new_malformed_request_variants")
def malformed_requests(old):
    if old:
        return "N/A: baseline delimiter failure covered by 13_missing_body_delimiter"
    with fixture() as f:
        for text in ("pr: 101\n---BODY---\n  \n", "pr: 101\npr: 102\n---BODY---\nBody\n", "pr: 101\n---BODY---\nBody\n---BODY---\n"):
            f.request(text)
            refused(f.run("lane-pr-requests"))
        require(not f.events("github-write"), "malformed request wrote GitHub")


def corpus():
    catch = ["Fix #104", "Closes: #106", "Fixes:#105", "Resolves owner/repo#107",
             "does not implement or close #782", "`Closes #830`", "CLOSED #99"]
    no_catch = ["see #123 for context", "refs #456", "prefix#789", "unfixed #12"]
    # Extract the actual shipped grammar, rather than duplicating it in a test.
    print("CORPUS: actual script grammars; batch old/new and lane old/new")
    for value in catch + no_catch:
        answers = []
        for tool in ("batch-land", "lane-pr-requests"):
            for old in (True, False):
                source = (HERE / (tool + (".before" if old else ""))).read_text()
                if tool == "lane-pr-requests" and old:
                    pattern = source.split("grep -inE '", 1)[1].split("'", 1)[0]
                else:
                    pattern = source.split("KW='", 1)[1].split("'", 1)[0]
                result = subprocess.run(["grep", "-iqE", pattern], input=value, text=True)
                caught = result.returncode == 0
                if not old or tool == "batch-land":
                    require(caught == (value in catch), f"{tool} {old} wrong for {value}")
                answers.append("CAUGHT" if caught else "CLEAR")
        print(f"  {value!r}: " + " / ".join(answers))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", default="")
    options = parser.parse_args()
    count = 0
    for name, fn in CASES:
        if not any(part in name for part in options.case.split(",")):
            continue
        outcomes = []
        for old in (True, False):
            detail = fn(old)
            outcomes.append(("OLD: " if old else "NEW: ") + (detail or "expected behavior reproduced"))
        print("PASS " + name + " | " + " | ".join(outcomes), flush=True)
        count += 1
    if not options.case:
        corpus()
    require(count > 0, "no cases selected")
    print(f"PASS: {count} cases", flush=True)


if __name__ == "__main__":
    if Path(sys.argv[0]).name in {"git", "gh", "gv-thermal", "flock", "jq"}:
        sys.exit(stub())
    main()
