"""Tests for todo prioritization and management logic."""
import json
import pytest


class TestGetNextTodo:
    """Tests for get_next_todo() - priority-based selection."""

    def test_returns_highest_priority(self):
        """Returns the todo with highest priority value."""
        import chief

        data = {
            "todos": [
                {"todo": "Low", "priority": 1, "done_at_commit": None},
                {"todo": "High", "priority": 10, "done_at_commit": None},
                {"todo": "Medium", "priority": 5, "done_at_commit": None},
            ]
        }

        result = chief.get_next_todo(data)

        assert result["todo"] == "High"
        assert result["priority"] == 10

    def test_skips_completed_todos(self):
        """Completed todos (done_at_commit set) are skipped."""
        import chief

        data = {
            "todos": [
                {"todo": "Done", "priority": 100, "done_at_commit": "abc123"},
                {"todo": "Pending", "priority": 1, "done_at_commit": None},
            ]
        }

        result = chief.get_next_todo(data)

        assert result["todo"] == "Pending"

    def test_returns_none_when_all_complete(self):
        """Returns None when no pending todos remain."""
        import chief

        data = {
            "todos": [
                {"todo": "Done1", "priority": 10, "done_at_commit": "abc"},
                {"todo": "Done2", "priority": 5, "done_at_commit": "def"},
            ]
        }

        result = chief.get_next_todo(data)

        assert result is None

    def test_returns_none_for_empty_list(self):
        """Returns None for empty todos list."""
        import chief

        data = {"todos": []}

        result = chief.get_next_todo(data)

        assert result is None

    def test_missing_priority_defaults_to_zero(self):
        """Todos without priority field default to 0."""
        import chief

        data = {
            "todos": [
                {"todo": "No priority", "done_at_commit": None},
                {"todo": "Has priority", "priority": 1, "done_at_commit": None},
            ]
        }

        result = chief.get_next_todo(data)

        # priority=1 beats default of 0
        assert result["todo"] == "Has priority"

    def test_equal_priority_returns_first(self):
        """When priorities are equal, returns first in list."""
        import chief

        data = {
            "todos": [
                {"todo": "First", "priority": 5, "done_at_commit": None},
                {"todo": "Second", "priority": 5, "done_at_commit": None},
            ]
        }

        result = chief.get_next_todo(data)

        # With equal priority, sort is stable so first wins
        assert result["todo"] == "First"


class TestLoadTodos:
    """Tests for load_todos() - JSON file loading."""

    def test_loads_valid_todos(self, tmp_path, monkeypatch):
        """Valid todos.json is loaded correctly."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")

        todos_data = {
            "todos": [
                {"todo": "Task 1", "priority": 10, "done_at_commit": None},
                {"todo": "Task 2", "priority": 5, "done_at_commit": None},
            ]
        }
        (tmp_path / "todos.json").write_text(json.dumps(todos_data))

        result = chief.load_todos()

        assert "todos" in result
        assert len(result["todos"]) == 2

    def test_missing_file_exits(self, tmp_path, monkeypatch):
        """Missing todos.json causes sys.exit(1)."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        with pytest.raises(SystemExit) as exc_info:
            chief.load_todos()
        assert exc_info.value.code == 1


class TestSaveTodos:
    """Tests for save_todos() - JSON file writing."""

    def test_saves_todos_correctly(self, tmp_path, monkeypatch):
        """Todos dict is saved to file correctly."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")

        todos_data = {"todos": [{"todo": "Test", "priority": 1}]}

        chief.save_todos(todos_data)

        saved = json.loads((tmp_path / "todos.json").read_text())
        assert saved == todos_data

    def test_overwrites_existing_file(self, tmp_path, monkeypatch):
        """Existing file is overwritten with new data."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")

        # Write initial data
        (tmp_path / "todos.json").write_text('{"todos": [{"todo": "Old"}]}')

        # Save new data
        new_data = {"todos": [{"todo": "New"}]}
        chief.save_todos(new_data)

        saved = json.loads((tmp_path / "todos.json").read_text())
        assert saved["todos"][0]["todo"] == "New"


class TestCleanDoneTodos:
    """Tests for clean_done_todos() - removing completed todos."""

    def test_removes_completed_todos(self, tmp_path, monkeypatch):
        """Todos with done_at_commit set are removed."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        initial = {
            "todos": [
                {"todo": "Done", "done_at_commit": "abc123"},
                {"todo": "Pending", "done_at_commit": None},
            ]
        }
        (tmp_path / "todos.json").write_text(json.dumps(initial))

        result = chief.clean_done_todos()

        assert result == 0
        saved = json.loads((tmp_path / "todos.json").read_text())
        assert len(saved["todos"]) == 1
        assert saved["todos"][0]["todo"] == "Pending"

    def test_returns_zero_when_nothing_to_clean(self, tmp_path, monkeypatch):
        """Returns 0 when no completed todos exist."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        initial = {"todos": [{"todo": "Pending", "done_at_commit": None}]}
        (tmp_path / "todos.json").write_text(json.dumps(initial))

        result = chief.clean_done_todos()

        assert result == 0

    def test_removes_all_when_all_complete(self, tmp_path, monkeypatch):
        """All todos removed when all are complete."""
        import chief

        monkeypatch.chdir(tmp_path)
        monkeypatch.setattr(chief, 'TODOS_FILE', "todos.json")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        initial = {
            "todos": [
                {"todo": "Done1", "done_at_commit": "abc"},
                {"todo": "Done2", "done_at_commit": "def"},
            ]
        }
        (tmp_path / "todos.json").write_text(json.dumps(initial))

        chief.clean_done_todos()

        saved = json.loads((tmp_path / "todos.json").read_text())
        assert saved["todos"] == []
