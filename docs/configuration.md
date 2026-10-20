# Configuration

## Where state lives

Chief keeps per-project state under `.chief/` in the target repository:

- `.chief/chief.yaml`: project config
- `.chief/chief.db`: SQLite state for todos, runs, jobs, events and readiness results
- `.chief/review/`: default `chief review` output, ignored by its own `.gitignore`

Set up a project with:

```bash
chief --project-dir /path/to/project init
```

`init` creates `.chief/chief.yaml`, writes `.chief/chief.example.yaml` from the embedded example, and adds these entries to `.gitignore` if they are missing:

- `.chief/chief.db`
- `.chief/chief.example.yaml`
- `.chief/codex-home`

If an older project still has `chief.yaml`, `chief.example.yaml` or `chief.db` at the repository root, move them with:

```bash
chief --project-dir /path/to/project migrate
```

## When `chief.yaml` is required

Settings resolve in this order, later wins:

1. built-in defaults
2. `.chief/chief.yaml`
3. CLI flags

These commands run without `.chief/chief.yaml` and use the built-in defaults plus your flags:

- `chief review`
- one-shot `loop_file` runs with `--file` or `--prompt`
- requirements ingestion

The persistent queue (`refactor`), suite commands, readiness checks and backend-managed projects need `.chief/chief.yaml`.

One-shot runs keep their run log in memory. Pass `--sqlite-log` to write it to `.chief/chief.db`.

## Example `chief.yaml`

The full commented example ships as [`.chief/chief.example.yaml`](../.chief/chief.example.yaml), with Rust, Python, TypeScript and Playwright suite patterns. A minimal config:

```yaml
chief:
  flow: loop_file # use `--file <path>` or `--prompt <text>`
  # flow: refactor # queued workflow
  agent: codex
  # model: gpt-5
  # model_reasoning_effort: high
  agent_extra_args: []
  agent_sandbox: bypass # or native; see docs/safety.md
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
  limit_reserve_percent: 10
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

Suites are described in [suites-and-readiness.md](./suites-and-readiness.md).

## `chief` settings

Every setting has a CLI flag with the same name in kebab case, for example `max_loop_iterations` is `--max-loop-iterations`. Run `chief --help` or `chief schema --json` for the full list.

### Agent

- `agent`: `codex` (default), `claude`, `opencode`, `cursor-agent` or `pi`. `cursor` is accepted as an alias for `cursor-agent`.
- `model`: passed to the agent. For `cursor-agent` use Cursor's exact model id, such as `gpt-5.4-xhigh`. For `pi` use `provider/model_id`, such as `openai/gpt-5`. The backend also accepts a model per project start.
- `model_reasoning_effort`: `low`, `medium`, `high`, `xhigh` or `max`. Currently only the `codex` adapter uses it.
- `agent_extra_args`: extra arguments appended to the agent command line.
- `agent_sandbox`: `bypass` (default) or `native`. See [safety.md](./safety.md).
- `agent_timeout_seconds`: per-call timeout; `0` disables it.

### MCP servers

- `chief init` writes `mcp_servers: {}`, so new projects run agents with no MCP servers until you edit or remove the key.
- Omit the key entirely to leave each agent's own MCP config untouched. `{}` forces no servers. A map lets chief translate one MCP config for Claude, Codex and Cursor.
- Supported transports: `stdio` and `streamable_http`. HTTP servers support JWT bearer auth with `token` or `token_env_var`.
- When `mcp_servers` is set, chief runs Claude with a generated strict MCP JSON config, Codex with an isolated `CODEX_HOME` containing a chief-managed `config.toml`, and Cursor with an isolated `HOME` containing a chief-managed `~/.cursor/mcp.json`.

### Convergence

- `max_loop_iterations`: most agent passes per work item.
- `required_stable_iterations`: consecutive passes without changes needed to finish. `chief review` ignores this key and defaults to 1; use `--required-stable-iterations` there.
- `change_exclude` / `--change-exclude <glob>` (alias `--watch-exclude`): glob patterns ignored by change detection. Chief always ignores its own `.chief/chief.db` and SQLite sidecar files.
- `--watch-only <path>`: `loop_file` only; restricts change detection to the given paths.
- `max_retries`: retry budget for queued todos. `loop_file` uses a single outer attempt and relies on convergence instead.

### Pacing and usage limits

- `respect_limits`: before each call, check the agent's usage limits with `agentusage` and wait when needed to stay under the slowest active limit. Supported for `codex` and `claude`; pacing is skipped when the lookup fails. Turn it off in CI or with API keys, where there are no subscription limits to read.
- `limit_reserve_percent` (default `10`): keep this percent of each active limit in reserve. Chief exits instead of calling the agent if a limit is already below the reserve.
- `agent_wait_seconds`: when set, a fixed wait between agent calls replaces `respect_limits` pacing. `0` means no wait.

### Output

- `agent_log_max_output_lines` and `agent_log_max_output_chars`: how much of the agent output tail is kept in logs.
- `use_agent_log_truncation_for_stdout_logs`: apply the same truncation to terminal output.
- `--verbosity silent|quiet|normal|verbose`: terminal output level (CLI only).
- `suite_command_timeout_seconds`: default timeout for suite and readiness commands.

## Introspection

```bash
chief schema --json                          # every command, option, default and note
chief config show                            # raw .chief/chief.yaml
chief config show --resolved --json          # defaults + file + CLI flags
chief list suites --json
chief explain flow --flow refactor
chief explain review
chief doctor                                 # checks the project and the agent tools
```

## Maintenance

```bash
chief check            # cached-or-fresh readiness checks (the ones backend start uses)
chief check --force
chief tail-events -n 50
chief clean-done       # remove completed todos that already have a commit hash
```
