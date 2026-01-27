#!/usr/bin/env python3
"""
chief.py - TDD Orchestrator for Claude Code

Runs a Red-Green-Refactor cycle using Claude Code as the coding agent.
Loads todos from todos.json and processes them by priority.
Configuration is loaded from chief.toml for language/framework flexibility.
"""

import argparse
import fnmatch
import hashlib
import json
import subprocess
import sys
import os
import textwrap
import tomllib
from pathlib import Path
from datetime import datetime
from dataclasses import dataclass
from enum import Enum
from typing import Any, Callable, Optional, TextIO
import atexit


# ============================================================================
# Prompt System - All Claude Prompt Types in One Place
# ============================================================================


class PromptType(Enum):
    """All Claude prompts - easy to find and audit."""

    RED_WRITE_TESTS = "red_write_tests"
    RED_REFINE_TESTS = "red_refine_tests"
    RED_VERIFY_EXISTING = "red_verify_existing"
    GREEN_IMPLEMENT = "green_implement"
    GREEN_IMPLEMENT_NO_TESTS = "green_implement_no_tests"
    FIX_FAILING_TESTS = "fix_failing_tests"
    FIX_FAILING_BUILD = "fix_failing_build"
    VERIFY_COMPLETION = "verify_completion"
    COMPARE_FAILURES = "compare_failures"


# ============================================================================
# PROMPT_TEMPLATES - All Claude Prompts in One Place
# ============================================================================

PROMPT_TEMPLATES: dict[PromptType, str] = {
    PromptType.RED_WRITE_TESTS: """Write or modify tests (Red phase of TDD) for the following task:

Task: {task}
{expectations_section}

Available test suites in this project:
{suite_info}

Instructions:
1. Analyze the task and determine which test suite(s) are appropriate
2. First, search for existing tests related to this functionality
3. **If comprehensive tests already exist for this task:**
   - Output a single line: TESTS_ALREADY_EXIST: path/to/test1.py, path/to/test2.py
   - List all relevant existing test files (comma-separated)
   - Do NOT create or modify any files
   - Stop here - do not proceed with further instructions
4. Follow the patterns and conventions used in existing tests (fixtures, configuration, setup/teardown, naming conventions)
5. Determine the nature of this task:

   **If MODIFYING existing behavior:**
   - Find and update existing tests to reflect the new expected behavior
   - The modified tests should FAIL until the implementation is updated
   - Keep test names/structure where possible, just update expectations

   **If ADDING new functionality:**
   - Write new failing tests for the new feature
   - Place them in the appropriate existing test file if one exists, or create a new one

   **If FIXING a bug:**
   - Add a regression test that currently FAILS (demonstrates the bug)
   - If existing tests have incorrect expectations, fix them

6. Write COMPREHENSIVE tests covering:
   - Happy path (normal expected usage)
   - Edge cases (empty input, boundary values)
   - Error conditions (invalid input, missing data)
   - Security considerations if applicable
7. Multiple test functions in one test file is expected and encouraged
8. Write tests in the correct location based on the suite conventions above

Only write/modify the tests, do not implement the feature.""",
    PromptType.RED_REFINE_TESTS: """We are in the RED PHASE for this task:

Task: {task}
{expectations_section}

There are already these related test file(s) (that fail as expected, because we are in RED phase, no implementation yet):
{file_list}

Your instructions are:
1. Read the test file(s) listed above
2. Check if the tests accurately represent the task description
3. Check for any bugs, typos, or logic errors in the tests
4. Ensure test coverage is comprehensive (happy path, edge cases, error conditions) and strict (not overly permissive)
5. If improvements are needed, edit the test file(s)
6. If the tests are already correct and complete, make NO changes

Only modify the tests if there are actual issues to fix.

DO NOT attempt to make the tests pass. They SHOULD fail at this point.""",
    PromptType.RED_VERIFY_EXISTING: """For this task, verify whether comprehensive tests already exist:

Task: {task}
{expectations_section}

Available test suites:
{suite_info}

If tests already exist that cover this task, output a single line:
TESTS_ALREADY_EXIST: path/to/test1.py, path/to/test2.py

If tests need to be written or modified, proceed to write them.""",
    PromptType.GREEN_IMPLEMENT: """Implement the following task:

Task: {task}

Tests have been created in the following locations:
{test_locations_str}

Do NOT modify any test files. Only implement the code to make ALL tests pass.

Implement the minimal code needed to pass the tests.""",
    PromptType.GREEN_IMPLEMENT_NO_TESTS: """Implement the following task:

Task: {task}
{expectations_section}
{retry_context}

Implement the task completely.""",
    PromptType.FIX_FAILING_TESTS: """The tests are failing. Fix the code to make them pass.

Original task: {task}

Test files (DO NOT MODIFY):
{test_locations_str}

Test failures:
{failure_output}

Analyze the test failures and fix the implementation code to make ALL tests pass.""",
    PromptType.FIX_FAILING_BUILD: """The build/validation command is failing. Fix the code to make it pass.

Original task: {task}

Test files (DO NOT MODIFY):
{test_locations_str}

Build failures:
{failure_output}

Analyze the build failures and fix the implementation code. The tests are already passing,
so ensure your fix does not break the tests. Common issues include:
- TypeScript compilation errors (unused variables, type mismatches)
- Linting errors
- Build configuration issues""",
    PromptType.VERIFY_COMPLETION: """Review the current state of the files for this task:

Task: {task}
{expectations_section}

Is the task fully completed? Output ONLY 'YES' or 'NO'.""",
}


TODOS_FILE = "todos.json"
CONFIG_FILE = "chief.toml"
MAX_IMPLEMENTATION_ATTEMPTS = 6
MAX_FIX_ATTEMPTS = 6
MAX_TEST_REFINEMENT_ITERATIONS = 6
STABILITY_ITERATIONS = 2  # Times Claude must give consistent answer before accepting

# Auto-push commits to remote (can be disabled with --no-autopush)
AUTOPUSH: bool = True


# ============================================================================
# ANSI Color Codes (stdlib-only terminal styling)
# ============================================================================
class Colors:
    """ANSI escape codes for terminal colors."""

    # Reset
    RESET = "\033[0m"
    # Styles
    BOLD = "\033[1m"
    DIM = "\033[2m"
    # Foreground colors
    RED = "\033[31m"
    GREEN = "\033[32m"
    YELLOW = "\033[33m"
    BLUE = "\033[34m"
    MAGENTA = "\033[35m"
    CYAN = "\033[36m"
    WHITE = "\033[37m"
    # Bright foreground
    BRIGHT_RED = "\033[91m"
    BRIGHT_GREEN = "\033[92m"
    BRIGHT_YELLOW = "\033[93m"
    BRIGHT_BLUE = "\033[94m"
    BRIGHT_MAGENTA = "\033[95m"
    BRIGHT_CYAN = "\033[96m"


class Logger:
    """Centralized logging and output formatting."""

    _log_file: Optional[TextIO] = None

    @classmethod
    def set_log_file(cls, log_file: Optional[TextIO]) -> None:
        """Set the log file for verbose output."""
        cls._log_file = log_file

    @staticmethod
    def color(text: str, *codes: str) -> str:
        """Wrap text with ANSI color codes."""
        if not sys.stdout.isatty():
            return text  # No colors if not a terminal
        return "".join(codes) + text + Colors.RESET

    @staticmethod
    def strip_ansi(text: str) -> str:
        """Remove ANSI escape codes from text."""
        import re

        return re.sub(r"\033\[[0-9;]*m", "", text)

    @staticmethod
    def timestamp() -> str:
        """Return current timestamp in HH:MM:SS format for log entries."""
        return datetime.now().strftime("%H:%M:%S")

    @classmethod
    def write(cls, text: str) -> None:
        """Write text to log file (without ANSI codes)."""
        if cls._log_file:
            cls._log_file.write(cls.strip_ansi(text))
            cls._log_file.flush()

    @classmethod
    def banner(cls, text: str, char: str = "=", width: int = 60) -> None:
        """Print a prominent banner."""
        line = char * width
        print(cls.color(line, Colors.BRIGHT_CYAN, Colors.BOLD))
        print(cls.color(text.center(width), Colors.BRIGHT_CYAN, Colors.BOLD))
        print(cls.color(line, Colors.BRIGHT_CYAN, Colors.BOLD))
        # Log with box-drawing characters for readability
        ts = cls.timestamp()
        cls.write(f"[{ts}] ┏" + "━" * 78 + "┓\n")
        cls.write(f"[{ts}] ┃" + text.center(78) + "┃\n")
        cls.write(f"[{ts}] ┗" + "━" * 78 + "┛\n")

    @classmethod
    def phase(cls, phase: str, description: str) -> None:
        """Print a TDD phase header (RED/GREEN/BUILD/REFINE/FIX)."""
        phase_colors = {
            "RED": Colors.BRIGHT_RED,
            "GREEN": Colors.BRIGHT_GREEN,
            "BUILD": Colors.BRIGHT_BLUE,
            "BUILD-FIX": Colors.BRIGHT_BLUE,
            "REFINE": Colors.BRIGHT_YELLOW,
            "FIX": Colors.BRIGHT_MAGENTA,
        }
        phase_color = phase_colors.get(phase, Colors.CYAN)
        print(
            f"\n{cls.color(f'[{phase}]', phase_color, Colors.BOLD)} {cls.color(description, Colors.WHITE)}"
        )
        # Log with visual separator for phases
        cls.write("\n")
        cls.write("─" * 80 + "\n")
        cls.write(f"[{cls.timestamp()}] ▶ [{phase}] {description}\n")
        cls.write("─" * 80 + "\n")

    @classmethod
    def info(cls, msg: str, indent: int = 0) -> None:
        """Print an informational message from the script."""
        prefix = "  " * indent
        print(f"{prefix}{cls.color('▸', Colors.CYAN)} {msg}")
        cls.write(f"[{cls.timestamp()}] {prefix}> {msg}\n")

    @classmethod
    def success(cls, msg: str, indent: int = 0) -> None:
        """Print a success message."""
        prefix = "  " * indent
        print(
            f"{prefix}{cls.color('✓', Colors.BRIGHT_GREEN, Colors.BOLD)} {cls.color(msg, Colors.GREEN)}"
        )
        cls.write(f"[{cls.timestamp()}] {prefix}[OK] {msg}\n")

    @classmethod
    def warning(cls, msg: str, indent: int = 0) -> None:
        """Print a warning message."""
        prefix = "  " * indent
        print(
            f"{prefix}{cls.color('⚠', Colors.BRIGHT_YELLOW, Colors.BOLD)} {cls.color(msg, Colors.YELLOW)}"
        )
        cls.write(f"[{cls.timestamp()}] {prefix}[WARN] {msg}\n")

    @classmethod
    def error(cls, msg: str, indent: int = 0) -> None:
        """Print an error message."""
        prefix = "  " * indent
        print(
            f"{prefix}{cls.color('✗', Colors.BRIGHT_RED, Colors.BOLD)} {cls.color(msg, Colors.RED)}"
        )
        cls.write(f"[{cls.timestamp()}] {prefix}[ERROR] {msg}\n")

    @classmethod
    def claude_start(cls) -> None:
        """Print marker for start of Claude Code output (log only)."""
        ts = cls.timestamp()
        cls.write("\n")
        cls.write(f"[{ts}] ╔" + "═" * 78 + "╗\n")
        cls.write(f"[{ts}] ║" + " CLAUDE OUTPUT ".center(78) + "║\n")
        cls.write(f"[{ts}] ╚" + "═" * 78 + "╝\n")
        cls.write("\n")

    @classmethod
    def claude_end(cls) -> None:
        """Print marker for end of Claude Code output (log only)."""
        ts = cls.timestamp()
        cls.write("\n")
        cls.write(f"[{ts}] ╔" + "═" * 78 + "╗\n")
        cls.write(f"[{ts}] ║" + " END CLAUDE OUTPUT ".center(78) + "║\n")
        cls.write(f"[{ts}] ╚" + "═" * 78 + "╝\n")
        cls.write("\n")

    @classmethod
    def section_divider(cls, label: str = "") -> None:
        """Write a section divider to the log file for readability."""
        if label:
            # Centered label in divider
            padding = (80 - len(label) - 4) // 2
            line = "─" * padding + f"┤ {label} ├" + "─" * padding
            # Ensure consistent width
            if len(line) < 80:
                line += "─" * (80 - len(line))
        else:
            line = "─" * 80
        cls.write(f"\n{line}\n")

    @classmethod
    def prompt(cls, prompt_text: str, label: str = "PROMPT TO CLAUDE") -> None:
        """Log a prompt being sent to Claude with clear visual demarcation."""
        ts = cls.timestamp()
        cls.write("\n")
        cls.write(f"[{ts}] ┌" + "─" * 78 + "┐\n")
        cls.write(f"[{ts}] │" + f" {label} ".center(78) + "│\n")
        cls.write(f"[{ts}] ├" + "─" * 78 + "┤\n")
        # Wrap each line to fit in box (74 chars content width)
        for line in prompt_text.split("\n"):
            if len(line) <= 74:
                cls.write(f"[{ts}] │  {line.ljust(75)} │\n")
            else:
                wrapped_lines = textwrap.wrap(line, width=74)
                for wrapped in wrapped_lines:
                    cls.write(f"[{ts}] │  {wrapped.ljust(75)} │\n")
        cls.write(f"[{ts}] └" + "─" * 78 + "┘\n")
        cls.write("\n")


