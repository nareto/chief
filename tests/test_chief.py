from __future__ import annotations

import datetime as dt
import json
import os
import stat
from pathlib import Path
from types import SimpleNamespace
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import chief


def _suite(**overrides: object) -> chief.TestSuite:
    data: dict[str, object] = {
        "name": "backend",
        "language": "python",
        "framework": "pytest",
        "test_root": "tests",
        "test_command": "pytest {target}",
        "target_type": chief.TargetType.file,
    }
    data.update(overrides)
    return chief.TestSuite(**data)  # type: ignore[arg-type]


def _state(
    *,
    phase: chief.Phase = chief.Phase.red,
    run_id: str = "run-1",
    current_todo_id: str = "todo-1",
    iofacade: SimpleNamespace | None = None,
) -> SimpleNamespace:
    return SimpleNamespace(
        run_id=run_id,
        current_todo_id=current_todo_id,
        current_phase=phase,
        iofacade=iofacade or SimpleNamespace(log_event=lambda event: None),
    )


class _ContextStrategy(chief.LoopStrategy):
    def attempt_fix(self) -> chief.SubprocessOutput:
        return chief.SubprocessOutput(
            exit_code=0,
            merged_output="",
            stdout="",
            stderr="",
            command="",
        )

    def check_goal(
        self, iteration_idx: int, iteration_output: chief.SubprocessOutput
    ) -> chief.LoopDecision:
        return chief.LoopDecision.success


def _make_context_strategy(dbclient: object) -> _ContextStrategy:
    iofacade = object.__new__(chief.ChiefIOFacade)
    iofacade.dbclient = dbclient  # type: ignore[attr-defined]
    iofacade._formatter = chief.EventFormatter()  # type: ignore[attr-defined]

    return _ContextStrategy(
        agent=SimpleNamespace(),
        chief_run_state=SimpleNamespace(run_id="current", iofacade=iofacade),
        todo=SimpleNamespace(todo_id="todo-1"),
    )


def test_todo_compute_id_ignores_surrounding_whitespace() -> None:
    first = chief.Todo.compute_id("Implement feature", "must pass")
    second = chief.Todo.compute_id("  Implement feature  ", "\nmust pass\n")
    assert first == second


def test_todo_load_from_dict_applies_defaults() -> None:
    todo = chief.Todo.load_from_dict("run-123", {"todo": "  Ship it  "})
    assert todo.run_id == "run-123"
    assert todo.todo == "Ship it"
    assert todo.expectations == ""
    assert todo.priority == 0
    assert todo.status == chief.TodoStatus.pending
    assert todo.todo_id == chief.Todo.compute_id("Ship it", "")


def test_todo_list_manager_remove_done_filters_only_completed_with_commit(
    tmp_path: Path,
) -> None:
    todos_path = tmp_path / "todos.json"
    todos_path.write_text(
        json.dumps(
            {
                "todos": [
                    {
                        "id": "done-1",
                        "todo": "done",
                        "expectations": "",
                        "priority": 1,
                        "status": "done",
                        "done_at_commit": "abc123",
                    },
                    {
                        "id": "done-2",
                        "todo": "done without commit",
                        "expectations": "",
                        "priority": 1,
                        "status": "done",
                        "done_at_commit": "",
                    },
                    {
                        "id": "pending-1",
                        "todo": "pending",
                        "expectations": "",
                        "priority": 1,
                        "status": "pending",
                    },
                ]
            }
        ),
        encoding="utf-8",
    )

    chief.TodoListManager(str(todos_path)).remove_done()

    data = json.loads(todos_path.read_text(encoding="utf-8"))
    ids = [todo["id"] for todo in data["todos"]]
    assert ids == ["done-2", "pending-1"]


def test_todo_list_manager_update_todo_status_updates_target_todo(tmp_path: Path) -> None:
    todos_path = tmp_path / "todos.json"
    todos_path.write_text(
        json.dumps(
            {
                "todos": [
                    {
                        "id": "a",
                        "todo": "alpha",
                        "expectations": "",
                        "priority": 1,
                        "status": "pending",
                    },
                    {
                        "id": "b",
                        "todo": "beta",
                        "expectations": "",
                        "priority": 2,
                        "status": "pending",
                    },
                ]
            }
        ),
        encoding="utf-8",
    )

    chief.TodoListManager(str(todos_path)).update_todo_status(
        "b", chief.TodoStatus.done, done_at_commit="deadbeef"
    )

    data = json.loads(todos_path.read_text(encoding="utf-8"))
    by_id = {todo["id"]: todo for todo in data["todos"]}
    assert by_id["a"]["status"] == "pending"
    assert by_id["b"]["status"] == "done"
    assert by_id["b"]["done_at_commit"] == "deadbeef"


