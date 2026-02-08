set shell := ["bash", "-euo", "pipefail", "-c"]

build:
    docker compose build frontend

sync-frontend-deps:
    @LOCK_HASH="$$(sha256sum frontend/package-lock.json | awk '{print $$1}')"; STAMP_PATH="frontend/node_modules/.package-lock.sha256"; STAMP_HASH="$$(cat "$$STAMP_PATH" 2>/dev/null || true)"; if [ ! -d frontend/node_modules ] || [ "$$LOCK_HASH" != "$$STAMP_HASH" ]; then docker compose run --rm --no-deps frontend npm ci; mkdir -p frontend/node_modules; printf '%s\n' "$$LOCK_HASH" > "$$STAMP_PATH"; fi

build-chief:
    cargo build --bin chief

up:
    just dev

down:
    docker compose down

logs:
    docker compose logs -f frontend

backend:
    cargo run --bin chief_backend -- --parent-dir "${CHIEF_PROJECTS_PARENT:-../../}" --host 0.0.0.0 --port 8000 --enable-terminal --allow-origin http://localhost:3000

dev:
    just down
    just sync-frontend-deps
    docker compose up -d frontend
    just backend
