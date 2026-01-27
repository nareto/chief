"""Tests for file pattern matching and suite detection logic."""
import pytest


# class TestFilterTestFiles:
#     """Tests for filter_test_files() - fnmatch-based filtering."""

#     def test_matches_test_prefix_pattern(self, sample_suite):
#         """Files matching test_*.py pattern are included."""
#         import chief

#         files = ["test_module.py", "module.py", "test_other.py"]
#         result = chief.SuiteManager.filter_test_files(files, sample_suite)

#         assert "test_module.py" in result
#         assert "test_other.py" in result
#         assert "module.py" not in result

#     def test_matches_test_suffix_pattern(self, sample_suite):
#         """Files matching *_test.py pattern are included."""
#         import chief

#         files = ["module_test.py", "module.py", "other_test.py"]
#         result = chief.SuiteManager.filter_test_files(files, sample_suite)

#         assert "module_test.py" in result
#         assert "other_test.py" in result
#         assert "module.py" not in result

#     def test_empty_patterns_returns_empty(self):
#         """Suite with no file_patterns returns empty list."""
#         import chief

#         suite = {"file_patterns": []}
#         files = ["test_foo.py", "foo_test.py"]
#         result = chief.SuiteManager.filter_test_files(files, suite)

#         assert result == []

#     def test_full_path_preserved(self, sample_suite):
#         """Full file paths are preserved in output."""
#         import chief

#         files = ["backend/tests/test_module.py", "backend/src/module.py"]
#         result = chief.SuiteManager.filter_test_files(files, sample_suite)

#         assert result == ["backend/tests/test_module.py"]

#     def test_no_duplicates_from_multiple_patterns(self, sample_suite):
#         """File matching multiple patterns appears only once."""
#         import chief

#         # test_foo_test.py matches both test_*.py and *_test.py
#         files = ["test_foo_test.py"]
#         result = chief.SuiteManager.filter_test_files(files, sample_suite)

#         assert result == ["test_foo_test.py"]
#         assert len(result) == 1

#     def test_non_matching_files_excluded(self, sample_suite):
#         """Files not matching any pattern are excluded."""
#         import chief

#         files = ["module.py", "conftest.py", "setup.py"]
#         result = chief.SuiteManager.filter_test_files(files, sample_suite)

#         assert result == []


class TestDetectSuiteFromPath:
    """Tests for detect_suite_from_path() - suite detection by path."""

    def test_matches_file_in_suite_root(self, mock_config):
        """File under suite's test_root matches that suite."""
        import chief

        suite = chief.detect_suite_from_path("backend/src/module.py")

        assert suite is not None
        assert suite["name"] == "backend"

    def test_matches_deeply_nested_file(self, mock_config):
        """Deeply nested file still matches correct suite."""
        import chief

        suite = chief.detect_suite_from_path("backend/src/deep/nested/module.py")

        assert suite is not None
        assert suite["name"] == "backend"

    def test_no_match_returns_none(self, monkeypatch):
        """File not under any suite's test_root returns None."""
        import chief

        config = {
            "suites": [{
                "name": "frontend",
                "language": "TypeScript",
                "framework": "vitest",
                "test_root": "frontend/",
                "test_command": "npm test",
                "target_type": "file",
            }]
        }
        monkeypatch.setattr(chief, 'CONFIG', config)

        suite = chief.detect_suite_from_path("backend/src/file.py")

        assert suite is None

    def test_dot_root_matches_everything(self, monkeypatch):
        """Suite with test_root='.' matches any path."""
        import chief

        config = {
            "suites": [{
                "name": "all",
                "language": "Python",
                "framework": "pytest",
                "test_root": ".",
                "test_command": "pytest",
                "target_type": "file",
            }]
        }
        monkeypatch.setattr(chief, 'CONFIG', config)

        assert chief.detect_suite_from_path("any/path/file.py") is not None
        assert chief.detect_suite_from_path("file.py") is not None

    def test_empty_root_matches_everything(self, monkeypatch):
        """Suite with test_root='' matches any path."""
        import chief

        config = {
            "suites": [{
                "name": "all",
                "language": "Python",
                "framework": "pytest",
                "test_root": "",
                "test_command": "pytest",
                "target_type": "file",
            }]
        }
        monkeypatch.setattr(chief, 'CONFIG', config)

        assert chief.detect_suite_from_path("any/path/file.py") is not None

    def test_first_matching_suite_wins(self, mock_multi_config):
        """When multiple suites could match, first one wins."""
        import chief

        # backend/ comes first in multi_suite_config
        suite = chief.detect_suite_from_path("backend/module.py")
        assert suite["name"] == "backend"


class TestGetSuiteByName:
    """Tests for get_suite_by_name() - lookup by name."""

    def test_finds_existing_suite(self, mock_config):
        """Returns suite when name matches."""
        import chief

        suite = chief.get_suite_by_name("backend")

        assert suite is not None
        assert suite["name"] == "backend"

    def test_returns_none_for_missing(self, mock_config):
        """Returns None when suite name not found."""
        import chief

        suite = chief.get_suite_by_name("nonexistent")

        assert suite is None

    def test_finds_second_suite(self, mock_multi_config):
        """Can find suites that aren't first in the list."""
        import chief

        suite = chief.get_suite_by_name("frontend")

        assert suite is not None
        assert suite["name"] == "frontend"


# class TestFilterTestFilesAllSuites:
#     """Tests for SuiteManager.filter_test_files_all_suites() - multi-suite grouping."""

#     def test_groups_files_by_suite(self, mock_multi_config):
#         """Files are grouped by their detected suite."""
#         import chief

#         files = [
#             "backend/test_api.py",
#             "frontend/App.test.ts",
#             "backend/src/module.py",  # Not a test file
#         ]
#         result = chief.SuiteManager.filter_test_files_all_suites(files)

#         assert "backend" in result
#         assert "frontend" in result
#         assert result["backend"] == ["backend/test_api.py"]
#         assert result["frontend"] == ["frontend/App.test.ts"]

#     def test_ignores_files_without_suite_match(self, mock_multi_config):
#         """Files not matching any suite are not included."""
#         import chief

#         files = ["other/test_foo.py"]
#         result = chief.SuiteManager.filter_test_files_all_suites(files)

#         # "other/" doesn't match backend/ or frontend/ test_roots
#         assert result == {}

#     def test_empty_files_returns_empty_dict(self, mock_multi_config):
#         """Empty file list returns empty dict."""
#         import chief

#         result = chief.SuiteManager.filter_test_files_all_suites([])

#         assert result == {}

#     def test_non_test_files_excluded(self, mock_multi_config):
#         """Files in suite but not matching test patterns are excluded."""
#         import chief

#         files = ["backend/src/module.py", "frontend/src/App.ts"]
#         result = chief.SuiteManager.filter_test_files_all_suites(files)

#         assert result == {}