def test_chief_toml_manager_load_parses_core_configuration(tmp_path: Path) -> None:
    toml_path = tmp_path / "chief.toml"
    toml_path.write_text(
        """
        [chief]
        agent = "codex"
        agent_extra_args = ["--model", "gpt-5"]
        max_retries = 4

        [[suites]]
        name = "py"
        language = "python"
        framework = "pytest"
        test_root = "."
        test_command = "pytest {target}"
        target_type = "file"
        default_target = "tests"
        file_patterns = ["test_*.py"]
        disallow_write_globs = ["tests/**"]
        lint_command = "ruff check {target}"
        lint_fix_command = "ruff check --fix {target}"
        strip_root_from_target = false
        [suites.env]
        FOO = "bar"
        """,
        encoding="utf-8",
    )

    loaded = chief.ChiefTomlManager(str(toml_path)).load()

    assert loaded.chief.agent == "codex"
    assert loaded.chief.agent_extra_args == ["--model", "gpt-5"]
    assert loaded.chief.max_retries == 4
    assert len(loaded.suites) == 1
    suite = loaded.suites[0]
    assert suite.name == "py"
    assert suite.target_type == chief.TargetType.file
    assert suite.file_patterns == ["test_*.py"]
    assert suite.disallow_write_globs == ["tests/**"]
    assert suite.env == {"FOO": "bar"}
    assert suite.strip_root_from_target is False


def test_codex_code_parse_output_supports_multiple_json_shapes() -> None:
    output = "\n".join(
        [
            json.dumps({"item": {"type": "agent_message", "text": "A"}}),
            json.dumps(
                {
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"text": "B"}, {"type": "output_text", "text": "C"}],
                    }
                }
            ),
            json.dumps({"item": {"type": "message", "role": "assistant", "content": "D"}}),
            json.dumps({"output_text": "E"}),
            "not-json",
        ]
    )

    parsed = chief.CodexCode().parse_output(output)
    assert parsed == "ABCDE"


def test_decode_jsonish_unwraps_double_encoded_json() -> None:
    encoded = json.dumps(json.dumps({"suite": "backend", "exit_code": 0}))
    decoded = chief._decode_jsonish(encoded)
    assert decoded == {"suite": "backend", "exit_code": 0}


def test_chief_loggable_event_round_trip_restores_payload_and_event_type(
    tmp_path: Path,
) -> None:
    db = chief.DBClient(str(tmp_path / "chief.db"))
    db.save_run(
        chief.Run(
            run_id="run-1",
            status="running",
            exit_status=None,
            started_at=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
            ended_at=None,
        )
    )
    db.save_event(
        chief.ChiefLoggableEvent(
            run_id="run-1",
            level="info",
            msg="Lint passed (backend)",
            event_type=chief.EventType.lint,
            timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
            payload={
                "suite": "backend",
                "command": "ruff check .",
                "output": "All checks passed!",
                "exit_code": 0,
            },
        )
    )

    events = db.get_events(
        ["run-1"],
        event_types=[chief.EventType.lint],
        limit=1,
    )
    assert len(events) == 1
    loaded = events[0]
    assert loaded.event_type == chief.EventType.lint
    assert isinstance(loaded.payload, dict)
    assert loaded.payload["suite"] == "backend"

    formatted = chief.EventFormatter().format_events(events)
    assert "LINT PASS (backend)" in formatted
    db.close()


def test_select_suites_returns_matching_suites_in_source_order() -> None:
    suites = [_suite(name="a"), _suite(name="b"), _suite(name="c")]
    selected = chief._select_suites(suites, ["c", "a"])
    assert [suite.name for suite in selected] == ["a", "c"]


