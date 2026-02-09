set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := true

build:
    docker compose build frontend

sync-frontend-deps:
    #!/bin/bash
    set -euo pipefail

    LOCK_HASH="$(sha256sum frontend/package-lock.json | awk '{print $1}')"
    STAMP_PATH="frontend/node_modules/.package-lock.sha256"
    STAMP_HASH="$(cat "$STAMP_PATH" 2>/dev/null || true)"

    if [ ! -d frontend/node_modules ] || [ "$LOCK_HASH" != "$STAMP_HASH" ]; then
        docker compose stop frontend >/dev/null 2>&1 || true
        docker compose run --rm --no-deps frontend npm ci
        mkdir -p frontend/node_modules
        printf '%s\n' "$LOCK_HASH" > "$STAMP_PATH"
    fi

build-chief:
    cargo build --bin chief

up:
    just dev

down:
    docker compose down

logs:
    docker compose logs -f frontend

frontend:
    just sync-frontend-deps
    docker compose up -d --no-deps --force-recreate frontend

backend port="8000" default_agents="1" max_agents="8" frontend_dir="frontend":
    cargo run --bin chief_backend -- --projects-dir "${PROJECTS_DIR}" --host 0.0.0.0 --port {{port}} --frontend-dir "{{frontend_dir}}" --default-agents-per-project {{default_agents}} --max-agents-per-project {{max_agents}} --enable-terminal --allow-origin http://localhost:3000 --allow-origin "http://${FRONTEND_HOST}:3000" --project "${PROJECT}"

dev:
    just frontend

dev-full:
    just down
    just frontend
    just backend
