# Chief

Chief is a Rust-based orchestration system for running coding-agent workflows against Git repositories.

This repo contains:

- `chief`: the single-project CLI
- `chief_backend`: the multi-project scheduler + HTTP/WebSocket API
- `frontend/`: the deprecated Next.js dashboard and project cockpit

Frontend note: `frontend/` is deprecated and should be treated as unmaintained unless work is explicitly requested there.

Chief stores state per target project in:

- `.chief/chief.yaml`: project config
- `.chief/chief.db`: SQLite state for todos, runs, jobs, events, and readiness
- `.beads/`: bd state, when initialized with `chief init`

Queued execution happens in sibling Git worktrees under:

- `../<project_name>__worktrees/chief_<job_id>`

Successful worker branches are merged back into the project's current branch.

## Runtime flows

Chief currently supports three flow kinds:

- `loop_file`: CLI-only. Runs a convergence loop from a Markdown task file.
- `bd`: converges against the current `bd ready --json` queue using `prompts/bd.md`.
- `refactor`: claims pending SQLite todos and runs the queued cleanup flow.

Prompt templates live in this repo's [`prompts/`](./prompts) directory:

- `loop_file_prompt.md`
- `structural_cleanup.md`
- `mechanical_cleanup.md`
- `requirements.md`
- `bd.md`

Requirements ingestion is separate from execution: it runs `prompts/requirements.md`, expects YAML shaped like `todos: [...]`, and replaces the SQLite todo queue for the target project.

## Safety notes

Normal queued runs use isolated Git worktrees instead of mutating your main checkout directly.

There are still destructive operations in the system:

- The backend `reset_workspace` action performs `git reset --hard HEAD` and `git clean -fd` in the target project.
- The backend `reset_db` action recreates `.chief/chief.db`.
- Worktree cleanup removes temporary worktrees with `git worktree remove --force` and deletes worker branches.

`reset_workspace` preserves `.chief/chief.db` and its SQLite sidecar files, but it will discard other local changes. Use it only when you intend to throw away the target project's uncommitted work.

## Requirements

For the CLI and backend:

- Rust toolchain with Cargo
- Git
- a supported coding-agent CLI on `PATH`
  - `codex` is the default
  - `claude`, `opencode`, and `cursor-agent` are also supported

For `chief init` and the `bd` flow:

- `bd` on `PATH`

For the deprecated frontend:

- Node.js 20+ and npm, or Docker

## Quick start for a target project

Build the binaries from this repo:

```bash
cargo build --bin chief --bin chief_backend
```

Initialize a target project:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  init \
  --chief-root /path/to/chief
```

Notes:

- `init` defaults `--chief-root` to `../chief`.
- It creates `.chief/chief.yaml`.
- It symlinks `.chief/chief.example.yaml` back to this repo's example file.
- It runs `bd init --agents-template <chief_root>/bd_AGENTS.md` if `.beads/` does not already exist.
- It appends these ignore entries if missing:
  - `.chief/chief.db`
  - `.chief/chief.example.yaml`
  - `.beads`

If an older project still uses root-level `chief.yaml`, `chief.example.yaml`, or `chief.db`, migrate it with:

```bash
cargo run --bin chief -- --project-dir /path/to/project migrate
```

## CLI usage

Without a subcommand, `chief` reads `.chief/chief.yaml` and resolves the flow from config. If that flow is `loop_file`, `--file` is required.

Run a file-driven loop:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --file docs/task.md
```

Equivalent explicit form:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  loop_file \
  --file docs/task.md
```

Run the `bd` convergence flow:

```bash
cargo run --bin chief -- --project-dir /path/to/project bd
```

Run the queued `refactor` flow:

```bash
cargo run --bin chief -- --project-dir /path/to/project refactor
```

Override the queued-flow retry budget or model at invocation time:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --flow refactor \
  --max-retries 4 \
  --model gpt-5
```

Process requirements into the SQLite todo queue:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --requirements "Add JWT auth and rotate refresh tokens"
```

Or load requirements from files:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --requirements-file docs/requirements.md \
  --requirements-file docs/followups.md
```

Useful maintenance commands:

