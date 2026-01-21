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
from typing import Optional, TextIO
import atexit


TODOS_FILE = "todos.json"
CONFIG_FILE = "chief.toml"
MAX_IMPLEMENTATION_ATTEMPTS = 6
MAX_FIX_ATTEMPTS = 6
MAX_TEST_REFINEMENT_ITERATIONS = 6
STABILITY_ITERATIONS = 2  # Times Claude must give consistent answer before accepting

# Global config loaded at startup
CONFIG: dict = {}
# Track which suites have had their setup run
SETUP_COMPLETED: set[str] = set()
# Auto-push commits to remote (can be disabled with --no-autopush)
AUTOPUSH: bool = True
# Log file for verbose output (console shows essentials only)
LOG_FILE: Optional[TextIO] = None

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


def color(text: str, *codes: str) -> str:
    """Wrap text with ANSI color codes."""
    if not sys.stdout.isatty():
        return text  # No colors if not a terminal
    return "".join(codes) + text + Colors.RESET


def strip_ansi(text: str) -> str:
    """Remove ANSI escape codes from text."""
    import re
    return re.sub(r'\033\[[0-9;]*m', '', text)


def log_write(text: str) -> None:
    """Write text to log file (without ANSI codes)."""
    if LOG_FILE:
        LOG_FILE.write(strip_ansi(text))
        LOG_FILE.flush()


def print_banner(text: str, char: str = "=", width: int = 60) -> None:
    """Print a prominent banner."""
    line = char * width
    print(color(line, Colors.BRIGHT_CYAN, Colors.BOLD))
    print(color(text.center(width), Colors.BRIGHT_CYAN, Colors.BOLD))
    print(color(line, Colors.BRIGHT_CYAN, Colors.BOLD))
    # Log with box-drawing characters for readability
    log_write("┏" + "━" * 78 + "┓\n")
    log_write("┃" + text.center(78) + "┃\n")
    log_write("┗" + "━" * 78 + "┛\n")


def print_phase(phase: str, description: str) -> None:
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
    print(f"\n{color(f'[{phase}]', phase_color, Colors.BOLD)} {color(description, Colors.WHITE)}")
    # Log with visual separator for phases
    log_write("\n")
    log_write("─" * 80 + "\n")
    log_write(f"▶ [{phase}] {description}\n")
    log_write("─" * 80 + "\n")


def print_info(msg: str, indent: int = 0) -> None:
    """Print an informational message from the script."""
    prefix = "  " * indent
    print(f"{prefix}{color('▸', Colors.CYAN)} {msg}")
    log_write(f"{prefix}> {msg}\n")


def print_success(msg: str, indent: int = 0) -> None:
    """Print a success message."""
    prefix = "  " * indent
    print(f"{prefix}{color('✓', Colors.BRIGHT_GREEN, Colors.BOLD)} {color(msg, Colors.GREEN)}")
    log_write(f"{prefix}[OK] {msg}\n")


def print_warning(msg: str, indent: int = 0) -> None:
    """Print a warning message."""
    prefix = "  " * indent
    print(f"{prefix}{color('⚠', Colors.BRIGHT_YELLOW, Colors.BOLD)} {color(msg, Colors.YELLOW)}")
    log_write(f"{prefix}[WARN] {msg}\n")


def print_error(msg: str, indent: int = 0) -> None:
    """Print an error message."""
    prefix = "  " * indent
    print(f"{prefix}{color('✗', Colors.BRIGHT_RED, Colors.BOLD)} {color(msg, Colors.RED)}")
    log_write(f"{prefix}[ERROR] {msg}\n")


def print_claude_start() -> None:
    """Print marker for start of Claude Code output (log only)."""
    log_write("\n")
    log_write("╔" + "═" * 78 + "╗\n")
    log_write("║" + " CLAUDE OUTPUT ".center(78) + "║\n")
    log_write("╚" + "═" * 78 + "╝\n")
    log_write("\n")


def print_claude_end() -> None:
    """Print marker for end of Claude Code output (log only)."""
    log_write("\n")
    log_write("╔" + "═" * 78 + "╗\n")
    log_write("║" + " END CLAUDE OUTPUT ".center(78) + "║\n")
    log_write("╚" + "═" * 78 + "╝\n")
    log_write("\n")


def log_section_divider(label: str = "") -> None:
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
    log_write(f"\n{line}\n")


def log_prompt(prompt: str, label: str = "PROMPT TO CLAUDE") -> None:
    """Log a prompt being sent to Claude with clear visual demarcation."""
    log_write("\n")
    log_write("┌" + "─" * 78 + "┐\n")
    log_write("│" + f" {label} ".center(78) + "│\n")
    log_write("├" + "─" * 78 + "┤\n")
    # Wrap each line to fit in box (74 chars content width)
    for line in prompt.split('\n'):
        if len(line) <= 74:
            log_write(f"│  {line.ljust(75)} │\n")
        else:
            wrapped_lines = textwrap.wrap(line, width=74)
            for wrapped in wrapped_lines:
                log_write(f"│  {wrapped.ljust(75)} │\n")
    log_write("└" + "─" * 78 + "┘\n")
    log_write("\n")


def parse_args() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(
        description="TDD Orchestrator for Claude Code - runs Red-Green-Refactor cycle"
    )
    parser.add_argument(
        "--no-autopush",
        action="store_true",
        help="Disable automatic git push after commits (commits are still created locally)"
    )
    parser.add_argument(
        "--clean-done",
        action="store_true",
        help="Remove completed todos (non-null done_at_commit) from todos.json and exit"
    )
    parser.add_argument(
        "--test-suite",
        metavar="NAME",
        help="Test a suite's configuration by running setup and a test command, then exit"
    )
    parser.add_argument(
        "--no-retry",
        action="store_true",
        help="Disable automatic retry on transient failures (retry is enabled by default)"
    )
    return parser.parse_args()


