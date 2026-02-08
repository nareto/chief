set shell := ["bash", "-euo", "pipefail", "-c"]

build:
    docker compose build

build-chief:
    cargo build --bin chief

up:
    docker compose up -d

down:
    docker compose down

logs:
    docker compose logs -f

dev:
    just down
    docker compose build backend
    docker compose up -d backend frontend
    just up
    just logs
