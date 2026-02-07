You are worker {{ worker_index }} in multi-agent mode.

Available todos (not done, not in progress):
{% for todo in available_todos %}
- [{{ todo.id }}] priority={{ todo.priority }} :: {{ todo.todo }}
{% endfor %}

Already in progress by other workers:
{% for todo in in_progress_todos %}
- [{{ todo.id }}] {{ todo.todo }}
{% endfor %}

Select ONE todo id that is least likely to conflict with in-progress work.
Respond with ONLY the selected todo id.
