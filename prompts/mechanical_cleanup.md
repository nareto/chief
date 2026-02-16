Your task is to review the whole codebase and clean it up, without changing observable behaviour. 

Do not do any major refactoring, but rather limit yourself to mechanical fixes:

- security naivety: no input validation, secrets like API keys hard-coded
- reinventing the wheel: custom code that could be done with the standard library
- style clashing: e.g. snake_case and camelCase
- dead code: unused imports, unreachable branches, functions or variables that are never used
- inconsistent error handling: mixing try/catch with returning errors, silently swallowing exceptions

The result should in principle adhere to best clean code principles (but do not obsess over this):
- Meaningful names that accurately represent the actual role in the code logic
- SRP: single responsibility
- DRY: avoid duplication
- KISS: avoid over-complication
- Testability: code should be testable (and tested)

Guardrails you NEED to respect:
- Ensure all tests pass 
- Do not modify existing tests, unless there is no other way 
- Do not alter outward-facing interfaces like API routes, CLI flags, config schema, exported symbols, DB schema...
- Do not alter program logic
- Do not alter UX flow


If you find the existing implementation to be sufficinetly clean, it is very important you do not modify any files. By not modifying, you are notifying the harness that is calling this process that the todo is properly done and we can move on. 

When in doubt about a change, don't make it.