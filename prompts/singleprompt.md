You are a coding agent asked to implement the following todo in this codebase.

TODO INFO:
id: {{ todo.id }}
todo: {{ todo.todo }}
expectations: {{ todo.expectations }}

Implement the task fully, including relevant tests. Reuse existing patterns and avoid codebase fragmentation.

The available test suites for this codebase are:

TEST SUITES INFO:
{% for suite in suites %}
name: {{ suite.name }}
test_root: {{ suite.test_root }}
{% if not loop.last %}

{% endif %}
{% endfor %}

If you need more context you can check:
- git history
- `todos.json` for the list of previous and future todos

{% if not first_attempt %}
Keep in mind this is not your first attempt at this todo.

{% if failed_lint %}
Your previous attempts did not pass lint. The tail of the lint output is:
{{ lint_tail_output }}

If you need the full outputs, run the queries below with `sqlite3 chief.db "<query>"`.

This query returns the most recent failed lint event for this todo in this run by event time (with `id` as tie-breaker), including full payload and command/output details.
`SELECT id,timestamp,phase,msg,payload FROM events WHERE run_id='{{ run_id }}' AND todo_id='{{ todo.id }}' AND event_type='lint' AND CAST(json_extract(payload,'$.exit_code') AS INTEGER) != 0 ORDER BY timestamp DESC, id DESC LIMIT 1;`
{% endif %}

{% if failed_test %}
Your previous attempts did not pass tests. The tail of the test output is:
{{ test_tail_output }}

If you need the full outputs, run the queries below with `sqlite3 chief.db "<query>"`.

This query returns the most recent failed test event for this todo in this run by event time (with `id` as tie-breaker), including full payload and command/output details.
`SELECT id,timestamp,phase,msg,payload FROM events WHERE run_id='{{ run_id }}' AND todo_id='{{ todo.id }}' AND event_type='test_run' AND CAST(json_extract(payload,'$.exit_code') AS INTEGER) != 0 ORDER BY timestamp DESC, id DESC LIMIT 1;`
{% endif %}
{% if not failed_lint and not failed_test %}
The previous attempts passed all linting and tests. Your job is then to verify that both the implementation and the tests are complete, fully covering the requested todo and its expectations, and properly integrating into the existing codebase.

{% endif %}
{% endif %}
