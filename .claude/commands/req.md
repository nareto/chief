---
allowed-tools: Edit(todos.json), Write(todos.json)
description: Add requirement
---


The following are new requirements for the project:

$ARGUMENTS

Your task is to break them down into single "todos", that correspond roughly at the jira story/task level. Add them to `todos.json` with appropriate priorities. Each todo description needs to start with the impact of the todo (i.e. the added value or user story), and only then followed by lower level technical details. This is to give some short context to whoever will pick up that todo do work on it. Remember the schema is that of @todos.json.example

After you added the todos, commit the changes (only for the todos.json file, ignore other unstaged changes in the repo)