def load_config() -> dict:
    """
    Load configuration from chief.toml.

    Expected structure (multi-suite format):
        [[suites]]
        name = "backend"
        language = "Python"
        framework = "pytest"
        test_root = "backend/"           # Working directory for test_init and test_command
        test_command = "pytest {target} -v"
        target_type = "file"
        file_patterns = ["test_*.py", "*_test.py"]
        disallow_write_globs = ["backend/tests/**"]
        test_init = "pip install -r requirements.txt"  # Runs in test_root
        test_setup = "docker compose up -d db"         # Runs in PROJECT ROOT
        post_green_command = "docker compose build"    # Runs in PROJECT ROOT after tests pass

        [[suites]]
        name = "frontend"
        language = "TypeScript"
        framework = "vitest"
        test_root = "frontend/"
        test_command = "npm test -- {target}"
        target_type = "file"
        file_patterns = ["*.test.ts", "*.spec.ts"]
        disallow_write_globs = ["frontend/**/*.test.ts"]

    Returns:
        Configuration dictionary with 'suites' array
    """
    if not Path(CONFIG_FILE).exists():
        print_error(f"{CONFIG_FILE} not found")
        print_info("Please create a chief.toml configuration file.")
        print()
        print(color("Example chief.toml:", Colors.DIM))
        print(color('[[suites]]', Colors.DIM))
        print(color('name = "backend"', Colors.DIM))
        print(color('language = "Python"', Colors.DIM))
        print(color('framework = "pytest"', Colors.DIM))
        print(color('test_root = "."', Colors.DIM))
        print(color('test_command = "pytest {target} -v"', Colors.DIM))
        print(color('target_type = "file"', Colors.DIM))
        print(color('file_patterns = ["test_*.py", "*_test.py"]', Colors.DIM))
        print(color('disallow_write_globs = ["tests/**", "test_*.py"]', Colors.DIM))
        sys.exit(1)

    with open(CONFIG_FILE, "rb") as f:
        config = tomllib.load(f)

    # Validate suites array exists
    if "suites" not in config or not config["suites"]:
        print_error(f"{CONFIG_FILE} must contain at least one [[suites]] entry")
        sys.exit(1)

    # Validate each suite
    required_suite_keys = ["name", "language", "framework", "test_root", "test_command", "target_type"]
    for i, suite in enumerate(config["suites"]):
        missing = [k for k in required_suite_keys if k not in suite]
        if missing:
            print_error(f"Suite {i+1} missing required keys: {', '.join(missing)}")
            sys.exit(1)

        # Set defaults for optional keys
        suite.setdefault("default_target", ".")
        suite.setdefault("file_patterns", [])
        suite.setdefault("disallow_write_globs", [])
        suite.setdefault("test_init", None)           # One-time dev env setup (run in test_root if validation fails)
        suite.setdefault("test_setup", None)          # Pre-test setup (run in PROJECT ROOT once per suite)
        suite.setdefault("post_green_command", None)  # Post-test validation (run in PROJECT ROOT after tests pass)
        suite.setdefault("env", {})                   # Environment variables for all commands

    return config


def get_suite_env(suite: dict) -> dict[str, str]:
    """
    Build environment dict for running suite commands.

    Merges the current environment with suite-specific env vars.
    Suite vars override existing environment vars.

    Args:
        suite: The suite configuration dict

    Returns:
        Environment dict to pass to subprocess
    """
    env = os.environ.copy()
    suite_env = suite.get("env", {})
    for key, value in suite_env.items():
        env[key] = str(value)
    return env


def validate_suite_environments():
    """
    Validate that all test suite commands can execute.
    If a suite fails validation and has a test_init command, run init and retry.
    Exits with error if any suite fails validation after init attempt.
    """
    print_info("Validating test suite environments...")

    for suite in CONFIG["suites"]:
        name = suite["name"]
        command = suite["test_command"]
        init_cmd = suite.get("test_init")
        suite_env = get_suite_env(suite)

        # Build validation command - use --version as a quick check
        if "{target}" in command:
            validation_cmd = command.replace("{target}", "--version 2>/dev/null || true")
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
            timeout=60
        )

        # If validation fails and we have an init command, try running it
        if result.returncode != 0 and init_cmd:
            print_warning(f"Suite '{name}' validation failed, running test_init...")
            print_info(f"  Init: {init_cmd}")

            # test_init runs in test_root
            init_result = subprocess.run(
                init_cmd,
                capture_output=True,
                text=True,
                shell=True,
                cwd=suite.get("test_root") or os.getcwd(),
                env=suite_env
            )
            if init_result.stdout:
                log_write(init_result.stdout)
            if init_result.stderr:
                log_write(init_result.stderr)

            if init_result.returncode != 0:
                print_error(f"Suite '{name}': test_init command failed")
                sys.exit(1)

            # Retry validation
            result = subprocess.run(
                validation_cmd,
                capture_output=True,
                text=True,
                shell=True,
                cwd=suite.get("test_root") or os.getcwd(),
                env=suite_env,
                timeout=60
            )

            if result.returncode != 0:
                print_error(f"Suite '{name}': still failing after test_init")
                print_error(f"  Command: {validation_cmd}")
                if result.stderr:
                    print(result.stderr)
                if result.stdout:
                    print(result.stdout)
                sys.exit(1)
        elif result.returncode != 0:
            print_error(f"Suite '{name}': environment validation failed (no test_init command defined)")
            print_error(f"  Command: {validation_cmd}")
            if result.stderr:
                print(result.stderr)
            if result.stdout:
                print(result.stdout)
            sys.exit(1)

        print(f"  {color('✓', Colors.BRIGHT_GREEN)} {color(name, Colors.MAGENTA)}: OK")

    print()


def run_suite_setup(suite: dict) -> None:
    """
    Run test_setup command for a suite (once before that suite's tests).
    Tracks completion to avoid re-running for multiple todos in same suite.
    Note: test_setup runs in PROJECT ROOT, not test_root.
    """
    name = suite["name"]

    # Skip if already set up
    if name in SETUP_COMPLETED:
        return

    setup_cmd = suite.get("test_setup")
    if not setup_cmd:
        SETUP_COMPLETED.add(name)
        return

    print_info(f"Running test_setup for suite '{name}': {setup_cmd}")

    # test_setup runs in PROJECT ROOT (not test_root)
    result = subprocess.run(
        setup_cmd,
        capture_output=True,
        text=True,
        shell=True,
        cwd=os.getcwd(),
        env=get_suite_env(suite)
    )
    if result.stdout:
        log_write(result.stdout)
    if result.stderr:
        log_write(result.stderr)

    if result.returncode != 0:
        print_error(f"test_setup failed for suite '{name}'")
        sys.exit(1)

    SETUP_COMPLETED.add(name)
    print_success(f"test_setup complete for suite '{name}'")


