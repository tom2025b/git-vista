# Coordinator tooling repair — 2026-09-10

Prepared and signed by **daybreak**.

The fixed scripts, unchanged original snapshots (`*.before`), executable
regressions (`test_tooling.py`), and atomic installer (`install.sh`) are kept
together here. This is a tooling review artifact; no Git-Vista PR or issue closure
is part of this work.

Findings 5–15 and both regressions introduced by the first repair have fixes with
before/after reproductions. Finding 4 has a new merge-time preflight, draft-first
creation, and a synchronous merge conditioned on the scanned head SHA. Its final
remote metadata race cannot be eliminated by these local scripts: GitHub's merge
endpoint accepts an expected head SHA but has no expected title/body version.
It must remain recorded as a limitation, not described as an atomic guarantee.
[GitHub's merge endpoint parameters](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request).

## Reproducing the checks

Run the entire harness, or append `--case` and a case name/prefix listed below.
Each test verifies both the old defect and the new intended behavior. Tests for
new functionality explicitly identify when the old script has no equivalent.
The harness creates disposable repositories, stubs every GitHub operation, and
uses small commands for thermal/lock checks. It performs no real build and writes
no real PR, issue, or lane request.

```fish
# ON TITAN
cd /home/tom/projects/git-vista/design-docs/handoffs/2026-09-10-tooling-fix
python3 test_tooling.py
```

The original lane discovery glob and original body temporary paths are redirected
to the fixture when executing the old scripts. Those are the only substitutions;
the preserved `.before` files themselves are unchanged. GitHub stubs delegate Git
operations to `/usr/bin/git` against local bare remotes. The concurrency test uses
an explicit barrier, and the thermal test holds a real flock descriptor.

Validation and installation results are recorded at the end of this report.

## Finding 4 — metadata can change after the creation gate

**Wrong:** successful creation was treated as a lasting closing-keyword approval.
The original has no merge-time command at all.

**Changed:** creation checks the source snapshots before push, after push, and
after creating a draft. It reads and scans the actual created batch metadata.
A private manifest records source metadata and exact base/head OIDs. The new
`--merge NAME` mode rereads all sources and the batch, rejects changed snapshots,
scans commit messages and current metadata, rejects changed main/batch heads and
pending auto-merge, marks the draft ready, and repeats the metadata checks directly
before a synchronous REST merge. The request supplies the exact scanned SHA and
explicit neutral merge-commit text. It never enables auto-merge or a merge queue.

**Reproduction:** `04_source_body_after_scan` changes source metadata at the push
barrier. OLD publishes successfully after its completed scan; NEW refuses before
PR creation. `04_batch_body_at_merge` creates a batch, adds an undeclared reference
to its body, and invokes the new merge preflight: NEW refuses without a merge
request. OLD already exited successfully and has no such entry point; its lack
of a later check is verified structurally. `04_change_during_ready` changes a
source during the ready operation and confirms the repeated checks refuse it.
`new_merge_positive_and_sha_condition` proves a clean merge still works and a
dry merge makes no writes. `new_merge_head_change_and_denial` tests a replaced
head and a server response with `merged: false`.

**Remaining boundary:** a remote editor can still change a title/body after the
last read but before the merge request is processed. The API cannot condition a
merge on a metadata version. Direct merges outside this tool also bypass its
checks. I did not change repository permissions, branch protections, or external
coordinator scripts to impose a broader workflow. Closing this residual race
requires a server-side/administrative control, beyond the three authorized tools.

## Finding 5 — concurrent runs share a mutable worktree

**Wrong:** one run could push another run's unscanned HEAD under its own name.

**Changed:** a nonblocking flock on the canonical worktree's sibling lock file
is held before any worktree access through completion. All GitHub/Git helpers
close that descriptor so persistent children cannot retain it. Push names the
scanned OID explicitly. Existing worktrees must belong to the expected repository
and be clean, with no unfinished merge; dirty state is left for inspection.

**Reproduction:** `05_concurrent_shared_worktree` pauses run A after its gate,
immediately before push. Run B builds a different batch containing a commit with
a closing reference allowed only for B. OLD lets B replace the shared worktree
and A pushes B's commit under A's branch; the test verifies that ancestry. NEW
refuses B at the lock, and A pushes its own batch. The separate
`05_push_scanned_oid_after_head_tamper` changes HEAD just before the push command:
OLD pushes the substituted head; NEW still pushes its previously scanned OID.
`new_lock_descriptors_excluded_from_helpers` checks child descriptors directly.

## Finding 6 — inspected files and merged commit differ

**Wrong:** mutable GitHub metadata could describe revision B while the merge
still used an earlier remote-tracking ref at revision A.

**Changed:** each source's metadata is captured in one checked response. Its
base and head are fetched and verified as commit objects. File inspection and
merge use those immutable OIDs; later metadata changes cause refusal.

**Reproduction:** `06_head_changes_after_fetch` moves the remote source from A to
B after OLD's branch fetch without updating its local tracking ref. OLD builds
A although the metadata describes B. NEW fetches and merges the captured B OID.
The test checks commit ancestry, not just logged command arguments.

## Finding 7 — incomplete/failed file enumeration and rename source paths

**Wrong:** the overlap pipeline could swallow an API failure, truncate the list
at 100 paths, omit rename source paths, and split filenames at whitespace.

**Changed:** the GitHub files/count API is no longer used. A checked local
three-dot Git diff between the pinned commits supplies the complete file set.
Rename detection is disabled so both old and new names participate. NUL-separated
paths preserve spaces and newlines. Shared paths require the existing explicit
overlap override. The old claim that disjoint files cannot interact semantically
has also been corrected.

**Reproduction:** `07_files_api_failure` makes the old overlap-specific API
request fail while two independently mergeable edits share a path: OLD silently
accepts; NEW detects the overlap from Git. `07_rename_source_overlap` renames a
file in one PR and edits its old path in the other: OLD accepts; NEW detects the
shared old path. The filenames contain spaces. `07_new_count_snapshot_race`
supplies a stale count and a 100-path list that omits the shared path from a
111-path change: OLD accepts; NEW finds the shared path beyond that boundary.

## Finding 8 — unchecked/nonunique generated body file

**Wrong:** a failed redirection/write or shared temporary pathname could still
be followed by successful PR creation.

**Changed:** a mode-private `mktemp` directory holds each run's files. One checked
JSON-to-text operation generates the body before push. The exact body file and
generated title are scanned before submission. Any generation failure exits
before push/create; all temporary output is cleaned on exit.

**Reproduction:** `08_body_write_failure` redirects OLD's body onto `/dev/full`:
the write fails, yet OLD reaches a successful stubbed create. For NEW, the body
generator emits a partial result and exits unsuccessfully, modelling a short
write; NEW refuses without push or create. Concurrent-run isolation is also
exercised by finding 5. `new_batch_create_failure` confirms failed PR creation
returns failure and saves no successful manifest.

## Finding 9 — failed edit consumes its request

**Wrong:** the GitHub pipeline's nonzero status was ignored; the request became
`.done` and the script reported success.

**Changed:** the GitHub command's result is checked directly. Only success allows
the request to be archived. Errors/refusals propagate through the aggregate exit
status. Request snapshots and a per-worktree lock prevent overlapping consumers;
changed requests and existing receipts are retained for inspection.

**Reproduction:** `09_failed_edit_retains_request` makes the GitHub edit fail.
OLD exits zero and consumes the request. NEW exits nonzero, leaves the request
pending, and creates no success receipt. `new_lane_create_failure_and_dry` checks
the same property for creation.

## Finding 10 — GitHub edits depend on the caller's repository

**Wrong:** a same-numbered PR in the caller's checkout could be edited.

**Changed:** repository identity comes from the lane's origin URL and is
validated. Every GitHub read/write supplies that identity with `--repo`.
Creation also supplies the lane branch and base explicitly.

**Reproduction:** `10_repo_independent_of_cwd` runs outside the lane checkout and
records the mutation arguments. OLD omits repository identity. NEW supplies the
repository derived from the lane origin. No real GitHub write is made.

## Finding 11 — omission of `pr:` never creates a PR

**Wrong:** requests without a PR number were skipped with a successful exit.

**Changed:** omission selects creation, requiring a title and an attached lane
branch. The base defaults to main. A checked query detects an existing open PR
for the same head/base and requests an explicit number instead of duplicating it.
The lane must have pushed its branch before submitting the request.

**Reproduction:** `11_omit_pr_creates` submits a valid request without a number.
OLD skips it; NEW invokes create with explicit repo/head/base/title/body and
archives the successful request. `new_lane_create_failure_and_dry` checks both
dry-run preservation and failed-create preservation.

## Finding 12 — lane closing grammar and allowlist bypasses

**Wrong:** colon/qualified forms escaped the grammar, a permitted reference
authorized an entire line, and allowlist text was executable regex syntax.

**Changed:** the reviewed batch grammar is copied verbatim. Every individual
matched issue must belong to a literal set of positive numbers. Regex fragments
are refused. Both submitted title and body are scanned.

**Reproduction:** `12_colon_bypass`, `12_qualified_bypass`,
`12_each_match_on_same_line`, `12_literal_allowlist`, and `12_title_gate` each
make OLD send an undeclared reference to the GitHub stub; NEW refuses each and
retains the request. `new_positive_literal_multiple_allowlist` proves multiple
explicit declarations still work. Full corpus output is retained with validation
results below; all seven required catches and four required non-catches agree
with the fixed grammar in both tools.

## Finding 13 — malformed request erases a PR body

**Wrong:** missing the delimiter produced an empty body, which was applied.

**Changed:** parsing reads a snapshot and only its header. It requires exactly
one delimiter, a nonblank body, valid fields, no duplicate headers, and a valid
number when `pr:` is present. Missing/empty numbers are not silently reinterpreted
as creation.

**Reproduction:** `13_missing_body_delimiter` submits a header with no delimiter.
OLD sends an empty body and consumes the request. NEW refuses before any GitHub
mutation. `new_malformed_request_variants` adds duplicate headers, duplicate
delimiters, and whitespace-only bodies; all remain pending without mutations.

## Finding 14 — `DRY=1` still makes live edits

**Wrong:** setting DRY was overwritten by the default DRY_RUN value.

**Changed:** either variable being exactly `1` enables dry mode, including when
the other is explicitly `0`. Validation and read-only queries can still run;
GitHub mutations and request consumption are skipped.

**Reproduction:** `14_dry_alias` submits a valid edit with DRY set to 1 and DRY_RUN
set to 0. OLD edits GitHub and moves the request. NEW does neither. The creation
and merge positive tests also exercise their dry paths.

## Finding 15 — thermal decision becomes stale while waiting

**Wrong:** the blocking flock path directly launched the command using the
thermal decision made before it queued.

**Changed:** both immediate and queued acquisitions keep the lock in the parent.
The same thermal gate runs again while the acquired slot is held, before any
build starts. The initial check, cooling wait, exit 75 on sustained heat, and
explicit thermal override remain. The thermal subprocess and build close the
lock descriptor. Child output/arguments/status remain intact. Timeout diagnostics
now depend on acquisition failure, not a child's coincidentally identical status.
The advertised slot option and invalid invocations are validated as well.

**Reproduction:** `15_thermal_after_slot_wait` starts cool with a real slot held,
waits until the caller is queued, changes the thermal stub to hot, then releases
the slot. OLD prints `started` and exits zero. NEW starts no child and exits 75.
`verified_initial_thermal_gate` proves the initial refusal still precedes flock.
`verified_child_streams_status_arguments` and
`verified_queued_child_streams_and_status` verify stdout, stderr, one argument
containing spaces, and exit 37 on both execution paths.
`verified_lock_held_and_not_inherited` checks that the parent holds the slot while
the child runs and that a surviving descendant cannot keep it afterward.

## The two regressions in the first repair; recheck of findings 1–3

The three-read file-count race is eliminated with finding 7: no count or files
response participates in the gate. The title mismatch is eliminated by generating
from the same source snapshot that was scanned, then scanning the exact output
file. `02_new_title_snapshot_race` makes OLD receive an unsafe title on its first
read and a safe title during its gate; OLD publishes the earlier unsafe title.
NEW refuses its unsafe snapshot before a write.

The existing batch grammar still passes the entire specified corpus.
`verified_03_metadata_failure` verifies that both the already repaired original
and the new script refuse failed closing-metadata reads. NEW also validates
metadata structure; `new_invalid_metadata_fails_closed` covers an incomplete JSON
response. The previously repaired buildlock stderr/descriptor properties are
covered on both immediate and queued paths as described above.

## Live state deliberately left alone

The real pending request at `/home/tom/projects/gv-831/PR-REQUEST.md` was read only
for its header/location and was not executed, renamed, or edited. The task is a
tool repair, so all GitHub operations in tests are stubs. No live batch was built
or merged, no issue was edited, and no real build was launched.

The checkout started on main, behind its existing origin/main by 40 commits, with
an unrelated modification to `.claude/skills/land/land.sh` and existing untracked
notes/image/files. Those changes are preserved. No pull, merge, branch cleanup,
commit, or PR was performed as part of this repair.

The existing push/create split can leave an integration branch if PR creation
fails; the script reports failure and leaves that branch for inspection. It does
not delete or force-push it automatically. A lost successful network response can
still require inspecting GitHub before retrying; this tool does not claim a
distributed transaction across GitHub and local request files.

## Validation and atomic installation

All **35 test cases passed**, with old/new assertions and the complete 11-input
grammar corpus. All three fixed scripts and the installer pass `bash -n` and
ShellCheck with no diagnostics. The complete successful output is in
[test-results.txt](test-results.txt). That stored output escapes hash signs as
`\u0023` to keep closing directives out of reports; the actual tests used the
literal corpus supplied in the handoff.

| Script | Original SHA-256 | Tested replacement SHA-256 |
| --- | --- | --- |
| batch-land | `34ee513ced59e40f8571f994de114d874802b3bf19162280140776b0248d3d35` | `39adf54d1b52286d2734c0a8c7b13e252bd6c0d215d549f35e8fb82efca2144f` |
| buildlock | `51c8cdc7ab48c4e748f32f2b21650c6c5a990a677cdb3af84d1817a183cb861c` | `662bc4760abd45d06416e7dad481e85d80ab8fd4e4c2d383b69e87e6ee79d980` |
| lane-pr-requests | `ee449bb8b3a82783b6685451db69677fd0e1cfd737b6ffd3068a8450d66a3ea0` | `4b7976c2cd8f14e34298d351848737962b4a2906e9f8320efa626b40d83ae0f2` |

The existing `.gitignore` ignores `/design-docs/`. The new review files were
explicitly marked with Git's intent-to-add so they appear in the working diff,
without changing ignore rules, staging their contents, or committing unrelated
work.

The installer checks the live bytes against the original snapshots, stages
complete sibling temporary files with the original executable mode, then runs
the exact requested process check immediately before each atomic `mv`. It refuses
any matching live process. `install.sh --rollback` uses the preserved originals
and the same checks; it is provided but has not been invoked.

All three tested replacements were installed atomically at
**2026-09-10T16:43:38Z (12:43:38 EDT)**. Before each replacement the command was
exactly `pgrep -af 'batch-land|buildlock|lane-pr-requests'`. Its **stdout was
empty and its exit status was 1 on all three checks**:

| Immediately before replacing | Exact pgrep stdout | pgrep status |
| --- | --- | --- |
| `/home/tom/.local/bin/batch-land` | empty (zero bytes) | 1 |
| `/home/tom/.local/bin/buildlock` | empty (zero bytes) | 1 |
| `/home/tom/.local/bin/lane-pr-requests` | empty (zero bytes) | 1 |

The checks ran outside the sandbox PID namespace, so they covered actual live
consumers. No matching process was present; none was stopped or interrupted.
The complete installer output and installed SHA-256 values are preserved in
[install-receipt.txt](install-receipt.txt). The installed bytes match the tested
copies, and their original executable modes were retained.

No requested local repair or installation was blocked. Finding 4's residual
remote metadata race is the outstanding limit: a local script cannot make GitHub
conditionally merge unchanged title/body versions or enforce use of this tool by
other actors. It is described above so the final independent review can assess
the new preflight without mistaking it for an atomic server-side guarantee.

— **daybreak**

---

## Addendum — N1 fix applied and installed, 2026-09-10 13:17 EDT

The independent audit (`../2026-09-10-max-astra-tooling-audit.md`, finding N1)
found the `--merge` manifest lived under `$WT.batch-land-state`, which resolves
onto `/tmp` (confirmed tmpfs), so a titan reboot between batch creation and merge
would lose the manifest and force a manual `gh pr merge` bypassing the whole gate.

**Fix:** `STATE` now resolves to `${BATCH_STATE_DIR:-$HOME/.local/state/batch-land}`
— a durable, non-tmpfs directory — instead of a path under the ephemeral worktree.
Applied by the coordinator (max), installed atomically (no live consumer at
install time; verified via `pgrep`), and the archived copy of `batch-land` in
this directory has been updated to match the newly installed bytes.

Original hash after the 2026-09-10 12:43 install (tested/audited): `39adf54d...`
New hash after the N1 fix (installed, this addendum): `a51d200b27ac0fed972b3d422915efa370112f2fdbfa9f8a1669983f94ceb235`

The one-line diff, for the record:

```diff
-STATE="$WT.batch-land-state"
+STATE="${BATCH_STATE_DIR:-$HOME/.local/state/batch-land}"
```

This addendum exists because codex's review of Round 9's plan (v2) caught that
this drift had happened without a matching artifact update — the exact
missing-review-trail failure this whole tooling-fix exercise was meant to close,
reproduced in miniature an hour after the audit finished. Recorded here rather
than silently edited into the original report.

**Signed:** max · 2026-09-10T15:50:00-04:00

---

## Addendum — 2026-09-10 ~16:05 EDT — N2: the origin gate refused every real run

**Found by:** max, on the FIRST genuine invocation of `batch-land` against the
live repository, during Round 9 (the wave Tom framed as a test of the day's
tooling). Severity: **total** — the tool could not run at all here.

### The defect

`check_origin` compared the clone's origin URL against a hardcoded
`REPO="tom2025b/Git-Vista"` using a case-**sensitive** bash `case` statement
(with `LC_ALL=C` set). The actual origin is
`https://github.com/tom2025b/git-vista.git`, and GitHub's own `.full_name` for
this repository is the lowercase `tom2025b/git-vista`. Every invocation died at
line 62 with `origin must identify tom2025b/Git-Vista` before doing any work.

### Why five prior passes missed it

`batch-land` carried 15 findings through a codex review, a partial self-fix, a
second review that found 2 new bugs in that fix, a full daybreak/astra repair,
and an independent Claude audit that returned **GO**. None caught this, because
**none of them ran the tool against the live origin** — the audit exercised it
in scratch directories with synthetic remotes, which is exactly where a
hardcoded-constant mismatch is invisible.

This is the day's standing rule landing on the tooling itself: *the one you
won't catch is the one you already checked.* The gate had been read by four
reviewers and executed by none.

### The fix

Two independent corrections, both in `check_origin` and the `REPO` constant:

1. `REPO` corrected to the canonical `tom2025b/git-vista`, as GitHub reports it.
2. The comparison folds case on both sides before matching.

**Case-folding here is a correctness fix, not a relaxed boundary.** GitHub
owner/repo names are case-insensitive *identifiers*: `tom2025b/Git-Vista` and
`tom2025b/git-vista` resolve to the same repository, and two repositories
cannot differ only by case. Folding therefore admits exactly the same set of
repositories and removes only false negatives.

### Proof the gate still holds

Three negative tests run against the fixed script, all correctly **refused**:

| Origin under test | Result |
|---|---|
| `https://github.com/attacker/evil.git` | refused |
| `https://github.com/someoneelse/git-vista.git` (right name, wrong owner) | refused |
| `https://github.com.evil.io/tom2025b/git-vista.git` (lookalike host) | refused |

Then the real repository passed and built integration head `41806404`.

### Hash change

| | |
|---|---|
| before | `a51d200b27ac0fed972b3d422915efa370112f2fdbfa9f8a1669983f94ceb235` |
| after | `0ceca97a87e74c989984c23c48ee7d5ea59517d4526408de852d170226839211` |

Backup of the pre-fix script: `~/.local/bin/batch-land.bak-20260910-160055`.
The archived copy in this directory was re-synced and verified byte-identical
to the installed script — the step that was missed on the N1 fix earlier today.

### Sibling tools checked for the same class

- `lane-pr-requests` — **immune by design.** It *derives* the repository from
  the origin URL rather than comparing against a hardcoded constant.
- `buildlock` — repo-agnostic, no origin handling at all.

`batch-land` was the only tool that pinned a repository name, and it pinned the
wrong casing.

**Signed:** max · 2026-09-10T16:10:00-04:00
