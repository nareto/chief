# Safety

Chief runs agents with nobody watching, so an approval prompt would hang the run. Chief therefore launches every agent in one of two modes, set with `chief.agent_sandbox` in `.chief/chief.yaml` or `--agent-sandbox` on the command line.

## `bypass` (default)

Chief turns off the agent's own sandbox and approval prompts:

| Agent | Flags chief adds |
| --- | --- |
| codex | `--dangerously-bypass-approvals-and-sandbox` |
| claude | `--dangerously-skip-permissions` |
| cursor-agent | `--force --trust --approve-mcps --sandbox disabled` |
| pi | `--approve` |
| opencode | none; opencode's own permission config decides |

The agent can then read, write, delete and run anything the user running chief can, including reaching the network and running MCP servers. Prompt injection from the code under review, or from anything else the agent reads, can turn into commands on your machine.

Use `bypass` only where that is acceptable: a container, a VM, a CI runner that is thrown away after the job, or a disposable checkout under a user with no access to anything valuable.

`bypass` stays the default so existing setups keep working. It may change in a later release.

## `native`

Chief keeps the agent's own sandbox on and grants only the permissions an unattended run needs:

| Agent | Flags chief adds | What that gives you |
| --- | --- | --- |
| codex | `--sandbox workspace-write` | Commands and edits can write only inside the workspace (plus temp dirs). `.git` stays read-only. Network is off by default. |
| claude | `--permission-mode acceptEdits --settings '{"sandbox":{"enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":false}}'` | File edits are auto-approved inside the working directory. Bash commands run in Claude's OS sandbox (bubblewrap and socat on Linux, Seatbelt on macOS). Claude refuses to start if the sandbox cannot start, and it never retries a failed command outside the sandbox. Tools that would still need approval are denied, since nobody can answer. |
| cursor-agent | `--force --trust --sandbox enabled` | Terminal commands run in Cursor's sandbox. `--force` is needed because print mode otherwise only proposes edits. MCP servers are not auto-approved. |
| opencode, pi | rejected | Chief knows no sandbox flag for them, so it refuses to start rather than pretend. Use `bypass` inside an isolated environment. |

On Linux, Claude Code's sandbox needs `bubblewrap` and `socat`. On Ubuntu 24.04 and later, the default AppArmor policy also stops `bubblewrap` from creating user namespaces until you add a profile for it; Claude Code's [sandboxing docs](https://code.claude.com/docs/en/sandboxing) give the profile, and [`examples/github-review.yml`](../examples/github-review.yml) installs it on GitHub's Ubuntu runners. If the sandbox cannot start, Claude Code exits instead of running unsandboxed, so a missing setup shows up as a failed run.

What `native` does not do:

- Each sandbox covers that agent's command execution. Built-in file tools (Claude's Edit and Write, Cursor's file writes) follow the agent's permission rules, not the OS sandbox. Claude limits auto-approved edits to the working directory; for Cursor, add `Write()` deny rules in `~/.cursor/cli-config.json` or `.cursor/cli.json` if you need limits.
- Reads are mostly unrestricted. The agents can usually read files outside the project, including credentials in your home directory, unless you configure read restrictions in the agent's own settings.
- MCP servers are separate processes. A sandboxed agent can still call an MCP server that is not sandboxed. Keep `mcp_servers: {}` (the `chief init` default) unless you need them.
- Agents can be configured further in their own config files (Codex `config.toml`, Claude `settings.json`, Cursor `cli-config.json`). Chief's flags combine with those settings and do not replace stricter ones.

`native` trades capability for safety. Tasks that need the network, a package install, or writes outside the repository (a shared build cache, for example) may fail under it. `chief review` only reads code and writes one file in the repository, so it is the best fit for `native`.

## Organization policies

Administrators can forbid the bypass mode. Chief does not try to get around such policies. With `bypass`, the agent then refuses to start or fails, so switch to `native`.

- **Codex**: an admin-enforced `requirements.toml` can restrict `allowed_sandbox_modes` (for example to `["read-only", "workspace-write"]`) and `allowed_approval_policies`. `native` asks for `workspace-write` and fits that example.
- **Claude Code**: managed settings can set `permissions.disableBypassPermissionsMode: "disable"`, which blocks `--dangerously-skip-permissions`. `native` uses `acceptEdits` and should work under that policy. Managed settings take precedence over the sandbox settings chief passes with `--settings`.
- **Cursor**: team and enterprise admin controls exist but were not tested with chief.

### What was tested

For chief 0.2.0, on Linux:

- Command construction for every agent and mode is covered by unit tests (`src/agent/tests.rs`).
- `codex sandbox` in `workspace-write` mode was run directly: writes inside the workspace succeeded, writes to `.git` failed with a read-only file system error, and the default read-only mode blocked all writes.
- End-to-end agent runs under `native` (Codex, Claude Code, Cursor) were not completed: the test machine had no valid credentials for Codex or Claude Code, and the Cursor CLI could not run there.
- Real organization policies (a managed Codex `requirements.toml`, Claude managed settings with bypass disabled) were not tested, because installing them needs system-level access the test machine did not allow. The behavior described above comes from each vendor's documentation.

Reports from runs under real policies are welcome as issues.

## `disallowed_paths` is not enforced

The agent interface carries a `disallowed_paths` list, but no adapter turns it into a restriction and no caller sets it today. Do not rely on it to protect files. Use `native` with the agent's own deny rules, or run chief where the files are not present.

## Destructive operations

Some chief operations delete data on purpose:

- The backend `reset_workspace` action runs `git reset --hard HEAD` and `git clean -fd` in the target project. It keeps `.chief/chief.db` and its SQLite sidecar files, but throws away all other uncommitted work.
- The backend `reset_db` action recreates `.chief/chief.db`.
- Worktree cleanup removes temporary worktrees with `git worktree remove --force` and deletes worker branches.
- `loop_file` runs in your checkout and commits whatever the agent left there. Start from a clean tree.

Queued `refactor` work runs in separate worktrees, so your main checkout is only touched by the final merge.

`chief review` never commits, and it fails if the agent edits a tracked file other than the findings file or moves `HEAD`. Chief does not undo such edits; it stops and lists them so you can inspect them. This is a check after the fact, not a sandbox.