# Convenience aliases for backward compatibility
def color(text: str, *codes: str) -> str:
    return Logger.color(text, *codes)


def strip_ansi(text: str) -> str:
    return Logger.strip_ansi(text)


def timestamp() -> str:
    return Logger.timestamp()


def log_write(text: str) -> None:
    Logger.write(text)


def print_banner(text: str, char: str = "=", width: int = 60) -> None:
    Logger.banner(text, char, width)


def print_phase(phase: str, description: str) -> None:
    Logger.phase(phase, description)


def print_info(msg: str, indent: int = 0) -> None:
    Logger.info(msg, indent)


def print_success(msg: str, indent: int = 0) -> None:
    Logger.success(msg, indent)


def print_warning(msg: str, indent: int = 0) -> None:
    Logger.warning(msg, indent)


def print_error(msg: str, indent: int = 0) -> None:
    Logger.error(msg, indent)


def print_claude_start() -> None:
    Logger.claude_start()


def print_claude_end() -> None:
    Logger.claude_end()


def log_section_divider(label: str = "") -> None:
    Logger.section_divider(label)


def log_prompt(prompt: str, label: str = "PROMPT TO CLAUDE") -> None:
    Logger.prompt(prompt, label)


def parse_args() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(
        description="TDD Orchestrator for Claude Code - runs Red-Green-Refactor cycle"
    )
    parser.add_argument(
        "--no-autopush",
        action="store_true",
        help="Disable automatic git push after commits (commits are still created locally)",
    )
    parser.add_argument(
        "--clean-done",
        action="store_true",
        help="Remove completed todos (non-null done_at_commit) from todos.json and exit",
    )
    parser.add_argument(
        "--test-suite",
        metavar="NAME",
        help="Test a suite's configuration by running setup and a test command, then exit",
    )
    parser.add_argument(
        "--no-retry",
        action="store_true",
        help="Disable automatic retry on transient failures (retry is enabled by default)",
    )
    return parser.parse_args()


class ConfigManager:
    """Manages configuration loading and suite environment setup."""

    _config: dict = {}
    _setup_completed: set[str] = set()
    _file_path: str = CONFIG_FILE

    @classmethod
    def set_file_path(cls, path: str) -> None:
        """Set the path to the config file."""
        cls._file_path = path

    @classmethod
    def get_config(cls) -> dict:
        """Get the loaded configuration."""
        return cls._config

    @classmethod
    def get_suites(cls) -> list[dict]:
        """Get the list of test suites."""
        return cls._config.get("suites", [])

    @classmethod
    def load(cls) -> dict:
        """
        Load configuration from chief.toml.

        Expected structure (multi-suite format):
            [[suites]]
            name = "backend"
            language = "Python"
            framework = "pytest"
            test_root = "backend/"
            test_command = "pytest {target} -v"
            target_type = "file"
            file_patterns = ["test_*.py", "*_test.py"]
            disallow_write_globs = ["backend/tests/**"]
            test_init = "pip install -r requirements.txt"
            test_setup = "docker compose up -d db"
            post_green_command = "docker compose build"

        Returns:
            Configuration dictionary with 'suites' array
        """
        if not Path(cls._file_path).exists():
            Logger.error(f"{cls._file_path} not found")
            Logger.info("Please create a chief.toml configuration file.")
            print()
            print(Logger.color("Example chief.toml:", Colors.DIM))
            print(Logger.color("[[suites]]", Colors.DIM))
            print(Logger.color('name = "backend"', Colors.DIM))
            print(Logger.color('language = "Python"', Colors.DIM))
            print(Logger.color('framework = "pytest"', Colors.DIM))
            print(Logger.color('test_root = "."', Colors.DIM))
            print(Logger.color('test_command = "pytest {target} -v"', Colors.DIM))
            print(Logger.color('target_type = "file"', Colors.DIM))
            print(
                Logger.color('file_patterns = ["test_*.py", "*_test.py"]', Colors.DIM)
            )
            print(
                Logger.color(
                    'disallow_write_globs = ["tests/**", "test_*.py"]', Colors.DIM
                )
            )
            sys.exit(1)

        with open(cls._file_path, "rb") as f:
            config = tomllib.load(f)

        # Validate suites array exists
        if "suites" not in config or not config["suites"]:
            Logger.error(f"{cls._file_path} must contain at least one [[suites]] entry")
            sys.exit(1)

        # Validate each suite
        required_suite_keys = [
            "name",
            "language",
            "framework",
            "test_root",
            "test_command",
            "target_type",
        ]
        for i, suite in enumerate(config["suites"]):
            missing = [k for k in required_suite_keys if k not in suite]
            if missing:
                Logger.error(f"Suite {i+1} missing required keys: {', '.join(missing)}")
                sys.exit(1)

            # Set defaults for optional keys
            suite.setdefault("default_target", ".")
            suite.setdefault("file_patterns", [])
            suite.setdefault("disallow_write_globs", [])
            suite.setdefault("test_init", None)
            suite.setdefault("test_setup", None)
            suite.setdefault("post_green_command", None)
            suite.setdefault("env", {})

        cls._config = config
        return config

    @classmethod
    def get_suite_env(cls, suite: dict) -> dict[str, str]:
        """
        Build environment dict for running suite commands.

        Merges the current environment with suite-specific env vars.
        Suite vars override existing environment vars.
        """
        env = os.environ.copy()
        suite_env = suite.get("env", {})
        for key, value in suite_env.items():
            env[key] = str(value)
        return env

    @classmethod
    def validate_environments(cls) -> None:
        """
        Validate that all test suite commands can execute.
        If a suite fails validation and has a test_init command, run init and retry.
        Exits with error if any suite fails validation after init attempt.
        """
        Logger.info("Validating test suite environments...")

        for suite in cls._config["suites"]:
            name = suite["name"]
            command = suite["test_command"]
            init_cmd = suite.get("test_init")
            suite_env = cls.get_suite_env(suite)

            # Build validation command - use --version as a quick check
            if "{target}" in command:
                validation_cmd = command.replace(
                    "{target}", "--version 2>/dev/null || true"
                )
            else:
                validation_cmd = command

            # First attempt (test_init and test_command run in test_root)
            result = subprocess.run(
                validation_cmd,
                capture_output=True,
                text=True,
                shell=True,
                cwd=suite.get("test_root") or os.getcwd(),
                env=suite_env,
                timeout=60,
            )

            # If validation fails and we have an init command, try running it
            if result.returncode != 0 and init_cmd:
                Logger.warning(
                    f"Suite '{name}' validation failed, running test_init..."
                )
                Logger.info(f"  Init: {init_cmd}")

                # test_init runs in test_root
                init_result = subprocess.run(
                    init_cmd,
                    capture_output=True,
                    text=True,
                    shell=True,
                    cwd=suite.get("test_root") or os.getcwd(),
                    env=suite_env,
                )
                if init_result.stdout:
                    Logger.write(init_result.stdout)
                if init_result.stderr:
                    Logger.write(init_result.stderr)

                if init_result.returncode != 0:
                    Logger.error(f"Suite '{name}': test_init command failed")
                    sys.exit(1)

                # Retry validation
                result = subprocess.run(
                    validation_cmd,
                    capture_output=True,
                    text=True,
                    shell=True,
                    cwd=suite.get("test_root") or os.getcwd(),
                    env=suite_env,
                    timeout=60,
                )

                if result.returncode != 0:
                    Logger.error(f"Suite '{name}': still failing after test_init")
                    Logger.error(f"  Command: {validation_cmd}")
                    if result.stderr:
                        print(result.stderr)
                    if result.stdout:
                        print(result.stdout)
                    sys.exit(1)
            elif result.returncode != 0:
                Logger.error(
                    f"Suite '{name}': environment validation failed (no test_init command defined)"
                )
                Logger.error(f"  Command: {validation_cmd}")
                if result.stderr:
                    print(result.stderr)
                if result.stdout:
                    print(result.stdout)
                sys.exit(1)

            print(
                f"  {Logger.color('✓', Colors.BRIGHT_GREEN)} {Logger.color(name, Colors.MAGENTA)}: OK"
            )

        print()

    @classmethod
    def run_suite_setup(cls, suite: dict) -> None:
        """
        Run test_setup command for a suite (once before that suite's tests).
        Tracks completion to avoid re-running for multiple todos in same suite.
        Note: test_setup runs in PROJECT ROOT, not test_root.
        """
        name = suite["name"]

        # Skip if already set up
        if name in cls._setup_completed:
            return

        setup_cmd = suite.get("test_setup")
        if not setup_cmd:
            cls._setup_completed.add(name)
            return

        Logger.info(f"Running test_setup for suite '{name}': {setup_cmd}")

        # test_setup runs in PROJECT ROOT (not test_root)
        result = subprocess.run(
            setup_cmd,
            capture_output=True,
            text=True,
            shell=True,
            cwd=os.getcwd(),
            env=cls.get_suite_env(suite),
        )
        if result.stdout:
            Logger.write(result.stdout)
        if result.stderr:
            Logger.write(result.stderr)

        if result.returncode != 0:
            Logger.error(f"test_setup failed for suite '{name}'")
            sys.exit(1)

        cls._setup_completed.add(name)
        Logger.success(f"test_setup complete for suite '{name}'")


