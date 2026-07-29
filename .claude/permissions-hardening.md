# Claude Code permission-hardening proposal

Status: proposal only. This document does not change either settings file.

Audited on 2026-07-29 against Claude Code 2.1.220 and the live repository state. The threat model assumes repository content has successfully manipulated the model. A permission is therefore safe only when its enforcement still bounds a deliberately malicious tool call.

## Executive recommendation

Use the Balanced Development profile below as the normal interactive profile, with these non-negotiable conditions:

1. Replace the accumulated `permissions.allow` array in `.claude/settings.local.json`; do not merely add the new project rules. Permission arrays merge across scopes, so layering a clean allowlist over the existing local file does not remove its old grants.
2. Keep `defaultMode` at `default`, disable bypass mode, keep `additionalDirectories` empty, and require confirmation for edits, executable builds, agents, web tools, Git/GitHub commands, interpreters, and every MCP call.
3. Enable Claude Code's OS sandbox with no unsandboxed retry. Deny subprocess access to credentials and Git/Claude configuration. Do not exempt `cargo`, `./dev`, `cc`, or `bwrap` from the sandbox.
4. Run real-host Landlock/seccomp/bubblewrap acceptance tests manually in a disposable VM or similarly isolated test host. Nested sandboxing can alter their observations, while exempting the test command would let repository-controlled test code run directly on the workstation.
5. Change `worktree.bgIsolation` from `none` to `worktree`. Have a trusted launcher or the human create the task worktree; do not give the model standing permission to mutate shared Git metadata.