def test_suite_config(suite_name: str) -> int:
    """
    Test a suite's configuration by running test_setup and a test command.

    Uses the same path stripping and cwd logic as run_tests() to verify
    the configuration is correct.

    Args:
        suite_name: Name of the suite to test

    Returns:
        Exit code (0 for success, 1 for failure)
    """
    suite = get_suite_by_name(suite_name)
    if not suite:
        print_error(f"Suite '{suite_name}' not found")
        print_info("Available suites:")
        for s in CONFIG["suites"]:
            print(f"  • {s['name']}")
        return 1

    print_banner(f"Testing suite: {suite_name}")
    print()

    # Show configuration
    print_info("Configuration:")
    print(f"  test_root: {color(suite.get('test_root', '.'), Colors.CYAN)}")
    print(f"  test_command: {color(suite['test_command'], Colors.CYAN)}")
    print(f"  default_target: {color(suite.get('default_target', '.'), Colors.CYAN)}")
    print(f"  strip_root_from_target: {color(str(suite.get('strip_root_from_target', True)), Colors.CYAN)}")
    if suite.get("post_green_command"):
        print(f"  post_green_command: {color(suite['post_green_command'], Colors.CYAN)}")
    print()

    # Run test_setup if configured (runs in PROJECT ROOT)
    setup_cmd = suite.get("test_setup")
    if setup_cmd:
        print_info(f"Running test_setup (in project root): {setup_cmd}")
        result = subprocess.run(
            setup_cmd,
            capture_output=True,
            text=True,
            shell=True,
            cwd=os.getcwd(),
            env=get_suite_env(suite)
        )
        if result.stdout:
            log_write(result.stdout)
        if result.stderr:
            log_write(result.stderr)
        if result.returncode != 0:
            print_error("test_setup failed")
            return 1
        print_success("test_setup complete")
        print()

    # Build test command using same logic as run_tests()
    target = suite.get("default_target", ".")
    command_template = suite["test_command"]
    root = suite.get("test_root", "")
    strip_root = suite.get("strip_root_from_target", True)

    # Show the path transformation
    print_info("Path resolution:")
    print(f"  Original target: {color(target, Colors.CYAN)}")

    transformed_target = target
    if strip_root and root and root != ".":
        normalized_root = root if root.endswith("/") else root + "/"
        if target.startswith(normalized_root):
            transformed_target = target[len(normalized_root):]
            print(f"  After stripping '{normalized_root}': {color(transformed_target, Colors.CYAN)}")
        else:
            print(f"  (target doesn't start with test_root, not stripped)")

    cwd = root or os.getcwd()
    print(f"  Working directory: {color(cwd, Colors.CYAN)}")

    # Build and show final command
    if "{target}" in command_template:
        test_command = command_template.format(target=transformed_target)
    else:
        test_command = command_template

    print(f"  Final command: {color(test_command, Colors.YELLOW)}")
    print()

    # Run the test command (runs in test_root)
    print_info("Running test_command...")
    result = subprocess.run(
        test_command,
        capture_output=True,
        text=True,
        shell=True,
        cwd=cwd,
        env=get_suite_env(suite)
    )
    if result.stdout:
        log_write(result.stdout)
    if result.stderr:
        log_write(result.stderr)

    print()
    if result.returncode == 0:
        print_success(f"Suite '{suite_name}' configuration is valid")
        return 0
    else:
        print_error(f"test_command failed with exit code {result.returncode}")
        return 1


def load_todos() -> dict:
    """Load todos from todos.json."""
    if not Path(TODOS_FILE).exists():
        print_error(f"{TODOS_FILE} not found")
        sys.exit(1)

    with open(TODOS_FILE, "r") as f:
        return json.load(f)


def save_todos(data: dict) -> None:
    """Save todos back to todos.json."""
    with open(TODOS_FILE, "w") as f:
        json.dump(data, f, indent=2)


def clean_done_todos() -> int:
    """
    Remove completed todos from todos.json.

    Removes all todos where done_at_commit is not null.

    Returns:
        Exit code (0 for success)
    """
    if not Path(TODOS_FILE).exists():
        print_error(f"{TODOS_FILE} not found")
        return 1

    with open(TODOS_FILE, "r") as f:
        data = json.load(f)

    if "todos" not in data:
        print_error("todos.json must have a 'todos' array")
        return 1

    original_count = len(data["todos"])
    data["todos"] = [t for t in data["todos"] if t.get("done_at_commit") is None]
    removed_count = original_count - len(data["todos"])

    if removed_count == 0:
        print_info("No completed todos to remove")
        return 0

    with open(TODOS_FILE, "w") as f:
        json.dump(data, f, indent=2)

    print_success(f"Removed {removed_count} completed todo(s) from {TODOS_FILE}")
    print_info(f"Remaining: {len(data['todos'])} pending todo(s)")
    return 0


def get_next_todo(data: dict) -> Optional[dict]:
    """Get the highest priority todo that hasn't been completed."""
    pending = [t for t in data["todos"] if t.get("done_at_commit") is None]
    if not pending:
        return None
    # Sort by priority descending (highest first)
    pending.sort(key=lambda x: x.get("priority", 0), reverse=True)
    return pending[0]


def detect_suite_from_path(file_path: str) -> Optional[dict]:
    """
    Determine which suite a file belongs to based on its path.

    Args:
        file_path: Path to a file

    Returns:
        The matching suite dict, or None if no match
    """
    for suite in CONFIG["suites"]:
        root = suite.get("test_root", "")
        # Normalize: ensure root ends with / for prefix matching (unless empty or ".")
        if root and root != "." and not root.endswith("/"):
            root = root + "/"
        # Check if file is under this root
        if root == "." or root == "" or file_path.startswith(root):
            return suite
    return None


def get_suite_by_name(name: str) -> Optional[dict]:
    """Get suite configuration by name."""
    for suite in CONFIG["suites"]:
        if suite["name"] == name:
            return suite
    return None


