You are a coding agent asked to implement the following todo in this codebase:

TODO INFO:
id: {{ todo.id }}
todo: {{ todo.todo }}
expectations: {{ todo.expectations }}

Implement the task fully, including relevant tests. Reuse what is already there, follow established patterns. Your goal is to implement the task while avoiding codebase fragmentation. 

The available test suites for this codebase are:

TEST SUITES INFO:
{% for suite in suites %}
name: {{ suite.name }}
test_root: {{ suite.test_root }}
{% endfor %}

If you need more context you can check:
- git history
- `todos.json` for the list of previous and future todos
- logs of preivous agentic runs in the sqlite file `chief.db` 

EXAMPLES OF SQLITE QUERIES:
//todo: 2-3 example queries with brief explanation to showcase schema

{% if not first_attempt %}
Keep in mind this is not your first attempt at this todo. 

{% if failed_lint %}
Your previous attempts did not pass the lint check. The tail of the linting output is:
{{ lint_tail_ouput }}

For the full output you can run this sqlite query on chief.db:
//todo: query formatted via jinja with the proper ids to find the exact lint output


{% elif failed_test %}
Your previous attempts did not pass the tests. The tail of the test output is:
{{ test_tail_ouput }}

For the full output you can run this sqlite query on chief.db:
//todo: query formatted via jinja with the proper ids to find the exact test output

{% else %}
The previous attempts passed all linting and tests. Your job is then to verify that both the implementation and the tests are complete, fully covering the requested todo and its expectations, and properly integrating into the existing codebase.


{% endif %}