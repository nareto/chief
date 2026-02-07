You are processing new requirements into todos.

Tasks:
1. Inspect the existing codebase.
2. Scaffold missing baseline structure only if needed.
3. Update chief.toml if required.
4. Update {{ todos_path }} with granular implementation-ready todos.

Todo quality constraints:
- Follow todos.json.example schema.
- Todo text should start from user value/impact.
- Each todo must be self-contained.
- Expectations should include concrete assertions suitable for tests.
- Assign priority where 100 means immediate work.

Requirements:
{{ requirements_text }}
