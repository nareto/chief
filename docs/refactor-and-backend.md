# The todo queue, `refactor` and `chief_backend`

## Requirements ingestion

Chief can turn free-form requirements into a todo queue stored in `.chief/chief.db`:

```bash
chief --project-dir /path/to/project --requirements "Add JWT auth and rotate refresh tokens"

chief --project-dir /path/to/project \
  --requirements-file /path/to/requirements.md \
  --requirements-file /path/to/followups.md
```

Ingestion runs `prompts/requirements.md` and expects raw YAML, or a fenced YAML block, shaped like:

```yaml
todos:
  - todo: Short actionable task title
    expectations: Detailed acceptance criteria and implementation notes
    priority: 1          # lower runs earlier
    test_suites: []      # suite names from chief.yaml
```

It replaces the project's todo queue, prints the resulting diff and exits without running a flow.

## The `refactor` flow

```bash
chief --project-dir /path/to/project refactor

# override the retry budget or the model for this run
chief --project-dir /path/to/project --flow refactor --max-retries 4 --model gpt-5
```

`refactor` claims pending todos and runs each one in its own Git worktree:

1. Chief creates `../<project>__worktrees/chief_<job_id>` on a new worker branch and prepares the todo's suites there (`test_init`, `test_setup`, suite caches).
2. Each pass starts a fresh agent. Passes alternate between `prompts/structural_cleanup.md` and `prompts/mechanical_cleanup.md`, both of which forbid behavior changes.
3. After each pass chief runs lint and tests for the todo's `test_suites`. A pass that changes nothing and keeps the suites green is stable; after `required_stable_iterations` stable passes the loop ends.
4. Chief runs `post_green_command` for those suites, commits as `chief(refactor): <todo title>`, merges the worker branch into the project's current branch with `git merge --no-ff`, and removes the worktree.

A todo that fails is retried up to `max_retries` times. A merge error puts the todo back in the queue.

## `chief_backend`

`chief_backend` runs the `refactor` queue for many projects at once and exposes an HTTP/WebSocket API.

```bash
chief_backend \
  --projects-dir /path/to/projects \
  --project /path/to/extra/repo \
  --host 127.0.0.1 \
  --port 8000 \
  --allow-origin http://localhost:5173 \
  --default-agents-per-project 1 \
  --max-agents-per-project 8 \
  --verbose
```

Project discovery:

- every direct child of `--projects-dir` that contains `.git` is a project;
- repeat `--project` to add more.

Runtime behavior:

- The backend only runs `refactor`. `loop_file` and `review` are CLI only.
- Starting a project runs readiness checks first unless the caller sets `start_anyway`; see [suites-and-readiness.md](./suites-and-readiness.md).
- Requested agent counts are clamped to `1..=--max-agents-per-project`.
- `pause` stops claiming new todos and lets active work finish. `stop` cancels active work.
- `--allow-origin` can be repeated; `--allow-origin "*"` allows any origin.
- Terminal WebSocket routes exist only with `--enable-terminal`. They give shell access to whoever can reach the API, so only enable them behind authentication on a trusted network.
- If `CHIEF_API_TOKEN` or `--api-token` is set, write and control routes require `Authorization: Bearer <token>` or `X-Chief-Token: <token>`.

API surface:

- backend settings, project listing and refresh;
- start, pause and stop, and stopping readiness checks;
- todo CRUD and delete-done;
- jobs, logs, state, events and event streaming;
- requirements ingestion;
- file diff lookup;
- `.chief/chief.yaml` read and write;
- suite checks and suite-check streaming;
- DB reset and trim, workspace reset (see [safety.md](./safety.md));
- the terminal WebSocket when enabled.

## Local development

With the repository's `justfile`:

```bash
export PROJECTS_DIR=/absolute/path/to/projects
export PROJECT=/absolute/path/to/one/project
just backend      # also: just dev, just up

just build        # both binaries
just build-chief
```
