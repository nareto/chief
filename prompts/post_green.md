We are in POST_GREEN phase for this todo.

TODO:
{{ todo.todo }}

Post-green commands:
{% for command in post_green_commands %}
- {{ command }}
{% endfor %}

Previous steps log:
{{ previous_steps_log }}

Instructions:
- Fix failures from post-green checks and linting.
- Keep tests passing while resolving build/typecheck/lint issues.
