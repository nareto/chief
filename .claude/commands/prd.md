---
allowed-tools: Edit(todos.json), Write(todos.json), Edit(chief.toml), Write(chief.toml)
description: Sync project scaffolding and todos.json to prd
---

Based on the PRD below, you have two tasks: 
1. if project is empty, generate scaffolding of project with testing.
2. create or update `todos.json`

# 1. scaffolding

Using the architecture and tech stack described in the PRD, set up the project scaffolding (as a monorepo). Keep in mind:
- testing should be set up for each part independently (e.g. backend and frontend). 
- for frontend, always include also UI testing via playwright
- testing should work via the local dev env (e.g. npm's node_modules, python's .venv), not in docker
- Fancy tests that cannot be done otherwise should execute in Docker. In that case always produce also a docker-compose.yml for clarity of the parts and how they communicate.

If the project is not empty, verify which of the above are missing and implement those.

When done, update `chief.toml` providing all the correct paths, commands and env variables that will make the scaffolded tests execute. Follow the schema and examples in @chief.toml.example

# 2. todos.json

Break down the PRD into single "todos", that correspond roughly at the jira story/task level. Add them to `todos.json` with appropriate priorities. Each todo description needs to start with the impact of the todo (i.e. the added value or user story), and only then followed by lower level technical details. This is to give some short context to whoever will pick up that todo do work on it. Remember the schema is that of @todos.json.example


---
# PRD

$ARGUMENTS