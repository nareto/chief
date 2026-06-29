# Chief

Chief is a Rust-based orchestration system for running unattended coding-agent workflows against Git repositories.

This repo contains:

- `chief`: the single-project CLI
- `chief_backend`: the multi-project scheduler + HTTP/WebSocket API

Chief stores state per target project in:

- `.chief/chief.yaml`: project config
- `.chief/chief.db`: SQLite state for todos, runs, jobs, events, and readiness

Queued workers, backend suite checks, and pre-run readiness checks use sibling Git worktrees under:

- `../<project_name>__worktrees/chief_<job_id>` for queued workers
- `../<project_name>__worktrees/chief_pre_run_checks_<token>` for readiness checks
- `../<project_name>__worktrees/chief_suite_check_<token>` for backend suite checks

Configured suite dependency caches are stored under:

- `../<project_name>__worktree_cache/<suite>/<cache_key>`

Successful queued worker branches are merged back into the target project's current branch.

## Runtime flows

Chief currently supports two flow kinds:

- `loop_file`: CLI-only. Builds a synthetic todo from `--file` or `--prompt`, runs convergence directly in the target checkout, and commits the result.
- `refactor`: queue-driven. Claims pending SQLite todos, runs each todo in an isolated worker worktree, and merges successful worker branches back.

Prompt templates live in this repo's [`prompts/`](./prompts) directory and are embedded into the binaries at compile time:

- `loop_file_prompt.md`
- `structural_cleanup.md`
- `mechanical_cleanup.md`
- `requirements.md`

Requirements ingestion is separate from execution: it runs `prompts/requirements.md`, expects raw YAML or a fenced YAML block shaped like `todos: [...]`, replaces the SQLite todo queue for the target project, prints the resulting diff, and exits without running a flow.

## Safety notes

Normal queued runs use isolated Git worktrees instead of mutating your main checkout directly. CLI `loop_file` runs in the target checkout and is meant for single-project direct work.

Agent adapters are configured for unattended automation. For example, the Codex, Claude, and Cursor adapters use their non-interactive or permission-bypass flags where required by those tools.

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

## Quick start for a target project

Build the binaries from this repo:

```bash
cargo build --bin chief --bin chief_backend
```

Initialize a target project:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  init
```

Notes:

- It creates `.chief/chief.yaml`.
- It writes `.chief/chief.example.yaml` from the embedded example config.
- It appends these ignore entries if missing:
  - `.chief/chief.db`
  - `.chief/chief.example.yaml`
  - `.chief/codex-home`

If an older project still uses root-level `chief.yaml`, `chief.example.yaml`, or `chief.db`, migrate it with:

```bash
cargo run --bin chief -- --project-dir /path/to/project migrate
```

## CLI usage

Without a subcommand, `chief` reads `.chief/chief.yaml`, applies CLI overrides, and resolves the flow from config. Precedence is:

1. defaults
2. `.chief/chief.yaml`
3. CLI flags

One-shot `loop_file` runs that provide `--file` or `--prompt` can run without `.chief/chief.yaml`; missing config uses the built-in defaults and an empty suite list. Persistent queued flows, suite commands, readiness checks, and backend-managed projects still expect `.chief/chief.yaml`.

If the resolved flow is `loop_file`, provide exactly one of `--file` or `--prompt`.

Run a file-driven loop:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --file /path/to/task.md
```

Run an inline prompt loop:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  --flow loop_file \
  --prompt "Tighten parser error messages"
```

For a drop-in configless script form, see [`examples/chief-one-shot.sh`](./examples/chief-one-shot.sh).

Scope loop convergence to specific paths:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  loop_file \
  --prompt "Fix the generated OpenAPI client" \
  --watch-only src/api \
  --watch-only openapi.json
```

Equivalent explicit file form:

```bash
cargo run --bin chief -- \
  --project-dir /path/to/project \
  loop_file \
  --file /path/to/task.md
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
  --requirements-file /path/to/requirements.md \
  --requirements-file /path/to/followups.md
```

Introspect the CLI and project config:

```bash
cargo run --bin chief -- schema --json
cargo run --bin chief -- --project-dir /path/to/project config show
cargo run --bin chief -- --project-dir /path/to/project config show --resolved --json
cargo run --bin chief -- --project-dir /path/to/project list suites --json
cargo run --bin chief -- --project-dir /path/to/project explain flow --flow refactor
cargo run --bin chief -- --project-dir /path/to/project doctor
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

Suite commands run in the suite's `test_root`. For commands that include `{target}`, `--target` overrides the suite's `default_target`.

## `.chief/chief.yaml`

The shipped example file is `.chief/chief.example.yaml`. A minimal current config looks like this:

```yaml
chief:
  flow: loop_file # use `--file <path>` or `--prompt <text>`
  # flow: refactor # queued workflow
  agent: codex
  # model: gpt-5
  # agent: cursor-agent
  # model: gpt-5.4-xhigh
  # model_reasoning_effort: high
  agent_extra_args: []
  mcp_servers: {} # default from `chief init`; remove this key to leave the agent MCP config untouched
  max_retries: 2
  max_loop_iterations: 20
  required_stable_iterations: 2
  change_exclude: [] # additional convergence change-detection exclude globs
  agent_timeout_seconds: 2700
  # agent_wait_seconds: 60 # fixed wait between agent calls; overrides respect_limits when set
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
    # lint_fix_command: cargo fmt
    post_green_command: cargo test
    # cleanup_command: pkill -f "cargo test.*$PWD" || true
    # cache_paths: [target]
    # cache_key_files: [Cargo.lock]
    # cache_mode: copy # copy (default) or symlink
    # command_timeout_seconds: 1800
    env: {}
    strip_root_from_target: true
