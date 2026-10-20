# Suites and readiness checks

A suite tells chief how to set up, lint and test one part of your project. Suites live under `suites:` in `.chief/chief.yaml`.

## Suite fields

```yaml
suites:
  - name: backend
    language: Rust
    framework: cargo test
    test_root: .                     # commands run here
    test_command: cargo test         # may contain {target}
    target_type: project
    default_target: .                # used for {target} when --target is not given
    file_patterns: []
    lint_command: cargo clippy
    # lint_fix_command: cargo fmt    # runs after a lint failure, then lint runs again
    post_green_command: cargo test   # runs once after convergence, before the commit
    # test_init: ...                 # prepares the suite before checks in a worktree
    # test_setup: ...                # same, runs after test_init
    # cleanup_command: pkill -f "cargo test.*$PWD" || true   # after test attempts
    # cache_paths: [target]
    # cache_key_files: [Cargo.lock]
    # cache_mode: copy               # copy (default) or symlink
    # command_timeout_seconds: 1800  # overrides suite_command_timeout_seconds
    env: {}
    strip_root_from_target: true
```

[`.chief/chief.example.yaml`](../.chief/chief.example.yaml) has ready-to-adapt patterns for Rust, Python, TypeScript and Playwright.

## Where suites run

- `loop_file` runs lint and test for every configured suite after each pass.
- `refactor` runs lint and test for the suites listed in the todo's `test_suites`.
- `chief review` runs no suites.

## Running suite commands yourself

```bash
chief suite test       --suite backend
chief suite test       --suite backend --target src/lib.rs
chief suite lint       --suite backend --target src
chief suite lint_fix   --suite backend --target src
chief suite test_init  --suite backend
chief suite test_setup --suite backend
```

Commands run in the suite's `test_root`. For commands containing `{target}`, `--target` replaces the suite's `default_target`.

## Readiness checks

Before the backend starts working on a project, it checks that the suites pass on the current code. Without that, an agent could spend its whole budget on a failure it did not cause.

- Checks run in a temporary worktree, `../<project>__worktrees/chief_pre_run_checks_<token>`.
- They run `test_init`, `test_setup`, `lint` and `test` for each suite.
- A passing result is cached, keyed by `.chief/chief.yaml` and the suite cache inputs. `chief check` reuses the cache; `chief check --force` ignores it.
- Callers can skip the checks with `start_anyway`.

Backend suite-check requests also run in temporary worktrees, `../<project>__worktrees/chief_suite_check_<token>`, and support `test` and `lint`. `post_green` is not available there.

## Dependency caches

`cache_paths` (for example `target` or `node_modules`) are copied from a passing readiness worktree into `../<project>__worktree_cache/<suite>/<cache_key>`. Later worker worktrees receive a copy, or a symlink with `cache_mode: symlink`, instead of rebuilding from scratch. The cache key combines `cache_key_files`, the suite config and the hash of `.chief/chief.yaml`.