```bash
# run cached-or-fresh readiness checks used by backend start
cargo run --bin chief -- --project-dir /path/to/project check
cargo run --bin chief -- --project-dir /path/to/project check --force

# print recent events from .chief/chief.db
cargo run --bin chief -- --project-dir /path/to/project tail-events -n 50

# remove completed todos that already have a commit hash
cargo run --bin chief -- --project-dir /path/to/project clean-done
```

Run suite commands from `.chief/chief.yaml`:

```bash
cargo run --bin chief -- --project-dir /path/to/project suite test --suite backend
cargo run --bin chief -- --project-dir /path/to/project suite test --suite backend --target src/lib.rs
cargo run --bin chief -- --project-dir /path/to/project suite lint --suite backend --target src
cargo run --bin chief -- --project-dir /path/to/project suite test_init --suite backend
cargo run --bin chief -- --project-dir /path/to/project suite test_setup --suite backend
cargo run --bin chief -- --project-dir /path/to/project suite lint_fix --suite backend --target src
```

## `.chief/chief.yaml`

The shipped example file is `.chief/chief.example.yaml`. A minimal current config looks like this:

```yaml
chief:
  flow: loop_file
  # flow: refactor
  agent: codex
  # model: gpt-5
  # agent: cursor-agent
  # model: gpt-5.4-xhigh
  # model_reasoning_effort: high
  agent_extra_args: []
  mcp_servers: {} # default from `chief init`; remove this key to use personal agent MCP config
  max_retries: 2
  max_loop_iterations: 20
  required_stable_iterations: 2
  agent_timeout_seconds: 2700
  suite_command_timeout_seconds: 1800
  agent_log_max_output_lines: 10
  agent_log_max_output_chars: 1500
  respect_limits: true
  use_agent_log_truncation_for_stdout_logs: false

suites:
  - name: backend
    language: Rust
    framework: cargo test
    test_root: .
    test_command: cargo test
    target_type: project
    default_target: .
    file_patterns: []
    lint_command: cargo clippy
    post_green_command: cargo test
    env: {}
    strip_root_from_target: true
```

Current config details worth knowing:

- `chief.agent` supports `codex`, `claude`, `opencode`, and `cursor-agent` (`cursor` is accepted as a compatibility alias).
- `chief.agent_extra_args` is passed directly to the agent CLI invocation.
- For `cursor-agent`, pass Cursor's exact model id in `chief.model` such as `gpt-5.4-xhigh`.
- `chief init` writes `mcp_servers: {}` by default, so new projects do not inherit personal Claude/Codex/Cursor MCP servers unless you add them to `chief.yaml`.
- `chief.mcp_servers` is agent-independent. Omit the key entirely to preserve each CLI's normal MCP loading. Set it to `{}` to force no MCP servers. Set it to a map to let chief translate the same MCP config for Claude, Codex, or Cursor.
- `chief.mcp_servers` supports `stdio` and `streamable_http` transports. HTTP servers support JWT bearer auth with either `token` or `token_env_var`.
- When `chief.mcp_servers` is set, chief runs Claude with a generated strict MCP JSON config, Codex with an isolated `CODEX_HOME` containing a chief-managed `config.toml`, and Cursor with an isolated `HOME` containing a chief-managed `~/.cursor/mcp.json`.
- `chief.model_reasoning_effort` currently affects the `codex` adapter.
- `chief.respect_limits` checks usage with `agentusage` before each call and waits when needed to stay under the slowest active limit.
- `max_retries` is the queued-work retry budget used by the worktree scheduler.
- `max_loop_iterations` and `required_stable_iterations` control convergence behavior.
- `suite_command_timeout_seconds` is the default timeout for suite/readiness commands.
- Each suite can override timeout with `command_timeout_seconds`.
- Suites can also define `test_init`, `test_setup`, `lint_fix_command`, `cleanup_command`, `cache_paths`, `cache_key_files`, `cache_mode`, `default_target`, `file_patterns`, and `env`.

The example file also includes ready-to-adapt Rust, Python, TypeScript, and Playwright suite patterns.

## Backend

`chief_backend` manages multiple projects at once.