```

Current config details worth knowing:

- `.chief/chief.yaml` provides project defaults. It is optional for one-shot CLI `loop_file` or requirements-only invocations when all needed options are supplied with CLI flags.
- When `.chief/chief.yaml` is missing, Chief resolves `chief` from built-in defaults and `suites` as an empty list.
- Persistent project workflows, backend/readiness workflows, and suite commands require `.chief/chief.yaml`.
- `chief.agent` supports `codex`, `claude`, `opencode`, and `cursor-agent` (`cursor` is accepted as a compatibility alias).
- `chief.agent_extra_args` is passed directly to the agent CLI invocation.
- `chief.model` can be supplied in config or as `--model`; backend project starts also accept a per-start model override.
- For `cursor-agent`, pass Cursor's exact model id in `chief.model` such as `gpt-5.4-xhigh`.
- `chief init` writes `mcp_servers: {}` by default, so new projects deliberately run agents with no MCP servers unless you edit or remove this key.
- `chief.mcp_servers` is a shared config format. Omit the key entirely to leave the agent MCP config untouched. Set it to `{}` to force no MCP servers. Set it to a map to let chief translate the same MCP config for Claude, Codex, or Cursor.
- `chief.mcp_servers` supports `stdio` and `streamable_http` transports. HTTP servers support JWT bearer auth with either `token` or `token_env_var`.
- When `chief.mcp_servers` is set, chief runs Claude with a generated strict MCP JSON config, Codex with an isolated `CODEX_HOME` containing a chief-managed `config.toml`, and Cursor with an isolated `HOME` containing a chief-managed `~/.cursor/mcp.json`.
- `chief.model_reasoning_effort` currently affects the `codex` adapter.
- `chief.respect_limits` checks usage with `agentusage` before each call and waits when needed to stay under the slowest active limit.
- `chief.agent_wait_seconds`, when set, applies a fixed wait between agent call starts and overrides the `respect_limits`/`agentusage` pacing logic. Use `0` to bypass waiting while still disabling `respect_limits` pacing.
- `agent_timeout_seconds: 0` disables the per-agent timeout. Suite command timeouts always clamp to at least one second.
- `max_retries` is the queued-work retry budget used by the worktree scheduler. `loop_file` uses one outer retry loop and relies on convergence iterations instead.
- `max_loop_iterations` and `required_stable_iterations` control convergence behavior.
- Chief always excludes its own SQLite runtime state (`.chief/chief.db` and sidecars) from convergence change detection.
- `change_exclude` adds project-specific glob filters for convergence change detection; the CLI equivalent is repeatable `--change-exclude <glob>` (`--watch-exclude` is an alias).
- `--watch-only <path>` scopes `loop_file` stability checks to specific paths.
- `suite_command_timeout_seconds` is the default timeout for suite/readiness commands.
- Each suite can override timeout with `command_timeout_seconds`.
- `test_init` and `test_setup` prepare a suite before checks in a worker. `lint_fix_command` runs after a lint failure and is followed by a lint re-check.
- `cleanup_command` runs after test command attempts. It is also used by readiness checks and backend suite-check endpoints for `test` runs.
- `cache_paths` are snapshotted from a successful readiness worktree and copied or symlinked into future worker worktrees. `cache_key_files`, the suite config, and the `.chief/chief.yaml` hash determine the cache key.
- Suites can also define `default_target`, `file_patterns`, `target_type`, `env`, and `strip_root_from_target`.

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
  --allow-origin http://localhost:5173 \
  --default-agents-per-project 1 \
  --max-agents-per-project 8 \
  --enable-terminal \
  --verbose
```

Important runtime behavior:

- backend start only supports `refactor`; `loop_file` is intentionally CLI-only
- project start runs pre-run readiness checks unless the caller sets `start_anyway`
- readiness checks run in a temporary worktree, execute configured `test_init`, `test_setup`, `lint`, and `test` commands, and cache a passing result by `.chief/chief.yaml` plus suite-cache inputs
- successful readiness checks prime configured suite caches for later worker worktrees
- backend suite-check requests run in temporary worktrees and support `test` and `lint`; `post_green` is part of normal flows, not the suite-check endpoint
- requested agent counts are clamped to `1..=--max-agents-per-project`
- `pause` drains active work without claiming more todos; `stop` requests cancellation of active work
- terminal WebSocket routes are only mounted when `--enable-terminal` is set
- `--allow-origin` can be repeated; `--allow-origin "*"` allows any origin
- if `CHIEF_API_TOKEN` or `--api-token` is set, write/control routes require auth

Accepted auth headers:

- `Authorization: Bearer <token>`
- `X-Chief-Token: <token>`

Current API surface includes:

- backend settings
- project listing and refresh
- start, pause, and stop controls
- readiness stop
- todo CRUD and delete-done
- jobs, logs, state, events, and event streaming
- requirements ingestion
- file diff lookup
- `.chief/chief.yaml` read/write
- suite checks and suite-check streaming
- DB reset and DB trim
- workspace reset
- terminal WebSocket access when enabled

## Local development

The repo's `justfile` is the quickest way to run the backend during development:

```bash
export PROJECTS_DIR=/absolute/path/to/projects
export PROJECT=/absolute/path/to/one/project

just backend
```

Useful recipes:

```bash
just build    # build both binaries
just build-chief
just dev      # alias for backend
just up       # alias for dev
```

## Testing and utility scripts

```bash
cargo check
cargo test
```

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

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](./LICENSE).