# Convenience aliases for backward compatibility
def load_config() -> dict:
    return ConfigManager.load()


def get_suite_env(suite: dict) -> dict[str, str]:
    return ConfigManager.get_suite_env(suite)


def validate_suite_environments() -> None:
    ConfigManager.validate_environments()


def run_suite_setup(suite: dict) -> None:
    ConfigManager.run_suite_setup(suite)


def test_suite_config(suite_name: str) -> int:
    return TestRunner.test_suite_config(suite_name)


class TodoManager:
    """Manages todo CRUD operations for todos.json."""

    _file_path: str = TODOS_FILE

    @classmethod
    def set_file_path(cls, path: str) -> None:
        """Set the path to the todos file."""
        cls._file_path = path

    @classmethod
    def load(cls) -> dict:
        """Load todos from todos.json."""
        if not Path(cls._file_path).exists():
            Logger.error(f"{cls._file_path} not found")
            sys.exit(1)

        with open(cls._file_path, "r") as f:
            return json.load(f)

    @classmethod
    def save(cls, data: dict) -> None:
        """Save todos back to todos.json."""
        with open(cls._file_path, "w") as f:
            json.dump(data, f, indent=2)

    @classmethod
    def clean_done(cls) -> int:
        """
        Remove completed todos from todos.json.

        Removes all todos where done_at_commit is not null.

        Returns:
            Exit code (0 for success)
        """
        if not Path(cls._file_path).exists():
            Logger.error(f"{cls._file_path} not found")
            return 1

        with open(cls._file_path, "r") as f:
            data = json.load(f)

        if "todos" not in data:
            Logger.error("todos.json must have a 'todos' array")
            return 1

        original_count = len(data["todos"])
        data["todos"] = [t for t in data["todos"] if t.get("done_at_commit") is None]
        removed_count = original_count - len(data["todos"])

        if removed_count == 0:
            Logger.info("No completed todos to remove")
            return 0

        with open(cls._file_path, "w") as f:
            json.dump(data, f, indent=2)

        Logger.success(
            f"Removed {removed_count} completed todo(s) from {cls._file_path}"
        )
        Logger.info(f"Remaining: {len(data['todos'])} pending todo(s)")
        return 0

    @classmethod
    def get_next(cls, data: dict) -> Optional[dict]:
        """Get the highest priority todo that hasn't been completed."""
        pending = [t for t in data["todos"] if t.get("done_at_commit") is None]
        if not pending:
            return None
        # Sort by priority descending (highest first)
        pending.sort(key=lambda x: x.get("priority", 0), reverse=True)
        return pending[0]


# Convenience aliases for backward compatibility
def load_todos() -> dict:
    return TodoManager.load()


def save_todos(data: dict) -> None:
    TodoManager.save(data)


def clean_done_todos() -> int:
    return TodoManager.clean_done()


def get_next_todo(data: dict) -> Optional[dict]:
    return TodoManager.get_next(data)


class SuiteManager:
    """Manages test suite detection, filtering, and path operations."""

    @classmethod
    def detect_from_path(cls, file_path: str) -> Optional[dict]:
        """
        Determine which suite a file belongs to based on its path.

        Args:
            file_path: Path to a file

        Returns:
            The matching suite dict, or None if no match
        """
        for suite in ConfigManager.get_suites():
            root = suite.get("test_root", "")
            # Normalize: ensure root ends with / for prefix matching (unless empty or ".")
            if root and root != "." and not root.endswith("/"):
                root = root + "/"
            # Check if file is under this root
            if root == "." or root == "" or file_path.startswith(root):
                return suite
        return None

    @classmethod
    def get_by_name(cls, name: str) -> Optional[dict]:
        """Get suite configuration by name."""
        for suite in ConfigManager.get_suites():
            if suite["name"] == name:
                return suite
        return None

    @classmethod
    def filter_test_files_all_suites(cls, files: list[str]) -> dict[str, list[str]]:
        """
        Filter files to test files and group by suite.

        Args:
            files: List of file paths to check

        Returns:
            Dict mapping suite name -> list of test files for that suite
        """
        suite_test_files: dict[str, list[str]] = {}

        for filepath in files:
            # First, determine which suite this file belongs to based on path
            suite = cls.detect_from_path(filepath)
            if not suite:
                continue

            # Check if it's a test file for that suite
            file_patterns = suite.get("file_patterns", [])
            if not file_patterns:
                continue

            filename = Path(filepath).name
            for pattern in file_patterns:
                if fnmatch.fnmatch(filename, pattern):
                    suite_name = suite["name"]
                    if suite_name not in suite_test_files:
                        suite_test_files[suite_name] = []
                    suite_test_files[suite_name].append(filepath)
                    break

        return suite_test_files

    # @classmethod
    # def filter_test_files(cls, files: list[str], suite: dict) -> list[str]:
    #     """
    #     Filter a list of files to only include test files matching suite's patterns.

    #     Args:
    #         files: List of file paths
    #         suite: The test suite configuration to use

    #     Returns:
    #         List of file paths matching test file patterns
    #     """
    #     file_patterns = suite.get("file_patterns", [])

    #     if not file_patterns:
    #         return []

    #     test_files = []
    #     for filepath in files:
    #         filename = Path(filepath).name
    #         for pattern in file_patterns:
    #             if fnmatch.fnmatch(filename, pattern):
    #                 test_files.append(filepath)
    #                 break

    #     return test_files

    @classmethod
    def get_all_disallowed_paths(cls) -> list[str]:
        """
        Get disallowed paths from ALL suites for multi-suite protection.

        Returns:
            Combined list of paths to protect from writes
        """
        all_paths = []
        for suite in ConfigManager.get_suites():
            all_paths.extend(cls.get_disallowed_paths(suite))
        return list(set(all_paths))  # Deduplicate

    @classmethod
    def get_disallowed_paths(cls, suite: dict) -> list[str]:
        """
        Get list of paths to disallow writing to, based on suite's disallow_write_globs.
        Expands globs to actual file paths that exist.

        Args:
            suite: The suite configuration dict
        """
        globs = suite.get("disallow_write_globs", [])
        paths = []

        for pattern in globs:
            # Use glob to find matching files
            if "**" in pattern or "*" in pattern:
                matched = list(Path(".").glob(pattern))
                paths.extend(str(p) for p in matched)
            else:
                # Literal path
                if Path(pattern).exists():
                    paths.append(pattern)

        return paths

    @classmethod
    def get_target_type_description(cls, suite: dict) -> str:
        """Get a human-readable description of what kind of test target to create."""
        target_type = suite["target_type"]
        language = suite["language"]
        framework = suite["framework"]

        descriptions = {
            "file": f"Create a test file using {language} and {framework}.",
            "package": f"Create tests in the appropriate package directory for {language}/{framework}.",
            "project": f"Add tests to the project's test directory following {framework} conventions.",
            "repo": f"Add tests following the repository's {framework} test structure.",
        }

        return descriptions.get(target_type, descriptions["file"])


# Convenience aliases for backward compatibility
def detect_suite_from_path(file_path: str) -> Optional[dict]:
    return SuiteManager.detect_from_path(file_path)


def get_suite_by_name(name: str) -> Optional[dict]:
    return SuiteManager.get_by_name(name)


def filter_test_files_all_suites(files: list[str]) -> dict[str, list[str]]:
    return SuiteManager.filter_test_files_all_suites(files)


