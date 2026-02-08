improve clarity and readability of rust backend:

  1. Split src/bin/backend.rs into modules.
     src/bin/backend.rs:98 currently mixes app bootstrap, DTOs, HTTP handlers, query parsing, and
     terminal websocket logic. I’d split into api/router.rs, api/handlers/*.rs, api/types.rs, api/
     error.rs, api/terminal_ws.rs.
  2. Replace flow strings with a typed enum in core.
     src/flow.rs:534 silently defaults unknown flow names to tdd, which hurts clarity and
     correctness. Keep strings at API/DB boundaries, but parse once into FlowKind in src/service.rs
     and src/scheduler.rs. Unknown flows in input sould result in error.
  3. Add a service layer so handlers are thin.
     Handlers in src/bin/backend.rs directly touch scheduler/store everywhere. Introduce
     ProjectService methods and make each handler mostly parse input -> call service -> map
     response.
  4. Break scheduler into supervisor, worker, and selector modules.
     src/scheduler.rs:175 + src/scheduler.rs:351 is the hardest part to read because selection,
     spawning, worktree lifecycle, merge, and DB updates are interleaved. Splitting these into
     focused modules will make control flow much easier to follow.
  5. Stop swallowing state-update errors.
     There are several let _ = ... writes in failure paths (for example src/service.rs:291, src/
     service.rs:307, src/scheduler.rs:463). At minimum, record a DB event when those writes fail so
     failures are observable.
  6. Use typed API responses instead of ad-hoc serde_json::Value.
     Returning Json<serde_json::Value> everywhere makes contracts implicit. Define explicit response
     structs for projects/jobs/logs/state endpoints.
  7. Centralize event creation.
     Event writing is repeated in many places with similar payload patterns (src/flow.rs, src/
     service.rs, src/scheduler.rs). A small EventLogger helper would remove duplication and make
     event semantics consistent.
  8. Add focused orchestration tests.
     Tests for flow parsing, build_flow behavior, run/job/todo transitions, and scheduler spawn/stop
     semantics will make the structure self-documenting and safer to refactor.

