Your task is to review the whole codebase and clean it up, without changing observable behaviour. Look out for:

- mechanical fixes:
    - style clashing: e.g. snake_case and camelCase
    - dead code: unused imports, unreachable branches, functions or variables that are never used
    - inconsistent error handling: mixing try/catch with returning errors, silently swallowing exceptions
- structural fixes:
    - security naivety: no input validation, secrets like API keys hard-coded
    - spaghetti code: unclear, intricated codepaths
    - codebase fragmentation: mix of different, overlapping patterns or styles 
    - code duplication
    - library fragmentation: use of multiple libraries that offer similar feature sets in different places of the codebase
    - reinventing the wheel: custom code that could be done with the standard library
    - boilerplate overload: excessive use of design patterns where they aren't needed
    - project structure clash: conflicting patterns for directory and modules structure or file naming
    - mock data leftovers: hard-coded variables, unfinished "TODO" comments


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

When in doubt about a change, don't make it.