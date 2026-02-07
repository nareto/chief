We are doing a TDD development cycle for the todo described below and are currently in POST-GREEN phase: the green phase is complete and it passed the tests from red phase, but the final "post green" validation commands described below failed. Please do whatever modifications are necessary to make these commands run succesfully.

Below you will find:
- information on the specific todo
- the post green commands that failed
- a log of previous development cycles tackling this same task.

---
TODO INFO:
task: {{ todo.todo }}
expectations: {{ todo.expectations }}

POST GREEN COMMANDs:
{% for command in post_green_commands %}
{{ command }}
{% if not loop.last %}

{% endif %}
{% endfor %}

LOG:
{{ previous_steps_log }}
