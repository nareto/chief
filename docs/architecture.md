# Architecture

## The core idea: convergence

Every chief flow is one loop:

1. Start a fresh agent process with a prompt. Nothing carries over from the previous pass except the files on disk.
2. When it exits, compare the working tree with the snapshot taken before the pass, and run any configured checks.
3. A pass that changed nothing (and kept the checks green) is **stable**. After `required_stable_iterations` stable passes in a row, the work is done. After `max_loop_iterations` passes without that, the run fails.

The prompts tell the agent to leave the files alone when the task is already done, so "no changes" is the agent's way of saying "finished". Because each pass starts with fresh context, a later agent re-checks the earlier agents' work instead of trusting their memory of it.

Change detection snapshots a signature of each file that `git` reports as changed, before and after the pass. `change_exclude` globs drop paths from the snapshot, and `--watch-only` replaces it with a snapshot of only the given paths. Chief's own `.chief/chief.db` is always excluded.

## Flows

| Flow | Entry point | Where it runs | Prompt(s) | Stable when | Ends with |
| --- | --- | --- | --- | --- | --- |
| `loop_file` | `chief --prompt`/`--file`, `chief loop_file` | your checkout | `loop_file_prompt.md` | no file changes, suites green | `post_green_command`, commit |
| `refactor` | `chief refactor`, `chief_backend` | one worktree per todo | `structural_cleanup.md` / `mechanical_cleanup.md`, alternating | no file changes, todo's suites green | `post_green_command`, commit, merge into the base branch |
| `review` | `chief review` | your checkout | `review.md` | findings file unchanged and valid | nothing; no commit |

Requirements ingestion (`prompts/requirements.md`) is not a flow: it runs the agent once and turns the answer into todos for `refactor`.

Prompts are Markdown files with MiniJinja templating, embedded into the binaries at build time from [`prompts/`](../prompts).

## Code layout

- `src/bin/chief.rs`, `src/bin/chief/`: the single-project CLI, including `init`, suite commands, `schema` and `explain`.
- `src/bin/chief_backend.rs`, `src/bin/backend/`: the multi-project backend, HTTP/WebSocket API and readiness checks.
- `src/flow/`: flows and the convergence loop.
  - `loop_policy.rs`: `ConvergenceLoopPolicy`, the shared loop.
  - `strategies/`: one phase strategy per flow (`loop_file_phase.rs`, `refactor_phase.rs`, `review_phase.rs`). A strategy runs one agent pass and decides whether it was stable, needs another pass, or failed.
  - `execution/`: running the agent, change snapshots, usage-limit pacing, suite command execution and run history.
- `src/review.rs`: the findings file format, the default paths and base resolution for `chief review`.
- `src/agent/`: adapters for `codex`, `claude`, `opencode`, `cursor-agent` and `pi`: command lines, sandbox flags, MCP config translation and output parsing.
- `src/service/`: project context (config, store, git, prompts), the project registry and the engine that runs one flow or the queue.
- `src/scheduler/`: worker supervision, worktree lifecycle and merging for queued work.
- `src/storage/`, `src/storage.rs`: SQLite persistence for todos, runs, jobs, events and readiness results.
- `src/worktree_cache/`, `src/worktree_cache.rs`: suite dependency caches shared across worktrees.
- `src/git.rs`: the Git operations chief uses, behind a trait so tests can fake them.
- `src/config.rs`: `chief.yaml` types, defaults and overrides.
- `prompts/`: the embedded prompt templates.

## Paths on disk

For a project at `/path/to/project`:

- `/path/to/project/.chief/`: config, SQLite state and default review output.
- `/path/to/project__worktrees/chief_<job_id>`: queued worker worktrees.
- `/path/to/project__worktrees/chief_pre_run_checks_<token>`: readiness checks.
- `/path/to/project__worktrees/chief_suite_check_<token>`: backend suite checks.
- `/path/to/project__worktree_cache/<suite>/<cache_key>`: suite dependency caches.

## Testing

```bash
cargo test
cargo clippy --all-targets
```

Flow tests drive the real loop with scripted agents and fake Git implementations (`src/flow/tests.rs`, `src/flow/review_tests.rs`). The CLI tests in `src/bin/chief/tests.rs` cover argument parsing, schema, help and configless runs.
