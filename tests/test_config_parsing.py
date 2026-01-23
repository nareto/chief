"""Tests for config loading and validation logic."""
import pytest


class TestLoadConfig:
    """Tests for load_config() function - config validation contracts."""

    def test_valid_single_suite_parses(self, tmp_path, monkeypatch):
        """Valid config with single suite loads without error."""
        config_content = '''
[[suites]]
name = "backend"
language = "Python"
framework = "pytest"
test_root = "."
test_command = "pytest {target} -v"
target_type = "file"
'''
        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")

        config = chief.load_config()

        assert "suites" in config
        assert len(config["suites"]) == 1
        assert config["suites"][0]["name"] == "backend"

    def test_valid_multi_suite_parses(self, tmp_path, monkeypatch):
        """Valid config with multiple suites loads all suites."""
        config_content = '''
[[suites]]
name = "backend"
language = "Python"
framework = "pytest"
test_root = "backend/"
test_command = "pytest {target}"
target_type = "file"

[[suites]]
name = "frontend"
language = "TypeScript"
framework = "vitest"
test_root = "frontend/"
test_command = "npm test {target}"
target_type = "file"
'''
        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")

        config = chief.load_config()

        assert len(config["suites"]) == 2
        assert config["suites"][0]["name"] == "backend"
        assert config["suites"][1]["name"] == "frontend"

    def test_missing_config_file_exits(self, tmp_path, monkeypatch):
        """Missing chief.toml causes sys.exit(1)."""
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        with pytest.raises(SystemExit) as exc_info:
            chief.load_config()
        assert exc_info.value.code == 1

    def test_empty_suites_array_exits(self, tmp_path, monkeypatch):
        """Config with empty suites array causes sys.exit(1)."""
        config_content = 'suites = []'
        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        with pytest.raises(SystemExit) as exc_info:
            chief.load_config()
        assert exc_info.value.code == 1

    def test_missing_suites_key_exits(self, tmp_path, monkeypatch):
        """Config without suites key causes sys.exit(1)."""
        config_content = 'some_other_key = "value"'
        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        with pytest.raises(SystemExit) as exc_info:
            chief.load_config()
        assert exc_info.value.code == 1

    @pytest.mark.parametrize("missing_key", [
        "name",
        "language",
        "framework",
        "test_root",
        "test_command",
        "target_type",
    ])
    def test_missing_required_key_exits(self, tmp_path, monkeypatch, missing_key):
        """Each required suite key is validated - missing any causes exit."""
        base_suite = {
            "name": "test",
            "language": "Python",
            "framework": "pytest",
            "test_root": ".",
            "test_command": "pytest",
            "target_type": "file",
        }
        del base_suite[missing_key]

        # Build TOML manually
        lines = ["[[suites]]"]
        for k, v in base_suite.items():
            lines.append(f'{k} = "{v}"')
        config_content = "\n".join(lines)

        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")
        monkeypatch.setattr(chief, 'LOG_FILE', None)

        with pytest.raises(SystemExit) as exc_info:
            chief.load_config()
        assert exc_info.value.code == 1

    def test_optional_keys_get_defaults(self, tmp_path, monkeypatch):
        """Optional keys not present get default values."""
        config_content = '''
[[suites]]
name = "test"
language = "Python"
framework = "pytest"
test_root = "."
test_command = "pytest"
target_type = "file"
'''
        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")

        config = chief.load_config()
        suite = config["suites"][0]

        # Verify defaults are set
        assert suite["default_target"] == "."
        assert suite["file_patterns"] == []
        assert suite["disallow_write_globs"] == []
        assert suite["test_init"] is None
        assert suite["test_setup"] is None
        assert suite["post_green_command"] is None
        assert suite["env"] == {}

    def test_explicit_optional_values_preserved(self, tmp_path, monkeypatch):
        """Explicitly set optional values are preserved, not overwritten by defaults."""
        config_content = '''
[[suites]]
name = "test"
language = "Python"
framework = "pytest"
test_root = "src/"
test_command = "pytest {target}"
target_type = "file"
default_target = "tests/"
file_patterns = ["test_*.py"]
test_init = "pip install -e ."
env = { DEBUG = "1" }
'''
        (tmp_path / "chief.toml").write_text(config_content)
        monkeypatch.chdir(tmp_path)

        import chief
        monkeypatch.setattr(chief, 'CONFIG_FILE', "chief.toml")

        config = chief.load_config()
        suite = config["suites"][0]

        assert suite["default_target"] == "tests/"
        assert suite["file_patterns"] == ["test_*.py"]
        assert suite["test_init"] == "pip install -e ."
        assert suite["env"] == {"DEBUG": "1"}