def filter_test_files_all_suites(files: list[str]) -> dict[str, list[str]]:
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
        suite = detect_suite_from_path(filepath)
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


def run_tests_for_all_affected_suites(
    suite_test_files: dict[str, list[str]]
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
        suite = get_suite_by_name(suite_name)
        if not suite:
            print_warning(f"Suite '{suite_name}' not found, skipping")
            continue

        # Run setup for this suite
        run_suite_setup(suite)

        # Run tests for each test file in this suite
        for test_file in test_files:
            passed, stdout, stderr = run_tests(test_file, suite)
            results[f"{suite_name}:{test_file}"] = (passed, stdout, stderr)
            if not passed:
                all_passed = False

    return all_passed, results


def get_all_disallowed_paths() -> list[str]:
    """
    Get disallowed paths from ALL suites for multi-suite protection.

    Returns:
        Combined list of paths to protect from writes
    """
    all_paths = []
    for suite in CONFIG["suites"]:
        all_paths.extend(get_disallowed_paths(suite))
    return list(set(all_paths))  # Deduplicate


def get_disallowed_paths(suite: dict) -> list[str]:
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


def run_claude_code(prompt: str, disallow_paths: list[str] | None = None) -> tuple[int, str, str]:
    """
    Run claude code with the given prompt.

    Args:
        prompt: The prompt to send to claude code
        disallow_paths: Paths to block write access to

    Returns:
        Tuple of (return_code, stdout, stderr)
    """
    # Prompt must be positional argument right after -p (not via stdin)
    # --verbose shows real-time tool calls and agent activity
    cmd = ["claude", "-p", prompt, "--permission-mode", "acceptEdits", "--verbose"]

    # Add disallowed paths if any
    for path in (disallow_paths or []):
        cmd.extend(["--disallowedTools", f"Edit:{path}", "--disallowedTools", f"Write:{path}"])

    print_info("Invoking Claude Code...")
    log_prompt(prompt)
    print_claude_start()

    # Stream output to log file while capturing for parsing
    process = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,  # Merge stderr into stdout
        text=True,
        cwd=os.getcwd()
    )

    stdout_lines = []
    for line in process.stdout:
        log_write(line)  # Write to log file only
        stdout_lines.append(line)

    process.wait()
    print_claude_end()
    return process.returncode, "".join(stdout_lines), ""


def run_tests(target: str, suite: dict) -> tuple[bool, str, str]:
    """
    Run tests on the specified target using the suite's test_command.

    Args:
        target: The test target (file, package, project, or repo path)
        suite: The test suite configuration to use

    Returns:
        Tuple of (passed, stdout, stderr)
    """
    print_info(f"Running tests: {color(target, Colors.WHITE, Colors.BOLD)} (suite: {suite['name']})")

    command_template = suite["test_command"]

    # Strip test_root prefix from target by default (configurable via strip_root_from_target)
    strip_root = suite.get("strip_root_from_target", True)
    root = suite.get("test_root", "")

    transformed_target = target
    if strip_root and root and root != ".":
        # Normalize root to end with /
        normalized_root = root if root.endswith("/") else root + "/"
        if target.startswith(normalized_root):
            transformed_target = target[len(normalized_root):]

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
        env=get_suite_env(suite)
    )

    # Log test output
    if result.stdout:
        log_write(result.stdout)
    if result.stderr:
        log_write(result.stderr)

    passed = result.returncode == 0
    return passed, result.stdout, result.stderr


def git_commit_and_tag(message: str) -> str:
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
    subprocess.run(["git", "commit", "-m", message], check=True, capture_output=True)

    # Get commit hash
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True
    )
    commit_hash = result.stdout.strip()

    # Create tag with timestamp
    tag_name = f"chief-{datetime.now().strftime('%Y%m%d-%H%M%S')}"
    subprocess.run(["git", "tag", tag_name], check=True, capture_output=True)

    print_success(f"Committed: {commit_hash[:8]} (tag: {tag_name})")
    return commit_hash


def git_push_with_tags() -> bool:
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


def git_commit_todos(todo_text: str) -> None:
    """Commit todos.json update after marking a todo as done."""
    subprocess.run(["git", "add", "todos.json"], check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", f"chief: mark done - {todo_text[:50]}"],
        check=True,
        capture_output=True
    )
    # Push is non-fatal for todos commit
    git_push_with_tags()


def get_dirty_files() -> set[str]:
    """Get set of currently modified, staged, or untracked files."""
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        capture_output=True,
        text=True
    )
    files = set()
    for line in result.stdout.strip().split("\n"):
        if line:
            # Format is "XY filename" or "XY filename -> newname" for renames
            parts = line[3:].split(" -> ")
            files.add(parts[-1])  # Use the destination name for renames
    return files


def git_revert_changes(baseline_files: set[str] | None = None) -> None:
    """Revert uncommitted changes made since baseline (or all except todos.json if no baseline)."""
    print_warning("Reverting uncommitted changes...")

    if baseline_files is None:
        # Revert everything except todos.json (legacy behavior)
        subprocess.run(["git", "checkout", "--", ".", ":!todos.json"], capture_output=True)
        subprocess.run(["git", "clean", "-fd", "--exclude=todos.json"], capture_output=True)
        return

    # Only revert files that weren't dirty before
    current_files = get_dirty_files()
    files_to_revert = current_files - baseline_files - {"todos.json"}

    if not files_to_revert:
        print_info("No new changes to revert")
        return

    # Separate tracked (checkout) vs untracked (clean) files
    result = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard"],
        capture_output=True,
        text=True
    )
    untracked = set(result.stdout.strip().split("\n")) if result.stdout.strip() else set()

    tracked_to_revert = [f for f in files_to_revert if f not in untracked]
    untracked_to_revert = [f for f in files_to_revert if f in untracked]

    if tracked_to_revert:
        subprocess.run(["git", "checkout", "--"] + tracked_to_revert, capture_output=True)

    for f in untracked_to_revert:
        try:
            Path(f).unlink(missing_ok=True)
        except (OSError, IsADirectoryError):
            subprocess.run(["rm", "-rf", f], capture_output=True)


def find_recent_test_files(since_mtime: float, suite: dict) -> list[str]:
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


