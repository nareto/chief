---
allowed-tools: Edit(todos.yaml), Write(todos.yaml), Bash(git log:*), Bash(head:*)
description: Repriorities todos
---

Based on the current state of the project, reprioritise the the todos in todos.yaml (100 is the highest priority, 0 the lowest). Remember the schema is that of @todos.example.yaml

Last 20 commits: !`git log --oneline | head -n 20`
