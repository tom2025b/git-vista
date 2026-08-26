# 0092 — A refused worktree sibling is listed, not dropped; git's facts and the app's fence stay two fields, never one

**Status:** Accepted — implemented and tested
**Date:** 2026-08-26
**Issue:** [#546](https://github.com/tom2025b/git-vista/issues/546)

---

## Context

`docs/superpowers/specs/m3.23-worktrees.md` §1 designs a read-only worktree
census: enumerate the linked worktrees of the served repository (`git
worktree list --porcelain`) and report them with three kinds of fact kept
deliberately separate — `locked`/`prunable` (git's own flags) and
`serviceable` (whether this application's allowed-roots fence permits opening
the sibling at all). The spec is thorough and mostly correct, but it is a
design sketch written before anyone tried to implement it, and several of its
details either don't compile as written or don't survive contact with a real
repository. This ADR records what changed and why, since #546 is stage one of
four and the shape chosen here is inherited by the rest of milestone M11.

## Decision 1 — The wire types live in `git-vista-protocol`, using its own conventions, not the spec's literal domain sketch

The spec's `WorktreeSibling`/`Serviceable`/`WorktreeCensus` are written using
`git-vista-core` types (`WorktreeId`, `BranchName`, `CommitOid`, `PathBuf`) and
a tuple variant (`Observed(Vec<WorktreeSibling>)`). Neither survives as
written:

- `git-vista-protocol` does not depend on `git-vista-core` at all (by design —
  see the crate's own doc comment on the transport/domain boundary), so a core
  `WorktreeId` cannot appear in a wire struct there. Every id is the **opaque
  string form**, exactly the convention `RepositoryDescriptor` already
  established.
- `PathBuf` on the wire would leak the server's filesystem layout to the
  browser by default — the same problem `RepositoryDescriptor::path` already
  solved. `WorktreeSibling` follows that precedent exactly: `name: String`
  (always, a display label) plus `path: Option<String>`, omitted unless the
  operator set `GIT_VISTA_EXPOSE_PATHS`.
- `Observed(Vec<WorktreeSibling>)` as a tuple variant does not serialize under
  `#[serde(tag = "kind")]` (serde's internally-tagged representation requires
  every variant's content to be a JSON object; a bare `Vec` is a JSON array).
  `WorktreeCensus::Observed` is a struct variant (`{ siblings: Vec<…> }`)
  instead — same information, a shape that actually serializes.

`BranchName`/`CommitOid` **are** reused, but from `git-vista-protocol::plan`
(where they already live as validating newtypes), not from core — the same
pattern `StashEntry::oid` already uses for a response DTO.

## Decision 2 — `head` is `Option<CommitOid>`, not `CommitOid`

Empirically verified (`git init` a repo, `git worktree list --porcelain`
before any commit): an unborn worktree reports `HEAD
0000000000000000000000000000000000000000` — git's null-oid sentinel, not a
real object. Passing that through as a `CommitOid` would assert a commit
exists where none does. This is the exact fact
`git_vista_protocol::history::HeadState::Unborn` already exists to state about
the *current* worktree's HEAD; `WorktreeSibling::head` states the same fact
about a sibling's HEAD, so it gets the same `Option`, `None` for the null oid,
never a fabricated commit id.

## Decision 3 — `bare` is a new field, a third git-native flag the spec didn't anticipate

Verified by hand: from a linked worktree of a bare-hub layout (`git init
--bare hub.git`, `git worktree add` a sibling, list from inside the sibling),
`git worktree list --porcelain` reports the bare directory itself as its own
record — `worktree <path>` followed only by `bare`, no `HEAD`, no `branch`.
That is git handing over a third boolean on the same footing as `locked` and
`prunable`. Folding it away — dropping the row, or reporting it as an
ordinary worktree with an absent HEAD (indistinguishable from a corrupt read)
— is exactly the "never fold a real git flag into something it isn't"
mistake this spec exists to correct for `locked`/`prunable`/`serviceable`.
`WorktreeSibling::bare: bool` states it directly. `Serviceable` still applies
to a bare row unchanged: a bare repository is independently registerable
(`RepositoryKind::Bare`), so no fourth `Serviceable` variant is needed.

## Decision 4 — A `Missing` sibling's id is the admin directory's, resolved by exact correlation, not derived from a naming convention

The spec flags its own gap: `WorktreeId::from_git_dir` hashes a canonical git
directory, but a `prunable`-with-a-gone-directory sibling has no directory to
open — `<path>/.git` cannot resolve because `<path>` no longer exists. The
spec offers two ways out: make the mapping to the surviving
`<common-dir>/worktrees/<name>` administrative directory total, or give the
row its own (non-`WorktreeId`) type.

The mapping **can** be made total, exactly, with no naming guess: each admin
directory's own `gitdir` file records the working tree's `.git` pointer-file
path (verified: `cat .git/worktrees/<name>/gitdir` prints
`<worktree-path>/.git`). `correlate_missing_admin_dir` reads every admin
directory under the repository's common dir (`git rev-parse
--git-common-dir`, spawned lazily — only when a `prunable` sibling's live open
actually fails) and matches that recorded path against the porcelain-reported
one. A match resolves to exactly one admin directory or none; two admin
directories claiming the same path is treated as ambiguous and refuses to
guess (`CensusFailed`), the same fail-closed posture as everywhere else in
this module. `WorktreeId::from_git_dir` then hashes that admin directory's own
canonical path — the metadata that survives the working tree's deletion,
which is the entire reason git can still list the entry at all.

The refinement past the spec's literal "prunable ⇒ Missing": `prunable` is
git's flag, and it does not by itself prove the directory is gone — a plain
(non-`--expire`) `git worktree list` in practice never marks a *present*
directory prunable, but nothing about the porcelain contract *guarantees*
that for every future git version. So a `prunable` sibling is resolved live
first (`read_repo_facts` against its reported path); only a failure to open
it falls back to admin-directory correlation and `Serviceable::Missing`. A
`prunable` sibling that still opens keeps its real `serviceable` value
(`Yes`/`OutsideAllowedRoots`) with `prunable: true` recorded alongside — never
silently forced to `Missing`, which would be exactly the kind of
fact-folding this design exists to prevent, just one level down.

## Decision 5 — No `-z`; the newline-terminated porcelain form is parsed instead

`git worktree list --porcelain -z` NUL-terminates records, which is the
safer contract for a path containing a literal newline. It is not used:
`docs/SUPPORTED_VERSIONS.md` documents a git floor of 2.32. The git-scm
manual for 2.31 documents `list`, `--porcelain`, and `-v`/`--verbose` and
says nothing about `-z` at all; 2.32 has no distinct page of its own (its
URL redirects to 2.31's); the current manual documents `-z`. Taken together,
`-z` was added to `worktree list` at some later version, after this
project's documented floor. Parsing the newline form inherits git's own
limitation at
that floor (a literal newline in a worktree path cannot be parsed
unambiguously) rather than a defect in the parser — and the one place that
could bite silently, quoting, doesn't apply here: the manual documents that
only the lock *reason* is quoted/escaped when `-z` is absent, and
`WorktreeSibling` carries no reason field at all, so the parser only ever
needs to recognise the `locked`/`prunable` label, never interpret the
escaping of text after it.

## Decision 6 — The parser is strict: every unrecognised line is a hard error

Per the issue's own instruction, matching this codebase's established
posture for a fact that must never silently vanish
(`RecoveryClass::CheckFailed` on an unrecognised ref shape,
`HeadState::Unresolvable`): an attribute line before any `worktree` line, a
second `worktree` line before the first record's blank-line terminator, an
unrecognised attribute label, `branch`+`detached` (or `bare`) together, a
non-`bare` record with no `HEAD` line, a `bare` record carrying one, or a
`branch`/`HEAD` value that doesn't fit this app's own validated shape — every
one of these is `WorktreeCensus::CensusFailed`, never a skipped line. A
dropped worktree is indistinguishable from one that never existed, and this
census exists specifically so nothing downstream can make that mistake.

## Decision 7 — The allowed-roots check and the path-exposure flag are injected parameters, not global reads

`worktree_census`/`resolve_sibling` take `expose_paths: bool` and
`path_is_allowed: &dyn Fn(&Path) -> bool` rather than calling
`crate::state::expose_paths()`/`crate::state::path_is_allowed` internally —
the same hoist `Catalog::descriptor`/`descriptor_with_policy` already make,
for the same reason stated in that function's own doc comment: the process
catalog is a `OnceLock` shared by every test in the binary, so a function
that reaches into it cannot be unit-tested for "outside the allowed roots"
without depending on what some other test in the same binary happened to
register first. A production call site (added when this is wired to a
handler, out of scope for #546) supplies
`crate::state::expose_paths()`/`&crate::state::path_is_allowed`; every test
here supplies its own local closure instead.

## Decision 8 — No HTTP route in this slice

Acceptance criterion 1 asks for a query "produced from `git worktree list
--porcelain`" landing in the protocol crate — a function and its wire type,
not a route. This repeats the staging this codebase already used for the
M4.31 conflict scan (`conflicts.rs`) and the M2.21a tag contract: the
read/write primitive and its typed vocabulary land and are reviewed first,
wiring follows in a later, separate slice. `worktree_census` has no caller
outside its own tests (`#[cfg_attr(not(test), allow(dead_code))]`, matching
`conflicts`'s own attribute) and touches no route table, so
`route_authz`'s structural gate has nothing new to classify.

## Consequences

**No new sandbox tier or grant.** `git worktree list --porcelain`'s argv
first token, `worktree`, is absent from `sandbox::REMOTE_SUBCOMMANDS`, so the
declared `NetworkNeed::Local` (the same arity `handlers::read::worktree_status`
already uses) agrees with the argv cross-check and reuses the existing
repo-scoped grant. Resolving a sibling's identity — including one outside the
allowed roots — runs through `git_vista_git::read_repo_facts`, the exact
function `Catalog::register` already calls *before* its own allowed-roots
check; computing identity has never been the sandboxed, security-sensitive
step, only serving a repository (executing git inside it, admitting it to the
catalog) is.

**The census is a shared primitive other issues will call, not extend.**
#547 (the checkout-collision precondition) and #549/#550
(`AddWorktree`/`RemoveWorktree`) consume `WorktreeCensus` as-is; none of them
should need a new `Serviceable` variant or a second enumeration path, per the
spec's own "one fallible enumeration primitive, shared by all three
consumers."

**A worktree path containing a literal newline is unparseable at the
documented git floor.** This is git's own porcelain-format limitation at
2.32, inherited rather than introduced, and it can be revisited if the floor
ever moves past whatever version added `-z` for `worktree list`.

## Alternatives considered

**Two-state `usable: bool`.** Rejected by the spec itself, and re-confirmed
here: it cannot distinguish "git refuses this" from "this app refuses this,"
which is exactly the blind spot a future collision check would inherit
(a hidden sibling that still holds a real branch checkout).

**Folding every `prunable` sibling straight to `Missing`.** Rejected in
Decision 4: it would force a live-but-flagged-stale sibling into a state that
claims its directory is gone, which is a fact-folding regression of the exact
kind this design otherwise removes.

**A naming-convention guess for the Missing row's admin directory** (e.g.
"named after the working tree's basename"). Rejected: git's own admin
`gitdir` file gives an exact, non-heuristic correlation, and the spec's own
argument against relying on human-readable conventions (porcelain is the
stable contract; formats meant for eyes are not) applies just as much to a
naming convention as to the human-format `git worktree list` output it
already rejects `-z`-free parsing for elsewhere.

**Wiring an HTTP route in this slice.** Rejected in Decision 8: the
acceptance criteria describe a query, not an endpoint, and #546 is
deliberately scoped to have no browser surface at all.