class TestRunner:
    """Manages test execution across suites."""

    @classmethod
    def run_tests(cls, target: str, suite: dict) -> tuple[bool, str, str]:
        """
        Run tests on the specified target using the suite's test_command.

        Args:
            target: The test target (file, package, project, or repo path)
            suite: The test suite configuration to use

        Returns:
            Tuple of (passed, stdout, stderr)
        """
        Logger.info(
            f"Running tests: {Logger.color(target, Colors.WHITE, Colors.BOLD)} (suite: {suite['name']})"
        )

        command_template = suite["test_command"]

        # Strip test_root prefix from target by default (configurable via strip_root_from_target)
        strip_root = suite.get("strip_root_from_target", True)
        root = suite.get("test_root", "")

        transformed_target = target
        if strip_root and root and root != ".":
            # Normalize root to end with /
            normalized_root = root if root.endswith("/") else root + "/"
            if target.startswith(normalized_root):
                transformed_target = target[len(normalized_root) :]

        # Substitute {target} with the (possibly transformed) path
        if "{target}" in command_template:
            test_command = command_template.format(target=transformed_target)
        else:
            test_command = command_template

        # Use shell=True since commands may contain shell builtins (cd, source)
        # and operators (&&, ||, ;)
        # test_command runs in test_root
        result = subprocess.run(
            test_command,
            capture_output=True,
            text=True,
            shell=True,
            cwd=suite.get("test_root") or os.getcwd(),
            env=ConfigManager.get_suite_env(suite),
        )

        # Log test output
        if result.stdout:
            Logger.write(result.stdout)
        if result.stderr:
            Logger.write(result.stderr)

        passed = result.returncode == 0
        return passed, result.stdout, result.stderr

    @classmethod
    def run_for_all_affected_suites(
        cls, suite_test_files: dict[str, list[str]]
    ) -> tuple[bool, dict[str, tuple[bool, str, str]]]:
        """
        Run tests for all affected suites.

        Args:
            suite_test_files: Dict mapping suite name -> list of test files

        Returns:
            Tuple of (all_passed, results_by_suite)
            where results_by_suite maps suite_name:test_file -> (passed, stdout, stderr)
        """
        all_passed = True
        results: dict[str, tuple[bool, str, str]] = {}

        for suite_name, test_files in suite_test_files.items():
            suite = SuiteManager.get_by_name(suite_name)
            if not suite:
                Logger.warning(f"Suite '{suite_name}' not found, skipping")
                continue

            # Run setup for this suite
            ConfigManager.run_suite_setup(suite)

            # Run tests for each test file in this suite
            for test_file in test_files:
                passed, stdout, stderr = cls.run_tests(test_file, suite)
                results[f"{suite_name}:{test_file}"] = (passed, stdout, stderr)
                if not passed:
                    all_passed = False

        return all_passed, results

    @classmethod
    def run_post_green_commands(
        cls, suite_test_files: dict[str, list[str]]
    ) -> tuple[bool, dict[str, tuple[bool, str, str]]]:
        """
        Run post_green_command for all affected suites.

        Args:
            suite_test_files: Dict mapping suite name -> list of test files

        Returns:
            Tuple of (all_passed, results_by_suite)
            where results_by_suite maps suite_name -> (passed, stdout, stderr)
        """
        all_passed = True
        results: dict[str, tuple[bool, str, str]] = {}

        for suite_name in suite_test_files.keys():
            suite = SuiteManager.get_by_name(suite_name)
            if not suite:
                continue

            post_green_cmd = suite.get("post_green_command")
            if not post_green_cmd:
                continue

            Logger.info(
                f"Running post_green_command for suite '{suite_name}': {post_green_cmd}"
            )

            # post_green_command runs in PROJECT ROOT (not test_root)
            result = subprocess.run(
                post_green_cmd,
                capture_output=True,
                text=True,
                shell=True,
                cwd=os.getcwd(),
                env=ConfigManager.get_suite_env(suite),
            )

            passed = result.returncode == 0
            results[suite_name] = (passed, result.stdout, result.stderr)

            # Log output
            if result.stdout:
                Logger.write(result.stdout)
            if result.stderr:
                Logger.write(result.stderr)

            if passed:
                Logger.success(f"post_green_command passed for suite '{suite_name}'")
            else:
                Logger.error(f"post_green_command failed for suite '{suite_name}'")
                all_passed = False

        return all_passed, results

    @classmethod
    def find_recent_test_files(cls, since_mtime: float, suite: dict) -> list[str]:
        """
        Find test files modified after the given timestamp.

        Args:
            since_mtime: Unix timestamp; only return files modified after this
            suite: The test suite configuration to use

        Returns:
            List of test file paths modified since the timestamp
        """
        file_patterns = suite.get("file_patterns", [])

        if not file_patterns:
            return []

        test_files = []
        for pattern in file_patterns:
            glob_pattern = pattern if pattern.startswith("**/") else f"**/{pattern}"
            for path in Path(".").glob(glob_pattern):
                if path.stat().st_mtime > since_mtime:
                    test_files.append(str(path))

        return test_files

    @classmethod
    def test_suite_config(cls, suite_name: str) -> int:
        """
        Test a suite's configuration by running test_setup and a test command.

        Args:
            suite_name: Name of the suite to test

        Returns:
            Exit code (0 for success, 1 for failure)
        """
        suite = SuiteManager.get_by_name(suite_name)
        if not suite:
            Logger.error(f"Suite '{suite_name}' not found")
            Logger.info("Available suites:")
            for s in ConfigManager.get_suites():
                print(f"  • {s['name']}")
            return 1

        Logger.banner(f"Testing suite: {suite_name}")
        print()

        # Show configuration
        Logger.info("Configuration:")
        print(f"  test_root: {Logger.color(suite.get('test_root', '.'), Colors.CYAN)}")
        print(f"  test_command: {Logger.color(suite['test_command'], Colors.CYAN)}")
        print(
            f"  default_target: {Logger.color(suite.get('default_target', '.'), Colors.CYAN)}"
        )
        print(
            f"  strip_root_from_target: {Logger.color(str(suite.get('strip_root_from_target', True)), Colors.CYAN)}"
        )
        if suite.get("post_green_command"):
            print(
                f"  post_green_command: {Logger.color(suite['post_green_command'], Colors.CYAN)}"
            )
        print()

        # Run test_setup if configured (runs in PROJECT ROOT)
        setup_cmd = suite.get("test_setup")
        if setup_cmd:
            Logger.info(f"Running test_setup (in project root): {setup_cmd}")
            result = subprocess.run(
                setup_cmd,
                capture_output=True,
                text=True,
                shell=True,
                cwd=os.getcwd(),
                env=ConfigManager.get_suite_env(suite),
            )
            if result.stdout:
                Logger.write(result.stdout)
            if result.stderr:
                Logger.write(result.stderr)
            if result.returncode != 0:
                Logger.error("test_setup failed")
                return 1
            Logger.success("test_setup complete")
            print()

        # Build test command using same logic as run_tests()
        target = suite.get("default_target", ".")
        command_template = suite["test_command"]
        root = suite.get("test_root", "")
        strip_root = suite.get("strip_root_from_target", True)

        # Show the path transformation
        Logger.info("Path resolution:")
        print(f"  Original target: {Logger.color(target, Colors.CYAN)}")

        transformed_target = target
        if strip_root and root and root != ".":
            normalized_root = root if root.endswith("/") else root + "/"
            if target.startswith(normalized_root):
                transformed_target = target[len(normalized_root) :]
                print(
                    f"  After stripping '{normalized_root}': {Logger.color(transformed_target, Colors.CYAN)}"
                )
            else:
                print("  (target doesn't start with test_root, not stripped)")

        cwd = root or os.getcwd()
        print(f"  Working directory: {Logger.color(cwd, Colors.CYAN)}")

        # Build and show final command
        if "{target}" in command_template:
            test_command = command_template.format(target=transformed_target)
        else:
            test_command = command_template

        print(f"  Final command: {Logger.color(test_command, Colors.YELLOW)}")
        print()

        # Run the test command (runs in test_root)
        Logger.info("Running test_command...")
        result = subprocess.run(
            test_command,
            capture_output=True,
            text=True,
            shell=True,
            cwd=cwd,
            env=ConfigManager.get_suite_env(suite),
        )
        if result.stdout:
            Logger.write(result.stdout)
        if result.stderr:
            Logger.write(result.stderr)

        print()
        if result.returncode == 0:
            Logger.success(f"Suite '{suite_name}' configuration is valid")
            return 0
        else:
            Logger.error(f"test_command failed with exit code {result.returncode}")
            return 1


# Convenience aliases for backward compatibility
def run_tests_for_all_affected_suites(
    suite_test_files: dict[str, list[str]],
) -> tuple[bool, dict[str, tuple[bool, str, str]]]:
    return TestRunner.run_for_all_affected_suites(suite_test_files)


def get_all_disallowed_paths() -> list[str]:
    return SuiteManager.get_all_disallowed_paths()


def get_disallowed_paths(suite: dict) -> list[str]:
    return SuiteManager.get_disallowed_paths(suite)


def run_claude_code(
    prompt: str, disallow_paths: list[str] | None = None
) -> tuple[int, str, str]:
    """
    Run claude code with the given prompt.

    Args:
        prompt: The prompt to send to claude code
        disallow_paths: Paths to block write access to

    Returns:
        Tuple of (return_code, stdout, stderr)
    """
    # Pass prompt via stdin using "-p -" to avoid ARG_MAX limit on large prompts
    # --verbose shows real-time tool calls and agent activity
    cmd = ["claude", "-p", "-", "--permission-mode", "acceptEdits", "--verbose"]

    # Add disallowed paths if any
    for path in disallow_paths or []:
        cmd.extend(
            ["--disallowedTools", f"Edit:{path}", "--disallowedTools", f"Write:{path}"]
        )

    print_info("Invoking Claude Code...")
    log_prompt(prompt)
    print_claude_start()

    # Stream output to log file while capturing for parsing
    process = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,  # Merge stderr into stdout
        text=True,
        cwd=os.getcwd(),
    )

    # Write prompt to stdin and close to signal EOF
    process.stdin.write(prompt)
    process.stdin.close()

    stdout_lines = []
    for line in process.stdout:
        log_write(line)  # Write to log file only
        stdout_lines.append(line)

    process.wait()
    print_claude_end()
    return process.returncode, "".join(stdout_lines), ""


# def run_tests(target: str, suite: dict) -> tuple[bool, str, str]:
#     return TestRunner.run_tests(target, suite)


