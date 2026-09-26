# ADR 0152 — Windows containment is a native backend, and Windows refuses to start until it exists

**Status:** Proposed — direction chosen by Tom (option (b), 2026-09-25 22:20 EDT); this document's details await his read
**Date:** 2026-09-25
**Issue:** [#860](https://github.com/tom2025b/git-vista/issues/860) (M14.4)
**Follows:** [0029](0029-strict-tier-hard-fail-when-unavailable.md) (hard-fail when the Strict tier is unavailable), [0030](0030-git-process-sandbox.md) (the git-process sandbox and its three tiers), [0151](0151-the-windows-client-is-a-webview-over-the-http-api.md) (the Windows client is a webview over the HTTP API)
**Supersedes:** nothing
**Superseded by:** nothing

## Context

ADR 0151 chose how Git-Vista reaches Windows: a Tauri v2 window around the
existing web UI, talking to an in-process `git-vista-server` over loopback
HTTP. That makes the server itself the thing that has to run on Windows.

The server will not start there today, and that is by design. Before it binds a
listener or runs any repository code, `main.rs:223` calls
`sandbox::probe::run_at_startup()` and exits with status 1 on any verdict other
than `Contained`. The comment above that call says why: *"There is no degrade: a
verdict other than `Contained` means no server, full stop (ADR 0029)."*
`Contained` means bubblewrap, Landlock and seccomp compose on this host.
None of the three exists on Windows.

Why the sandbox exists at all: Git runs code the repository controls (hooks,
filters, config keys that name executables). `SECURITY_MODEL.md` treats that code
as hostile. On Linux it runs inside one of three tiers (ADR 0030), chosen by
`tier_for` (`sandbox/mod.rs:891`):

- **Strict**, for local operations on untrusted repositories:
  - **Network:** none. It runs in a fresh network namespace.
  - **Writes:** the repository and `/dev` only.
  - **Read and execute:** the system trees (`/usr`, `/bin`, `/lib`, `/lib64`,
    `/etc`), a fresh `/proc` for the sandbox's own pid namespace (not the
    host's), and `$HOME`, **except** a withheld list of credential paths
    (`DEFAULT_SECRET_EXCLUDES`, `sandbox/mod.rs:230`). That list includes
    `.ssh`, `.config/gh`, `.aws`, `.netrc`, `.git-credentials`, `.gnupg`,
    `.docker`, `.kube` and browser profiles. Home stays readable because Git
    reads `~/.gitconfig` (ADR 0030).
  - **The trust store is withheld even from the write grant.** `policy_for`
    adds the sandbox trust directory (`state::sandbox_trust_dir()`) to the
    excludes (`sandbox/mod.rs:1125-1128`). An exclude outranks a grant, so the
    trust store stays out of reach even when the served repository tree
    contains it. Without that, a hostile hook could write its own trust marker,
    and the next operation on that repository would run Unsandboxed
    (`sandbox/mod.rs:1098-1117`; ADR 0030 §3).
  - **Local sockets:** `AF_UNIX` is denied by seccomp (`SECURITY_MODEL.md`,
    *Sandbox Mechanism Boundaries*).
- **Network**, for remote operations: Landlock allows TCP only on an
  enumerated port list (ADR 0028).
- **Unsandboxed:** reachable only through persisted repository trust.
  `trust::grant` has no production caller (ADR 0030 §3; it is `#[cfg(test)]` in
  `sandbox/trust.rs`), so no production path reaches this tier today.

The #860 investigation (codex on titan, 2026-09-25; every citation re-checked
against `main` before it was posted to the issue) re-read ADRs 0029 and 0030.
ADR 0029 decided what happens when a host *lacks a prerequisite an operator can
supply*; its remedy is installing `bwrap` and enabling unprivileged user
namespaces. It did not weigh a platform where *no implementation exists at all*.
Its reasoning still applies to Windows:

- Never degrade Strict to Network.
- Never degrade and block hooks as a middle path.
- Treat an unknown observation as its own distinct value, never coerced into a
  permissive answer: *"refuse, or fail-safe default, but NEVER fail-open."*
