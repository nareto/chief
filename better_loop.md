# Better Loop Plan (Fail-Fast, Per-Todo Retries)

## High-Level Goals

- Keep retries strictly **per todo** (`max_retries` applies to one todo only).
- Keep inner phase loops bounded (default `6`), but make that bound configurable in `chief.yaml`.
- Remove `attempted` todo status entirely.
- Keep todo lifecycle minimal: `pending` -> `in_progress` -> `done`.
- Fail-safe default: if a todo exhausts retry budget or hits an uncaught error, stop the run immediately.
- Use the same semantics for both backend project runs and CLI runs.

## Why Current Behavior Produced 5050 Loops

Current implementation mixes concerns:

- Backend supervisor keeps looping while any todo is "available":
  - `src/scheduler/supervisor.rs:90`
  - `src/scheduler/supervisor.rs:190`
- "Available" includes `attempted`:
  - `src/storage.rs:187`
  - `src/storage.rs:191`
- Failed worker marks todo as `attempted`, so it becomes selectable again:
  - `src/scheduler/worker.rs:323`
  - `src/domain.rs:37`

Result: a failed todo is repeatedly re-selected in the same run.

## Target Semantics

### Control-Flow Contract

For each project run:

1. Claim next `pending` todo.
2. Process that todo with:
   - inner phase loops (`max_loop_iterations`, default 6)
   - outer per-todo retry loop (`max_retries`)
3. If todo succeeds: mark `done`, continue to next todo.
4. If todo exhausts retries or has uncaught error: mark todo as not done (`pending`) and **terminate run immediately**.

No project-level retry budget.  
No re-picking same failed todo within the same run because the run stops.

### Pseudocode (Desired)

```text
loop:
  todo = claim_next_pending()
  if none: return success

  result = run_single_todo_with_retries(todo, max_retries)
  if success:
    mark done
    continue
  else:
    mark pending
    return failure
```

## Implementation Guidelines (Code-Level)

## 1) Remove `attempted` Status

- Update enum and string mapping:
  - `src/domain.rs:34`
  - `src/domain.rs:42`
- Remove parser support as a first-class state, but keep backward compatibility by mapping legacy `"attempted"` -> `pending`:
  - `src/storage.rs:912`
- Remove API acceptance of `attempted`:
  - `src/bin/backend/api/service.rs:1263`
- Update tests/fixtures referencing `attempted`:
  - `src/storage.rs` tests near `1577`
  - `src/scheduler/worker.rs` tests near `589`, `716`
  - `src/scheduler/supervisor.rs` tests near `498`
  - `src/bin/backend/api/service.rs` tests near `2146`, `2222`

## 2) Make Selection Strictly `pending`

Currently selectable = not done + not in_progress:
- `src/storage.rs:187`
- `src/storage.rs:191`

Change selection to `status == pending` only.

This should apply consistently to:
- backend scheduling path
- CLI engine path

## 3) Add Atomic "Claim Next Pending" API

Avoid list-then-claim race patterns:
- current picker path:
  - `src/service.rs:107`
  - `src/service.rs:615`
  - `src/storage.rs:328`

Add a storage method with one transaction:

- `claim_next_pending_todo(order: priority DESC, id ASC)`:
  - select one pending
  - update to `in_progress`
  - sync `todos.yaml`
  - return claimed todo

Then use this single API everywhere.

## 4) Keep Per-Todo Retry Loop as the Only Retry Budget

Keep and reuse:
- `src/service.rs:405` (`run_single_todo_with_retries`)
- `src/orchestrator.rs:83` (retry primitive)

On retry exhaustion:
- return terminal failure for that todo
- do not continue run

## 5) Backend: Switch to Fail-Fast Run Termination on Todo Failure

Current backend only auto-stops on `unrecoverable` worker result:
- `src/scheduler/supervisor.rs:227`
- `src/scheduler/supervisor.rs:235`

New behavior:
- any non-cancelled worker failure should request stop/cancel immediately for that project run.
- keep multi-agent support: cancel other workers and finish run as failure.

Worker failure state update:
- on terminal todo failure, set todo back to `pending` (not `attempted`):
  - current attempted write at `src/scheduler/worker.rs:323` should change.

## 6) CLI: Remove Outer Project-Level Retry Wrapper

Current CLI path uses project-level retry wrapper:
- `src/bin/chief.rs:217`
- `src/service.rs:721`

Refactor to fail-fast todo queue processing:
- loop over claimed pending todos
- each todo uses `run_single_todo_with_retries`
- first terminal todo failure exits non-zero immediately

`max_retries` remains a per-todo budget (CLI override still valid):
- `src/bin/chief.rs:213`

## 7) Make `max_loop` Configurable in `chief.yaml`

Current loop bounds are hardcoded defaults:
- convergence max loops 6: `src/flow.rs:1040`
- until-pass max loops 6: `src/flow.rs:1167`
- stable iterations 2: `src/flow.rs:1039`

Add config fields in `ChiefConfig`:
- `max_loop_iterations` (default `6`)
- `required_stable_iterations` (default `2`)

Config touchpoint:
- `src/config.rs:47`

Wire policy from config into flow construction:
- current flow build call:
  - `src/service.rs:358` (`build_flow(flow_kind)`)
- change to pass loop policy derived from `chief.yaml` values.

## 8) Preserve Retry Cleanup Behavior

Keep existing per-retry cleanup (`git reset --hard`, `git clean -fd`) before attempt > 1:
- `src/service.rs:431`
- `src/service.rs:443`
- `src/service.rs:887`

This remains part of "Ralph Wiggum technique" per todo.

## 9) Persistence and Compatibility

- Persist todo/job/event updates to both DB and `todos.yaml` as today:
  - `src/storage.rs:203`
  - `src/storage.rs:604`
- Backward compatibility:
  - if existing DB/YAML contains `attempted`, treat it as `pending` at load/parse time.

## 10) Test Plan

- Unit:
  - `claim_next_pending_todo` ordering and atomic claim semantics.
  - parser maps legacy `"attempted"` -> `pending`.
  - config parsing defaults and overrides for new loop fields.
- Integration:
  - single todo exhausts retries -> run exits failure immediately, todo remains `pending`.
  - backend multi-agent: one worker terminal failure cancels peers and ends project run.
  - no repeated same-todo re-pick within one run after retry exhaustion.
- Regression:
  - successful todo path still marks `done` and continues to next todo.

## 11) Intended Outcome

- No infinite useless loops on one todo.
- Retry semantics are intuitive: `max_retries` means retries for a single todo only.
- Status model stays simple and explicit.
- Backend and CLI behavior converge to the same fail-fast contract.