class GitOperations:
    """Collection of git-related operations."""

    @staticmethod
    def commit_and_tag(message: str) -> str:
        """
        Commit changes and create a tag (does not push).

        Returns:
            The commit hash

        Raises:
            subprocess.CalledProcessError if commit fails
        """
        # Stage all changes
        subprocess.run(["git", "add", "-A"], check=True, capture_output=True)

        # Commit
        subprocess.run(
            ["git", "commit", "-m", message], check=True, capture_output=True
        )

        # Get commit hash
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"], capture_output=True, text=True, check=True
        )
        commit_hash = result.stdout.strip()

        # Create tag with timestamp
        tag_name = f"chief-{datetime.now().strftime('%Y%m%d-%H%M%S')}"
        subprocess.run(["git", "tag", tag_name], check=True, capture_output=True)

        print_success(f"Committed: {commit_hash[:8]} (tag: {tag_name})")
        return commit_hash

    @staticmethod
    def push_with_tags() -> bool:
        """
        Push commits and tags to remote.

        Returns:
            True if push succeeded, False if failed (non-fatal)
        """
        if not AUTOPUSH:
            print_info("Auto-push disabled, skipping push")
            return True

        try:
            subprocess.run(["git", "push"], check=True, capture_output=True)
            subprocess.run(["git", "push", "--tags"], check=True, capture_output=True)
            print_success("Pushed to remote")
            return True
        except subprocess.CalledProcessError as e:
            print_warning(f"Push failed (commit is saved locally): {e}")
            return False

    @staticmethod
    def commit_todos(todo_text: str) -> None:
        """Commit todos.json update after marking a todo as done."""
        subprocess.run(["git", "add", "todos.json"], check=True, capture_output=True)
        subprocess.run(
            ["git", "commit", "-m", f"chief: mark done - {todo_text[:50]}"],
            check=True,
            capture_output=True,
        )
        # Push is non-fatal for todos commit
        GitOperations.push_with_tags()

    @staticmethod
    def get_dirty_files() -> set[str]:
        """Get set of currently modified, staged, or untracked files."""
        result = subprocess.run(
            ["git", "status", "--porcelain"], capture_output=True, text=True
        )
        files = set()
        for line in result.stdout.rstrip("\n").split("\n"):
            if line:
                # Format is "XY filename" or "XY filename -> newname" for renames
                parts = line[3:].split(" -> ")
                files.add(parts[-1])  # Use the destination name for renames
        return files

    @staticmethod
    def revert_changes(baseline_files: set[str] | None = None) -> None:
        """Revert uncommitted changes made since baseline (or all except todos.json if no baseline)."""
        print_warning("Reverting uncommitted changes...")

        if baseline_files is None:
            # Revert everything except todos.json (legacy behavior)
            subprocess.run(
                ["git", "checkout", "--", ".", ":!todos.json"], capture_output=True
            )
            subprocess.run(
                ["git", "clean", "-fd", "--exclude=todos.json"], capture_output=True
            )
            return

        # Only revert files that weren't dirty before
        current_files = GitOperations.get_dirty_files()
        files_to_revert = current_files - baseline_files - {"todos.json"}

        if not files_to_revert:
            print_info("No new changes to revert")
            return

        # Separate tracked (checkout) vs untracked (clean) files
        result = subprocess.run(
            ["git", "ls-files", "--others", "--exclude-standard"],
            capture_output=True,
            text=True,
        )
        untracked = (
            set(result.stdout.strip().split("\n")) if result.stdout.strip() else set()
        )

        tracked_to_revert = [f for f in files_to_revert if f not in untracked]
        untracked_to_revert = [f for f in files_to_revert if f in untracked]

        if tracked_to_revert:
            subprocess.run(
                ["git", "checkout", "--"] + tracked_to_revert, capture_output=True
            )

        for f in untracked_to_revert:
            try:
                Path(f).unlink(missing_ok=True)
            except (OSError, IsADirectoryError):
                subprocess.run(["rm", "-rf", f], capture_output=True)

    @staticmethod
    def get_status_snapshot() -> dict[str, str]:
        """
        Capture current git status as a snapshot of {filepath: status_code}.

        Uses git status --porcelain=v1 for stable parseable output.

        Returns:
            Dict mapping file paths to their git status codes (e.g., 'M ', '??', 'A ')
        """
        result = subprocess.run(
            ["git", "status", "--porcelain=v1"],
            capture_output=True,
            text=True,
            cwd=os.getcwd(),
        )
        snapshot = {}
        for line in result.stdout.rstrip("\n").split("\n"):
            if not line:
                continue
            status = line[:2]
            filepath = line[3:].strip()
            if " -> " in filepath:
                filepath = filepath.split(" -> ")[1]
            snapshot[filepath] = status
        return snapshot

    @staticmethod
    def detect_changed_files(baseline_snapshot: dict[str, str]) -> list[str]:
        """
        Detect files that have changed since the baseline snapshot.

        A file is considered changed if:
        - It's new in git status (wasn't in the baseline snapshot)
        - OR its status code changed (e.g., from clean to modified)

        Args:
            baseline_snapshot: Dict from get_status_snapshot() captured before changes

        Returns:
            List of file paths that changed since the baseline
        """
        current_snapshot = GitOperations.get_status_snapshot()
        changed_files = []

        for filepath, status in current_snapshot.items():
            # File is new to git status OR has different status than before
            if (
                filepath not in baseline_snapshot
                or baseline_snapshot[filepath] != status
            ):
                if Path(filepath).exists():
                    changed_files.append(filepath)

        return changed_files


# def find_recent_test_files(since_mtime: float, suite: dict) -> list[str]:
#     return TestRunner.find_recent_test_files(since_mtime, suite)


# def filter_test_files(files: list[str], suite: dict) -> list[str]:
#     return SuiteManager.filter_test_files(files, suite)


def get_file_hashes(files: list[str]) -> dict[str, Optional[str]]:
    """
    Get MD5 hashes of files for change detection.

    Args:
        files: List of file paths to hash

    Returns:
        Dict mapping filepath -> hash (or None if file doesn't exist)
    """
    hashes = {}
    for filepath in files:
        if Path(filepath).is_file():
            with open(filepath, "rb") as f:
                hashes[filepath] = hashlib.md5(f.read()).hexdigest()
        else:
            hashes[filepath] = None
    return hashes


def read_test_file_contents(files: list[str]) -> str:
    """
    Read contents of test files for inclusion in refinement prompt.

    Args:
        files: List of test file paths to read

    Returns:
        Combined contents of all test files with headers
    """
    contents = []
    for filepath in files:
        if Path(filepath).is_file():
            with open(filepath, "r") as f:
                contents.append(f"--- {filepath} ---\n{f.read()}")
    return "\n\n".join(contents)


def get_target_type_description(suite: dict) -> str:
    return SuiteManager.get_target_type_description(suite)


# ============================================================================
# Stability Loop Abstraction
# ============================================================================


@dataclass
class StabilityResult:
    """Result from checking one iteration of a stability loop."""

    is_stable: bool  # Does this iteration meet the stability criterion?
    should_fail: bool  # Early exit with failure? (e.g., NO response)
    value: Any  # Extracted value from this iteration


class StabilityLoop:
    """Manages stability and retry loops for Claude interactions."""

    @classmethod
    def run(
        cls,
        prompt_builder: Callable[[int], str],
        stability_checker: Callable[[int, str, Any], StabilityResult],
        max_iterations: int = MAX_FIX_ATTEMPTS,
        stability_threshold: int = STABILITY_ITERATIONS,
        before_call: Callable[[int], Any] | None = None,
        phase_name: str = "STABILITY",
    ) -> tuple[bool, Any]:
        """
        Generic stability loop - calls Claude until output/side-effects stabilize.

        This is the core abstraction for:
        - "Stability loops": Wait for consistent output (same response N times)
        - "Refinement loops": Wait for side effects to stop (no file changes N times)

        Args:
            prompt_builder: (iteration) -> prompt string
            stability_checker: (iteration, stdout, pre_state) -> StabilityResult
            max_iterations: Maximum iterations before giving up
            stability_threshold: Consecutive stable iterations required
            before_call: Optional (iteration) -> pre_state, called before each Claude call
                         (used for capturing file hashes before Claude modifies files)
            phase_name: Name for logging

        Returns:
            (success, final_value) - success is True if stability was reached
        """
        stable_count = 0
        last_value: Any = None

        for i in range(1, max_iterations + 1):
            # Optional pre-call hook (e.g., capture file hashes)
            pre_state = before_call(i) if before_call else None

            prompt = prompt_builder(i)
            returncode, stdout, stderr = run_claude_code(prompt)

            if returncode != 0:
                Logger.warning(f"Claude error in {phase_name}: {stderr}", indent=1)
                stable_count = 0
                continue

            result = stability_checker(i, stdout, pre_state)

            if result.should_fail:
                return (False, result.value)

            if result.is_stable:
                stable_count += 1
                Logger.info(
                    f"{phase_name}: stable ({stable_count}/{stability_threshold})",
                    indent=1,
                )
                if stable_count >= stability_threshold:
                    Logger.success(f"{phase_name}: stabilized")
                    return (True, result.value)
            else:
                stable_count = 0
                Logger.info(f"{phase_name}: not stable yet", indent=1)

            last_value = result.value

        Logger.warning(
            f"{phase_name}: did not stabilize after {max_iterations} iterations"
        )
        return (False, last_value)

    @classmethod
    def run_with_retry(cls, max_retries: int = 10, tail_lines: int = 150) -> int:
        """
        Run chief.py in a retry loop until failures stabilize or succeeds.

        Uses Claude to semantically compare failure outputs.
        Stops when:
        - Exit code is 0 (success)
        - Claude says two consecutive failures are for the same reason
        - Max retries reached
        """
        # Build command: same script with --no-retry to avoid infinite recursion
        cmd = (
            [sys.executable, __file__]
            + [arg for arg in sys.argv[1:] if arg != "--no-retry"]
            + ["--no-retry"]
        )

        last_tail: str | None = None

        for attempt in range(1, max_retries + 1):
            print(f"\n{'='*60}")
            print(f"RETRY WRAPPER: Attempt {attempt}/{max_retries}")
            print(f"{'='*60}\n")

            result = subprocess.run(cmd, capture_output=True, text=True)

            # Print output to console
            if result.stdout:
                print(result.stdout, end="")
            if result.stderr:
                print(result.stderr, end="", file=sys.stderr)

            # Success - done
            if result.returncode == 0:
                return 0

            # Get last N lines for comparison
            lines = result.stdout.strip().split("\n")
            current_tail = "\n".join(lines[-tail_lines:])

            # Check if same failure reason as last run
            if last_tail is not None:
                print("\n[RETRY WRAPPER] Asking Claude if failures are same reason...")
                if failures_same_reason(last_tail, current_tail):
                    print("[RETRY WRAPPER] Same failure reason detected - stopping")
                    return result.returncode
                print("[RETRY WRAPPER] Different failure reason - will retry...")

            last_tail = current_tail

        print(f"\n[RETRY WRAPPER] Max retries ({max_retries}) reached")
        return 1


# Convenience aliases for backward compatibility
def run_stability_loop(
    prompt_builder: Callable[[int], str],
    stability_checker: Callable[[int, str, Any], StabilityResult],
    max_iterations: int = MAX_FIX_ATTEMPTS,
    stability_threshold: int = STABILITY_ITERATIONS,
    before_call: Callable[[int], Any] | None = None,
    phase_name: str = "STABILITY",
) -> tuple[bool, Any]:
    return StabilityLoop.run(
        prompt_builder,
        stability_checker,
        max_iterations,
        stability_threshold,
        before_call,
        phase_name,
    )


# ============================================================================
# ContextWindow - Structured input to Claude prompts
# ============================================================================