def test_apply_readonly_globs_and_restore_permissions_round_trip(tmp_path: Path) -> None:
    file_path = tmp_path / "data.txt"
    file_path.write_text("x", encoding="utf-8")
    original_mode = stat.S_IMODE(os.lstat(file_path).st_mode)

    restored = chief._apply_readonly_globs([str(file_path)])
    readonly_mode = stat.S_IMODE(os.lstat(file_path).st_mode)
    assert readonly_mode & stat.S_IWUSR == 0

    chief._restore_permissions(restored)
    restored_mode = stat.S_IMODE(os.lstat(file_path).st_mode)
    assert restored_mode == original_mode


def test_build_phase_context_prefers_current_run_events() -> None:
    now = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    event = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="info",
        msg="current event",
        event_type=chief.EventType.agent_response,
        timestamp=now,
    )
    calls: list[str] = []

    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            calls.append(f"events:{run_ids[0]}")
            return [event] if run_ids == ["current"] else []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            calls.append("recent")
            return ["previous"]

    iofacade = object.__new__(chief.ChiefIOFacade)
    iofacade.dbclient = FakeDB()  # type: ignore[attr-defined]
    iofacade._formatter = chief.EventFormatter()  # type: ignore[attr-defined]
    context = chief.ChiefIOFacade.get_phase_context(
        iofacade,
        todo_id="todo-1",
        run_id="current",
        phase=chief.Phase.red,
        event_types=[chief.EventType.agent_response],
    )
    assert context
    assert context != "No previous attempts recorded."
    assert calls == ["events:current"]


def test_build_phase_context_falls_back_to_previous_run() -> None:
    now = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    old_event = chief.ChiefLoggableEvent(
        run_id="previous",
        todo_id="todo-1",
        level="warning",
        msg="previous event",
        event_type=chief.EventType.phase_failure,
        timestamp=now,
    )

    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            if run_ids == ["current"]:
                return []
            if run_ids == ["previous"]:
                return [old_event]
            return []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            return ["previous"]

    iofacade = object.__new__(chief.ChiefIOFacade)
    iofacade.dbclient = FakeDB()  # type: ignore[attr-defined]
    iofacade._formatter = chief.EventFormatter()  # type: ignore[attr-defined]
    context = chief.ChiefIOFacade.get_phase_context(
        iofacade,
        todo_id="todo-1",
        run_id="current",
        phase=chief.Phase.green,
        event_types=[chief.EventType.phase_failure],
    )
    assert "previous event" in context


def test_build_phase_context_returns_default_message_when_no_events() -> None:
    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            return []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            return []

    iofacade = object.__new__(chief.ChiefIOFacade)
    iofacade.dbclient = FakeDB()  # type: ignore[attr-defined]
    iofacade._formatter = chief.EventFormatter()  # type: ignore[attr-defined]
    context = chief.ChiefIOFacade.get_phase_context(
        iofacade,
        todo_id="todo-1",
        run_id="current",
        phase=chief.Phase.green,
        event_types=[chief.EventType.phase_failure],
    )
    assert context == "No previous attempts recorded."


def test_strategy_context_drops_old_lint_failures_after_latest_pass() -> None:
    now = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    lint_fail = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="warning",
        msg="Lint failed (backend)",
        event_type=chief.EventType.lint,
        timestamp=now,
        phase=chief.Phase.red,
        payload={
            "command": "ruff check .",
            "output": "E999 bad",
            "exit_code": 1,
            "suite": "backend",
        },
    )
    lint_pass = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="info",
        msg="Lint passed (backend)",
        event_type=chief.EventType.lint,
        timestamp=now + dt.timedelta(seconds=1),
        phase=chief.Phase.red,
        payload={
            "command": "ruff check .",
            "output": "",
            "exit_code": 0,
            "suite": "backend",
        },
    )

    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            return [lint_pass, lint_fail] if run_ids == ["current"] else []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            return []

    strategy = _make_context_strategy(FakeDB())
    context = strategy._build_previous_steps_log(
        phase=chief.Phase.red,
        event_types=[chief.EventType.lint],
        prune_resolved_checks=True,
    )
    assert "LINT PASS (backend)" in context
    assert "LINT FAIL" not in context


