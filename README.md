# Chief

Chief runs coding agents (Codex, Claude Code, Cursor, opencode, pi) unattended against a Git repository and keeps calling them until their output stops changing.

`chief review` uses that loop to review a branch: the agent writes findings to a YAML file, and chief stops once a pass finds nothing new.

## Install

```bash
cargo install --locked chief --git https://github.com/nareto/chief
```

You also need `git` and at least one agent CLI on `PATH` (`codex` is the default; `claude`, `cursor-agent`, `opencode` and `pi` also work).

## Quick start: review a branch

```bash
cd your-repo
git switch your-feature-branch
chief review --agent claude --max-loop-iterations 5
cat .chief/review/findings.yaml
```

How it converges: every pass starts a fresh agent with no memory of earlier passes. It reads the findings file, reviews the diff against the base branch and appends only findings that are not already listed. Chief stops after the first pass that adds nothing, or after `--max-loop-iterations` passes.

Each finding has a severity from P0 to P3. P0 and P1 mean "this should stop the merge". Chief never edits your code and never commits during a review. To post findings on pull requests and fail the check on P0/P1, see [`examples/github-review.yml`](./examples/github-review.yml) and [docs/review.md](./docs/review.md).

## What else chief does

- **`loop_file`**: run one task from a prompt or Markdown file until the agent stops changing files, then commit. See [docs/loop_file.md](./docs/loop_file.md).
- **`refactor` and `chief_backend`**: turn requirements into a todo queue and work through it in isolated Git worktrees, from the CLI or from a multi-project HTTP/WebSocket backend. See [docs/refactor-and-backend.md](./docs/refactor-and-backend.md).
- **Suites and readiness checks**: tell chief how to run your tests and linters. Queued todos are checked against them, and the backend runs them before starting a project. See [docs/suites-and-readiness.md](./docs/suites-and-readiness.md).

## Safety

By default chief turns off each agent's own sandbox and approval prompts, so agents can change anything your user can. Run it in a container, VM or disposable checkout, or set `agent_sandbox: native` (`--agent-sandbox native`) to keep the sandbox on for Codex, Claude Code and Cursor. Details: [docs/safety.md](./docs/safety.md).

## Documentation

- [docs/review.md](./docs/review.md): `chief review`, the findings format, CI usage
- [docs/configuration.md](./docs/configuration.md): `.chief/chief.yaml`, CLI overrides, introspection commands
- [docs/loop_file.md](./docs/loop_file.md): single-task convergence runs
- [docs/refactor-and-backend.md](./docs/refactor-and-backend.md): the todo queue, requirements ingestion and `chief_backend`
- [docs/suites-and-readiness.md](./docs/suites-and-readiness.md): suite commands, readiness checks and caches
- [docs/safety.md](./docs/safety.md): agent sandbox modes, organization policies and destructive operations
- [docs/architecture.md](./docs/architecture.md): how the pieces fit together

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets
```

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](./LICENSE).
