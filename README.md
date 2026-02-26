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

Ralph Wiggum works great for tasks where you can easily verify correctness (e.g. green phase). For other tasks, we use the idea of convergence: if the agent, when asked to improve on the existing work, does not do any changes, and it does so twice in a row, then we consider the task a success.

Chief uses two loop types:

- **Convergence loop**: require consecutive stable outcomes (used in RED and no-test GREEN tasks).
- **Until-pass loop**: repeatedly run checks and ask the agent to fix failures until all checks pass (used for linting, GREEN with tests, and POST_GREEN).

## ⚠️ Potential Data Loss Warning

**Chief performs destructive Git operations.**

To recover from failed TDD cycles, this tool may use `git reset --hard` and `git clean -fd` to revert local changes between retry loops. It assumes it is the sole actor in the repository during execution.

- **Start Clean:** Ensure you have no uncommitted changes or untracked files before running.
- **Hands Off:** Do not modify files manually while the script is active.
- **Data Loss:** Any file created or modified manually during a Chief run runs a high risk of being deleted if the agent triggers a rollback.

## Rust Runtime Layout

Chief is a Rust TDD orchestration system with:

- `chief` binary for single-project execution (current Chief flow).
- `chief_backend` binary for multi-project orchestration + introspection API.
- responsive frontend (`frontend/`) for operating the backend.

The system keeps **per-project** state local:

- `chief.yaml`
- `todos.yaml`
- `chief.db` (SQLite)

There is no centralized database.

## Architecture

Core library modules:

- `src/domain.rs`: strongly-typed core models (`Todo`, `EventRecord`, `JobRecord`, `Phase`, `TodoStatus`, etc).
- `src/config.rs`: `chief.yaml` parsing for `chief` and `suites`.
- `src/storage.rs`: per-project SQLite + `todos.yaml` synchronization.
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

- `single_prompt` (default): convergence loop with per-iteration checks over the todo's configured suites.
- `tdd`: legacy RED -> GREEN -> POST_GREEN flow.
- `loop_file` (CLI-only): convergence loop driven by a markdown file loaded via `chief loop_file --file ...`.

Adding a new strategy means implementing `TodoFlow` and (optionally) custom `PhaseStrategy` + `LoopPolicy` combinations.

## Multi-agent backend model

The backend manages multiple projects under one projects directory, plus optional manual project paths.

For each project, the scheduler supports configurable parallel coding agents:

- default is `1` (single worker).
- each claimed todo runs in a dedicated git worktree at `../<project_name>__worktrees/<job_id>`.
- `agents` controls how many workers can run in parallel.
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
cargo run --bin chief -- --project-dir /path/to/project
```

Run one `loop_file` execution directly from a markdown plan/task file (no todo queueing):

```bash
cargo run --bin chief -- --project-dir /path/to/project loop_file --file plan.md
```

Notes for `loop_file`:

- It always runs as a single todo execution (no outer todo queue).
- Outer retries are effectively disabled (`max_retries = 1` for this flow).
- Default inner loop iterations are `20` when `chief.max_loop_iterations` is not explicitly set in `chief.yaml`.
- If `chief.max_loop_iterations` is explicitly set, that value takes precedence.

From inside a target project directory (with `chief` on `PATH`), initialize symlinked example files and minimal local configs:

```bash
chief init
```

This assumes the chief repo is in `../chief`, otherwise specify it:

```bash
chief init --chief-root /path/to/chief
```

`init` is idempotent: existing files/symlinks are left unchanged and only missing ones are created.

Common options:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --flow tdd \
  --model gpt-5
```

Requirements -> todos mode:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --requirements "Add JWT auth and refresh token rotation"
```

Tail events:

```bash
cargo run --bin chief -- --project-dir /path/to/project tail-events --limit 50
```

Clean completed todos:

```bash
cargo run --bin chief -- --project-dir /path/to/project clean-done
```

Run suite commands directly for one configured suite:

```bash
# test and lint commands support --target (for {target} placeholders)
cargo run --bin chief -- --project-dir /path/to/project suite test --suite backend --target src/lib.rs
cargo run --bin chief -- --project-dir /path/to/project suite lint --suite backend --target src

