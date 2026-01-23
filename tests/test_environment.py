"""Tests for environment variable handling."""
import os
import pytest
from unittest.mock import patch


class TestGetSuiteEnv:
    """Tests for get_suite_env() - environment variable merging."""

    def test_merges_with_current_env(self):
        """Suite env is merged with current environment."""
        import chief

        with patch.dict(os.environ, {"EXISTING": "value"}, clear=True):
            suite = {"env": {"NEW_VAR": "new_value"}}
            result = chief.get_suite_env(suite)

        assert result["EXISTING"] == "value"
        assert result["NEW_VAR"] == "new_value"

    def test_suite_vars_override_existing(self):
        """Suite vars override existing environment vars."""
        import chief

        with patch.dict(os.environ, {"OVERRIDE_ME": "old"}, clear=True):
            suite = {"env": {"OVERRIDE_ME": "new"}}
            result = chief.get_suite_env(suite)

        assert result["OVERRIDE_ME"] == "new"

    def test_converts_values_to_strings(self):
        """Non-string values are converted to strings."""
        import chief

        with patch.dict(os.environ, {}, clear=True):
            suite = {"env": {"NUMBER": 42, "BOOL": True}}
            result = chief.get_suite_env(suite)

        assert result["NUMBER"] == "42"
        assert result["BOOL"] == "True"

    def test_handles_missing_env_key(self):
        """Handles suite without env key gracefully."""
        import chief

        with patch.dict(os.environ, {"EXISTING": "value"}, clear=True):
            suite = {}  # No env key
            result = chief.get_suite_env(suite)

        assert result["EXISTING"] == "value"

    def test_handles_empty_env(self):
        """Handles empty env dict."""
        import chief

        with patch.dict(os.environ, {"EXISTING": "value"}, clear=True):
            suite = {"env": {}}
            result = chief.get_suite_env(suite)

        assert result == {"EXISTING": "value"}

    def test_does_not_modify_original_environ(self):
        """Original os.environ is not modified."""
        import chief

        original_value = "original"
        with patch.dict(os.environ, {"TEST_VAR": original_value}, clear=True):
            suite = {"env": {"TEST_VAR": "overridden"}}
            result = chief.get_suite_env(suite)

            # Result has the override
            assert result["TEST_VAR"] == "overridden"
            # But original environ is unchanged
            assert os.environ["TEST_VAR"] == original_value


class TestStripAnsi:
    """Tests for strip_ansi() - ANSI code removal."""

    def test_removes_color_codes(self):
        """Removes ANSI color codes."""
        import chief

        text = "\033[31mred text\033[0m"
        result = chief.strip_ansi(text)

        assert result == "red text"

    def test_removes_style_codes(self):
        """Removes ANSI style codes (bold, dim, etc.)."""
        import chief

        text = "\033[1m\033[2mbold dim text\033[0m"
        result = chief.strip_ansi(text)

        assert result == "bold dim text"

    def test_preserves_plain_text(self):
        """Plain text without ANSI codes is unchanged."""
        import chief

        text = "plain text with no codes"
        result = chief.strip_ansi(text)

        assert result == text

    def test_handles_multiple_codes(self):
        """Handles multiple ANSI codes in sequence."""
        import chief

        text = "\033[31m\033[1mred bold\033[0m normal \033[32mgreen\033[0m"
        result = chief.strip_ansi(text)

        assert result == "red bold normal green"

    def test_handles_extended_colors(self):
        """Handles extended 256-color ANSI sequences."""
        import chief

        text = "\033[38;5;196mextended color\033[0m"
        result = chief.strip_ansi(text)

        assert result == "extended color"


class TestColor:
    """Tests for color() - ANSI code application."""

    def test_adds_codes_when_terminal(self):
        """Adds ANSI codes when stdout is a terminal."""
        import chief

        with patch('sys.stdout.isatty', return_value=True):
            result = chief.color("test", chief.Colors.RED)

        assert "\033[31m" in result
        assert "test" in result
        assert "\033[0m" in result  # Reset code

    def test_returns_plain_when_not_terminal(self):
        """Returns plain text when stdout is not a terminal."""
        import chief

        with patch('sys.stdout.isatty', return_value=False):
            result = chief.color("test", chief.Colors.RED)

        assert result == "test"
        assert "\033[" not in result

    def test_combines_multiple_codes(self):
        """Combines multiple style codes."""
        import chief

        with patch('sys.stdout.isatty', return_value=True):
            result = chief.color("test", chief.Colors.RED, chief.Colors.BOLD)

        assert "\033[31m" in result  # RED
        assert "\033[1m" in result   # BOLD
