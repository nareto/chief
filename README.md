# Chief (Rust)

**Chief** is an automated TDD orchestrator that enforces discipline on your coding agent: write failing tests first, then implement, then verify.

This is an implementation of the [Ralph Wiggum method](https://ghuntley.com/ralph/) from Geoffrey Huntley.

## How It Works

Chief runs a phase-based loop for each todo:

1. **RED**: write or refine tests
  - stability required to pass phase (see next section)
2. **GREEN**: implement features
  - tests must pass to pass the phase.
3. **POST_GREEN**: optional lint/build commands after tests pass.
4. **Commit**: auto-commit and tag on success.

Chief records all events (agent prompts/responses, diffs, test outputs) in `chief.db` and queries it to give context to subsequent prompts.

### Iteration Loops

Ralph Wiggum works great for tasks where you can easily verify correctness (e.g. green phase). For other tasks, we use the idea of stability: if the agent, when asked to improve on the existing work, does not do any changes, and it does so twice in a row, then we consider the task a success.

Chief uses two loop types:

- **Convergence loop**: require consecutive stable outcomes (used in RED and no-test GREEN tasks).
- **Until-pass loop**: repeatedly run checks and ask the agent to fix failures until all checks pass (used for linting, GREEN with tests, and POST_GREEN).

## ⚠️ Potential Data Loss Warning

**Chief performs destructive Git operations.**

To recover from failed TDD cycles, this tool utilizes `git checkout` to revert changes. It assumes it is the sole actor in the repository during execution.

- **Start Clean:** Ensure you have no uncommitted changes or untracked files before running.
- **Hands Off:** Do not modify files manually while the script is active.
- **Data Loss:** Any file created or modified manually during a Chief run runs a high risk of being deleted if the agent triggers a rollback.

## Rust Runtime Layout

Chief is a Rust TDD orchestration system with:

- `cli` binary for single-project execution (current Chief flow).
- `backend` binary for multi-project orchestration + introspection API.
- responsive frontend (`frontend/`) for operating the backend.

The system keeps **per-project** state local:

- `chief.toml`
- `todos.json`
- `chief.db` (SQLite)

There is no centralized database.

## Architecture

Core library modules:

- `src/domain.rs`: strongly-typed core models (`Todo`, `EventRecord`, `JobRecord`, `Phase`, `TodoStatus`, etc).
- `src/config.rs`: `chief.toml` parsing for `[chief]`, `[backend]`, and `[[suites]]`.
- `src/storage.rs`: per-project SQLite + `todos.json` synchronization.
- `src/prompt.rs`: prompt loading/rendering from `prompts/*.md` using Jinja syntax (`minijinja`).
- `src/agent.rs`: coding-agent abstraction and concrete CLI agent adapters.
- `src/git.rs`: git/worktree operations.
- `src/flow.rs`: pluggable orchestration building blocks.
- `src/service.rs`: project registry + execution engine.
- `src/scheduler.rs`: multi-agent backend scheduler.

### Pluggable flow design (lego bricks)

`src/flow.rs` is intentionally modular:

- `PhaseStrategy` trait: behavior for one phase (`RED`, `GREEN`, `POST_GREEN`, etc).
- `LoopPolicy` trait: convergence loops vs until-pass loops.
- `TodoFlow` trait: composition of phase strategies into full todo workflows.

Included flows:

- `tdd` (default): RED -> GREEN -> POST_GREEN.
- `single_prompt`: one implementation-focused loop (for experimentation).

Adding a new strategy means implementing `TodoFlow` and (optionally) custom `PhaseStrategy` + `LoopPolicy` combinations.

## Multi-agent backend model

The backend manages multiple projects under one parent directory.

For each project, the scheduler supports configurable parallel coding agents:

- default is `1` (no parallelism).
- when `agents > 1`, each worker runs in a dedicated git worktree.
- todo selection is serialized (one selector at a time) to reduce conflicts.
- for workers after the first, selector prompt (`prompts/todo_select.md`) includes:
  - available todos (not done, not in progress)
  - currently in-progress todos
- each worker processes exactly one todo, exits, and then the scheduler spawns the next worker.
- successful worker branches are merged back to mainline branch.

## Prompts

All prompts are Markdown templates with Jinja syntax under:

- `prompts/red.md`
- `prompts/green.md`
- `prompts/post_green.md`
- `prompts/lint_fix.md`
- `prompts/requirements.md`
- `prompts/todo_select.md`

Each project can own its own `prompts/` directory.

## CLI usage

Run one project directly:

```bash
cargo run --bin cli -- --project-dir /path/to/project
```

Common options:

```bash
cargo run --bin cli -- \
  --project-dir /path/to/project \
  --flow tdd \
  --model gpt-5
```

Requirements -> todos mode:

```bash
cargo run --bin cli -- \
  --project-dir /path/to/project \
  --requirements "Add JWT auth and refresh token rotation"
```

Tail events:

```bash
cargo run --bin cli -- --project-dir /path/to/project --tail-events 50
```

## Backend usage

Run backend over a parent directory containing multiple git projects:

```bash
cargo run --bin backend -- --parent-dir /path/to/projects --port 8000
```

Frontend can be served by nginx in docker compose (see below), and calls backend APIs for:

- project status and runtime controls
- start/stop project schedulers
- job list and todo table
- filtered logs
- requirements ingestion
- websocket terminal command execution

## Docker compose

Build and run:

```bash
docker compose up --build
```

Projects mount is generic in `docker-compose.yml`:

```bash
CHIEF_PROJECTS_PARENT=/absolute/path/to/projects docker compose up --build
```

For machine-specific setup (for example NFS), use `docker-compose.override.yml` locally. That file is gitignored.

Services:

- `backend` on `http://localhost:8000`
- `frontend` on `http://localhost:3000`

`frontend` proxies `/api/*` and websocket terminal traffic to backend.

## Config (`chief.toml`) quick example

```toml
[chief]
agent = "codex"
model = "gpt-5"
max_retries = 10
agent_timeout_seconds = 2700

[backend]
default_agents_per_project = 1
max_agents_per_project = 8

[[suites]]
name = "backend"
language = "Rust"
framework = "cargo test"
test_root = "."
test_command = "cargo test"
target_type = "project"
lint_command = "cargo clippy"
post_green_command = "cargo test"
```

See `chief.toml.example` for more patterns.

## Current status

- Rust library and both binaries compile (`cargo check`).
- Legacy Python implementation has been removed.