def git_get_status_snapshot() -> dict[str, str]:
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
        cwd=os.getcwd()
    )
    snapshot = {}
    for line in result.stdout.strip().split("\n"):
        if not line:
            continue
        status = line[:2]
        filepath = line[3:].strip()
        if " -> " in filepath:
            filepath = filepath.split(" -> ")[1]
        snapshot[filepath] = status
    return snapshot


def git_detect_changed_files(baseline_snapshot: dict[str, str]) -> list[str]:
    """
    Detect files that have changed since the baseline snapshot.

    A file is considered changed if:
    - It's new in git status (wasn't in the baseline snapshot)
    - OR its status code changed (e.g., from clean to modified)

    Args:
        baseline_snapshot: Dict from git_get_status_snapshot() captured before changes

    Returns:
        List of file paths that changed since the baseline
    """
    current_snapshot = git_get_status_snapshot()
    changed_files = []

    for filepath, status in current_snapshot.items():
        # File is new to git status OR has different status than before
        if filepath not in baseline_snapshot or baseline_snapshot[filepath] != status:
            if Path(filepath).exists():
                changed_files.append(filepath)

    return changed_files


def filter_test_files(files: list[str], suite: dict) -> list[str]:
    """
    Filter a list of files to only include test files matching suite's patterns.

    Args:
        files: List of file paths
        suite: The test suite configuration to use

    Returns:
        List of file paths matching test file patterns
    """
    file_patterns = suite.get("file_patterns", [])

    if not file_patterns:
        return []

    test_files = []
    for filepath in files:
        filename = Path(filepath).name
        for pattern in file_patterns:
            if fnmatch.fnmatch(filename, pattern):
                test_files.append(filepath)
                break

    return test_files


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
    todo_text = todo.get("todo", "")

    expectations = todo.get("expectations", "")
    expectations_section = ""
    if expectations:
        expectations_section = f"\nExpected outcome (from product manager):\n{expectations}"

    # Build suite info section listing all available suites
    suite_info_lines = []
    for suite in CONFIG["suites"]:
        patterns = suite.get("file_patterns", [])
        patterns_str = ", ".join(patterns) if patterns else "none"
        suite_info_lines.append(
            f"- {suite['name']}: {suite['language']}/{suite['framework']} "
            f"(test_root: {suite['test_root']}, test patterns: {patterns_str})"
        )
    suite_info = "\n".join(suite_info_lines)

    prompt = f"""Write or modify tests (Red phase of TDD) for the following task:

Task: {todo_text}
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

Only write/modify the tests, do not implement the feature."""

    # Capture git baseline before RED phase
    baseline_snapshot = git_get_status_snapshot()

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
                    print_info(f"Suite '{color(suite_name, Colors.MAGENTA)}': {', '.join(files)}")
                all_test_artifacts = stable_tests
                verified_existing = True
                # Fall through to refinement loop (don't return early)
            else:
                print_warning("Existing test files don't match any suite patterns")
        # If stability failed or no suite match, fall through to normal git-based detection

    if not verified_existing:
        # Detect new/modified files via git and map to suites
        changed_files = git_detect_changed_files(baseline_snapshot)
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
            print_info(f"Suite '{color(suite_name, Colors.MAGENTA)}': {', '.join(files)}")

    # --- REFINEMENT LOOP ---
    files_to_monitor = list(all_test_artifacts)

    if not files_to_monitor:
        print_warning("No test files to refine, skipping refinement loop")
        return suite_test_files, all_test_artifacts

    no_change_count = 0
    for refine_iter in range(1, MAX_TEST_REFINEMENT_ITERATIONS + 1):
        if no_change_count >= STABILITY_ITERATIONS:
            print_success(f"Tests stable (no changes for {STABILITY_ITERATIONS} consecutive iterations)")
            break

        print_phase("REFINE", f"Test refinement iteration {refine_iter}/{MAX_TEST_REFINEMENT_ITERATIONS}")
        print_info(f"Monitoring {len(files_to_monitor)} file(s) for changes: {', '.join(files_to_monitor)}")

        # Capture state before
        hashes_before = get_file_hashes(files_to_monitor)

        # Build refinement prompt with file paths (Claude Code can read them)
        file_list = "\n".join(f"- {f}" for f in files_to_monitor)

        refine_prompt = f"""We are in the RED PHASE for this task:

Task: {todo_text}
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

DO NOT attempt to make the tests pass. They SHOULD fail at this point.
"""

        # Run Claude to review/refine
        returncode, stdout, stderr = run_claude_code(refine_prompt)

        if returncode != 0:
            print_warning(f"Claude returned error during refinement: {stderr}", indent=1)
            # Continue anyway - tests might still be usable

        # Capture state after
        hashes_after = get_file_hashes(files_to_monitor)

        # Deterministic change detection
        if hashes_before == hashes_after:
            no_change_count += 1
            print_info(f"No changes detected (stable count: {no_change_count}/{STABILITY_ITERATIONS})", indent=1)
        else:
            no_change_count = 0
            changed_files = [f for f in files_to_monitor if hashes_before.get(f) != hashes_after.get(f)]
            print_warning(f"Test files modified: {', '.join(changed_files)} — will refine again", indent=1)

    if no_change_count < STABILITY_ITERATIONS:
        print_warning("Refinement loop hit max iterations without stabilizing")
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
            target = line[len("TEST_TARGET:"):].strip()
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
            files_str = line[len("TESTS_ALREADY_EXIST:"):].strip()
            files = [f.strip().strip("`\"'") for f in files_str.split(",")]
            return [f for f in files if f]
    return []


