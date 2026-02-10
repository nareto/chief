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
- `todos.yaml` for the list of previous and future todos


{% if not first_attempt %}
Keep in mind this is not your first iteration at implementing this todo.

{% if touched_files_since_last_retry_reset %}
The files touched by your previous iterations are:
{% for path in touched_files_since_last_retry_reset %}
- `{{ path }}`
{% endfor %}
{% endif %}


{% if failed_lint %}
Your previous work did not pass lint. 
{% for failure in lint_failures %}
Command:
{% if failure.command %}
`{{ failure.command }}`
{% else %}
(The command that failed was not captured.)
{% endif %}
Output (tail only):
{{ failure.output_tail }}
{% if not loop.last %}
{% endif %}
{% endfor %}

If you need the full outputs, run the queries below with `sqlite3 chief.db "<query>"`.

This query returns the most recent failed lint event for this todo in this run by event time (with `id` as tie-breaker), including full payload and command/output details.
`SELECT id,timestamp,phase,msg,payload FROM events WHERE run_id='{{ run_id }}' AND todo_id='{{ todo.id }}' AND event_type='lint' AND CAST(json_extract(payload,'$.exit_code') AS INTEGER) != 0 ORDER BY timestamp DESC, id DESC LIMIT 1;`
{% endif %}

{% if failed_test %}
Your previous work did not pass tests.
{% for failure in test_failures %}
Command:
{% if failure.command %}
`{{ failure.command }}`
{% else %}
(The command that failed was not captured.)
{% endif %}
Output (tail only):
{{ failure.output_tail }}
{% if not loop.last %}

{% endif %}
{% endfor %}

If you need the full outputs, run the queries below with `sqlite3 chief.db "<query>"`.

This query returns the most recent failed test event for this todo in this run by event time (with `id` as tie-breaker), including full payload and command/output details.
`SELECT id,timestamp,phase,msg,payload FROM events WHERE run_id='{{ run_id }}' AND todo_id='{{ todo.id }}' AND event_type='test_run' AND CAST(json_extract(payload,'$.exit_code') AS INTEGER) != 0 ORDER BY timestamp DESC, id DESC LIMIT 1;`
{% endif %}
{% if not failed_lint and not failed_test %}
After some checks, we just learned that the work done in previous iterations passed all linting and tests. In this case, your task changes slightly: check that the files touched by previous iterations do indeed satisfy the todo. 

If you find the implementation and the tests to be both complete, it is very important you do not modify any files. By not modifying, you are notifying the harness that is calling this process that the todo is properly done and we can move on. 

However, if you find anything lacking, please do the appropriate modifications (both in implementation and tests) and we will continue to work on this todo.
{% endif %}
{% endif %}