- A boot check alone is not enough; each operation must refuse as well.

Its remedy does not transfer, because there is nothing to install. So Windows
was a genuine new decision, put to Tom as three options.

The diagram at the end of this section shows the Linux containment this has to
be matched against.

```mermaid
---
config:
  flowchart:
    wrappingWidth: 460
---
flowchart TD
    OP["<b>A git operation</b><br/>runs repository-controlled code:<br/>hooks, filters, configured executables"]
    TIER{"<b>tier_for(need, trusted)</b><br/>sandbox/mod.rs:891"}
    STRICT["<b>Strict</b><br/>no network, writes: repo + /dev only,<br/>reads: system + home MINUS withheld secrets,<br/>trust store withheld even from writes,<br/>AF_UNIX denied by seccomp"]
    NET["<b>Network</b><br/>Landlock TCP port list, ADR 0028<br/>ports, never hosts"]
    UNS["<b>Unsandboxed</b><br/>only via persisted trust,<br/>trust::grant is test-only today"]
    GATE["<b>Boot gate, main.rs:223</b><br/>any verdict but Contained:<br/>exit 1 before listening"]
    KEY["<b>KEY</b><br/>blue = decision point<br/>green = contained tiers<br/>grey = not contained, no production route<br/>red = the refusal gate"]

    OP --> TIER
    TIER -->|"local op, untrusted"| STRICT
    TIER -->|"remote op, untrusted"| NET
    TIER -->|"trusted repo"| UNS
    GATE -.->|"guards all of it"| TIER

    classDef decide fill:#eaf2fa,stroke:#14406f,stroke-width:3px,color:#0d2b4d
    classDef good fill:#e8f5e9,stroke:#2e7d32,stroke-width:3px,color:#1b5e20
    classDef uncontained fill:#eeeeee,stroke:#616161,stroke-width:3px,color:#212121
    classDef gate fill:#fdecea,stroke:#a01b1b,stroke-width:3px,color:#6b1111
    classDef legend fill:#f5f5f5,stroke:#616161,stroke-width:2px,color:#212121

    class OP,TIER decide
    class STRICT,NET good
    class UNS uncontained
    class GATE gate
    class KEY legend
```

## Decision

1. **Direction: Windows gets a real, native containment backend** (option (b),
   chosen by Tom on 2026-09-25). On Windows, hostile repository code runs inside
   an OS-enforced boundary that **grants no more than the Linux Strict tier
   grants**. That is proven by measurement, not assumed by analogy. It never runs
   with the server account's own access.

2. **ADR 0029's reasoning applies to Windows unchanged.** There is no degrade to a
   weaker tier and no degrade-and-block-hooks. An unknown observation is never
   coerced into a permissive answer. Refusal holds at boot **and** per operation,
   because 0029 records that a boot gate alone was not enough.

3. **Until that backend exists and passes its own escape battery, Windows refuses
   to start.** This is option (a), kept as the *interim*, not rejected. One part
   of it is new work: today's refusal text can tell a Windows user to install
   `bwrap`, which is wrong advice on Windows. The interim refusal needs a
   Windows-specific message that names the real reason: "no Windows sandbox yet".

4. **Linux behavior does not change.** Every Windows change is additive and
   gated to non-unix targets (`cfg(windows)` / `cfg(not(unix))`), following the
   pattern #862 established. The seven required CI checks all run on Linux, and
   they remain the merge gate for every Windows PR. A Windows change that alters
   Linux behavior is a defect, whatever else it achieves.