def test_strategy_context_drops_old_test_failures_after_latest_pass() -> None:
    now = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    test_fail = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="warning",
        msg="Test failed",
        event_type=chief.EventType.test_run,
        timestamp=now,
        phase=chief.Phase.green,
        payload={
            "command": "pytest tests",
            "output": "FAILED",
            "exit_code": 1,
        },
    )
    test_pass = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="info",
        msg="Test passed",
        event_type=chief.EventType.test_run,
        timestamp=now + dt.timedelta(seconds=1),
        phase=chief.Phase.green,
        payload={
            "command": "pytest tests",
            "output": "ok",
            "exit_code": 0,
        },
    )

    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            return [test_pass, test_fail] if run_ids == ["current"] else []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            return []

    strategy = _make_context_strategy(FakeDB())
    context = strategy._build_previous_steps_log(
        phase=chief.Phase.green,
        event_types=[chief.EventType.test_run],
        prune_resolved_checks=True,
    )
    assert "TEST PASS: pytest tests" in context
    assert "TEST FAIL" not in context


def test_strategy_context_drops_resolved_phase_failures() -> None:
    now = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    phase_failure = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="warning",
        msg="Stability loop failed once",
        event_type=chief.EventType.phase_failure,
        timestamp=now,
        phase=chief.Phase.red,
    )
    lint_pass = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="info",
        msg="Lint passed (backend)",
        event_type=chief.EventType.lint,
        timestamp=now + dt.timedelta(seconds=1),
        phase=chief.Phase.red,
        payload={
            "command": "ruff check .",
            "output": "",
            "exit_code": 0,
            "suite": "backend",
        },
    )

    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            return [lint_pass, phase_failure] if run_ids == ["current"] else []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            return []

    strategy = _make_context_strategy(FakeDB())
    context = strategy._build_previous_steps_log(
        phase=chief.Phase.red,
        event_types=[chief.EventType.phase_failure, chief.EventType.lint],
        prune_resolved_checks=True,
        prune_resolved_phase_failures=True,
    )
    assert "PHASE FAILURE" not in context
    assert "LINT PASS (backend)" in context


def test_strategy_context_falls_back_when_current_events_are_other_phase() -> None:
    now = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    current_green = chief.ChiefLoggableEvent(
        run_id="current",
        todo_id="todo-1",
        level="info",
        msg="current green",
        event_type=chief.EventType.phase_failure,
        timestamp=now,
        phase=chief.Phase.green,
    )
    previous_red = chief.ChiefLoggableEvent(
        run_id="previous",
        todo_id="todo-1",
        level="info",
        msg="previous red",
        event_type=chief.EventType.phase_failure,
        timestamp=now,
        phase=chief.Phase.red,
    )

    class FakeDB:
        def get_events(self, run_ids, **kwargs):  # type: ignore[no-untyped-def]
            if run_ids == ["current"]:
                return [current_green]
            if run_ids == ["previous"]:
                return [previous_red]
            return []

        def get_recent_run_ids_for_todo(self, todo_id, exclude_run_id):  # type: ignore[no-untyped-def]
            return ["previous"]

    strategy = _make_context_strategy(FakeDB())
    context = strategy._build_previous_steps_log(
        phase=chief.Phase.red,
        event_types=[chief.EventType.phase_failure],
    )
    assert "previous red" in context
    assert "current green" not in context


def test_event_formatter_formats_test_failures_without_framework_specific_parsing() -> None:
    event = chief.ChiefLoggableEvent(
        run_id="run-1",
        level="warning",
        msg="Test command result",
        event_type=chief.EventType.test_run,
        timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        payload={
            "command": "run-tests tests",
            "exit_code": 1,
            "output": "FAILED tests/test_sample.py::test_example - AssertionError: boom",
        },
    )

    formatted = chief.EventFormatter().format_events([event])
    assert "TEST FAIL (exit code 1): run-tests tests" in formatted
    assert (
        "Output:\nFAILED tests/test_sample.py::test_example - AssertionError: boom"
        in formatted
    )
    assert "\n  - tests/test_sample.py::test_example" not in formatted


def test_event_formatter_formats_lint_failures_without_parser_rules() -> None:
    event = chief.ChiefLoggableEvent(
        run_id="run-1",
        level="warning",
        msg="Lint command result",
        event_type=chief.EventType.lint,
        timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        payload={
            "suite": "backend",
            "exit_code": 1,
            "output": "src/app.py:10:5: E501 line too long",
        },
    )

    formatted = chief.EventFormatter().format_events([event])
    assert "LINT FAIL (backend):" in formatted
    assert "Output:\nsrc/app.py:10:5: E501 line too long" in formatted


