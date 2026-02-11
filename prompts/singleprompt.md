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
Your previous work did not pass lint. Use sqlite command on chief.db to get the full output.
{% for failure in lint_failures %}
COMMAND: {% if failure.command %}`{{ failure.command }}`{% else %}(The command that failed was not captured.){% endif %}
OUTPUT TAIL:
{{ failure.output_tail }}
SQLITE QUERY: `{{ failure.sqlite_query }}`
{% if not loop.last %}
{% endif %}
{% endfor %}
{% endif %}

{% if failed_test %}
Your previous work did not pass tests. Use sqlite command on chief.db to get the full output.
{% for failure in test_failures %}
COMMAND: {% if failure.command %}`{{ failure.command }}`{% else %}(The command that failed was not captured.){% endif %}
OUTPUT TAIL:
{{ failure.output_tail }}
SQLITE QUERY: `{{ failure.sqlite_query }}`
{% if not loop.last %}

{% endif %}
{% endfor %}
{% endif %}

{% if failed_other %}
Your previous work had the following failures. Use sqlite command on chief.db to get the full output. 
{% for failure in other_failures %}
EVENT TYPE: `{{ failure.event_type }}`
MESSAGE: {{ failure.message }}
COMMAND: {% if failure.command %}`{{ failure.command }}`{% else %}(The command that failed was not captured.){% endif %}
OUTPUT TAIL:
{{ failure.output_tail }}
SQLITE QUERY: `{{ failure.sqlite_query }}`
{% if not loop.last %}
{% endif %}
{% endfor %}
{% endif %}
{% if not failed_lint and not failed_test and not failed_other %}


---
After some checks, we just learned that the work done in previous iterations passed all linting and tests. In this case, your task changes slightly: check that the files touched by previous iterations do indeed satisfy the todo. 

If you find the implementation and the tests to be both complete, it is very important you do not modify any files. By not modifying, you are notifying the harness that is calling this process that the todo is properly done and we can move on. 

However, if you find anything lacking, please do the appropriate modifications (both in implementation and tests) and we will continue to work on this todo.
{% endif %}
{% endif %}
