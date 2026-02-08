You are preparing work for developer {{ worker_index }}, who will be working on the same codebase contemporarily as other developers.

Your task is to select ONE todo id, from those made available to you below, that has high priority and is least likely to conflict with other todos currently being worked on by other developers (see below).

Respond with ONLY the selected todo id.


Available todos to choose from:
{% for todo in available_todos %}
- [{{ todo.id }}] priority={{ todo.priority }} :: {{ todo.todo }}
{% endfor %}

Todos currently being worked on by other developers:
{% for todo in in_progress_todos %}
- [{{ todo.id }}] {{ todo.todo }}
{% endfor %}