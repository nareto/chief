  The codebase has already been through 5 recent cleanup commits focused specifically on deduplication, dead code removal, type safety, and mechanical cleanup:

  - 4fb1731 - deduplicate WaitState, payload_from_json, process group helpers
  - c56ec49 - remove dead code, deduplicate error handling, improve type safety
  - 7229024 - deduplicate WebSocket utils, endpoint fallback, flow resolution
  - 297b79a - apply safe mechanical cleanup pass
  - adbfef2 - better stabilizing prompt

  Compilation: Zero warnings from cargo check.
  Tests: All 67 frontend tests pass, all Rust tests pass.

  Remaining issues found and why I'm NOT modifying them:

  1. Test helper duplication (TempDir, init_git_repo, pending_todo duplicated across scheduler/worker.rs, scheduler/supervisor.rs, service.rs) — These are all in #[cfg(test)] modules. The guardrail
  says "Do not modify existing tests, unless there is no other way."
  2. Ignored placeholder test in supervisor.rs with TODO comment — This is test code, off-limits per guardrails.
  3. Missing "lint_fix" case in parse_event_type (storage.rs) — While this is an inconsistency where EventType::LintFix events can be written to SQLite but read back as Msg, fixing it would alter
  observable behavior (events would change type in the API response). The guardrail says "Do not alter program logic."
  4. Large files (flow.rs ~4K lines, backend api/service.rs ~80KB) — Breaking these up would be structural refactoring that risks altering outward-facing interfaces.
  5. Frontend patterns (duplicate JSON parsing in api.ts, monolithic cockpit page, form state boilerplate) — These are structural refactoring opportunities that risk altering UX flow and would require
  modifying tests.
  6. Frontend naming collision (two local phaseLabel functions in different files) — The terminal-panel one is file-local and doesn't cause any actual conflict.
  7. Query key magic strings in frontend — Creating a constants object would be adding new abstraction, which the instructions caution against ("Don't create helpers, utilities, or abstractions for
  one-time operations").

  Conclusion

  The codebase is sufficiently clean after the recent cleanup passes. All remaining issues are either in test code (off-limits), would require structural refactoring (too risky given guardrails), or
  would alter observable behavior. No modifications are warranted.