@dataclass
class Context:
    """
    Represents what we're sending to Claude's context window.
    Think of it as an array of sections we allocate to.
    """

    task: str  # The todo text (always present)
    expectations: str | None = None  # PM expectations (optional)
    suite_info: str | None = None  # Available test suites
    test_files: list[str] | None = None  # Test file paths
    test_locations_str: str | None = None  # Formatted test locations
    failure_output: str | None = None  # Test/build failure details
    retry_context: str | None = None  # Previous attempt info

    def expectations_section(self) -> str:
        """Format expectations for inclusion in prompt."""
        if self.expectations:
            return f"\n\nExpected outcome:\n{self.expectations}"
        return ""

    def build_prompt(self, prompt_type: PromptType) -> str:
        """Build prompt from template + context fields."""
        template = PROMPT_TEMPLATES[prompt_type]
        return template.format(
            task=self.task,
            expectations_section=self.expectations_section(),
            suite_info=self.suite_info or "",
            test_locations_str=self.test_locations_str or "",
            failure_output=self.failure_output or "",
            retry_context=self.retry_context or "",
            file_list="\n".join(f"- {f}" for f in (self.test_files or [])),
        )


# ============================================================================
# TodoProcessor - Orchestrates TDD cycle for a single todo
# ============================================================================


class TodoProcessor:
    """
    Orchestrates the TDD cycle for a single todo item.

    This class provides the ONE PLACE where the core TDD logic is crystal clear:
    1. RED phase: Write failing tests
    2. GREEN phase: Implement until tests pass
    3. BUILD phase: Post-green validation
    4. Commit on success
    """

    def __init__(self, todo: dict, data: dict):
        self.todo = todo
        self.data = data
        self.todo_text = todo.get("todo", "")
        self.testable = todo.get("testable", True)

        # State populated during phases
        self.suite_test_files: dict[str, list[str]] = {}  # suite -> files
        self.all_test_artifacts: list[str] = []  # all test files
        self.test_results: dict[str, tuple[bool, str, str]] = {}
        self.build_results: dict[str, tuple[bool, str, str]] = {}
        self.baseline_files: set[str] = set()  # for revert
        self.success = False

    def process(self) -> bool:
        """
        Main TDD cycle - the ONE PLACE where core logic is clear.

        Returns:
            True if todo was completed successfully, False otherwise
        """
        self._print_banner()

        # Non-testable tasks use different flow
        if not self.testable:
            return self._process_no_tests()

        # === RED PHASE: Write failing tests ===
        if not self._run_red_phase():
            return False

        # === GREEN + BUILD PHASES with retry ===
        for attempt in range(1, MAX_IMPLEMENTATION_ATTEMPTS + 1):
            self.baseline_files = GitOperations.get_dirty_files()

            if self._run_green_phase(attempt):
                if self._run_build_phase():
                    self.success = True
                    break

            self._revert_if_not_final(attempt)

        if not self.success:
            print_error(
                f"Failed to complete todo after {MAX_IMPLEMENTATION_ATTEMPTS} attempts"
            )
            return False

        # === COMMIT ===
        return self._commit()

    # -------------------------------------------------------------------------
    # Phase Methods
    # -------------------------------------------------------------------------

    def _run_red_phase(self) -> bool:
        """Generate failing tests with stability + refinement loops."""
        print_phase("RED", "Writing failing tests...")

        self.suite_test_files, self.all_test_artifacts = write_test_for_todo(self.todo)

        if not self.suite_test_files:
            print_error("Failed to create tests, skipping todo")
            return False

        # Run setup for each affected suite
        for suite_name in self.suite_test_files.keys():
            suite = get_suite_by_name(suite_name)
            if suite:
                run_suite_setup(suite)

        # Verify tests fail (as expected for Red phase)
        all_passed, self.test_results = run_tests_for_all_affected_suites(
            self.suite_test_files
        )
        if all_passed:
            print_warning("All tests passed before implementation (expected to fail)")
            print_info("Proceeding to GREEN phase anyway")
        else:
            print_success("Tests fail as expected (Red phase complete)")

        return True

    def _run_green_phase(self, attempt: int) -> bool:
        """Implement code and run fix loop if tests fail."""
        print_phase(
            "GREEN", f"Implementation attempt {attempt}/{MAX_IMPLEMENTATION_ATTEMPTS}"
        )

        success, _, _ = implement_todo(
            self.todo, self.suite_test_files, self.all_test_artifacts
        )

        if not success:
            print_error("Claude Code returned error during implementation")
            return False

        # Run tests for all affected suites
        all_passed, self.test_results = run_tests_for_all_affected_suites(
            self.suite_test_files
        )

        if not all_passed:
            # Tests failed, enter test fix loop
            print_warning("Tests failed, entering fix loop...")
            all_passed = self._run_test_fix_loop()

        if all_passed:
            print_success("All tests passed!")

        return all_passed

    def _run_test_fix_loop(self) -> bool:
        """Fix failing tests until pass or attempts exhausted."""
        for fix_iter in range(1, MAX_FIX_ATTEMPTS + 1):
            print_phase("FIX", f"Test fix attempt {fix_iter}/{MAX_FIX_ATTEMPTS}")

            success, _, _ = fix_failing_tests(
                self.todo,
                self.suite_test_files,
                self.all_test_artifacts,
                self.test_results,
            )

            if not success:
                print_error("Claude Code returned error during fix", indent=1)
                continue

            all_passed, self.test_results = run_tests_for_all_affected_suites(
                self.suite_test_files
            )

            if all_passed:
                print_success("All tests passed after fix!")
                return True

        print_warning("Test fix loop exhausted")
        return False

    def _run_build_phase(self) -> bool:
        """Run post-green commands with build-fix loop."""
        print_phase("BUILD", "Running post_green_commands...")
        build_passed, self.build_results = run_post_green_commands(
            self.suite_test_files
        )

        if build_passed:
            return True

        # Build failed, enter build fix loop
        print_warning("post_green_command failed, entering build fix loop...")

        for build_fix_iter in range(1, MAX_FIX_ATTEMPTS + 1):
            print_phase(
                "BUILD-FIX", f"Build fix attempt {build_fix_iter}/{MAX_FIX_ATTEMPTS}"
            )

            success, _, _ = fix_failing_build(
                self.todo,
                self.suite_test_files,
                self.all_test_artifacts,
                self.build_results,
            )

            if not success:
                print_error("Claude Code returned error during build fix", indent=1)
                continue

            # Re-run tests first (fix might have broken them)
            print_info("Re-running tests after build fix...")
            all_passed, self.test_results = run_tests_for_all_affected_suites(
                self.suite_test_files
            )

            if not all_passed:
                print_warning("Tests failed after build fix, need to fix tests too")
                all_passed = self._run_test_fix_loop_during_build()
                if not all_passed:
                    continue

            # Tests pass, now check post_green again
            print_info("Re-running post_green_commands...")
            build_passed, self.build_results = run_post_green_commands(
                self.suite_test_files
            )

            if build_passed:
                print_success("All tests and post_green_commands passed!")
                return True

        print_warning("Build fix loop exhausted")
        return False

    def _run_test_fix_loop_during_build(self) -> bool:
        """Mini test fix loop during build fix phase."""
        for test_fix_iter in range(1, MAX_FIX_ATTEMPTS + 1):
            print_phase(
                "FIX",
                f"Test fix attempt {test_fix_iter}/{MAX_FIX_ATTEMPTS} (during build fix)",
            )

            success, _, _ = fix_failing_tests(
                self.todo,
                self.suite_test_files,
                self.all_test_artifacts,
                self.test_results,
            )

            if not success:
                print_error("Claude Code returned error during test fix", indent=1)
                continue

            all_passed, self.test_results = run_tests_for_all_affected_suites(
                self.suite_test_files
            )

            if all_passed:
                return True

        print_warning("Could not fix tests during build fix loop")
        return False

    # -------------------------------------------------------------------------
    # Helper Methods
    # -------------------------------------------------------------------------

    def _print_banner(self) -> None:
        """Print the todo banner."""
        print()
        print_banner(
            f"TODO: {self.todo_text[:50]}{'...' if len(self.todo_text) > 50 else ''}"
        )
        print_info(f"Full task: {self.todo_text}")
        print_info(
            f"Priority: {color(str(self.todo.get('priority', 0)), Colors.YELLOW, Colors.BOLD)}"
        )

    def _revert_if_not_final(self, attempt: int) -> None:
        """Revert changes unless this was the final attempt."""
        if attempt < MAX_IMPLEMENTATION_ATTEMPTS:
            print_warning("Reverting changes...")
            GitOperations.revert_changes(self.baseline_files)
        else:
            print_info("Keeping changes for inspection (final attempt)")

    def _commit(self) -> bool:
        """Git commit and tag on success."""
        try:
            commit_hash = GitOperations.commit_and_tag(f"chief: {self.todo_text}")
            GitOperations.push_with_tags()  # Non-fatal
            self.todo["done_at_commit"] = commit_hash
            save_todos(self.data)
            GitOperations.commit_todos(self.todo_text)
            return True
        except subprocess.CalledProcessError as e:
            print_error(f"Git commit failed: {e}")
            GitOperations.revert_changes(self.baseline_files)
            return False

    def _process_no_tests(self) -> bool:
        """
        Process a non-testable todo using semantic verification.

        Instead of TDD, this:
        1. Has Claude implement the task
        2. Verifies completion via semantic review with stability check
        """
        # Outer retry loop
        for attempt in range(1, MAX_IMPLEMENTATION_ATTEMPTS + 1):
            print_phase(
                "GREEN",
                f"Implementation attempt {attempt}/{MAX_IMPLEMENTATION_ATTEMPTS}",
            )

            # Snapshot dirty files before implementation
            baseline_dirty = GitOperations.get_dirty_files()

            # Step A: Implementation (with retry message on attempt 2+)
            success, _, _ = implement_todo_no_tests(self.todo, is_retry=(attempt > 1))

            if not success:
                print_error("Claude Code returned error during implementation")
                if attempt < MAX_IMPLEMENTATION_ATTEMPTS:
                    GitOperations.revert_changes(baseline_dirty)
                else:
                    print_info("Keeping changes for inspection (final attempt)")
                continue

            # Step B: Verification
            print_phase("VERIFY", f"Semantic verification (attempt {attempt})")

            # Step C: Decision
            if verify_completion_stable(self.todo):
                # Verified complete - commit and mark done
                try:
                    commit_hash = GitOperations.commit_and_tag(
                        f"chief: {self.todo_text}"
                    )
                    GitOperations.push_with_tags()  # Non-fatal
                    self.todo["done_at_commit"] = commit_hash
                    save_todos(self.data)
                    GitOperations.commit_todos(self.todo_text)
                    return True
                except subprocess.CalledProcessError as e:
                    print_error(f"Git operation failed: {e}")
                    GitOperations.revert_changes(baseline_dirty)
                    continue
            else:
                # Verification failed
                print_warning(
                    "Semantic verification failed, will retry implementation..."
                )
                if attempt < MAX_IMPLEMENTATION_ATTEMPTS:
                    GitOperations.revert_changes(baseline_dirty)
                else:
                    print_info("Keeping changes for inspection (final attempt)")

        print_error(
            f"Failed to complete todo after {MAX_IMPLEMENTATION_ATTEMPTS} attempts"
        )
        return False