def verify_existing_tests_stable(todo: dict, initial_tests: list[str], suite_info: str) -> list[str]:
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
    todo_text = todo.get("todo", "")
    expectations = todo.get("expectations", "")
    expectations_section = ""
    if expectations:
        expectations_section = f"\n\nExpected outcome:\n{expectations}"

    # Track all responses as sets for intersection computation
    all_responses: list[set[str]] = [set(initial_tests)]

    prompt = f"""For this task, verify whether comprehensive tests already exist:

Task: {todo_text}
{expectations_section}

Available test suites:
{suite_info}

If tests already exist that cover this task, output a single line:
TESTS_ALREADY_EXIST: path/to/test1.py, path/to/test2.py

If tests need to be written or modified, proceed to write them."""

    previous_intersection: set[str] | None = None
    stable_count = 0

    for i in range(STABILITY_ITERATIONS + 1):
        returncode, stdout, stderr = run_claude_code(prompt)

        if returncode != 0:
            print_warning(f"Claude returned error: {stderr}", indent=1)
            continue

        current_tests = extract_existing_tests(stdout)

        if not current_tests:
            print_warning("Claude did not confirm existing tests, will write new tests", indent=1)
            return []

        # Add this response to our collection
        all_responses.append(set(current_tests))

        # Compute intersection of ALL responses so far
        current_intersection = set.intersection(*all_responses)

        print_info(f"Existing tests: {', '.join(sorted(current_tests))}", indent=1)

        # Check if intersection is empty - no common files across all responses
        if not current_intersection:
            print_warning("No common tests across responses, will write new tests", indent=1)
            return []

        # Check if intersection is stable (same as previous iteration)
        if current_intersection == previous_intersection:
            stable_count += 1
            print_info(f"Intersection stable ({stable_count}/{STABILITY_ITERATIONS}): {', '.join(sorted(current_intersection))}", indent=1)
        else:
            stable_count = 1
            previous_intersection = current_intersection
            print_info(f"Intersection: {', '.join(sorted(current_intersection))}", indent=1)

        if stable_count >= STABILITY_ITERATIONS:
            result = sorted(current_intersection)
            print_success(f"Existing tests confirmed: {', '.join(result)}")
            return result

    print_warning("Failed to stabilize existing tests intersection, will write new tests")
    return []


def implement_todo_no_tests(todo: dict, is_retry: bool = False) -> tuple[bool, str, str]:
    """
    Run claude code to implement a non-testable todo.

    Args:
        todo: The todo item
        is_retry: Whether this is a retry after previous verification failed

    Returns:
        Tuple of (success, stdout, stderr)
    """
    todo_text = todo.get("todo", "")
    expectations = todo.get("expectations", "")

    expectations_section = ""
    if expectations:
        expectations_section = f"\n\nExpected outcome:\n{expectations}"

    retry_section = ""
    if is_retry:
        retry_section = "\n\nPrevious verification failed. Please fix outstanding issues."

    prompt = f"""Implement the following task:

Task: {todo_text}
{expectations_section}
{retry_section}

Implement the task completely."""

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
    todo_text = todo.get("todo", "")
    expectations = todo.get("expectations", "")

    expectations_section = ""
    if expectations:
        expectations_section = f"\n\nExpected outcome:\n{expectations}"

    prompt = f"""Review the current state of the files for this task:

Task: {todo_text}
{expectations_section}

Is the task fully completed? Output ONLY 'YES' or 'NO'."""

    consecutive_yes = 0

    for i in range(MAX_FIX_ATTEMPTS):
        print_info(f"Verification attempt {i + 1}/{MAX_FIX_ATTEMPTS}...")

        returncode, stdout, stderr = run_claude_code(prompt)

        if returncode != 0:
            print_warning("Claude returned error during verification")
            consecutive_yes = 0
            continue

        # Parse response - normalize and scan for YES/NO
        response = stdout.strip().upper()
        verified = None
        for line in response.split('\n'):
            line = line.strip()
            if line == 'YES':
                verified = True
                break
            elif line == 'NO':
                verified = False
                break

        if verified is None:
            if response == 'YES':
                verified = True
            elif response == 'NO':
                verified = False

        if verified is None:
            print_warning(f"Unexpected response (not YES/NO): {stdout[:100]}")
            consecutive_yes = 0
            continue

        if verified:
            consecutive_yes += 1
            print_info(f"Verification: YES ({consecutive_yes}/{STABILITY_ITERATIONS})")
            if consecutive_yes >= STABILITY_ITERATIONS:
                print_success("Task verified complete (stable)")
                return True
        else:
            print_warning("Verification: NO - task not complete")
            return False  # Fail immediately on NO

    print_warning(f"Verification did not stabilize after {MAX_FIX_ATTEMPTS} attempts")
    return False


