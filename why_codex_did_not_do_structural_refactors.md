# Why Codex Did Not Do Structural Refactors

1. The guardrails were strict: no observable behavior changes, no interface changes, no logic changes, and "when in doubt, don't make it."
2. The structural issues identified (for example long functions, many-argument APIs, decomposition opportunities) are cross-cutting refactors and are not guaranteed no-ops.
3. Structural refactors in this codebase would require touching call graphs and error-flow boundaries, which can introduce subtle runtime behavior changes even when tests still pass.
4. A safe structural pass should be scoped separately, with stronger characterization tests and explicit agreement on acceptable refactor risk.
