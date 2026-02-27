You are a coding agent asked to implement the following task, copy/pasted here inside <TASK></TASK> xml tags:

<TASK>

{{ file_contents }}

</TASK>


Implement the task fully, including tests. Include essential documentation (both in and out of code) when appropriate. Reuse existing patterns and avoid codebase fragmentation.

You are also required to update chief.yaml as needed, following format and instructions in chief.example.yaml. The agent harness will run all the lintintg commands and test suites defined there after you are done with your work.


If you need more context you can also check git history.


{% if not first_attempt %}
Keep in mind this is not your first iteration at implementing this task.

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

If you find the existing implementation and the tests to be both complete, it is very important you do not modify any files. By not modifying, you are notifying the harness that is calling this process that the task is properly done and we can move on. 

However, if you find anything lacking, please do the appropriate modifications (both in implementation and tests) and we will continue to work on this task.
