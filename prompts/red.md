We are doing TDD development cycle for the todo described below and are currently in RED phase. Complete the tests for this task - some might already be present, and actually they even might already be sufficient. The functionality is not there yet, so the tests are supposed to fail.

The tests you write should be:
- comprehensive (cover all required functionality, across all the mentioned test suites)
- cover both happy path and edge cases
- cover invalid/missing input
- cover security (if applicable)

Furthermore, before jumping in the tests, you should gather any additional context you need on the codebase and other tests. Reuse what is already there, follow established patterns. Your goal is to implement the task while avoiding codebase and test fragmentation.

If you find out that no modifications are required to achieve the task, output simply "NO CHANGES" (no quotes).

Below you will find:
- information on the specific todo
- the test suites affected by this task, for each of which we most probably need tests
- a log of previous development cycles tackling this same task.

---
TODO INFO:
task: {{ todo.todo }}
expectations: {{ todo.expectations }}

TEST SUITES INFO:
{% for suite in suites %}
name: {{ suite.name }}
test_root: {{ suite.test_root }}
{% if not loop.last %}

{% endif %}
{% endfor %}

LOG:
{{ previous_steps_log }}