Project discovery works like this:

- every direct child directory under `--projects-dir` that contains `.git` is considered a project
- additional project paths can be added with repeated `--project` flags

Start the backend:

```bash
cargo run --bin chief_backend -- \
  --projects-dir /path/to/projects \
  --project /path/to/extra/repo \
  --host 0.0.0.0 \
  --port 8000 \
  --default-agents-per-project 1 \
  --max-agents-per-project 8 \
  --enable-terminal \
  --allow-origin http://localhost:3000
```

Important runtime behavior:

- backend start only supports `bd` and `refactor`
- `loop_file` is intentionally CLI-only
- project start runs readiness checks unless the caller sets `start_anyway`
- terminal WebSocket routes are only mounted when `--enable-terminal` is set
- if `CHIEF_API_TOKEN` or `--api-token` is set, write/control routes require auth

Accepted auth headers:

- `Authorization: Bearer <token>`
- `X-Chief-Token: <token>`

Current API surface includes:

- dashboard project listing and refresh
- start, pause, and stop controls
- todo CRUD and delete-done
- jobs, logs, state, events, and event streaming
- requirements ingestion
- file diff lookup
- `.chief/chief.yaml` read/write
- suite checks and suite-check streaming
- DB reset and DB trim
- workspace reset
- terminal WebSocket access when enabled

## Frontend

The UI in [`frontend/`](./frontend) is a Next.js 14 app backed by the backend API.

It currently provides:

- a dashboard of discovered projects
- per-project cockpit views
- live event streaming
- interactive terminal access
- todo management
- requirements submission
- readiness status and streaming output
- suite-check execution
- diff inspection
- project settings editing for `.chief/chief.yaml`

Runtime configuration:

- `CHIEF_BACKEND_URL` controls the frontend's `/api/*` rewrite target
- `NEXT_PUBLIC_CHIEF_WS_BASE` controls WebSocket base URLs

## Local development

The repo's `justfile` is the quickest way to run the current dev setup.

Frontend only:

```bash
just frontend
```

This:

- syncs `frontend/node_modules` with `docker compose run --rm --no-deps frontend npm ci` when needed
- starts the frontend container on `http://localhost:3000`

Backend only:

```bash
export PROJECTS_DIR=/absolute/path/to/projects
export PROJECT=/absolute/path/to/one/project
export FRONTEND_HOST=127.0.0.1

just backend
```

Combined local dev:

```bash
just dev-full
```

Other useful recipes:

```bash
just dev      # alias for frontend only
just down     # stop compose services
just logs     # tail frontend logs
```

Current Compose behavior is intentionally limited:

- `docker-compose.yml` defines only the `frontend` service
- that container expects a backend already running on the host at `http://host.docker.internal:8000`
- it exposes the frontend on `http://localhost:3000`

You can also run the frontend without Docker:

```bash
cd frontend
npm install
CHIEF_BACKEND_URL=http://localhost:8000 \
NEXT_PUBLIC_CHIEF_WS_BASE=ws://localhost:8000 \
npm run dev
```

## Testing and utility scripts

Rust:

```bash
cargo check
cargo test
```

Frontend:

```bash
cd frontend
npm test
```

There is also a small helper for recording per-ticket Rust test evidence:

```bash
ops/per_ticket_cargo_test.sh <ticket-id> [ticket-id...]
```

It writes logs and a TSV summary under `.chief/evidence/`.

## Repository layout

- `src/bin/chief.rs`: single-project CLI
- `src/bin/chief_backend.rs`: backend entry point
- `src/bin/backend/`: backend API, routing, and readiness logic
- `src/service/`: project context, registry, and engine
- `src/scheduler/`: multi-worker scheduling and worktree lifecycle
- `src/flow/`: flow definitions, loop policy, prompt phases, and suite execution
- `src/storage/` and `src/storage.rs`: SQLite persistence
- `src/agent/`: `codex`, `claude`, `opencode`, and `cursor-agent` process adapters
- `prompts/`: Markdown/Jinja prompt templates
- `frontend/`: Next.js dashboard
- `ops/`: small operational scripts and config fragments
