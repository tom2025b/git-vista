# Verified facts for the M1.13 design (all empirically tested, git 2.43.0)

## Config scope precedence — core.hooksPath

Tested with a global `~/.gitconfig` containing `core.hooksPath = <dir>` and a
hook that announces itself:

| Invocation | Result |
|---|---|
| `GIT_CONFIG_NOSYSTEM=1 git commit` | **GLOBAL HOOK FIRED** — NOSYSTEM does not touch global config |
| `GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null git commit` | no hook fired — global suppressed |
| `GIT_CONFIG_NOSYSTEM=1 git -c core.hooksPath=<repo> commit` | **REPO HOOK FIRED** — command-line `-c` overrides the global redirect |

Conclusion: `-c core.hooksPath=<explicit>` on the command line is the mechanism
that makes the permitted hook source explicit and testable. It beats every
config scope, including repo-local.

## What env_clear actually breaks

`env -i PATH=$PATH git ...` (nothing but PATH):
- `git --version` works
- `git rev-parse --git-dir` works
- `git status --porcelain` works, rc=0
- `git commit` works when identity is supplied
- git finds its own subcommands. **GIT_EXEC_PATH is NOT required.**

## How push actually authenticates in THIS repo

- `git remote -v` → `https://github.com/tom2025b/git-vista.git`. **HTTPS, not SSH.**
- SSH to github fails even with `SSH_AUTH_SOCK` set and an agent running
  (`Permission denied (publickey)`), so SSH is not the shipped path.
- Auth comes from a credential helper in **global** config:
  `credential.https://github.com.helper = !/usr/bin/gh auth git-credential`
- `gh` reads its token from `/home/tom/.config/gh/hosts.yml`, so the helper
  needs `HOME` (or `XDG_CONFIG_HOME`) to function.
- Identity for commits is **repo-local** (`.git/config`: tom2025b /
  262510778+tom2025b@users.noreply.github.com), so suppressing global config
  does NOT break commit identity in this repo.

## The sharpest argument for suppressing global config

A credential helper value beginning with `!` is executed **by a shell**. Git
will happily run `sh -c '/usr/bin/gh auth git-credential'` on our behalf,
sourced from a config file the server does not control. Therefore the issue's
first acceptance criterion — "no shell-string execution is used" — CANNOT be
satisfied by our argv hygiene alone. Config is a shell-execution vector.

## The real regression under strict env + suppressed global config

Not SSH push. It is **HTTPS push losing its credential helper**, and therefore
its token. Any design must state this precisely and pin it with a test.