5. **A feasibility spike gates all backend work, with two stated stop conditions.**
   Before any Windows backend is built, measure on a real Windows box whether Git
   for Windows can run inside an AppContainer or Less-Privileged AppContainer
   (LPAC) token whose grants **translate Strict's shape** (context, above):
   - **Writes:** the repository only. Linux also grants `/dev`, which has no
     direct Windows counterpart. Whatever null-device access Git's runtime needs
     on Windows is measured by the spike, not assumed.
   - **Read and execute:** Git's install directory and the system directories
     Git needs.
   - **Read and execute:** the user profile, **except** the Windows equivalents
     of every path in `DEFAULT_SECRET_EXCLUDES`, and except Windows Credential
     Manager.
   - **Withheld even from the write grant: the sandbox trust directory.** It
     stays out of reach even when it lies inside the repository tree, and the
     withholding must outrank the write grant, exactly as it does on Linux.
   - **Network:** no network capability at all.
   - **Local IPC:** denied. Linux denies `AF_UNIX`; on Windows the equivalents
     include `AF_UNIX` sockets and named pipes to host services.

   The spike must prove **both** halves:

   - **Git works under those grants.** `git status` and `git commit` on a
     repository inside the grant, and a hook firing (a `pre-commit`). Git for
     Windows ships its own shell runtime for hooks; that dependency is an
     assumption this spike confirms, not a measured fact.
   - **The container reaches nothing Strict withholds.** Each of these must be
     *denied*, measured:
     - a read of a file outside every grant;
     - a read of each withheld credential path;
     - a network connection;
     - a credential-helper or Credential Manager read that would return a stored
       credential;
     - a write outside the repository;
     - a write into the sandbox trust directory, **including when it lies inside
       the repository tree** (the Linux trust-marker bypass this exclude exists
       to stop);
     - a connection to a host service over local IPC (an `AF_UNIX` socket or a
       named pipe).

     Microsoft documents that a regular AppContainer is already granted access to
     *"certain system files/directories, common registry keys and COM objects"*,
     and that its profile's `LOCALAPPDATA`, `TEMP` and `TMP` are writable
     ([Launch an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)).
     The spike records every such default the container actually has, and says
     whether each one exceeds Strict. LPAC withholds those defaults unless a
     capability grants them (same page), which is why it is the preferred
     candidate.

   **Stop conditions: either one sends the direction back to Tom for a fresh
   decision.**
   - **(A)** Git cannot do the work with Strict-shaped grants.
   - **(B)** The container can reach something Strict withholds, and that access
     cannot be removed.

   Neither condition falls through to an unsandboxed mode (option (c)), and
   neither is resolved by quietly widening grants until something works.