def test_event_formatter_truncates_output_to_last_ten_lines_by_default() -> None:
    output = "\n".join(f"line-{i}" for i in range(1, 16))
    event = chief.ChiefLoggableEvent(
        run_id="run-1",
        level="warning",
        msg="Test command result",
        event_type=chief.EventType.test_run,
        timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        payload={"command": "run-tests tests", "exit_code": 1, "output": output},
    )

    formatted = chief.EventFormatter().format_events([event])
    output_block = formatted.split("Output:\n", 1)[1]
    assert output_block.splitlines() == [f"line-{i}" for i in range(6, 16)]


def test_event_formatter_uses_shared_instance_truncation_limits() -> None:
    output = "0123456789abcdef"
    formatter = chief.EventFormatter(max_output_lines=10, max_output_chars=6)
    test_event = chief.ChiefLoggableEvent(
        run_id="run-1",
        level="warning",
        msg="Test command result",
        event_type=chief.EventType.test_run,
        timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        payload={"command": "run-tests tests", "exit_code": 1, "output": output},
    )
    lint_event = chief.ChiefLoggableEvent(
        run_id="run-1",
        level="warning",
        msg="Lint command result",
        event_type=chief.EventType.lint,
        timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        payload={"suite": "backend", "exit_code": 1, "output": output},
    )

    formatted = formatter.format_events([test_event, lint_event])
    assert formatted.count("Output:\nabcdef") == 2
    assert "0123456789" not in formatted


def test_test_runner_format_target_strips_test_root_prefix() -> None:
    runner = chief.TestRunner(
        _suite(test_root="tests", default_target="tests", strip_root_from_target=True),
        _state(),
    )
    assert runner._format_target("tests/unit/test_sample.py") == "unit/test_sample.py"
    assert runner._format_target(None) == "tests"