# prepare/fix commands from chief.yaml
cargo run --bin chief -- --project-dir /path/to/project suite test_init --suite backend
cargo run --bin chief -- --project-dir /path/to/project suite test_setup --suite backend
cargo run --bin chief -- --project-dir /path/to/project suite lint_fix --suite backend --target src
```

## Backend usage

Run backend over a projects directory containing multiple git projects:

```bash
cargo run --bin chief_backend -- --projects-dir /path/to/projects --port 8000
```

Add extra projects outside that directory with one or more `--project` flags:

```bash
cargo run --bin chief_backend -- \
  --projects-dir /path/to/projects \
  --project /path/to/another/repo \
  --project /path/to/one-more/repo \
  --port 8000
```

Backend security flags/environment:

- `CHIEF_API_TOKEN` (or `--api-token`): optional, but strongly recommended for deployment.
- `--allow-origin`: CORS allowlist. Defaults to `http://localhost:3000`.
- `--enable-terminal`: terminal websocket is disabled by default unless this flag is set.

When `CHIEF_API_TOKEN` is set, sensitive routes require auth via one of:

- `Authorization: Bearer <token>`
- `X-Chief-Token: <token>`

Sensitive routes include project control/write operations (start/stop/refresh, add todo, requirements, config updates, terminal websocket).

Example production-style backend start:

```bash
CHIEF_API_TOKEN='replace-with-long-random-token' \
cargo run --bin chief_backend -- \
  --projects-dir /path/to/projects \
  --project /path/to/another/repo \
  --host 0.0.0.0 \
  --port 8000 \
  --allow-origin https://chief.example.com
```

Example authenticated request:

```bash
curl -X POST http://localhost:8000/api/projects/myproj/start \
  -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -d '{"agents":1,"flow":"tdd"}'
```

Frontend is a Next.js app (`frontend/`) and calls backend APIs for:

- project status and runtime controls
- start/stop project schedulers
- job list and todo table
- event tape queries (`/events`)
- requirements ingestion
- websocket terminal command execution
- per-project state (`/state`)
- file diffs (`/file_diff`)

## Docker compose

Build and run:

```bash
docker compose up --build
```

Projects mount is generic in `docker-compose.yml`:

```bash
CHIEF_PROJECTS_DIR=/absolute/path/to/projects docker compose up --build
```

For deployment, create a `.env` file (or export env vars) and include at minimum:

```dotenv
CHIEF_PROJECTS_DIR=/absolute/path/to/projects
CHIEF_API_TOKEN=replace-with-long-random-token
```

Then run:

```bash
docker compose up --build
```

Compose automatically loads `.env` from the project root. Keep this file out of source control.

Terminal websocket in compose:

- Disabled by default (backend starts without `--enable-terminal`).
- To enable it, add a local `docker-compose.override.yml` command override:

```yaml
services:
  backend:
    command:
      - --projects-dir
      - /workspace/projects
      - --host
      - 0.0.0.0
      - --port
      - "8000"
      - --enable-terminal
```

For machine-specific setup (for example NFS), use `docker-compose.override.yml` locally. That file is gitignored.

Services:

- `backend` on `http://localhost:8000`
- `frontend` on `http://localhost:3000`

`frontend` rewrites `/api/*` to backend. Terminal websocket defaults to `ws://localhost:8000` (configurable with `NEXT_PUBLIC_CHIEF_WS_BASE`).

## Config (`chief.yaml`) quick example

```yaml
chief:
  flow: single_prompt # available: single_prompt, tdd, loop_file (CLI-only)
  agent: codex
  model: gpt-5
  max_retries: 10
  max_loop_iterations: 6 # shared by all flows
  required_stable_iterations: 2
  agent_timeout_seconds: 2700
  suite_command_timeout_seconds: 1800

suites:
  - name: backend
    language: Rust
    framework: cargo test
    test_root: .
    test_command: cargo test
    target_type: project
    lint_command: cargo clippy
    post_green_command: cargo test
```

See `chief.example.yaml` for more patterns.
Backend runtime settings are configured on the `chief_backend` command line (see `just backend`).

## Current status

- Rust library and both binaries compile (`cargo check`).
- Legacy Python implementation has been removed.