The official references used here are the [permission-rule reference](https://code.claude.com/docs/en/permissions), [settings schema reference](https://code.claude.com/docs/en/configuration), [tools reference](https://code.claude.com/docs/en/tools-reference), [sandbox reference](https://code.claude.com/docs/en/sandboxing), and [hooks reference](https://code.claude.com/docs/en/hooks).

## Grounding: live state

At audit time:

- `.claude/settings.json` was exactly `{"worktree":{"bgIsolation":"none"}}` apart from formatting. `none` lets background sessions edit the shared checkout directly; Claude Code 2.1.220 documents `worktree` as the default isolation mode.
- `.claude/settings.local.json` was 21,466 bytes and had an `allow` array but no `ask`, `deny`, `defaultMode`, or `additionalDirectories` entry.
- The dangerous rules named in the task were still present:
  - `Bash(python3 *)` and `Bash(python3 -)`;
  - `Bash(xargs -I{} sh -c '...')`;
  - `Bash(grep -E *)`;
  - `Read(//home/tom/**)` and `Read(//tmp/**)`;
  - `Bash(git push *)`, `Bash(gh api *)`, `Bash(gh repo *)`, and `Bash(gh auth *)`;
  - `Bash(sudo -n true)` and `Bash(sudo -n ufw status verbose)`;
  - numerous `Bash(curl ...)` rules, including rules that address port 8080.
- The local allowlist also contained Git writes (`add`, `commit`, `checkout`, `merge`, `reset`, `rm`, `mv`, and `./dev wip`), process killing, service/printer administration, interpreters, arbitrary shell scripts, executable compilers, and server start/stop commands.
- No project `.mcp.json` exists. User configuration names five MCP servers: `document-catalog`, `github`, `memwatch`, `opsmcp`, and `printpdf`.
- User hooks currently run `atuin hook claude-code` before and after Bash, a Python Markdown-to-PDF hook after Edit/Write, and a shared-file backup script before Edit/Write.
- `core.hooksPath` is unset. `.git/hooks` contains samples only and no active non-sample hook. This is current evidence, not a durable guarantee.
- There is no first-party `build.rs` in this checkout, but the locked dependency graph contains many proc-macro crates and registry dependencies can contain build scripts.

The local settings file is edited frequently. Re-run the verification commands in the final checklist immediately before applying any replacement.

## Permission-by-permission review

Verdicts refer to the normal Balanced profile. “Ask” means each call must remain visible to a human; never select a persistent “always allow” response for a broad prefix.

| Surface | What it enables | Worst outcome after prompt injection | Verdict and reason | What breaks if denied |
|---|---|---|---|---|
| `Bash` | Runs commands and child processes with the session environment. | Arbitrary code, file mutation, credential theft, network exfiltration, process/service control, or persistence. A broad interpreter rule voids narrower rules. | **Ask**, except a few exact version checks and `cargo fmt --check`. OS sandboxing is mandatory. | No builds, tests, formatting checks, `cc` probes, bwrap tests, mutation matrix, Git inspection, or developer scripts. |
| `Read` | Reads files; its path rules also cover Grep/Glob/LSP on a best-effort basis. | Secret collection, prompt injection from unrelated repos/dependency sources, or staging data for exfiltration. | **Allow** inside the repository, **Deny** credential/config paths, and leave outside-project access ungranted. The sandbox must enforce the same boundary for subprocesses. | Claude cannot understand or review the codebase; denying Cargo registry/toolchain reads at the sandbox layer also prevents builds. |
| `Edit` | Targeted file replacement; path rules also govern all built-in edit tools. | Source/CI/test-tripwire compromise, a malicious `build.rs`, or a script later executed by a human/checkpointer. | **Ask** for ordinary project files; **Deny** `.git`, live Claude settings, MCP configuration, and credential files. | Claude cannot implement fixes or documentation changes. |
| `Write` | Creates or overwrites complete files. Path-qualified `Write(...)` rules are not consulted in 2.1.220; use `Edit(path)` rules for path enforcement. | Same as Edit, plus replacement of whole scripts/configuration. | **Ask** as a bare tool and rely on `Edit(path)` for paths. | New files and complete rewrites become impossible. |
| `WebFetch` | Fetches page content and returns untrusted text to the model. | Exfiltration in URLs/requests and a second prompt-injection channel. It does not control `curl` or other Bash networking. | **Ask** every domain; direct Bash network clients are denied. | No live documentation/page retrieval. Cached/local docs must suffice. |
| `WebSearch` | Sends queries to Anthropic's search backend and returns titles/URLs. It has no rule specifier. | Sensitive repository content embedded in a query; untrusted search snippets influencing the agent. | **Ask** as the bare `WebSearch` tool. | No current web discovery; research must be performed manually or from supplied sources. |
| `Agent` (formerly commonly described as Task/subagent) | Spawns an autonomous subagent that may inherit tools. | Parallelized exploitation, hidden intermediate actions, and multiplied prompt-injection exposure. | **Ask** every `Agent` call. Named-agent allow rules should be added only after auditing that agent's tool list. | No delegated exploration/review; the main context must do the work serially. |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`, `TaskStop` | Manages Claude's in-session task list/background jobs. These are distinct from `Agent` in 2.1.220. | Workflow confusion or stopping legitimate background work; they do not by themselves grant shell/file authority. | **Allow/default** for list management; Bash background launches and `Agent` remain Ask-gated. | Planning/status tracking and controlled cancellation become awkward, but code execution is unaffected. |
| MCP: `document-catalog` | Exact tools are not visible in settings; likely searches/reads a document catalog. | Reading or transmitting documents outside this repo, plus prompt injection from cataloged content. | **Ask** every call until `/mcp` confirms exact tool names and side effects. | Catalog lookup and document retrieval. |
| MCP: `github` | Remote GitHub reads and writes using authenticated identity. | Source/issue exfiltration, comments/releases/PR mutation, branch changes, or social-engineering other users. | **Ask** every call; do not allow the server wholesale. | Automated PR/issue/release inspection and mutation. |
| MCP: `memwatch` | Exact contract is not present in the repo settings. | Exposure of process/memory/host state or an unexpected write if the server has active tools. | **Ask** every call pending `/mcp` tool-by-tool audit. | Memory diagnostics supplied by that server. |
| MCP: `opsmcp` | Host and Linux-Ops-Suite diagnostics/operations. | Host configuration or service mutation, broad filesystem reads, and operational data exfiltration. | **Ask** every call; mutation tools should be denied individually after enumeration. | Automated host-health and operations workflows. |
| MCP: `printpdf` | The observed tools create/convert PDFs, print files, inspect/cancel jobs, and update a journal. | Printing attacker content, canceling jobs, writing files/journal entries, or using conversion parsers on hostile input. | **Ask** every call. | PDF generation/conversion, printer operations, and journal automation. |
| `permissions.defaultMode` | Establishes the baseline for unlisted tools. | `acceptEdits`, `auto`, or `bypassPermissions` turns model judgment into authorization. | **Allow only `default`** normally; use `dontAsk` for Maximum Security. | `dontAsk` makes every unlisted promptable operation fail, including edits and builds. |
| `permissions.additionalDirectories` | Extends automatic file access beyond the launch directory. | Reads/edits other repos, home files, and secrets. | **Deny expansion**: set `[]`; add no persistent directory. | Cross-repo tasks and external documentation require a separately scoped session or one-time human action. |
| `permissions.disableBypassPermissionsMode` | Prevents activation of bypass mode and disables `--dangerously-skip-permissions`. | If omitted, a manipulated agent or unsafe launch can bypass the permission layer. | **Allow the control**: set it to the documented string `"disable"`. | Only deliberately unsafe bypass workflows; normal development is unaffected. |
| `hooks` / `disableAllHooks` | Runs commands at tool/session lifecycle events; hooks can block, ask, or allow requests. | A hook is privileged code triggered by attacker-controlled tool input or edited file content. An unsafe PermissionRequest hook can approve operations. | **Deny user/project hooks for this repo** with `disableAllHooks: true` until each hook is separately threat-modeled. Managed hooks remain an administrative option. | Atuin command capture, automatic Markdown-to-PDF conversion, and shared-file backups stop. |

### Why allowlists, not a denylist alone

Rules are evaluated Deny, then Ask, then Allow. A denylist cannot enumerate Python, Node, shell builtins, compiler plugins, `awk system()`, `find -exec`, `xargs`, downloaded binaries, or a newly introduced interpreter. The default profile therefore grants only exact low-risk commands and leaves executable operations at Ask. Denies are defense-in-depth for actions that should never be performed from this repo, not the primary boundary.

Claude Code parses shell separators, but wildcard Bash rules remain broad: one `*` can cross multiple arguments. Wrapper and argument parsing also evolves. Do not treat a narrow-looking shell prefix as a capability-safe API.

## Recommended `/permissions` configuration

This is the recommended Balanced Development project configuration. It is valid JSON and uses keys documented for Claude Code 2.1.220. It is a **replacement target**, not an overlay on the current local allowlist.

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "worktree": {
    "bgIsolation": "worktree",
    "baseRef": "head"
  },
  "permissions": {
    "defaultMode": "default",
    "disableBypassPermissionsMode": "disable",
    "additionalDirectories": [],
    "allow": [
      "Bash(cargo --version)",
      "Bash(rustc --version)",
      "Bash(trunk --version)",
      "Bash(cargo fmt --all -- --check)"
    ],
    "ask": [
      "Edit",
      "Write",
      "NotebookEdit",
      "Agent",
      "WebFetch",
      "WebSearch",
      "mcp__*",
      "Bash(git *)",
      "Bash(gh *)",
      "Bash(cargo build *)",
      "Bash(cargo check *)",
      "Bash(cargo clippy *)",
      "Bash(cargo test *)",
      "Bash(trunk build *)",
      "Bash(./dev gate *)",
      "Bash(bash *)",
      "Bash(sh *)",
      "Bash(fish *)",
      "Bash(python3 *)",
      "Bash(node *)",
      "Bash(perl *)",
      "Bash(ruby *)",
      "Bash(awk *)",
      "Bash(grep *)",
      "Bash(rg *)",
      "Bash(sed *)",
      "Bash(find *)",
      "Bash(xargs *)",
      "Bash(rsync *)",
      "Bash(cc *)",
      "Bash(gcc *)",
      "Bash(systemctl *)"
    ],
    "deny": [
      "Bash(sudo *)",
      "Bash(curl *)",
      "Bash(wget *)",
      "Bash(nc *)",
      "Bash(ncat *)",
      "Bash(socat *)",
      "Bash(ssh *)",
      "Bash(scp *)",
      "Bash(sftp *)",
      "Bash(pkill *)",
      "Bash(kill *)",
      "Bash(killall *)",
      "Bash(shutdown *)",
      "Bash(reboot *)",
      "Bash(mount *)",
      "Bash(umount *)",
      "Bash(./gv *)",
      "Bash(./dev serve *)",
      "Bash(*8080*)",
      "Read(//home/tom/.ssh/**)",
      "Read(//home/tom/.claude.json)",
      "Read(//home/tom/.claude/**)",
      "Read(//home/tom/.config/gh/**)",
      "Read(//home/tom/.gitconfig)",
      "Read(//home/tom/.cargo/credentials*)",
      "Read(//home/tom/.netrc)",
      "Read(//proc/**)",
      "Read(/.env)",
      "Read(/.env.*)",
      "Read(**/secrets/**)",
      "Edit(/.git/**)",
      "Edit(/.claude/settings.json)",
      "Edit(/.claude/settings.local.json)",
      "Edit(/.mcp.json)"
    ]
  },
  "disableAllHooks": true,
  "env": {
    "CARGO_NET_OFFLINE": "true",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_ASKPASS": "/bin/false",
    "SSH_ASKPASS": "/bin/false",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_PAGER": "cat",
    "PAGER": "cat",
    "BROWSER": "/bin/false"
  },
  "sandbox": {
    "enabled": true,
    "failIfUnavailable": true,
    "autoAllowBashIfSandboxed": false,
    "allowUnsandboxedCommands": false,
    "filesystem": {
      "denyWrite": [
        "./.git",
        "./.claude"
      ],
      "denyRead": [
        "~/"
      ],
      "allowRead": [
        ".",
        "~/.cargo/bin",
        "~/.cargo/registry",
        "~/.cargo/git",
        "~/.rustup/toolchains"
      ]
    },
    "credentials": {
      "files": [
        { "path": "~/.ssh", "mode": "deny" },
        { "path": "~/.claude", "mode": "deny" },
        { "path": "~/.claude.json", "mode": "deny" },
        { "path": "~/.config/gh", "mode": "deny" },
        { "path": "~/.gitconfig", "mode": "deny" },
        { "path": "~/.cargo/credentials.toml", "mode": "deny" },
        { "path": "~/.netrc", "mode": "deny" }
      ],
      "envVars": [
        { "name": "ANTHROPIC_API_KEY", "mode": "deny" },
        { "name": "CLAUDE_CODE_OAUTH_TOKEN", "mode": "deny" },
        { "name": "GH_TOKEN", "mode": "deny" },
        { "name": "GITHUB_TOKEN", "mode": "deny" },
        { "name": "AWS_ACCESS_KEY_ID", "mode": "deny" },
        { "name": "AWS_SECRET_ACCESS_KEY", "mode": "deny" },
        { "name": "OPENAI_API_KEY", "mode": "deny" },
        { "name": "SSH_AUTH_SOCK", "mode": "deny" }
      ]
    },
    "network": {
      "allowedDomains": [],
      "allowAllUnixSockets": false
    }
  }
}
```

Important limitation: `sandbox.network.strictAllowlist` is documented but ignored in project and local settings. Do not put it in this repository JSON and assume enforcement. If Tom wants unapproved domains to fail rather than prompt, put `"strictAllowlist": true` in trusted user or managed settings; 2.1.220 supports it. For organization-grade enforcement, managed settings should also set `allowManagedPermissionRulesOnly`, `allowManagedMcpServersOnly`, and `allowManagedHooksOnly` after the relevant allowlists are defined.

## Profile 1: Maximum Security

This profile is suitable for reading and narrowly checking a hostile checkout. It intentionally cannot implement or execute the project.

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "worktree": {
    "bgIsolation": "worktree",
    "baseRef": "head"
  },
  "permissions": {
    "defaultMode": "dontAsk",
    "disableBypassPermissionsMode": "disable",
    "additionalDirectories": [],
    "allow": [
      "Bash(cargo --version)",
      "Bash(rustc --version)",
      "Bash(trunk --version)",
      "Bash(cargo fmt --all -- --check)"
    ],
    "ask": [],
    "deny": [
      "Edit",
      "Write",
      "NotebookEdit",
      "Agent",
      "WebFetch",
      "WebSearch",
      "mcp__*",
      "Bash(git *)",
      "Bash(gh *)",
      "Bash(cargo build *)",
      "Bash(cargo check *)",
      "Bash(cargo clippy *)",
      "Bash(cargo test *)",
      "Bash(trunk build *)",
      "Bash(./dev *)",
      "Bash(./gv *)",
      "Bash(bash *)",
      "Bash(sh *)",
      "Bash(fish *)",
      "Bash(python3 *)",
      "Bash(node *)",
      "Bash(perl *)",
      "Bash(ruby *)",
      "Bash(awk *)",
      "Bash(cat *)",
      "Bash(grep *)",
      "Bash(rg *)",
      "Bash(head *)",
      "Bash(tail *)",
      "Bash(sed *)",
      "Bash(find *)",
      "Bash(xargs *)",
      "Bash(rsync *)",
      "Bash(cc *)",
      "Bash(gcc *)",
      "Bash(curl *)",
      "Bash(wget *)",
      "Bash(nc *)",
      "Bash(socat *)",
      "Bash(ssh *)",
      "Bash(scp *)",
      "Bash(sudo *)",
      "Bash(*8080*)",
      "Read(//home/tom/.ssh/**)",
      "Read(//home/tom/.claude.json)",
      "Read(//home/tom/.claude/**)",
      "Read(//home/tom/.config/gh/**)",
      "Read(//home/tom/.gitconfig)",
      "Read(//tmp/**)",
      "Read(//proc/**)",
      "Read(/.env)",
      "Read(/.env.*)"
    ]
  },
  "disableAllHooks": true,
  "env": {
    "CARGO_NET_OFFLINE": "true",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_ASKPASS": "/bin/false",
    "SSH_ASKPASS": "/bin/false",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_PAGER": "cat",
    "PAGER": "cat",
    "BROWSER": "/bin/false"
  },
  "sandbox": {
    "enabled": true,
    "failIfUnavailable": true,
    "autoAllowBashIfSandboxed": false,
    "allowUnsandboxedCommands": false,
    "filesystem": {
      "denyWrite": [
        "./.git",
        "./.claude"
      ],
      "denyRead": [
        "~/"
      ],
      "allowRead": [
        ".",
        "~/.cargo/bin",
        "~/.rustup/toolchains"
      ]
    },
    "credentials": {
      "files": [
        { "path": "~/.ssh", "mode": "deny" },
        { "path": "~/.claude", "mode": "deny" },
        { "path": "~/.claude.json", "mode": "deny" },
        { "path": "~/.config/gh", "mode": "deny" },
        { "path": "~/.gitconfig", "mode": "deny" },
        { "path": "~/.cargo/credentials.toml", "mode": "deny" },
        { "path": "~/.netrc", "mode": "deny" }
      ],
      "envVars": [
        { "name": "ANTHROPIC_API_KEY", "mode": "deny" },
        { "name": "CLAUDE_CODE_OAUTH_TOKEN", "mode": "deny" },
        { "name": "GH_TOKEN", "mode": "deny" },
        { "name": "GITHUB_TOKEN", "mode": "deny" },
        { "name": "AWS_ACCESS_KEY_ID", "mode": "deny" },
        { "name": "AWS_SECRET_ACCESS_KEY", "mode": "deny" },
        { "name": "OPENAI_API_KEY", "mode": "deny" },
        { "name": "SSH_AUTH_SOCK", "mode": "deny" }
      ]
    },
    "network": {
      "allowedDomains": [],
      "deniedDomains": ["*"],
      "allowAllUnixSockets": false
    }
  }
}
```

