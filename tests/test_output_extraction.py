"""Tests for extracting markers from Claude output."""
import pytest


class TestExtractExistingTests:
    """Tests for extract_existing_tests() - TESTS_ALREADY_EXIST parsing."""

    def test_extracts_single_file(self):
        """Extracts single file from output."""
        import chief

        output = "Some text\nTESTS_ALREADY_EXIST: tests/test_api.py\nMore text"

        result = chief.extract_existing_tests(output)

        assert result == ["tests/test_api.py"]

    def test_extracts_multiple_files(self):
        """Extracts comma-separated files."""
        import chief

        output = "TESTS_ALREADY_EXIST: test_a.py, test_b.py, test_c.py"

        result = chief.extract_existing_tests(output)

        assert result == ["test_a.py", "test_b.py", "test_c.py"]

    def test_strips_backticks(self):
        """Removes backticks from paths."""
        import chief

        output = "TESTS_ALREADY_EXIST: `test_a.py`, `test_b.py`"

        result = chief.extract_existing_tests(output)

        assert result == ["test_a.py", "test_b.py"]

    def test_strips_quotes(self):
        """Removes single and double quotes from paths."""
        import chief

        output = "TESTS_ALREADY_EXIST: 'test_a.py', \"test_b.py\""

        result = chief.extract_existing_tests(output)

        assert result == ["test_a.py", "test_b.py"]

    def test_returns_empty_when_not_found(self):
        """Returns empty list when marker not found."""
        import chief

        output = "No tests exist marker here"

        result = chief.extract_existing_tests(output)

        assert result == []

    def test_handles_whitespace(self):
        """Handles extra whitespace correctly."""
        import chief

        output = "TESTS_ALREADY_EXIST:   test_a.py  ,  test_b.py  "

        result = chief.extract_existing_tests(output)

        assert result == ["test_a.py", "test_b.py"]

    def test_filters_empty_entries(self):
        """Empty entries from extra commas are filtered."""
        import chief

        output = "TESTS_ALREADY_EXIST: test_a.py,, test_b.py,"

        result = chief.extract_existing_tests(output)

        # Empty strings should be filtered out
        assert "" not in result
        assert "test_a.py" in result
        assert "test_b.py" in result

    def test_multiline_finds_marker(self):
        """Finds marker in multiline output."""
        import chief

        output = """
Claude is analyzing the codebase...
Looking for existing tests...
TESTS_ALREADY_EXIST: tests/test_module.py
Done.
"""

        result = chief.extract_existing_tests(output)

        assert result == ["tests/test_module.py"]


class TestExtractTestTarget:
    """Tests for extract_test_target() - TEST_TARGET parsing."""

    def test_extracts_target_path(self):
        """Extracts path from TEST_TARGET line."""
        import chief

        output = "Some output\nTEST_TARGET: tests/test_module.py\nMore output"

        result = chief.extract_test_target(output)

        assert result == "tests/test_module.py"

    def test_strips_backticks(self):
        """Removes backticks from target."""
        import chief

        output = "TEST_TARGET: `tests/test_module.py`"

        result = chief.extract_test_target(output)

        assert result == "tests/test_module.py"

    def test_strips_quotes(self):
        """Removes quotes from target."""
        import chief

        output = 'TEST_TARGET: "tests/test_module.py"'

        result = chief.extract_test_target(output)

        assert result == "tests/test_module.py"

    def test_returns_none_when_not_found(self):
        """Returns None when no TEST_TARGET found."""
        import chief

        output = "No target here"

        result = chief.extract_test_target(output)

        assert result is None

    def test_returns_none_for_empty_target(self):
        """Returns None for empty target value."""
        import chief

        output = "TEST_TARGET:   "

        result = chief.extract_test_target(output)

        assert result is None

    def test_handles_whitespace(self):
        """Handles extra whitespace correctly."""
        import chief

        output = "TEST_TARGET:    tests/test_module.py   "

        result = chief.extract_test_target(output)

        assert result == "tests/test_module.py"

    def test_first_match_wins(self):
        """If multiple TEST_TARGET lines, first one wins."""
        import chief

        output = """
TEST_TARGET: first.py
TEST_TARGET: second.py
"""

        result = chief.extract_test_target(output)

        assert result == "first.py"
