---
allowed-tools: Edit(todos.json), Write(todos.json), Bash(git log:*), Bash(head:*)
description: Repriorities todos
---

Based on the current state of the project, reprioritise the the todos in todos.json (100 is the highest priority, 0 the lowest). Remember the schema is that of @todos.json.example

Last 20 commits: !`git log --oneline | head -n 20`