def implement_todo(
    todo: dict,
    suite_test_files: dict[str, list[str]],
    all_test_artifacts: list[str]
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
    todo_text = todo.get("todo", "")

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

    prompt = f"""Implement the following task:

Task: {todo_text}

Tests have been created in the following locations:
{test_locations_str}

Do NOT modify any test files. Only implement the code to make ALL tests pass.

Implement the minimal code needed to pass the tests."""

    returncode, stdout, stderr = run_claude_code(prompt, disallow_paths=extra_disallow)

    return returncode == 0, stdout, stderr


def fix_failing_tests(
    todo: dict,
    suite_test_files: dict[str, list[str]],
    all_test_artifacts: list[str],
    test_results: dict[str, tuple[bool, str, str]]
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
    todo_text = todo.get("todo", "")

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

    prompt = f"""The tests are failing. Fix the code to make them pass.

Original task: {todo_text}

Test files (DO NOT MODIFY):
{test_locations_str}

Test failures:
{failure_output}

Analyze the test failures and fix the implementation code to make ALL tests pass."""

    returncode, stdout, stderr = run_claude_code(prompt, disallow_paths=extra_disallow)

    return returncode == 0, stdout, stderr


def run_post_green_commands(
    suite_test_files: dict[str, list[str]]
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
        suite = get_suite_by_name(suite_name)
        if not suite:
            continue

        post_green_cmd = suite.get("post_green_command")
        if not post_green_cmd:
            continue

        print_info(f"Running post_green_command for suite '{suite_name}': {post_green_cmd}")

        # post_green_command runs in PROJECT ROOT (not test_root)
        result = subprocess.run(
            post_green_cmd,
            capture_output=True,
            text=True,
            shell=True,
            cwd=os.getcwd(),
            env=get_suite_env(suite)
        )

        passed = result.returncode == 0
        results[suite_name] = (passed, result.stdout, result.stderr)

        # Log output
        if result.stdout:
            log_write(result.stdout)
        if result.stderr:
            log_write(result.stderr)

        if passed:
            print_success(f"post_green_command passed for suite '{suite_name}'")
        else:
            print_error(f"post_green_command failed for suite '{suite_name}'")
            all_passed = False

    return all_passed, results


def fix_failing_build(
    todo: dict,
    suite_test_files: dict[str, list[str]],
    all_test_artifacts: list[str],
    build_results: dict[str, tuple[bool, str, str]]
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
    todo_text = todo.get("todo", "")

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

    prompt = f"""The build/validation command is failing. Fix the code to make it pass.

Original task: {todo_text}

Test files (DO NOT MODIFY):
{test_locations_str}

Build failures:
{failure_output}

Analyze the build failures and fix the implementation code. The tests are already passing,
so ensure your fix does not break the tests. Common issues include:
- TypeScript compilation errors (unused variables, type mismatches)
- Linting errors
- Build configuration issues"""

    returncode, stdout, stderr = run_claude_code(prompt, disallow_paths=extra_disallow)

    return returncode == 0, stdout, stderr


def process_todo_no_tests(todo: dict, data: dict) -> bool:
    """
    Process a non-testable todo using semantic verification.

    Instead of TDD, this:
    1. Has Claude implement the task
    2. Verifies completion via semantic review with stability check

    Args:
        todo: The todo item
        data: The full todos data structure

    Returns:
        True if todo was completed successfully, False otherwise
    """
    todo_text = todo.get("todo", "")

    # Outer retry loop
    for attempt in range(1, MAX_IMPLEMENTATION_ATTEMPTS + 1):
        print_phase("GREEN", f"Implementation attempt {attempt}/{MAX_IMPLEMENTATION_ATTEMPTS}")

        # Snapshot dirty files before implementation
        baseline_dirty = get_dirty_files()

        # Step A: Implementation (with retry message on attempt 2+)
        success, _, _ = implement_todo_no_tests(todo, is_retry=(attempt > 1))

        if not success:
            print_error("Claude Code returned error during implementation")
            if attempt < MAX_IMPLEMENTATION_ATTEMPTS:
                git_revert_changes(baseline_dirty)
            else:
                print_info("Keeping changes for inspection (final attempt)")
            continue

        # Step B: Verification
        print_phase("VERIFY", f"Semantic verification (attempt {attempt})")

        # Step C: Decision
        if verify_completion_stable(todo):
            # Verified complete - commit and mark done
            try:
                commit_hash = git_commit_and_tag(f"chief: {todo_text}")
                git_push_with_tags()  # Non-fatal
                todo["done_at_commit"] = commit_hash
                save_todos(data)
                git_commit_todos(todo_text)
                return True
            except subprocess.CalledProcessError as e:
                print_error(f"Git operation failed: {e}")
                git_revert_changes(baseline_dirty)
                continue
        else:
            # Verification failed
            print_warning("Semantic verification failed, will retry implementation...")
            if attempt < MAX_IMPLEMENTATION_ATTEMPTS:
                git_revert_changes(baseline_dirty)
            else:
                print_info("Keeping changes for inspection (final attempt)")

    print_error(f"Failed to complete todo after {MAX_IMPLEMENTATION_ATTEMPTS} attempts")
    return False


def process_todo(todo: dict, data: dict) -> bool:
    """
    Process a single todo through the TDD cycle.
    Suites are detected automatically from the test files Claude creates.

    Args:
        todo: The todo item
        data: The full todos data structure

    Returns:
        True if todo was completed successfully, False otherwise
    """
    todo_text = todo.get("todo", "")
    print()
    print_banner(f"TODO: {todo_text[:50]}{'...' if len(todo_text) > 50 else ''}")
    print_info(f"Full task: {todo_text}")
    print_info(f"Priority: {color(str(todo.get('priority', 0)), Colors.YELLOW, Colors.BOLD)}")

    # Check if this todo is testable
    testable = todo.get("testable", True)

    if not testable:
        # Non-testable task: use file-change verification instead of tests
        return process_todo_no_tests(todo, data)

    # Step 1: Write failing tests (Red) - Claude chooses which suite(s)
    print_phase("RED", "Writing failing tests...")
    suite_test_files, all_test_artifacts = write_test_for_todo(todo)

    if not suite_test_files:
        print_error("Failed to create tests, skipping todo")
        return False

    # Run setup for each affected suite
    for suite_name in suite_test_files.keys():
        suite = get_suite_by_name(suite_name)
        if suite:
            run_suite_setup(suite)

    # Verify tests fail (as expected for Red phase)
    all_passed, results = run_tests_for_all_affected_suites(suite_test_files)
    if all_passed:
        print_warning("All tests passed before implementation (expected to fail)")
        print_info("Proceeding to GREEN phase anyway")
    else:
        print_success("Tests fail as expected (Red phase complete)")

    # Secondary loop: implement and verify
    for secondary_iter in range(1, MAX_IMPLEMENTATION_ATTEMPTS + 1):
        print_phase("GREEN", f"Implementation attempt {secondary_iter}/{MAX_IMPLEMENTATION_ATTEMPTS}")

        # Snapshot dirty files before implementation so we only revert what we change
        baseline_files = get_dirty_files()

        # Step 2: Implement the todo (pass test_artifacts to lock test files)
        success, _, _ = implement_todo(todo, suite_test_files, all_test_artifacts)

        if not success:
            print_error("Claude Code returned error during implementation")
            if secondary_iter < MAX_IMPLEMENTATION_ATTEMPTS:
                git_revert_changes(baseline_files)
            else:
                print_info("Keeping changes for inspection (final attempt)")
            continue

        # Step 3: Run tests for all affected suites
        all_passed, results = run_tests_for_all_affected_suites(suite_test_files)

        if not all_passed:
            # Tests failed, enter test fix loop
            print_warning("Tests failed, entering fix loop...")

            for fix_iter in range(1, MAX_FIX_ATTEMPTS + 1):
                print_phase("FIX", f"Test fix attempt {fix_iter}/{MAX_FIX_ATTEMPTS}")

                success, _, _ = fix_failing_tests(todo, suite_test_files, all_test_artifacts, results)

                if not success:
                    print_error("Claude Code returned error during fix", indent=1)
                    continue

                # Run tests again for all affected suites
                all_passed, results = run_tests_for_all_affected_suites(suite_test_files)

                if all_passed:
                    print_success("All tests passed after fix!")
                    break  # Exit test fix loop, proceed to post_green check

            if not all_passed:
                # Test fix loop exhausted without passing
                print_warning("Test fix loop exhausted, reverting changes...")
                if secondary_iter < MAX_IMPLEMENTATION_ATTEMPTS:
                    git_revert_changes(baseline_files)
                else:
                    print_info("Keeping changes for inspection (final attempt)")
                continue  # Retry GREEN phase

        # Tests passed (either initially or after fixes)
        print_success("All tests passed!")

        # Step 4: Run post_green_commands for all affected suites
        print_phase("BUILD", "Running post_green_commands...")
        build_passed, build_results = run_post_green_commands(suite_test_files)

        if build_passed:
            # All checks passed - commit
            try:
                commit_hash = git_commit_and_tag(f"chief: {todo_text}")
                git_push_with_tags()  # Non-fatal
                todo["done_at_commit"] = commit_hash
                save_todos(data)
                git_commit_todos(todo_text)
                return True
            except subprocess.CalledProcessError as e:
                print_error(f"Git commit failed: {e}")
                git_revert_changes(baseline_files)
                continue

        # post_green_command failed, enter build fix loop
        print_warning("post_green_command failed, entering build fix loop...")

        for build_fix_iter in range(1, MAX_FIX_ATTEMPTS + 1):
            print_phase("BUILD-FIX", f"Build fix attempt {build_fix_iter}/{MAX_FIX_ATTEMPTS}")

            success, _, _ = fix_failing_build(todo, suite_test_files, all_test_artifacts, build_results)

            if not success:
                print_error("Claude Code returned error during build fix", indent=1)
                continue

            # Re-run tests first (fix might have broken them)
            print_info("Re-running tests after build fix...")
            all_passed, results = run_tests_for_all_affected_suites(suite_test_files)

            if not all_passed:
                print_warning("Tests failed after build fix, need to fix tests too")
                # Re-enter a mini test fix loop
                for test_fix_iter in range(1, MAX_FIX_ATTEMPTS + 1):
                    print_phase("FIX", f"Test fix attempt {test_fix_iter}/{MAX_FIX_ATTEMPTS} (during build fix)")
                    success, _, _ = fix_failing_tests(todo, suite_test_files, all_test_artifacts, results)
                    if not success:
                        print_error("Claude Code returned error during test fix", indent=1)
                        continue
                    all_passed, results = run_tests_for_all_affected_suites(suite_test_files)
                    if all_passed:
                        break
                if not all_passed:
                    print_warning("Could not fix tests during build fix loop")
                    continue  # Continue build fix loop

            # Tests pass, now check post_green again
            print_info("Re-running post_green_commands...")
            build_passed, build_results = run_post_green_commands(suite_test_files)

            if build_passed:
                print_success("All tests and post_green_commands passed!")
                try:
                    commit_hash = git_commit_and_tag(f"chief: {todo_text}")
                    git_push_with_tags()  # Non-fatal
                    todo["done_at_commit"] = commit_hash
                    save_todos(data)
                    git_commit_todos(todo_text)
                    return True
                except subprocess.CalledProcessError as e:
                    print_error(f"Git commit failed: {e}", indent=1)
                    break  # Break build fix loop, will revert in secondary

        # Build fix loop exhausted, revert and retry secondary
        print_warning("Build fix loop exhausted, reverting changes...")
        if secondary_iter < MAX_IMPLEMENTATION_ATTEMPTS:
            git_revert_changes(baseline_files)
        else:
            print_info("Keeping changes for inspection (final attempt)")

    # Secondary loop exhausted
    print_error(f"Failed to complete todo after {MAX_IMPLEMENTATION_ATTEMPTS} attempts")
    return False


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
        ["claude", "-p", prompt, "--no-input"],
        capture_output=True,
        text=True
    )

    response = result.stdout.strip().upper()

    # Parse response - look for YES/NO
    for line in response.split('\n'):
        line = line.strip()
        if line == 'YES':
            return True
        elif line == 'NO':
            return False

    # Unparseable - assume same reason, exit
    print("[RETRY WRAPPER] Could not parse Claude response, assuming same reason")
    return True


def run_with_retry(max_retries: int = 10, tail_lines: int = 150) -> int:
    """
    Run chief.py in a retry loop until failures stabilize or succeeds.

    Uses Claude to semantically compare failure outputs.
    Stops when:
    - Exit code is 0 (success)
    - Claude says two consecutive failures are for the same reason
    - Max retries reached
    """
    # Build command: same script with --no-retry to avoid infinite recursion
    cmd = [sys.executable, __file__] + [
        arg for arg in sys.argv[1:] if arg != "--no-retry"
    ] + ["--no-retry"]

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
        lines = result.stdout.strip().split('\n')
        current_tail = '\n'.join(lines[-tail_lines:])

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


def main():
    """Main orchestration loop."""
    global CONFIG, AUTOPUSH, LOG_FILE

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
    LOG_FILE = open("chief.log", "a")
    atexit.register(LOG_FILE.close)

    # Write timestamp separator for this run
    log_write("\n\n")
    log_write("╔" + "═" * 78 + "╗\n")
    log_write("║" + "".center(78) + "║\n")
    log_write("║" + f"CHIEF RUN: {datetime.now().isoformat()}".center(78) + "║\n")
    log_write("║" + "".center(78) + "║\n")
    log_write("╚" + "═" * 78 + "╝\n")
    log_write("\n")

    print_banner("CHIEF - TDD Orchestrator for Claude Code")
    print()

    # Load configuration
    CONFIG = load_config()

    # Handle --test-suite (needs config but not todos)
    if args.test_suite:
        return test_suite_config(args.test_suite)

    print_info(f"Loaded {color(str(len(CONFIG['suites'])), Colors.YELLOW, Colors.BOLD)} test suite(s):")
    for suite in CONFIG["suites"]:
        print(f"  {color('•', Colors.CYAN)} {color(suite['name'], Colors.MAGENTA, Colors.BOLD)}: "
              f"{suite['language']}/{suite['framework']} (test_root: {suite['test_root']})")
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
    print_info(f"Loaded {color(str(total_count), Colors.YELLOW)} todos "
               f"({color(str(pending_count), Colors.BRIGHT_GREEN, Colors.BOLD)} pending)")

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

        success = process_todo(todo, data)

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
