Your task is to review the whole codebase and refactor as needed, without changing observable behaviour. 

Look out for structural fixes: break up oversized files, extract cohesive modules and subsystems, rename for clarity, eliminate duplication across module boundaries, and simplify tangled code paths. More examples of stuff to fix: 
    - unhealthy length: functions, classes or files that are too long and should be broken up into multiple parts. Approximate target (not hard rules): files at most ~500 lines, functions at most ~80 lines
    - spaghetti code: unclear, intricated codepaths
    - bad folder structure: no folder hierarchy, no grouping of sub-systems
    - codebase fragmentation: mix of different, overlapping patterns or styles 
    - code duplication
    - library fragmentation: use of multiple libraries that offer similar feature sets in different places of the codebase
    - boilerplate overload: excessive use of design patterns where they aren't needed
    - project structure clash: conflicting patterns for directory and modules structure or file naming
    - mock data leftovers: hard-coded variables, unfinished "TODO" comments

The result should in principle adhere to best clean code principles (but do not obsess over this):
- Clean and meaningful folder structure and file names
- Meaningful names that accurately represent the actual role in the code logic
- SRP: single responsibility
- DRY: avoid duplication
- KISS: avoid over-complication
- Testability: code should be testable (and tested)


Guardrails you NEED to respect:
- Ensure all tests pass 
- You may update test imports, module paths, and helper locations to match new structure, but do not change what any test asserts or the scenarios it covers.
- Do not alter outward-facing interfaces like API routes, CLI flags, config schema,  DB schema...
- Do not change what the code does (but you are free to change how it's organized)
- Do not alter UX flow

If you find the existing implementation to be sufficiently clean, it is very important you do not modify any files. By not modifying, you are notifying the harness that is calling this process that the todo is properly done and we can move on. 
