We are in RED phase for this todo.

TODO:
{{ todo.todo }}

Expectations:
{{ todo.expectations }}

Suites:
{% for suite in suites %}
- {{ suite.name }}
  language: {{ suite.language }}
  framework: {{ suite.framework }}
  test_root: {{ suite.test_root }}
  test_command: {{ suite.test_command }}
{% endfor %}

Previous steps log:
{{ previous_steps_log }}

Instructions:
- First inspect existing tests and patterns.
- Add or refine tests for happy path, edge cases, invalid input, and security-relevant behavior where applicable.
- Prefer existing conventions and fixtures.
- Do not implement production behavior in this phase.
- If no test changes are needed, respond exactly: NO CHANGES
