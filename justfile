set shell := ["bash", "-euo", "pipefail", "-c"]

build:
    docker compose build frontend

build-chief:
    cargo build --bin chief

up:
    docker compose up -d frontend

down:
    docker compose down

logs:
    docker compose logs -f frontend

backend:
    cargo run --bin chief_backend -- --parent-dir "${CHIEF_PROJECTS_PARENT:-./projects}" --host 0.0.0.0 --port 8000 --enable-terminal --allow-origin http://localhost:3000

dev:
    just down
    docker compose build frontend
    docker compose up -d frontend
    just backend
