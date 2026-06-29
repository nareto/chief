#!/usr/bin/env bash
set -euo pipefail

PROMPT=""

if [[ -z "${PROMPT//[[:space:]]/}" ]]; then
  printf 'Set PROMPT at the top of %s first.\n' "$0" >&2
  exit 64
fi

chief \
  --project-dir . \
  --agent codex \
  --mcp-servers '{}' \
  --respect-limits true \
  --max-loop-iterations 8 \
  --required-stable-iterations 2 \
  --agent-timeout-seconds 2700 \
  --prompt "$PROMPT"
