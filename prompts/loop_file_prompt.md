You are a coding agent being run by a harness. Your goal is to implement the following task, copy/pasted here inside <TASK></TASK> xml tags:

<TASK>

{{ file_contents }}

</TASK>


Implement the task fully, including tests. Include essential documentation (both in and out of code) when appropriate. Reuse existing patterns and avoid codebase fragmentation.

If needed, update chief.yaml, following format and instructions in chief.example.yaml. The harness will run all the linting commands and test suites defined there after you are done with your work. Please keep in mind that these tests passing is no measure of completeness of the task. The only source of truth is the difference between the task specified above and the existing codebase.

If you find the existing implementation and the tests to satisfy the task at 100%, in all details, it is very important you do not modify any files. By not modifying, you are notifying the harness that is calling this process that the task is properly done and we can move on. 

When in doubt, do the modifications.

When done with your work, commit all changes.


{% if not first_attempt %}
Please keep in mind this is not your first iteration at implementing this task.

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


{% endif %}
{% endif %}

