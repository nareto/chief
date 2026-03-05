use std::path::{Path, PathBuf};

pub const CHIEF_DIR_NAME: &str = ".chief";
pub const CHIEF_DB_FILE_NAME: &str = "chief.db";
pub const CHIEF_YAML_FILE_NAME: &str = "chief.yaml";
pub const CHIEF_EXAMPLE_FILE_NAME: &str = "chief.example.yaml";

pub fn chief_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(CHIEF_DIR_NAME)
}

pub fn chief_db_path(project_dir: &Path) -> PathBuf {
    chief_dir(project_dir).join(CHIEF_DB_FILE_NAME)
}

pub fn chief_yaml_path(project_dir: &Path) -> PathBuf {
    chief_dir(project_dir).join(CHIEF_YAML_FILE_NAME)
}

pub fn chief_example_path(project_dir: &Path) -> PathBuf {
    chief_dir(project_dir).join(CHIEF_EXAMPLE_FILE_NAME)
}

pub fn legacy_root_file_path(project_dir: &Path, file_name: &str) -> PathBuf {
    project_dir.join(file_name)
}

pub fn chief_relative_path(file_name: &str) -> String {
    format!("{CHIEF_DIR_NAME}/{file_name}")
}
