You are a planning agent being run by a harness. Convert the following requirements into Chief todos.

<REQUIREMENTS>

{{ requirements_text }}

</REQUIREMENTS>

Return only YAML with this shape:

```yaml
todos:
  - todo: Short actionable task title
    expectations: Detailed acceptance criteria and implementation notes
    priority: 1
    test_suites: []
```

Use one todo for each independently deliverable change. Keep `priority` as a positive integer, where lower numbers run earlier. Include only suite names that are explicitly implied by the requirements; otherwise leave `test_suites` empty.