6. **Candidate mechanism mapping: documented by Microsoft, not measured here.**
   Nothing below has been run on this project's hardware; the spike measures it.

   - **Strict → an LPAC (preferred) or AppContainer token with no network
     capability**, plus DACL grants that translate Strict's shape. Microsoft
     documents that an AppContainer's effective access is the *intersection* of
     the user's rights and the container's own SIDs. Access has to be granted
     explicitly, and a container without the network capability "cannot access
     the network"
     ([Launch an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)).
   - **Network → an open question the spike must answer.** Linux's Network tier
     allows TCP only on an enumerated port list. Microsoft describes AppContainer
     network access as coarse capabilities: *"Granular access can be granted for
     Internet access, Intranet access, and acting as a server"*
     ([AppContainer isolation](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)).
     Neither page describes a per-port restriction. **If Windows can only express
     "outbound allowed" rather than "outbound on these ports", the Windows Network
     tier is a weaker boundary than Linux's.** That comes back to Tom, named as
     such, before the Network tier is built. It is not decided here and not
     assumed to be equivalent.
   - **Process lifecycle (the reaper's job) → a Job Object** with
     `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and neither breakaway limit set.
     Microsoft documents that children created with `CreateProcess` join the
     parent's job by default, and that closing the last job handle then
     terminates the job's processes. It also documents two exceptions: children
     created through `Win32_Process.Create` do not join, and nested-job breakaway
     settings can change association
     ([Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)).
     So "every descendant of a hook ends up in the job" is a claim the spike and
     the escape battery must prove, not one this record makes.
   - **Unsandboxed → unchanged.** It is still reachable only through persisted
     trust, which still has no production caller. This ADR does not add a
     Windows route to it.

The diagram at the end of this section maps each Linux tier to its Windows
candidate. The one link that may be weaker is marked amber.

```mermaid
---
config:
  flowchart:
    wrappingWidth: 460
---
flowchart TD
    LS["<b>Linux Strict</b><br/>no network, writes: repo + /dev,<br/>reads: system + home minus secrets,<br/>trust store withheld, AF_UNIX denied"]
    LN["<b>Linux Network</b><br/>TCP on enumerated ports only"]
    LR["<b>Linux reaper</b><br/>gv-sandbox-reaper"]
    WS["<b>Windows Strict candidate</b><br/>LPAC preferred, no network capability,<br/>DACL grants translating Strict's shape"]
    WN["<b>Windows Network: OPEN QUESTION</b><br/>documented capabilities are<br/>Internet / Intranet / server,<br/>no per-port restriction documented"]
    WR["<b>Windows lifecycle candidate</b><br/>Job Object, KILL_ON_JOB_CLOSE,<br/>no breakaway allowed"]
    SPIKE["<b>Spike measures all three</b><br/>nothing here is measured yet"]
    KEY["<b>KEY</b><br/>blue = Linux, as built<br/>green = Windows candidate, documented<br/>amber = may be weaker, back to Tom<br/>grey = measurement still to happen"]

    LS --> WS
    LN --> WN
    LR --> WR
    WS --> SPIKE
    WN --> SPIKE
    WR --> SPIKE

    classDef linux fill:#eaf2fa,stroke:#14406f,stroke-width:3px,color:#0d2b4d
    classDef win fill:#e8f5e9,stroke:#2e7d32,stroke-width:3px,color:#1b5e20
    classDef open fill:#fff8e1,stroke:#8a5a00,stroke-width:3px,color:#5c3d00
    classDef measure fill:#eeeeee,stroke:#616161,stroke-width:3px,color:#212121
    classDef legend fill:#f5f5f5,stroke:#616161,stroke-width:2px,color:#212121

    class LS,LN,LR linux
    class WS,WR win
    class WN open
    class SPIKE measure
    class KEY legend
```

## Alternatives considered

**(a) Refuse on Windows permanently.** Rejected as the *permanent* answer: it
means no native Windows server, ever, and 0151 has already committed to
delivering one. Kept as the **interim** (Decision §3).

**(c) Ship an explicitly consented, unsandboxed Windows mode.** Rejected. Without
containment, repository-controlled code runs with the server account's
privileges. It can read reachable files and credentials, reach the network,
modify other repositories, and leave processes running. Loopback binding,
bootstrap authentication, origin checks and CSRF protect the API boundary; they
do not contain a hostile hook that an authorized operation triggers.
`SECURITY_MODEL.md` draws exactly that line. There is also nothing to switch on:
`trust::grant` has no production caller (ADR 0030 §3). A consent flow would be
new security surface built to carry a posture ADR 0029 already refused.

**Degrade to a weaker tier when containment is unavailable.** Rejected for the
reason ADR 0029 gives: it hands a weaker policy to exactly the operations the
stronger one exists for.

**A Job-Object-only wrapper, labelled "sandboxed".** Rejected, and named here
because it is the easy adjacent mistake. Job Objects group processes, enforce
limits and kill trees. Microsoft notes that *"security limits must be set
individually for each process"*
([Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)).
A job does not restrict what files, credentials or network a process can reach.
Calling it a sandbox would be a boundary that is "silently narrower than it
sounds", the exact failure `SECURITY_MODEL.md`'s *Sandbox Mechanism Boundaries*
section exists to prevent. A Job Object is the right *lifecycle* tool (Decision
§6), never the containment itself.

## Consequences

- **No native Windows server until the backend ships.** The interim is an honest
  refusal, not a working product.
- **This is weeks of work, not days.** codex's #860 estimate was multiple
  engineer-weeks, plausibly a month or more with validation. That is an
  engineering estimate, not a measured schedule.
- **The spike can end the direction.** If either stop condition fires (Decision
  §5), the direction goes back to Tom rather than forward to a weaker mode.
- **The Windows Network tier may end up weaker than Linux's**, and if it does,
  that is decided openly, not discovered later.
- **Linux is untouched throughout** (Decision §4), and Linux CI stays the gate.
- **Compile blockers inside `sandbox/` are now unblocked for mechanical fixing.**
  They were held until this direction was chosen: `capabilities.rs:140`'s
  ungated `libc::syscall`, `probe.rs:320`'s ungated `PermissionsExt::set_mode`,
  `trust.rs:58`'s ungated `OsStrExt`. Gating them compiles under option (b) and
  does not choose any security behavior.

**Work breakdown**, to be filed as M14 issues once this ADR lands, so each can
cite it. Items 2 and 3 can start now; the spike gates items 4 to 7.

1. **Spike:** Git for Windows inside LPAC/AppContainer; both halves of Decision §5.
2. **Interim Windows refusal message:** accurate, names "no Windows sandbox yet".
   Can start now.
3. **Compile gates in `sandbox/`:** the three blockers above. Can start now.
4. **Windows Strict backend.**
5. **Job Object lifecycle:** the reaper's Windows equivalent.
6. **Windows Network tier decision:** back to Tom if it is weaker than Linux.
7. **Boot probe, per-operation refusal, and escape battery:** behavioral probes
   that prove every denial in Decision §5, at boot and per operation, replacing
   the Linux capability checks. The spike's list is the minimum feasibility
   gate, not the whole bar: the escape battery must cover every Strict layer
   ADR 0030 §6 records, so that "no more than Strict" is something the tests
   actually measure.

The Windows server may start only when item 7 passes. The diagram at the end of
this section shows the order and where the stop conditions sit.

```mermaid
---
config:
  flowchart:
    wrappingWidth: 460
---
flowchart TD
    I2["<b>2. Interim refusal message</b><br/>can start now"]
    I3["<b>3. Compile gates in sandbox/</b><br/>can start now, no behavior chosen"]
    I1["<b>1. Spike on a real Windows box</b><br/>Git works AND nothing withheld is reachable"]
    STOP["<b>Stop condition A or B fires</b><br/>back to Tom for a fresh decision,<br/>never forward to an unsandboxed mode"]
    I4["<b>4. Strict backend</b>"]
    I5["<b>5. Job Object lifecycle</b>"]
    I6["<b>6. Network tier decision</b><br/>back to Tom if weaker than Linux"]
    I7["<b>7. Boot probe + per-operation refusal<br/>+ escape battery</b>"]
    SHIP["<b>Windows server may start</b><br/>only when 7 passes"]
    KEY["<b>KEY</b><br/>blue = can start now<br/>green = gated on the spike<br/>red = the stop conditions"]

    I1 -->|"both halves pass"| I4
    I1 -->|"A or B"| STOP
    I4 --> I5
    I4 --> I6
    I5 --> I7
    I6 --> I7
    I7 --> SHIP

    classDef now fill:#eaf2fa,stroke:#14406f,stroke-width:3px,color:#0d2b4d
    classDef gated fill:#e8f5e9,stroke:#2e7d32,stroke-width:3px,color:#1b5e20
    classDef stop fill:#fdecea,stroke:#a01b1b,stroke-width:3px,color:#6b1111
    classDef legend fill:#f5f5f5,stroke:#616161,stroke-width:2px,color:#212121

    class I2,I3,I1 now
    class I4,I5,I6,I7,SHIP gated
    class STOP stop
    class KEY legend
```

## What this does not decide

- The Windows Network tier's shape (Decision §6, left open for the spike).
- AppContainer versus LPAC. LPAC is preferred because it withholds the regular
  AppContainer defaults; the spike measures whether Git runs in it.
- macOS. #367's non-Windows scope is untouched.

## Evidence and its limits

Nothing in this record has been run on Windows. The server source citations
(`main.rs:223`, `sandbox/mod.rs:230`, `:891` and `:1098-1128`, `capabilities.rs:140`,
`probe.rs:320`, `trust.rs:58`) were re-read on `main` at `30bbd565`. The Windows
mechanism statements quote Microsoft's documentation, fetched 2026-09-25; they
describe documented behavior, not measured behavior. The spike (work item 1) is
where they become measurements.

**Signed:** max · 2026-09-25T22:55:00-04:00