def test_linting_fix_strategy_run_lint_checks_runs_fix_command_before_retry(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    suite = _suite(
        lint_command="lint {target}",
        lint_fix_command="lint-fix {target}",
        test_root="tests",
    )
    logged: list[chief.ChiefLoggableEvent] = []
    state = _state(
        phase=chief.Phase.red,
        run_id="run-1",
        current_todo_id="todo-1",
        iofacade=SimpleNamespace(log_event=lambda event: logged.append(event)),
    )
    strategy = chief.LintingFixStrategy(
        agent=chief.CodexCode(),
        chief_run_state=state,
        todo=SimpleNamespace(todo_id="todo-1"),
        suites=[suite],
    )

    calls: list[tuple[str, bool]] = []
    outputs = [
        chief.SubprocessOutput(
            exit_code=1,
            merged_output="lint failed",
            stdout="",
            stderr="",
            command="lint tests",
        ),
        chief.SubprocessOutput(
            exit_code=0,
            merged_output="fixed",
            stdout="",
            stderr="",
            command="lint-fix tests",
        ),
        chief.SubprocessOutput(
            exit_code=0,
            merged_output="ok",
            stdout="",
            stderr="",
            command="lint tests",
        ),
    ]

    def fake_run_command(
        cmd: str, cwd: str, env: dict[str, str], stream_output: bool = True
    ) -> chief.SubprocessOutput:
        calls.append((cmd, stream_output))
        return outputs.pop(0)

    monkeypatch.setattr(chief.SubprocessRunner, "run", fake_run_command)

    failures = strategy._run_lint_checks(phase_label="red", run_fix=True)
    assert failures == []
    assert calls == [
        ("lint tests", False),
        ("lint-fix tests", False),
        ("lint tests", False),
    ]
    assert len(logged) == 3
    assert [event.msg for event in logged] == [
        "Lint failed (backend)",
        "Lint fix command result",
        "Lint passed (backend)",
    ]


def test_chief_iofacade_log_event_hides_lint_failure_output_in_stdout_logs() -> None:
    class FakeDB:
        def __init__(self) -> None:
            self.saved: list[chief.ChiefLoggableEvent] = []

        def save_event(self, event: chief.ChiefLoggableEvent) -> None:
            self.saved.append(event)

    class FakeLogger:
        def __init__(self) -> None:
            self.logged: list[tuple[str, str]] = []

        def warning(self, message: str) -> None:
            self.logged.append(("warning", message))

        def info(self, message: str) -> None:
            self.logged.append(("info", message))

    iofacade = object.__new__(chief.ChiefIOFacade)
    iofacade.dbclient = FakeDB()  # type: ignore[attr-defined]
    iofacade.logger = FakeLogger()  # type: ignore[attr-defined]

    raw_output = "line-1\nline-2\nline-3"
    event = chief.ChiefLoggableEvent(
        run_id="run-1",
        level="warning",
        msg="Lint failed (backend)",
        event_type=chief.EventType.lint,
        timestamp=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        payload={
            "suite": "backend",
            "command": "ruff check tests",
            "exit_code": 1,
            "output": raw_output,
        },
    )

    chief.ChiefIOFacade.log_event(iofacade, event)
    assert len(iofacade.dbclient.saved) == 1  # type: ignore[attr-defined]
    assert iofacade.dbclient.saved[0].payload["output"] == raw_output  # type: ignore[attr-defined]
    logged_message = iofacade.logger.logged[0][1]  # type: ignore[attr-defined]
    assert "line-1" not in logged_message
    assert "<omitted; full lint output saved to DB and prompt tail>" in logged_message


def test_test_runner_validate_or_init_runs_init_after_missing_command(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    suite = _suite(
        target_type=chief.TargetType.project,
        default_target=None,
        test_init="bootstrap-tests",
    )
    runner = chief.TestRunner(suite, _state())
    run_calls: list[str | None] = []
    fake_outputs = iter(
        [
            chief.SubprocessOutput(
                exit_code=127,
                merged_output="command not found",
                stdout="",
                stderr="",
                command="pytest .",
            ),
            chief.SubprocessOutput(
                exit_code=0,
                merged_output="ok",
                stdout="",
                stderr="",
                command="pytest .",
            ),
        ]
    )

    def fake_run(target: str | None = None) -> chief.SubprocessOutput:
        run_calls.append(target)
        return next(fake_outputs)

    init_calls: list[tuple[object, ...]] = []

    def fake_subprocess_run(*args, **kwargs):  # type: ignore[no-untyped-def]
        init_calls.append(args)
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(runner, "run", fake_run)
    monkeypatch.setattr(chief.subprocess, "run", fake_subprocess_run)

    runner.validate_or_init()
    assert run_calls == [None, None]
    assert len(init_calls) == 1
    assert init_calls[0][0] == "bootstrap-tests"


def test_linting_fix_strategy_check_goal_runs_lint_and_succeeds(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    suite = _suite(lint_command="lint {target}", test_root="tests")
    run_fix_flags: list[bool] = []

    def fake_run_lint_checks(  # type: ignore[no-untyped-def]
        self, phase_label, run_fix=False
    ):
        run_fix_flags.append(run_fix)
        return []

    monkeypatch.setattr(chief.LintingFixStrategy, "_run_lint_checks", fake_run_lint_checks)

    state = _state(phase=chief.Phase.red)
    todo = SimpleNamespace(todo_id="todo-1")
    strategy = chief.LintingFixStrategy(
        agent=chief.CodexCode(),
        chief_run_state=state,
        todo=todo,
        suites=[suite],
    )

    decision = strategy.check_goal(
        0, chief.SubprocessOutput(exit_code=0, merged_output="", stdout="", stderr="")
    )
    assert decision == chief.LoopDecision.success
    assert run_fix_flags == [False]


def test_linting_fix_strategy_precheck_runs_lint_once_without_fix(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    suite = _suite(
        lint_command="lint {target}",
        lint_fix_command="lint-fix {target}",
        test_root="tests",
    )
    run_fix_flags: list[bool] = []

    def fake_run_lint_checks(  # type: ignore[no-untyped-def]
        self, phase_label, run_fix=False
    ):
        run_fix_flags.append(run_fix)
        return []

    monkeypatch.setattr(chief.LintingFixStrategy, "_run_lint_checks", fake_run_lint_checks)

    state = _state(phase=chief.Phase.red)
    strategy = chief.LintingFixStrategy(
        agent=chief.CodexCode(),
        chief_run_state=state,
        todo=SimpleNamespace(todo_id="todo-1"),
        suites=[suite],
    )

    chief.UntilPassLoopContext(strategy=strategy).run()
    assert run_fix_flags == [False]


def test_post_green_strategy_enables_pre_loop_stability_check() -> None:
    strategy = chief.PostGreenStrategy(
        agent=SimpleNamespace(),
        chief_run_state=SimpleNamespace(current_phase=chief.Phase.post_green),
        todo=SimpleNamespace(todo_id="todo-1"),
        suites=[],
    )
    assert strategy.check_goal_before_loop is True


def test_convergence_loop_context_short_circuits_on_precheck_success() -> None:
    decisions: list[int] = []
    runs: list[int] = []

    class Strategy:
        check_goal_before_loop = True
        chief_run_state = SimpleNamespace(
            run_id="run-1",
            current_phase=chief.Phase.red,
            iofacade=SimpleNamespace(log_event=lambda event: None),
        )
        todo = SimpleNamespace(todo_id="todo-1")

        def attempt_fix(self) -> chief.SubprocessOutput:
            runs.append(1)
            return chief.SubprocessOutput(
                exit_code=0, merged_output="", stdout="", stderr="", command=""
            )

        def check_goal(  # type: ignore[no-untyped-def]
            self, iteration_idx, iteration_output
        ) -> chief.LoopDecision:
            decisions.append(iteration_idx)
            return chief.LoopDecision.success

    chief.ConvergenceLoopContext(strategy=Strategy(), required_stable_iterations=2).run()
    assert decisions == [-1]
    assert runs == []


def test_convergence_loop_context_does_not_count_precheck_stable() -> None:
    decisions = iter(
        [
            chief.LoopDecision.stable,
            chief.LoopDecision.stable,
            chief.LoopDecision.stable,
        ]
    )
    seen_indices: list[int] = []
    runs: list[int] = []

    class Strategy:
        check_goal_before_loop = True
        chief_run_state = SimpleNamespace(
            run_id="run-1",
            current_phase=chief.Phase.red,
            iofacade=SimpleNamespace(log_event=lambda event: None),
        )
        todo = SimpleNamespace(todo_id="todo-1")

        def attempt_fix(self) -> chief.SubprocessOutput:
            runs.append(1)
            return chief.SubprocessOutput(
                exit_code=0, merged_output="", stdout="", stderr="", command=""
            )

        def check_goal(  # type: ignore[no-untyped-def]
            self, iteration_idx, iteration_output
        ) -> chief.LoopDecision:
            seen_indices.append(iteration_idx)
            return next(decisions)

    context = chief.ConvergenceLoopContext(
        strategy=Strategy(), required_stable_iterations=2
    )
    context.run()
    assert seen_indices == [-1, 0, 1]
    assert len(runs) == 2


def test_convergence_loop_context_requires_consecutive_stable_iterations() -> None:
    decisions = iter(
        [
            chief.LoopDecision.stable,
            chief.LoopDecision.retry,
            chief.LoopDecision.stable,
            chief.LoopDecision.stable,
        ]
    )
    runs: list[int] = []

    class Strategy:
        check_goal_before_loop = False
        chief_run_state = SimpleNamespace(
            run_id="run-1",
            current_phase=chief.Phase.red,
            iofacade=SimpleNamespace(log_event=lambda event: None),
        )
        todo = SimpleNamespace(todo_id="todo-1")

        def attempt_fix(self) -> chief.SubprocessOutput:
            runs.append(1)
            return chief.SubprocessOutput(
                exit_code=0, merged_output="", stdout="", stderr="", command=""
            )

        def check_goal(  # type: ignore[no-untyped-def]
            self, iteration_idx, iteration_output
        ) -> chief.LoopDecision:
            return next(decisions)

    context = chief.ConvergenceLoopContext(
        strategy=Strategy(), required_stable_iterations=2
    )
    context.run()
    assert len(runs) == 4


def test_convergence_loop_context_raises_when_never_stable() -> None:
    class Strategy:
        check_goal_before_loop = False
        chief_run_state = SimpleNamespace(
            run_id="run-1",
            current_phase=chief.Phase.green,
            iofacade=SimpleNamespace(log_event=lambda event: None),
        )
        todo = SimpleNamespace(todo_id="todo-1")

        def attempt_fix(self) -> chief.SubprocessOutput:
            return chief.SubprocessOutput(
                exit_code=0, merged_output="", stdout="", stderr="", command=""
            )

        def check_goal(  # type: ignore[no-untyped-def]
            self, iteration_idx, iteration_output
        ) -> chief.LoopDecision:
            return chief.LoopDecision.retry

    context = chief.ConvergenceLoopContext(
        strategy=Strategy(), required_stable_iterations=2, max_loops=3
    )
    with pytest.raises(chief.UnrecoverableError, match="failed to converge"):
        context.run()
