# How to launch the Pro (thomas2010) worker session

Everything is already set up. Two steps.

## 1. Open a new terminal and start Claude Code with the isolated config

```fish
env CLAUDE_CONFIG_DIR=/home/tom/.claude-pro claude
```

`CLAUDE_CONFIG_DIR` relocates the whole config directory — settings, session
history, and credentials. That is what lets a second account stay logged in
simultaneously without disturbing this session's login.

The directory `/home/tom/.claude-pro/` already exists and already has the global
rules symlinked into it, so the Pro session inherits every standing order
(commit identity, PDF conventions, ADR rules, the never-delete-a-branch rule).
Without that symlink it would have started with none of them — worth knowing if
you ever set up a third.

First launch will prompt for login. Sign in as **thomas2010**. That credential
is stored inside `/home/tom/.claude-pro/` and does not touch this session.

## 2. Paste this prompt

```
Read /home/tom/projects/Git-Vista/.claude/parallel/pro-task.md and do exactly what it says.

You are the Pro worker in a two-account parallel-work experiment. A Max
orchestrator session is running concurrently in a different worktree and is
actively editing files under crates/ — the task file lists what you may and may
not touch, and that list is a hard boundary, not a suggestion.

Your worktree is /home/tom/projects/Git-Vista-pro and you must stay inside it.
Do not cd into /home/tom/projects/Git-Vista to do work; a different session owns
that checkout's git index and you would corrupt it.

When you are done, write your results to
/home/tom/projects/Git-Vista/.claude/parallel/pro-result.md (note: that is in
the MAIN checkout, the shared mailbox, not your worktree) and set worker.status
to "done" in state.json in that same directory.

If you become blocked, write the blocker into pro-result.md and stop rather than
guessing or widening your scope.
```

## What is already done for you

- Worktree `/home/tom/projects/Git-Vista-pro` created on branch
  `worker/pro/adr-0023-index-repair` at base SHA `635b3df`.
- `/home/tom/.claude-pro/` created with global `CLAUDE.md` symlinked in.
- Repo working agreement `CLAUDE.md` copied into the Pro worktree (it is
  untracked in this repo, so it does not travel through git on its own — a real
  gotcha worth remembering).
- Mailbox at `/home/tom/projects/Git-Vista/.claude/parallel/` (gitignored;
  both sessions use that one absolute path, so no commit/push cycle is needed
  to exchange messages).

## Checking on it from this session

Max polls `pro-result.md` and `state.json` at checkpoints rather than
continuously. Nothing needs to be forwarded by hand.

---

**Signed:** thomas2025 · 2026-07-27T19:39:00-04:00
