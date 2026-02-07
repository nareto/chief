You are worker {{ worker_index }} in multi-agent mode.

Available todos (not done and not in progress):
{% for todo in available_todos %}
- [{{ todo.id }}] priority={{ todo.priority }} :: {{ todo.todo }}
{% endfor %}

Todos currently in progress by other workers:
{% for todo in in_progress_todos %}
- [{{ todo.id }}] {{ todo.todo }}
{% endfor %}

Select one todo that can be developed with the lowest merge-conflict risk.
Return only the selected todo id and nothing else.