### Maximum Security breakage in this repository

- `./dev gate` is impossible: clippy, tests, and Trunk are denied. Only the exact non-writing rustfmt check remains.
- `cargo test --workspace` is impossible. This is intentional because build scripts and proc macros execute during a nominal test command.
- The escape battery is impossible: its Rust test compiles C with `cc`, spawns `gv-sandbox`, applies Landlock/seccomp, and invokes bwrap.
- The mutation matrix is impossible: it copies the tree, applies patches, and rebuilds two crates seven times; `rsync`, `patch`, those Cargo builds, the C probes, and bwrap execution are denied.
- Application or documentation edits are impossible because Edit/Write are denied.
- Git history/status commands are denied along with Git writes. Review must use the checked-out files or a separately trusted read-only export.
- Web research, subagents, and all five MCP servers are unavailable.

This is a review/quarantine profile, not a development profile. That is an honest limitation, not a reason to weaken it silently.

## Profile 2: Balanced Development

The Balanced profile is the Recommended configuration above, repeated here literally so either profile can be copied independently.

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "worktree": {
    "bgIsolation": "worktree",
    "baseRef": "head"
  },
  "permissions": {
    "defaultMode": "default",
    "disableBypassPermissionsMode": "disable",
    "additionalDirectories": [],
    "allow": [
      "Bash(cargo --version)",
      "Bash(rustc --version)",
      "Bash(trunk --version)",
      "Bash(cargo fmt --all -- --check)"
    ],
    "ask": [
      "Edit",
      "Write",
      "NotebookEdit",
      "Agent",
      "WebFetch",
      "WebSearch",
      "mcp__*",
      "Bash(git *)",
      "Bash(gh *)",
      "Bash(cargo build *)",
      "Bash(cargo check *)",
      "Bash(cargo clippy *)",
      "Bash(cargo test *)",
      "Bash(trunk build *)",
      "Bash(./dev gate *)",
      "Bash(bash *)",
      "Bash(sh *)",
      "Bash(fish *)",
      "Bash(python3 *)",
      "Bash(node *)",
      "Bash(perl *)",
      "Bash(ruby *)",
      "Bash(awk *)",
      "Bash(grep *)",
      "Bash(rg *)",
      "Bash(sed *)",
      "Bash(find *)",
      "Bash(xargs *)",
      "Bash(rsync *)",
      "Bash(cc *)",
      "Bash(gcc *)",
      "Bash(systemctl *)"
    ],
    "deny": [
      "Bash(sudo *)",
      "Bash(curl *)",
      "Bash(wget *)",
      "Bash(nc *)",
      "Bash(ncat *)",
      "Bash(socat *)",
      "Bash(ssh *)",
      "Bash(scp *)",
      "Bash(sftp *)",
      "Bash(pkill *)",
      "Bash(kill *)",
      "Bash(killall *)",
      "Bash(shutdown *)",
      "Bash(reboot *)",
      "Bash(mount *)",
      "Bash(umount *)",
      "Bash(./gv *)",
      "Bash(./dev serve *)",
      "Bash(*8080*)",
      "Read(//home/tom/.ssh/**)",
      "Read(//home/tom/.claude.json)",
      "Read(//home/tom/.claude/**)",
      "Read(//home/tom/.config/gh/**)",
      "Read(//home/tom/.gitconfig)",
      "Read(//home/tom/.cargo/credentials*)",
      "Read(//home/tom/.netrc)",
      "Read(//proc/**)",
      "Read(/.env)",
      "Read(/.env.*)",
      "Read(**/secrets/**)",
      "Edit(/.git/**)",
      "Edit(/.claude/settings.json)",
      "Edit(/.claude/settings.local.json)",
      "Edit(/.mcp.json)"
    ]
  },
  "disableAllHooks": true,
  "env": {
    "CARGO_NET_OFFLINE": "true",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_ASKPASS": "/bin/false",
    "SSH_ASKPASS": "/bin/false",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_PAGER": "cat",
    "PAGER": "cat",
    "BROWSER": "/bin/false"
  },
  "sandbox": {
    "enabled": true,
    "failIfUnavailable": true,
    "autoAllowBashIfSandboxed": false,
    "allowUnsandboxedCommands": false,
    "filesystem": {
      "denyWrite": [
        "./.git",
        "./.claude"
      ],
      "denyRead": [
        "~/"
      ],
      "allowRead": [
        ".",
        "~/.cargo/bin",
        "~/.cargo/registry",
        "~/.cargo/git",
        "~/.rustup/toolchains"
      ]
    },
    "credentials": {
      "files": [
        { "path": "~/.ssh", "mode": "deny" },
        { "path": "~/.claude", "mode": "deny" },
        { "path": "~/.claude.json", "mode": "deny" },
        { "path": "~/.config/gh", "mode": "deny" },
        { "path": "~/.gitconfig", "mode": "deny" },
        { "path": "~/.cargo/credentials.toml", "mode": "deny" },
        { "path": "~/.netrc", "mode": "deny" }
      ],
      "envVars": [
        { "name": "ANTHROPIC_API_KEY", "mode": "deny" },
        { "name": "CLAUDE_CODE_OAUTH_TOKEN", "mode": "deny" },
        { "name": "GH_TOKEN", "mode": "deny" },
        { "name": "GITHUB_TOKEN", "mode": "deny" },
        { "name": "AWS_ACCESS_KEY_ID", "mode": "deny" },
        { "name": "AWS_SECRET_ACCESS_KEY", "mode": "deny" },
        { "name": "OPENAI_API_KEY", "mode": "deny" },
        { "name": "SSH_AUTH_SOCK", "mode": "deny" }
      ]
    },
    "network": {
      "allowedDomains": [],
      "allowAllUnixSockets": false
    }
  }
}
```

### Balanced Development tradeoffs

- Source and documentation edits are possible after a visible approval. `.git`, live Claude settings, and MCP configuration remain blocked.
- `./dev gate` is Ask-gated at its executable stages: native and wasm clippy, workspace tests, and Trunk build; only the exact rustfmt check is allowed. C compilers and bwrap-related shell orchestration are also Ask-gated. The mutation matrix copies the tree and rebuilds two crates seven times, so it is Ask-gated as executable code rather than treated as a read-only verification script. Approving any of these authorizes repository and dependency code execution inside the OS sandbox; it is not a claim that the command is intrinsically safe.
- Direct network clients are denied. Cargo is offline by default. Updating dependencies therefore requires a separate, explicit maintenance session with a reviewed lockfile and a narrow network policy.
- Git and `gh` commands prompt. Because `.git` writes are blocked by the sandbox, Claude can inspect Git after approval but cannot commit, change refs/config, or push. A human or trusted external checkpointer must perform Git writes.
- The escape battery may fail or produce altered observations under nested bwrap/seccomp/Landlock. Do not add `cargo test` or `./dev gate` to `sandbox.excludedCommands`; use a disposable real-host verifier instead.
- Hooks are disabled, so Atuin capture, PDF auto-generation, and the edit-backup hook do not run in this repository.

## Hardening beyond `/permissions`

### Filesystem and process isolation

- Enable the built-in sandbox, set `failIfUnavailable: true`, set `autoAllowBashIfSandboxed: false`, and set `allowUnsandboxedCommands: false`. Otherwise a failed sandbox can silently become a host command.
- The default sandbox permits reads across most of the computer, including credentials. Use `denyRead: ["~/"]` with specific read re-openings for this checkout and the Rust toolchain/cache. Keep credential paths separately denied.
- Keep `.git` and `.claude` non-writable to sandboxed subprocesses. This prevents malicious tests/build scripts from changing refs, hooks, Git config, or Claude policy.
- Do not add Docker, `cargo`, `./dev`, `cc`, interpreters, or bwrap to `excludedCommands`. An exclusion moves repository-controlled code outside the OS boundary.
- Use a disposable VM/container with no credentials for real-host sandbox verification. This project's own tests deliberately exercise namespaces, Landlock, seccomp, hooks, and bwrap; nesting them under Claude's sandbox can invalidate the measurement.

### Git worktrees and shared state

- Set `worktree.bgIsolation` to `worktree`, not `none`. Background agents should not edit the main checkout.
- Prefer a trusted wrapper or human-created task worktree with a dedicated branch. Claude should receive the worktree path, not permission to create/change shared Git worktrees itself.
- Do not symlink caches or writable directories from the main checkout into an untrusted worktree unless their contents are non-executable and non-sensitive.
- The current external auto-checkpointer is a separate authority. Because it can commit an injected edit, its staging policy must exclude `.claude`, `.github/workflows`, executable scripts, `build.rs`, and other high-risk paths unless a human has reviewed them. Claude permissions cannot constrain a different process.

### Git configuration and hooks

- Keep Claude's subprocess environment on `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`, and noninteractive askpass helpers. This prevents ambient aliases, credential helpers, pagers, diff drivers, and global hooks from becoming hidden execution paths.
- For ordinary read-only Git commands, prefer explicit `git -c core.hooksPath=/dev/null -c diff.external= --no-pager ...` invocations after approval. Do not set repository Git config from Claude.
- `core.hooksPath` matters because it can redirect hook execution to tracked or attacker-written files. `git commit`, merge, rebase, checkout, and push can execute hooks. Git filters, diff drivers, fsmonitor, pagers, and aliases can also execute programs.
- A blanket global `core.hooksPath=/dev/null` would break this repository's escape battery: the battery intentionally creates temporary repositories and installs hooks to test containment. Disable hooks for Claude's own Git operations, while letting the isolated test harness control its fixture-local hook setup.
- `cargo test` can execute Rust test code that itself invokes Git. Therefore “the agent did not run git commit directly” does not prove hooks were not executed. The OS sandbox and disposable verifier are the real boundary.

### Environment variables and credentials

- Remove model subprocess access to OAuth/API/GitHub/cloud tokens and `SSH_AUTH_SOCK` with `sandbox.credentials.envVars` deny entries.
- Block credential files even if no corresponding variable is present. Do not rely on “Claude will not print it.”
- Keep Cargo offline for routine work. Dependency updates are a separate high-risk workflow because they introduce new build scripts and proc macros.
- Do not place secrets in project `env` settings, shell command strings, hook arguments, or test fixtures. Project content is part of the injection surface and may be committed.
- Pin a minimal `PATH` in a trusted launcher if practical. A settings-level PATH pin is not proposed here because this host's exact Rust/Trunk toolchain paths can change; an incorrect PATH would either break the gate or select an unexpected executable.

### MCP and hooks administration

- Run `/mcp` in a trusted interactive session and enumerate exact tools before considering per-tool allows. A server-level allow rule grants every current and future tool from that server.
- For stronger enforcement, use managed settings to allowlist MCP servers and hooks. Project settings can propose policy but are repository-controlled and cannot be the ultimate trust root.
- Audit every hook script as privileged code. Hooks that interpolate tool input into a shell command must be rejected or rewritten to parse structured JSON without `eval`, `sh -c`, or unquoted expansion.

## Self-challenge: attacks that remain

1. **Allowlisted executable replacement.** `cargo`, `rustc`, `trunk`, or `rustfmt` could be replaced in a writable PATH directory, turning even `--version` or fmt into code execution. **Decision:** tighten outside this repo by making toolchain/bin directories non-writable to Claude and launching with a trusted minimal PATH. If that cannot be guaranteed, move all four rules from Allow to Ask.
2. **Allowlisted interpreters and shell-fragment tools.** `python3`, Node, shells, and similar interpreters are universal escapes; `awk` has `system()`, `find` has `-exec`/`-delete`, and `xargs` executes commands or shell fragments. Any one broad Allow would void the rest of the command allowlist. **Decision:** the recommended profile allowlists no interpreter: these tools are Ask-gated in Balanced and denied in Maximum. Never add broad prefix allows.
3. **`build.rs` and proc macros.** A bare `cargo test`, `cargo check`, clippy, build, or Trunk build can execute dependency and workspace code before tests start. The lockfile already includes many proc-macro crates. **Decision:** Ask is accepted only inside the OS sandbox after reviewing Cargo changes; real assurance requires a disposable VM/read-only source mount with a separate writable target directory.
4. **Git hooks and hidden Git executors.** Tests or scripts can create repositories, set `core.hooksPath`, and invoke Git. Git config can also define aliases, filters, pagers, diff drivers, or fsmonitor commands. **Decision:** deny writes to the real `.git`, suppress ambient Git config, and use a disposable verifier for the intentional hook battery. Do not globally disable hooks in a way that makes the battery vacuous.
5. **Write-now, execute-later.** An approved Edit can add a malicious script, CI workflow, `build.rs`, Cargo config, test, hook, or generated file that a human/checkpointer later executes or commits. **Decision:** retain Ask for every edit, block Claude settings/Git metadata, and require a human diff review before builds and before the external checkpointer stages high-risk paths. If the checkpointer cannot enforce that separation, tighten further by stopping it during hostile-content review sessions.
6. **Approved build can rewrite the checkout.** The sandbox's working-directory write access means a malicious test/build script can alter source even though the Edit tool was Ask-gated. **Decision:** accepted only for Balanced inside a disposable worktree with a recoverable baseline. For hostile changes, use Maximum or a read-only source mount; Claude Code's project sandbox settings do not currently express “project read-only except target writable” as a simple profile here.
7. **Read-to-web exfiltration.** Claude can read legitimate proprietary source, then place it in a WebSearch query or an approved MCP/WebFetch call. **Decision:** web and MCP remain Ask-gated, direct Bash network clients are denied, and credentials are removed. For sensitive reviews, use Maximum, which denies all web/MCP access and sandbox network traffic.
8. **MCP semantic drift.** A previously read-only server can add a write tool while a server-wide allow persists. **Decision:** no server-wide allows; Ask every call and periodically re-enumerate `/mcp`.
9. **Hook parser vulnerabilities.** The current Python PDF hook and backup hook process attacker-influenced paths/content, while Atuin observes command activity. **Decision:** disable all hooks in both proposed profiles. Re-enable only individually after code review and adversarial path/content tests.
10. **Nested-sandbox false evidence.** Claude's bwrap/seccomp/network sandbox can make Git-Vista containment tests pass for the wrong outer-layer reason or fail because user namespaces/sockets are unavailable. **Decision:** do not treat an in-sandbox battery result as real-host evidence. Run the release battery in a disposable, credential-free verifier whose outer policy is documented.

## Application checklist (not executed)

1. Re-hash and re-audit both settings files; they are live and may have changed.
2. Back up `.claude/settings.local.json` outside the repository with mode 0600.
3. Replace, rather than merge, its accumulated permission array. Confirm no user/managed permission array reintroduces broad allows.
4. Apply one profile in a disposable clone/worktree first.
5. Run `claude doctor` and `/status`; project/local settings are rejected as a whole when invalid, so JSON parsing alone is insufficient.
6. Inspect `/permissions`, `/sandbox`, `/hooks`, and `/mcp` in the running 2.1.220 client. Confirm the resolved setting sources and exact MCP tool names.
7. Negative-test one denied secret read, one direct network client, one interpreter, one Git write, one MCP call, and one port-8080 command. Each must deny or prompt as declared.
8. Positive-test repository reading, a one-time approved edit in a disposable file, rustfmt check, and—only in a disposable verifier—the full `./dev gate`, escape battery, and mutation matrix.
9. Re-audit after any Claude Code upgrade because rule parsing, built-in read-only classification, and sandbox behavior are versioned security surfaces.
