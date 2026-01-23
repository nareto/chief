"""Shared fixtures for chief.py tests."""
import json
import os
import sys
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

# Add parent directory to path so we can import chief
sys.path.insert(0, str(Path(__file__).parent.parent))


@pytest.fixture
def temp_config_file(tmp_path):
    """Create a temporary chief.toml file."""
    def _create(content: str) -> Path:
        config_path = tmp_path / "chief.toml"
        config_path.write_text(content)
        return config_path
    return _create


@pytest.fixture
def temp_todos_file(tmp_path):
    """Create a temporary todos.json file."""
    def _create(data: dict) -> Path:
        todos_path = tmp_path / "todos.json"
        todos_path.write_text(json.dumps(data, indent=2))
        return todos_path
    return _create


@pytest.fixture
def mock_subprocess():
    """Mock subprocess.run for git/test commands."""
    with patch('chief.subprocess.run') as mock:
        yield mock


@pytest.fixture
def mock_subprocess_popen():
    """Mock subprocess.Popen for streaming output (claude)."""
    with patch('chief.subprocess.Popen') as mock:
        yield mock


@pytest.fixture
def clean_env():
    """Provide a clean, controlled environment."""
    with patch.dict(os.environ, {}, clear=True):
        yield os.environ


@pytest.fixture
def mock_isatty():
    """Control terminal detection for color tests."""
    with patch('sys.stdout.isatty') as mock:
        yield mock


@pytest.fixture
def sample_suite():
    """Return a typical suite configuration dict."""
    return {
        "name": "backend",
        "language": "Python",
        "framework": "pytest",
        "test_root": "backend/",
        "test_command": "pytest {target} -v",
        "target_type": "file",
        "file_patterns": ["test_*.py", "*_test.py"],
        "disallow_write_globs": ["backend/tests/**"],
        "default_target": ".",
        "env": {"TEST_MODE": "1"},
    }


@pytest.fixture
def sample_config(sample_suite):
    """Return a complete CONFIG dict with one suite."""
    return {"suites": [sample_suite]}


@pytest.fixture
def multi_suite_config():
    """Return a CONFIG dict with multiple suites."""
    return {
        "suites": [
            {
                "name": "backend",
                "language": "Python",
                "framework": "pytest",
                "test_root": "backend/",
                "test_command": "pytest {target} -v",
                "target_type": "file",
                "file_patterns": ["test_*.py", "*_test.py"],
                "disallow_write_globs": [],
                "default_target": ".",
                "env": {},
            },
            {
                "name": "frontend",
                "language": "TypeScript",
                "framework": "vitest",
                "test_root": "frontend/",
                "test_command": "npm test -- {target}",
                "target_type": "file",
                "file_patterns": ["*.test.ts", "*.spec.ts"],
                "disallow_write_globs": [],
                "default_target": ".",
                "env": {},
            },
        ]
    }


@pytest.fixture
def mock_config(sample_config, monkeypatch):
    """Set the global CONFIG variable to sample_config."""
    import chief
    monkeypatch.setattr(chief, 'CONFIG', sample_config)
    return sample_config


@pytest.fixture
def mock_multi_config(multi_suite_config, monkeypatch):
    """Set the global CONFIG variable to multi_suite_config."""
    import chief
    monkeypatch.setattr(chief, 'CONFIG', multi_suite_config)
    return multi_suite_config


@pytest.fixture
def mock_log_file(monkeypatch):
    """Disable logging to file."""
    import chief
    monkeypatch.setattr(chief, 'LOG_FILE', None)
