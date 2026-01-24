"""Tests for git status parsing logic."""
import pytest
from unittest.mock import MagicMock


class TestGetDirtyFiles:
    """Tests for GitOperations.get_dirty_files() - git porcelain output parsing."""

    def test_modified_file_is_dirty(self, mock_subprocess):
        """Modified file ( M) is included in dirty files."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout=" M src/module.py\n")

        result = chief.GitOperations.get_dirty_files()

        assert "src/module.py" in result

    def test_untracked_file_is_dirty(self, mock_subprocess):
        """Untracked file (??) is included in dirty files."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="?? new_file.py\n")

        result = chief.GitOperations.get_dirty_files()

        assert "new_file.py" in result

    def test_staged_added_file_is_dirty(self, mock_subprocess):
        """Staged added file (A ) is included in dirty files."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="A  staged.py\n")

        result = chief.GitOperations.get_dirty_files()

        assert "staged.py" in result

    def test_staged_modified_file_is_dirty(self, mock_subprocess):
        """Staged modified file (M ) is included in dirty files."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="M  modified_staged.py\n")

        result = chief.GitOperations.get_dirty_files()

        assert "modified_staged.py" in result

    def test_deleted_file_is_dirty(self, mock_subprocess):
        """Deleted file (D ) is included in dirty files."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="D  deleted.py\n")

        result = chief.GitOperations.get_dirty_files()

        assert "deleted.py" in result

    def test_renamed_file_uses_new_name(self, mock_subprocess):
        """Renamed file (R ) uses destination name, not source."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="R  old_name.py -> new_name.py\n")

        result = chief.GitOperations.get_dirty_files()

        assert "new_name.py" in result
        assert "old_name.py" not in result

    def test_empty_status_returns_empty_set(self, mock_subprocess):
        """Empty git status output returns empty set."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="")

        result = chief.GitOperations.get_dirty_files()

        assert result == set()

    def test_multiple_files_all_included(self, mock_subprocess):
        """Multiple files with different statuses are all included."""
        import chief
        mock_subprocess.return_value = MagicMock(
            stdout=" M modified.py\n?? untracked.py\nA  added.py\nD  deleted.py\n"
        )

        result = chief.GitOperations.get_dirty_files()

        assert len(result) == 4
        assert "modified.py" in result
        assert "untracked.py" in result
        assert "added.py" in result
        assert "deleted.py" in result


class TestGitGetStatusSnapshot:
    """Tests for GitOperations.get_status_snapshot() - returns dict of file -> status."""

    def test_returns_dict_of_status_codes(self, mock_subprocess):
        """Returns dict mapping filepath to status code."""
        import chief
        mock_subprocess.return_value = MagicMock(
            stdout=" M src/module.py\n?? new.py\nA  added.py\n"
        )

        result = chief.GitOperations.get_status_snapshot()

        assert result["src/module.py"] == " M"
        assert result["new.py"] == "??"
        assert result["added.py"] == "A "

    def test_renamed_uses_new_name(self, mock_subprocess):
        """Renamed file uses destination path as key."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="R  old.py -> new.py\n")

        result = chief.GitOperations.get_status_snapshot()

        assert "new.py" in result
        assert "old.py" not in result

    def test_empty_status_returns_empty_dict(self, mock_subprocess):
        """Empty status returns empty dict."""
        import chief
        mock_subprocess.return_value = MagicMock(stdout="")

        result = chief.GitOperations.get_status_snapshot()

        assert result == {}


class TestGitDetectChangedFiles:
    """Tests for GitOperations.detect_changed_files() - change detection vs baseline."""

    def test_new_file_detected(self, mock_subprocess, tmp_path, monkeypatch):
        """File new to git status (not in baseline) is detected as changed."""
        import chief
        monkeypatch.chdir(tmp_path)

        # Create the file so Path.exists() returns True
        (tmp_path / "new.py").write_text("content")

        mock_subprocess.return_value = MagicMock(stdout="?? new.py\n")

        baseline = {}  # Empty baseline
        result = chief.GitOperations.detect_changed_files(baseline)

        assert "new.py" in result

    def test_status_change_detected(self, mock_subprocess, tmp_path, monkeypatch):
        """File with changed status code is detected."""
        import chief
        monkeypatch.chdir(tmp_path)

        (tmp_path / "file.py").write_text("content")

        mock_subprocess.return_value = MagicMock(stdout=" M file.py\n")

        baseline = {"file.py": "??"}  # Was untracked, now modified
        result = chief.GitOperations.detect_changed_files(baseline)

        assert "file.py" in result

    def test_unchanged_file_not_detected(self, mock_subprocess, tmp_path, monkeypatch):
        """File with same status as baseline is not detected."""
        import chief
        monkeypatch.chdir(tmp_path)

        (tmp_path / "file.py").write_text("content")

        mock_subprocess.return_value = MagicMock(stdout=" M file.py\n")

        baseline = {"file.py": " M"}  # Same status
        result = chief.GitOperations.detect_changed_files(baseline)

        assert "file.py" not in result

    def test_nonexistent_file_not_included(self, mock_subprocess, tmp_path, monkeypatch):
        """File in git status but not on disk is not included."""
        import chief
        monkeypatch.chdir(tmp_path)

        # Don't create the file - it shouldn't exist
        mock_subprocess.return_value = MagicMock(stdout="?? ghost.py\n")

        baseline = {}
        result = chief.GitOperations.detect_changed_files(baseline)

        assert "ghost.py" not in result