def write_test_for_todo(todo: dict) -> tuple[dict[str, list[str]], list[str]]:
    """
    Run claude code to write failing tests for the todo.
    Claude can write tests for any suite(s) - suites are detected from git changes.

    Args:
        todo: The todo item

    Returns:
        Tuple of (suite_test_files, all_test_artifacts) where:
        - suite_test_files: Dict mapping suite name -> list of test files
        - all_test_artifacts: List of all test files created (for locking)
    """
    # Build suite info section listing all available suites
    suite_info_lines = []
    for suite in ConfigManager.get_suites():
        patterns = suite.get("file_patterns", [])
        patterns_str = ", ".join(patterns) if patterns else "none"
        suite_info_lines.append(
            f"- {suite['name']}: {suite['language']}/{suite['framework']} "
            f"(test_root: {suite['test_root']}, test patterns: {patterns_str})"
        )
    suite_info = "\n".join(suite_info_lines)

    context = Context(
        task=todo.get("todo", ""),
        expectations=todo.get("expectations", ""),
        suite_info=suite_info,
    )
    prompt = context.build_prompt(PromptType.RED_WRITE_TESTS)

    # Capture git baseline before RED phase
    baseline_snapshot = GitOperations.get_status_snapshot()

    returncode, stdout, stderr = run_claude_code(prompt)

    if returncode != 0:
        print_error(f"Claude Code failed during test creation: {stderr}")
        return {}, []

    # Check if Claude found existing tests (before checking git changes)
    existing_tests = extract_existing_tests(stdout)
    verified_existing = False
    if existing_tests:
        print_info("Claude reports tests already exist, verifying...")
        stable_tests = verify_existing_tests_stable(todo, existing_tests, suite_info)
        if stable_tests:
            # Map existing test files to their suites
            suite_test_files = filter_test_files_all_suites(stable_tests)
            if suite_test_files:
                for suite_name, files in suite_test_files.items():
                    print_info(
                        f"Suite '{color(suite_name, Colors.MAGENTA)}': {', '.join(files)}"
                    )
                all_test_artifacts = stable_tests
                verified_existing = True
                # Fall through to refinement loop (don't return early)
            else:
                print_warning("Existing test files don't match any suite patterns")
        # If stability failed or no suite match, fall through to normal git-based detection

    if not verified_existing:
        # Detect new/modified files via git and map to suites
        changed_files = GitOperations.detect_changed_files(baseline_snapshot)
        suite_test_files = filter_test_files_all_suites(changed_files)

        # Flatten all test artifacts for locking
        all_test_artifacts = []
        for files in suite_test_files.values():
            all_test_artifacts.extend(files)

        if not suite_test_files:
            print_warning("No test files detected after RED phase")
            return {}, []

        # Report which suites were affected
        for suite_name, files in suite_test_files.items():
            print_info(
                f"Suite '{color(suite_name, Colors.MAGENTA)}': {', '.join(files)}"
            )

    # --- REFINEMENT LOOP (using stability loop abstraction) ---
    files_to_monitor = list(all_test_artifacts)

    if not files_to_monitor:
        print_warning("No test files to refine, skipping refinement loop")
        return suite_test_files, all_test_artifacts

    # Build context for refinement prompts
    refinement_context = Context(
        task=todo.get("todo", ""),
        expectations=todo.get("expectations", ""),
        test_files=files_to_monitor,
    )
    refinement_prompt = refinement_context.build_prompt(PromptType.RED_REFINE_TESTS)

    def refinement_prompt_builder(iteration: int) -> str:
        print_phase(
            "REFINE",
            f"Test refinement iteration {iteration}/{MAX_TEST_REFINEMENT_ITERATIONS}",
        )
        print_info(
            f"Monitoring {len(files_to_monitor)} file(s) for changes: {', '.join(files_to_monitor)}"
        )
        return refinement_prompt

    def refinement_before_call(iteration: int) -> dict[str, str | None]:
        return get_file_hashes(files_to_monitor)

    def refinement_stability_checker(
        iteration: int, stdout: str, hashes_before: dict[str, str | None]
    ) -> StabilityResult:
        hashes_after = get_file_hashes(files_to_monitor)
        is_stable = hashes_before == hashes_after
        if not is_stable:
            changed_files = [
                f
                for f in files_to_monitor
                if hashes_before.get(f) != hashes_after.get(f)
            ]
            print_warning(
                f"Test files modified: {', '.join(changed_files)} — will refine again",
                indent=1,
            )
        return StabilityResult(
            is_stable=is_stable, should_fail=False, value=hashes_after
        )

    run_stability_loop(
        prompt_builder=refinement_prompt_builder,
        stability_checker=refinement_stability_checker,
        before_call=refinement_before_call,
        max_iterations=MAX_TEST_REFINEMENT_ITERATIONS,
        phase_name="REFINE",
    )
    # --- END REFINEMENT LOOP ---

    return suite_test_files, all_test_artifacts


def extract_test_target(output: str) -> Optional[str]:
    """
    Extract test target from claude code output.
    Looks for the structured "TEST_TARGET: <path>" format.
    """
    for line in output.split("\n"):
        line = line.strip()
        if line.startswith("TEST_TARGET:"):
            target = line[len("TEST_TARGET:") :].strip()
            # Remove any backticks or quotes
            target = target.strip("`\"'")
            if target:
                return target
    return None


def extract_existing_tests(output: str) -> list[str]:
    """
    Extract existing test files from claude code output.
    Looks for "TESTS_ALREADY_EXIST: path1, path2, ..." format.
    """
    for line in output.split("\n"):
        line = line.strip()
        if line.startswith("TESTS_ALREADY_EXIST:"):
            files_str = line[len("TESTS_ALREADY_EXIST:") :].strip()
            files = [f.strip().strip("`\"'") for f in files_str.split(",")]
            return [f for f in files if f]
    return []


def verify_existing_tests_stable(
    todo: dict, initial_tests: list[str], suite_info: str
) -> list[str]:
    """
    Verify TESTS_ALREADY_EXIST answer is stable using intersection-based approach.

    Collects multiple responses and finds the common intersection (files that appear
    in ALL responses). Accepts when the intersection is non-empty and stable for
    STABILITY_ITERATIONS consecutive responses. This handles Claude's natural variance
    in finding related test files while ensuring core files are consistently identified.

    Args:
        todo: The todo item
        initial_tests: Initial list of test paths from first response
        suite_info: Formatted string of available test suites

    Returns:
        List of confirmed test paths (intersection), or empty list if unstable
    """
    context = Context(
        task=todo.get("todo", ""),
        expectations=todo.get("expectations", ""),
        suite_info=suite_info,
    )
    prompt = context.build_prompt(PromptType.RED_VERIFY_EXISTING)

    # Track all responses as sets for intersection computation
    all_responses: list[set[str]] = [set(initial_tests)]
    prev_intersection: set[str] | None = None

    def intersection_prompt_builder(iteration: int) -> str:
        return prompt

    def intersection_stability_checker(
        iteration: int, stdout: str, pre_state: Any
    ) -> StabilityResult:
        nonlocal prev_intersection

        current_tests = extract_existing_tests(stdout)

        if not current_tests:
            print_warning(
                "Claude did not confirm existing tests, will write new tests", indent=1
            )
            return StabilityResult(is_stable=False, should_fail=True, value=[])

        # Add this response to our collection
        all_responses.append(set(current_tests))

        # Compute intersection of ALL responses so far
        current_intersection = set.intersection(*all_responses)

        print_info(f"Existing tests: {', '.join(sorted(current_tests))}", indent=1)

        # Check if intersection is empty - no common files across all responses
        if not current_intersection:
            print_warning(
                "No common tests across responses, will write new tests", indent=1
            )
            return StabilityResult(is_stable=False, should_fail=True, value=[])

        # Check if intersection is stable (same as previous iteration)
        is_stable = current_intersection == prev_intersection
        if not is_stable:
            print_info(
                f"Intersection: {', '.join(sorted(current_intersection))}", indent=1
            )
        prev_intersection = current_intersection
        return StabilityResult(
            is_stable=is_stable, should_fail=False, value=sorted(current_intersection)
        )

    success, result = run_stability_loop(
        prompt_builder=intersection_prompt_builder,
        stability_checker=intersection_stability_checker,
        max_iterations=STABILITY_ITERATIONS + 1,
        phase_name="VERIFY_EXISTING",
    )

    if success and result:
        print_success(f"Existing tests confirmed: {', '.join(result)}")
        return result
    return []


def implement_todo_no_tests(
    todo: dict, is_retry: bool = False
) -> tuple[bool, str, str]:
    """
    Run claude code to implement a non-testable todo.

    Args:
        todo: The todo item
        is_retry: Whether this is a retry after previous verification failed

    Returns:
        Tuple of (success, stdout, stderr)
    """
    retry_context = ""
    if is_retry:
        retry_context = (
            "\n\nPrevious verification failed. Please fix outstanding issues."
        )

    context = Context(
        task=todo.get("todo", ""),
        expectations=todo.get("expectations", ""),
        retry_context=retry_context,
    )
    prompt = context.build_prompt(PromptType.GREEN_IMPLEMENT_NO_TESTS)

    returncode, stdout, stderr = run_claude_code(prompt)

    return returncode == 0, stdout, stderr


def verify_completion_stable(todo: dict) -> bool:
    """
    Verify task completion using semantic review with stability check.

    Prompts Claude to review whether the task is fully completed.
    Requires STABILITY_ITERATIONS consecutive YES responses to return True.
    Returns False immediately on any NO response.

    Args:
        todo: The todo item containing 'todo' and 'expectations' fields

    Returns:
        True if task verified complete (stable YES), False otherwise
    """
    context = Context(
        task=todo.get("todo", ""),
        expectations=todo.get("expectations", ""),
    )
    prompt = context.build_prompt(PromptType.VERIFY_COMPLETION)

    def completion_prompt_builder(iteration: int) -> str:
        print_info(f"Verification attempt {iteration}/{MAX_FIX_ATTEMPTS}...")
        return prompt

    def completion_stability_checker(
        iteration: int, stdout: str, pre_state: Any
    ) -> StabilityResult:
        # Parse response - normalize and scan for YES/NO
        response = stdout.strip().upper()
        verified = None
        for line in response.split("\n"):
            line = line.strip()
            if line == "YES":
                verified = True
                break
            elif line == "NO":
                verified = False
                break

        if verified is None:
            if response == "YES":
                verified = True
            elif response == "NO":
                verified = False

        if verified is None:
            print_warning(f"Unexpected response (not YES/NO): {stdout[:100]}")
            return StabilityResult(is_stable=False, should_fail=False, value=None)

        if verified is False:
            print_warning("Verification: NO - task not complete")
            return StabilityResult(is_stable=False, should_fail=True, value=False)

        # YES response - counts as stable
        return StabilityResult(is_stable=True, should_fail=False, value=True)

    success, _ = run_stability_loop(
        prompt_builder=completion_prompt_builder,
        stability_checker=completion_stability_checker,
        max_iterations=MAX_FIX_ATTEMPTS,
        phase_name="VERIFY_COMPLETE",
    )
    return success


