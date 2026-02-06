# Chief

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

## Quick Start

### 1. Copy `chief.py` to your project

Chief is a single self-contained file with no dependencies beyond Python 3.11+ stdlib. Just copy it into your codebase:

```bash
cp chief.py /path/to/your/project/
```

### 2. Create your config file (`chief.toml`)

See `chief.toml.example` for more examples. Here's a minimal config:

```toml
[chief]
agent = "claude" # or "codex"
model = "gpt-5" # optional, codex only
agent_extra_args = []
max_retries = 10
agent_timeout_seconds = 1800

[[suites]]
name = "backend"
language = "Python"
framework = "pytest"
test_root = "."
test_command = "pytest {target} -v"
target_type = "file"
file_patterns = ["test_*.py", "*_test.py"]
disallow_write_globs = ["tests/**", "test_*.py"]
```

### 3. Create your task list (`todos.json`)

See `todos.json.example` for reference. Here's a sample:

```json
{
  "todos": [
    {
      "todo": "Add user authentication with JWT tokens",
      "priority": 10,
      "expectations": "Users can login and receive a JWT token for subsequent API calls"
    },
    {
      "todo": "Implement rate limiting for API endpoints",
      "priority": 5
    }
  ]
}
```

### 4. Run Chief

```bash
python chief.py
# or override codex model from CLI for this run
python chief.py --model gpt-5
```

### 5. Generate todos from requirements (agent-driven)

Chief can ask the configured agent to translate requirements directly into `todos.json`.
This works with both formal PRDs and vague requests.

```bash
# Inline requirement text (repeatable)
python chief.py --requirements "change that button to green"

# Requirements from file(s) (repeatable)
python chief.py --requirements-file prd.md
```

Behavior:
- Chief sends a focused prompt to the configured agent (based on the `/req` and `/prd` command style).
- The agent is asked to update `todos.json` following `todos.json.example`, and may scaffold if your prompt requires it.
- Chief prints a single combined `git diff HEAD` and exits.

## Optional: Claude Code Commands

Chief includes optional Claude Code slash commands that help manage your `todos.json`. To use them, symlink the `.claude` directory and example files into your project:

```bash
ln -s /path/to/chief/.claude /path/to/your/project/.claude
ln -s /path/to/chief/todos.json.example /path/to/your/project/todos.json.example
ln -s /path/to/chief/chief.toml.example /path/to/your/project/chief.toml.example
```

This gives you access to:

| Command | Description |
|---------|-------------|
| `/req <requirements>` | Break down requirements into todos and add them to `todos.json` |
| `/reprio` | Reprioritize todos based on recent project activity |
| `/prd <prd text>` | Generate project scaffolding and create todos from a PRD |

The commands reference the example files to understand the schema, so all three symlinks are needed.

## Features

- **Multi-Suite Support** - Handle monorepos with multiple languages/frameworks
- **Automatic Environment Setup** - Optional `test_init` commands for venvs, npm install, etc.
- **Pre-Test Setup** - Optional `test_setup` commands (Docker, test data seeding)
- **Linting/Validation** - Optional `lint_command` executed in RED and POST_GREEN
- **Post-Green Validation** - Optional `post_green_command` (builds, type checks)
- **Test File Protection** - Prevents agent from modifying tests during implementation
- **Smart Recovery** - Automatic retry loops with rollback on failure
- **Git Integration** - Auto-commit and tag on successful completion
- **Priority Queue** - Process todos by priority (highest first)
- **Requirements Intake Mode** - Convert PRDs/requirements into `todos.json` via a single agent run and show the resulting `git diff`

## Configuration Reference

### Global Options (`[chief]`)

| Field | Required | Description |
|-------|----------|-------------|
| `agent` | No | Coding agent to use (`claude` or `codex`) |
| `model` | No | Codex model name (only used when `agent = "codex"`) |
| `agent_extra_args` | No | Extra CLI args appended to the agent invocation |
| `max_retries` | No | Max number of run retries (default 10) |
| `agent_timeout_seconds` | No | Max seconds to wait for the coding agent (default 1800) |

### Suite Options (`[[suites]]`)

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique identifier for the suite |
| `language` | Yes | Programming language (Python, TypeScript, Go, etc.) |
| `framework` | Yes | Test framework (pytest, Jest, Vitest, go test, etc.) |
| `test_root` | Yes | Working directory for test commands |
| `test_command` | Yes | Test command. Use `{target}` for the test file/path |
| `target_type` | Yes | One of: `file`, `package`, `project`, `repo` |
| `default_target` | No | Default target when none detected |
| `file_patterns` | No | Glob patterns for test files |
| `disallow_write_globs` | No | Patterns for files agent cannot modify |
| `test_init` | No | Command to initialize dev environment |
| `test_setup` | No | Command to run before tests (once per suite) |
| `lint_command` | No | Lint/typecheck command (RED + POST_GREEN) |
| `post_green_command` | No | Command to run after tests pass |
| `env` | No | Environment variables for commands |
| `strip_root_from_target` | No | Whether to strip `test_root` prefix from `{target}` (default true) |

### Todo Options

| Field | Required | Description |
|-------|----------|-------------|
| `todo` | Yes | Description of the task |
| `priority` | No | Higher = processed first (default: 0) |
| `expectations` | No | Expected outcome (1-2 sentences) |
| `test_suites` | No | List of suites to use for this todo |
| `status` | No | `pending`, `in_progress`, `attempted`, `done` |
| `done_at_commit` | Auto | Set by Chief when completed |

## Examples

See `chief.toml.example` for configuration examples including:
- Python + pytest (with pyenv)
- TypeScript + Jest/Vitest
- Go + go test
- Rust + cargo test
- Multi-suite monorepo setups

## License

MIT
