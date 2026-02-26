# loop_file implementation plan

1. Define `loop_file` flow semantics inside the existing flow architecture:
   - use `max_loop_iterations` from `chief.yaml`
   - use `required_stable_iterations` from `chief.yaml`
   - ignore outer retries for this flow (effective `max_retries = 1` for this path)

2. Add `loop_file` to flow model plumbing:
   - extend flow parsing/resolution (`FlowKind`, parse errors, resolve helpers)
   - add a `LoopFileFlow` implementation under `src/flow/strategies/`
   - wire `build_flow(...)` to return this flow kind

3. Add CLI command `chief loop_file --file <path>` in `src/bin/chief.rs`:
   - read markdown file contents
   - build a synthetic in-memory todo context (not from `todos.yaml`)
   - run exactly one todo execution with `FlowKind::LoopFile`
   - no outer queueing over todos

4. Implement loop behavior in `LoopFileFlow`:
   - render `prompts/singleprompt_loadfile.md`
   - keep the same iterative failure context structure used by `single_prompt` (lint/test/other + sqlite queries)
   - convergence logic remains stable-iterations based, same as current flow framework

5. Reload `chief.yaml` after each agent iteration and before checks:
   - load from the active worktree path
   - use reloaded suites and reloaded timeout/config values for lint/test runs in that same iteration
   - this makes agent edits to `chief.yaml` effective immediately for subsequent checks

6. Keep this CLI-only for now:
   - frontend/UI unchanged
   - backend start endpoint can reject `loop_file` with a clear message (`CLI-only; use chief loop_file`)

7. Update `chief init` defaults:
   - replace generated `chief.yaml` from `chief: {}` to a full default `chief:` block with global options only
   - do not generate suite entries
   - include current defaults (`flow`, `agent`, retries/loop/timeouts/log settings, etc.)

8. Update docs/examples:
   - document `loop_file` in `README.md` with usage examples
   - update `chief.example.yaml` comments to mention the new flow and loop semantics
   - clarify that `max_loop_iterations` is shared by all flows

9. Add tests:
   - flow parsing/build tests for `loop_file`
   - CLI parse + command behavior tests for `loop_file --file`
   - flow tests proving config reload affects lint/test commands on next iteration
   - test that outer retry is effectively disabled for `loop_file`
   - `chief init` test asserting new default `chief.yaml` content

Optional follow-up decision:
- if desired, raise global default `max_loop_iterations` from `6` to `20`; otherwise `loop_file` inherits current global default until overridden in `chief.yaml`.
