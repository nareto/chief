# `loop_file`: one task until it converges

`loop_file` gives an agent one task, from a Markdown file or an inline prompt, and keeps calling it until a pass leaves the files unchanged. Then chief commits the result.

It runs directly in your checkout, not in a worktree, so commit or stash anything you care about first.

```bash
# inline prompt
chief --project-dir /path/to/project --prompt "Tighten parser error messages"

# Markdown task file
chief --project-dir /path/to/project loop_file --file /path/to/task.md

# with the flow spelled out
chief --project-dir /path/to/project --flow loop_file --prompt "Tighten parser error messages"
```

Pass exactly one of `--file` or `--prompt`. One-shot runs work without `.chief/chief.yaml`; see [`examples/chief-one-shot.sh`](../examples/chief-one-shot.sh) for a drop-in script.

## How it converges

1. Every pass starts a fresh agent with the task text (`prompts/loop_file_prompt.md`). The prompt asks the agent to compare the task with the current files and to change nothing if the task is already fully done.
2. If `.chief/chief.yaml` defines suites, chief runs their lint and test commands after each pass. A failing suite means the pass does not count as stable, and the loop goes on.
3. A pass that changes no files and passes the suites counts as stable. After `required_stable_iterations` stable passes in a row (default 2), the loop ends.
4. Chief runs each involved suite's `post_green_command`, then commits everything with the message `chief(loop_file): <task title>`.

If the loop reaches `max_loop_iterations` (default 20) without converging, the run fails and nothing is committed.

## Narrowing change detection

Some tasks touch files that change on every run, such as generated output or lock files. Two options keep them from blocking convergence:

```bash
# only these paths count as changes
chief loop_file --prompt "Fix the generated OpenAPI client" \
  --watch-only src/api \
  --watch-only openapi.json

# everything except these globs counts
chief --change-exclude 'dist/**' --prompt "..."
```

`change_exclude` can also be set in `chief.yaml`. `--watch-only` is CLI only.

`chief review` is built on the same loop: its findings file is the only watched path, and it skips the commit. See [review.md](./review.md).
