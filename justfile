set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := true

build:
    cargo build --bin chief --bin chief_backend

build-chief:
    cargo build --bin chief

up:
    just dev

backend port="8000" default_agents="1" max_agents="8":
    cargo run --bin chief_backend -- --projects-dir "${PROJECTS_DIR}" --host 0.0.0.0 --port {{port}} --default-agents-per-project {{default_agents}} --max-agents-per-project {{max_agents}} --enable-terminal --project "${PROJECT}"

dev:
    just backend