def implement_todo(
    todo: dict, suite_test_files: dict[str, list[str]], all_test_artifacts: list[str]
) -> tuple[bool, str, str]:
    """
    Run claude code to implement the todo.
    Blocks write access to test files via configured globs and test_artifacts.

    Args:
        todo: The todo item
        suite_test_files: Dict mapping suite name -> list of test files
        all_test_artifacts: List of all test files to lock (disallow writes)

    Returns:
        Tuple of (success, stdout, stderr)
    """
    # Build test locations string for prompt
    test_locations = []
    for suite_name, files in suite_test_files.items():
        test_locations.append(f"- {suite_name}: {', '.join(files)}")
    test_locations_str = "\n".join(test_locations)

    # Collect disallow paths from all affected suites + test artifacts
    extra_disallow = list(all_test_artifacts)
    for suite_name in suite_test_files.keys():
        suite = get_suite_by_name(suite_name)
        if suite:
            extra_disallow.extend(get_disallowed_paths(suite))
    extra_disallow = list(set(extra_disallow))  # Deduplicate

    context = Context(
        task=todo.get("todo", ""),
        test_locations_str=test_locations_str,
    )
    prompt = context.build_prompt(PromptType.GREEN_IMPLEMENT)

    returncode, stdout, stderr = run_claude_code(prompt, disallow_paths=extra_disallow)

    return returncode == 0, stdout, stderr


def fix_failing_tests(
    todo: dict,
    suite_test_files: dict[str, list[str]],
    all_test_artifacts: list[str],
    test_results: dict[str, tuple[bool, str, str]],
) -> tuple[bool, str, str]:
    """
    Run claude code to fix failing tests.

    Args:
        todo: The todo item
        suite_test_files: Dict mapping suite name -> list of test files
        all_test_artifacts: List of all test files to lock (disallow writes)
        test_results: Results from all suites (key -> (passed, stdout, stderr))

    Returns:
        Tuple of (success, stdout, stderr)
    """
    # Build test locations string for prompt
    test_locations = []
    for suite_name, files in suite_test_files.items():
        test_locations.append(f"- {suite_name}: {', '.join(files)}")
    test_locations_str = "\n".join(test_locations)

    # Build failure output from all failing suites
    failure_output_lines = []
    for key, (passed, stdout, stderr) in test_results.items():
        if not passed:
            failure_output_lines.append(f"=== {key} ===")
            failure_output_lines.append(f"STDOUT:\n{stdout}")
            if stderr:
                failure_output_lines.append(f"STDERR:\n{stderr}")
    failure_output = "\n\n".join(failure_output_lines)

    # Collect disallow paths from all affected suites + test artifacts
    extra_disallow = list(all_test_artifacts)
    for suite_name in suite_test_files.keys():
        suite = get_suite_by_name(suite_name)
        if suite:
            extra_disallow.extend(get_disallowed_paths(suite))
    extra_disallow = list(set(extra_disallow))  # Deduplicate

    context = Context(
        task=todo.get("todo", ""),
        test_locations_str=test_locations_str,
        failure_output=failure_output,
    )
    prompt = context.build_prompt(PromptType.FIX_FAILING_TESTS)

    returncode, stdout, stderr = run_claude_code(prompt, disallow_paths=extra_disallow)

    return returncode == 0, stdout, stderr


def run_post_green_commands(
    suite_test_files: dict[str, list[str]],
) -> tuple[bool, dict[str, tuple[bool, str, str]]]:
    return TestRunner.run_post_green_commands(suite_test_files)


def fix_failing_build(
    todo: dict,
    suite_test_files: dict[str, list[str]],
    all_test_artifacts: list[str],
    build_results: dict[str, tuple[bool, str, str]],
) -> tuple[bool, str, str]:
    """
    Run claude code to fix failing build/post_green_command.

    Args:
        todo: The todo item
        suite_test_files: Dict mapping suite name -> list of test files
        all_test_artifacts: List of all test files to lock (disallow writes)
        build_results: Results from post_green_commands (suite_name -> (passed, stdout, stderr))

    Returns:
        Tuple of (success, stdout, stderr)
    """
    # Build test locations string for prompt
    test_locations = []
    for suite_name, files in suite_test_files.items():
        test_locations.append(f"- {suite_name}: {', '.join(files)}")
    test_locations_str = "\n".join(test_locations)

    # Build failure output from all failing build commands
    failure_output_lines = []
    for suite_name, (passed, stdout, stderr) in build_results.items():
        if not passed:
            suite = get_suite_by_name(suite_name)
            cmd = suite.get("post_green_command", "unknown") if suite else "unknown"
            failure_output_lines.append(f"=== {suite_name}: {cmd} ===")
            failure_output_lines.append(f"STDOUT:\n{stdout}")
            if stderr:
                failure_output_lines.append(f"STDERR:\n{stderr}")
    failure_output = "\n\n".join(failure_output_lines)

    # Collect disallow paths from all affected suites + test artifacts
    extra_disallow = list(all_test_artifacts)
    for suite_name in suite_test_files.keys():
        suite = get_suite_by_name(suite_name)
        if suite:
            extra_disallow.extend(get_disallowed_paths(suite))
    extra_disallow = list(set(extra_disallow))  # Deduplicate

    context = Context(
        task=todo.get("todo", ""),
        test_locations_str=test_locations_str,
        failure_output=failure_output,
    )
    prompt = context.build_prompt(PromptType.FIX_FAILING_BUILD)

    returncode, stdout, stderr = run_claude_code(prompt, disallow_paths=extra_disallow)

    return returncode == 0, stdout, stderr


# ============================================================================
# Retry Wrapper (handles transient failures by re-running)
# ============================================================================


def failures_same_reason(tail1: str, tail2: str) -> bool:
    """
    Use Claude to determine if two failure outputs are for the same reason.

    Returns True if same reason (or unparseable), False if different.
    """
    prompt = f"""Compare these two script failure outputs and determine if they failed for the SAME reason.

FAILURE 1:
{tail1}

FAILURE 2:
{tail2}

Did both failures occur for the same underlying reason?
Answer ONLY 'YES' or 'NO'."""

    log_prompt(prompt, "FAILURE COMPARISON PROMPT")
    result = subprocess.run(
        ["claude", "-p", prompt, "--no-input"], capture_output=True, text=True
    )

    response = result.stdout.strip().upper()

    # Parse response - look for YES/NO
    for line in response.split("\n"):
        line = line.strip()
        if line == "YES":
            return True
        elif line == "NO":
            return False

    # Unparseable - assume same reason, exit
    print("[RETRY WRAPPER] Could not parse Claude response, assuming same reason")
    return True


def run_with_retry(max_retries: int = 10, tail_lines: int = 150) -> int:
    return StabilityLoop.run_with_retry(max_retries, tail_lines)


def main():
    """Main orchestration loop."""
    global AUTOPUSH

    # Parse command-line arguments
    args = parse_args()
    AUTOPUSH = not args.no_autopush

    # Handle --clean-done early (doesn't need config or log file)
    if args.clean_done:
        return clean_done_todos()

    # If retry is enabled (default), run via wrapper
    if not args.no_retry:
        return run_with_retry()

    # Open log file for verbose output (append mode)
    log_file = open("chief.log", "a")
    Logger.set_log_file(log_file)
    atexit.register(log_file.close)

    # Write timestamp separator for this run
    Logger.write("\n\n")
    Logger.write("╔" + "═" * 78 + "╗\n")
    Logger.write("║" + "".center(78) + "║\n")
    Logger.write("║" + f"CHIEF RUN: {datetime.now().isoformat()}".center(78) + "║\n")
    Logger.write("║" + "".center(78) + "║\n")
    Logger.write("╚" + "═" * 78 + "╝\n")
    Logger.write("\n")

    print_banner("CHIEF - TDD Orchestrator for Claude Code")
    print()

    # Load configuration
    load_config()

    # Handle --test-suite (needs config but not todos)
    if args.test_suite:
        return test_suite_config(args.test_suite)

    print_info(
        f"Loaded {color(str(len(ConfigManager.get_suites())), Colors.YELLOW, Colors.BOLD)} test suite(s):"
    )
    for suite in ConfigManager.get_suites():
        print(
            f"  {color('•', Colors.CYAN)} {color(suite['name'], Colors.MAGENTA, Colors.BOLD)}: "
            f"{suite['language']}/{suite['framework']} (test_root: {suite['test_root']})"
        )
    print()

    # Validate suite environments before processing
    validate_suite_environments()

    # Load todos
    data = load_todos()

    if "todos" not in data:
        print_error("todos.json must have a 'todos' array")
        sys.exit(1)

    pending_count = len([t for t in data["todos"] if t.get("done_at_commit") is None])
    total_count = len(data["todos"])
    print_info(
        f"Loaded {color(str(total_count), Colors.YELLOW)} todos "
        f"({color(str(pending_count), Colors.BRIGHT_GREEN, Colors.BOLD)} pending)"
    )

    if pending_count == 0:
        print()
        print_success("All todos already completed!")
        return 0

    # Process todos by priority
    completed_count = 0
    while True:
        # Reload todos.json each iteration to pick up any external changes
        data = load_todos()
        todo = get_next_todo(data)

        if todo is None:
            print()
            print_banner("All todos completed!", char="*")
            print_success(f"Completed {completed_count} todo(s) this session")
            break

        processor = TodoProcessor(todo, data)
        success = processor.process()

        if not success:
            print()
            print_banner("FAILED", char="!")
            print_error(f"Could not complete todo after maximum retries")
            print_error(f"Todo: {todo.get('todo', 'Unknown')}")
            print_info("Exiting...")
            sys.exit(1)

        completed_count += 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
